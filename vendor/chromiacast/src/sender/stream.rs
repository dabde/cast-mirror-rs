use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use bytes::BytesMut;

use crate::constants::{MAX_UNACKED_FRAMES, RTP_STANDARD_HEADER_SIZE};
use crate::crypto::FrameCrypto;
use crate::error::EnqueueError;
use crate::frame::{EncodedFrame, FrameDependency, FrameId};
use crate::ntp::NtpTimestamp;
use crate::rtcp::{AckBitvector, ReceiverReferenceTime, ReceiverReport};
use crate::rtp::{RtpPacket, RtpPacketizer};
use crate::sync_source::SyncSource;

use super::stats::StreamStatistics;

const SENDER_REPORT_HISTORY: usize = 16;
const MINIMUM_IN_FLIGHT_MEDIA_DURATION: Duration = Duration::from_millis(66);

/// Per-stream frame, retransmission, timing, and feedback state.
pub(crate) struct StreamState {
    crypto: FrameCrypto,
    packetizer: RtpPacketizer,
    sender_sync_source: SyncSource,
    receiver_sync_source: SyncSource,
    rtp_timebase: u32,
    target_playout_delay: Duration,
    adaptive_playout_delay: bool,
    pending_playout_delay: Option<Duration>,

    next_frame_id: FrameId,
    checkpoint: FrameId,
    pending: Box<[Option<PendingFrame>; MAX_UNACKED_FRAMES]>,

    last_media_timestamp: Option<Duration>,
    last_reference_time: Option<Instant>,
    last_rtp_timestamp: u32,

    rtp_packets_sent: u64,
    rtp_octets_sent: u64,
    packets_retransmitted: u64,
    frames_acked: u64,
    frames_dropped_or_skipped: u64,
    nack_count: u64,
    receiver_playout_delay: Option<Duration>,
    current_rtt: Option<Duration>,
    fraction_lost: Option<u8>,
    cumulative_packets_lost: Option<i32>,
    highest_sequence_number: Option<u32>,
    jitter: Option<u32>,
    last_ack_feedback_count: Option<u8>,
    last_ack_bitvector_bytes: Option<usize>,
    receiver_reference_time: Option<SystemTime>,
    sender_reports: VecDeque<SenderReportRecord>,

    send_queue: VecDeque<QueuedPacket>,
    retransmit_queue: VecDeque<QueuedPacket>,
}

struct PendingFrame {
    frame_id: FrameId,
    media_timestamp: Duration,
    duration: Option<Duration>,
    packets: Vec<RtpPacket>,
}

struct QueuedPacket {
    frame_id: FrameId,
    packet: RtpPacket,
}

struct SenderReportRecord {
    compact_ntp: u32,
    sent_at: Instant,
}

pub(crate) struct OutboundPacket {
    pub packet: RtpPacket,
    pub retransmission: bool,
}

pub(crate) struct StreamParameters {
    pub sender_sync_source: SyncSource,
    pub receiver_sync_source: SyncSource,
    pub rtp_payload_type: u8,
    pub rtp_timebase: u32,
    pub target_playout_delay: Duration,
    pub adaptive_playout_delay: bool,
    pub aes_key: [u8; 16],
    pub aes_iv_mask: [u8; 16],
    pub max_packet_size: usize,
}

impl StreamState {
    pub fn new(parameters: StreamParameters) -> Self {
        Self {
            crypto: FrameCrypto::new(parameters.aes_key, parameters.aes_iv_mask),
            packetizer: RtpPacketizer::new(
                parameters.sender_sync_source,
                parameters.rtp_payload_type,
                parameters.max_packet_size,
            ),
            sender_sync_source: parameters.sender_sync_source,
            receiver_sync_source: parameters.receiver_sync_source,
            rtp_timebase: parameters.rtp_timebase,
            target_playout_delay: parameters.target_playout_delay,
            adaptive_playout_delay: parameters.adaptive_playout_delay,
            pending_playout_delay: None,
            next_frame_id: FrameId::first(),
            checkpoint: FrameId::first() + -1,
            pending: Box::new(std::array::from_fn(|_| None)),
            last_media_timestamp: None,
            last_reference_time: None,
            last_rtp_timestamp: 0,
            rtp_packets_sent: 0,
            rtp_octets_sent: 0,
            packets_retransmitted: 0,
            frames_acked: 0,
            frames_dropped_or_skipped: 0,
            nack_count: 0,
            receiver_playout_delay: None,
            current_rtt: None,
            fraction_lost: None,
            cumulative_packets_lost: None,
            highest_sequence_number: None,
            jitter: None,
            last_ack_feedback_count: None,
            last_ack_bitvector_bytes: None,
            receiver_reference_time: None,
            sender_reports: VecDeque::with_capacity(SENDER_REPORT_HISTORY),
            send_queue: VecDeque::new(),
            retransmit_queue: VecDeque::new(),
        }
    }

    pub fn enqueue_frame(&mut self, frame: EncodedFrame) -> Result<FrameId, EnqueueError> {
        if frame.data.is_empty() {
            return self.reject_frame(EnqueueError::EmptyFrame);
        }
        if self.next_frame_id == FrameId::first() && frame.dependency != FrameDependency::KeyFrame {
            return self.reject_frame(EnqueueError::FirstFrameMustBeKeyFrame);
        }
        if self
            .last_media_timestamp
            .is_some_and(|last| frame.media_timestamp <= last)
        {
            return self.reject_frame(EnqueueError::NonMonotonicMediaTimestamp);
        }
        if self.in_flight_id_span() >= MAX_UNACKED_FRAMES {
            return self.reject_frame(EnqueueError::ReachedIdSpanLimit);
        }

        let frame_id = self.next_frame_id;
        let is_key = frame.dependency == FrameDependency::KeyFrame;
        let rtp_timestamp = media_to_rtp_timestamp(frame.media_timestamp, self.rtp_timebase);
        let referenced_frame_id = if is_key { frame_id } else { frame_id + -1 };

        let mut data = BytesMut::from(frame.data.as_ref());
        self.crypto.encrypt(frame_id, &mut data);
        let packets = self.packetizer.packetize(
            frame_id,
            is_key,
            referenced_frame_id,
            rtp_timestamp,
            self.pending_playout_delay.take(),
            &data.freeze(),
        );
        self.send_queue.extend(
            packets
                .iter()
                .cloned()
                .map(|packet| QueuedPacket { frame_id, packet }),
        );

        let slot = frame_id.raw() as usize % MAX_UNACKED_FRAMES;
        self.pending[slot] = Some(PendingFrame {
            frame_id,
            media_timestamp: frame.media_timestamp,
            duration: frame.duration,
            packets,
        });

        self.last_media_timestamp = Some(frame.media_timestamp);
        self.last_reference_time = Some(frame.reference_time);
        self.last_rtp_timestamp = rtp_timestamp;
        self.next_frame_id = frame_id.next();
        Ok(frame_id)
    }

    fn reject_frame(&mut self, error: EnqueueError) -> Result<FrameId, EnqueueError> {
        self.frames_dropped_or_skipped = self.frames_dropped_or_skipped.saturating_add(1);
        Err(error)
    }

    /// True until the first Receiver Report yields a round-trip time.
    pub fn awaiting_receiver_report(&self) -> bool {
        self.current_rtt.is_none()
    }

    pub fn next_frame_id(&self) -> FrameId {
        self.next_frame_id
    }

    pub fn supports_adaptive_playout_delay(&self) -> bool {
        self.adaptive_playout_delay
    }

    pub fn set_target_playout_delay(&mut self, delay: Duration) {
        debug_assert!(self.adaptive_playout_delay);
        self.target_playout_delay = delay;
        self.pending_playout_delay = Some(delay);
    }

    pub fn in_flight_count(&self) -> usize {
        self.pending.iter().flatten().count()
    }

    fn in_flight_id_span(&self) -> usize {
        (self.next_frame_id - (self.checkpoint + 1)) as usize
    }

    pub fn in_flight_media_duration(&self) -> Duration {
        let mut oldest = None::<Duration>;
        let mut newest_end = None::<Duration>;
        for frame in self.pending.iter().flatten() {
            oldest = Some(oldest.map_or(frame.media_timestamp, |oldest| {
                oldest.min(frame.media_timestamp)
            }));
            let end = frame
                .duration
                .and_then(|duration| frame.media_timestamp.checked_add(duration))
                .unwrap_or(frame.media_timestamp);
            newest_end = Some(newest_end.map_or(end, |newest| newest.max(end)));
        }
        match (oldest, newest_end) {
            (Some(oldest), Some(newest)) => newest.saturating_sub(oldest),
            _ => Duration::ZERO,
        }
    }

    /// Advance the cumulative ACK checkpoint and release acknowledged frames.
    pub fn acknowledge_up_to(&mut self, frame_id: FrameId) -> Vec<FrameId> {
        let mut acknowledged = Vec::new();
        let start = self.checkpoint + 1;
        if frame_id < start {
            return acknowledged;
        }
        for raw_id in start.raw()..=frame_id.raw() {
            let slot = raw_id as usize % MAX_UNACKED_FRAMES;
            if self.pending[slot]
                .as_ref()
                .is_some_and(|pending| pending.frame_id.raw() == raw_id)
            {
                self.pending[slot] = None;
                let frame_id = FrameId(raw_id);
                self.cancel_queued_frame(frame_id);
                acknowledged.push(frame_id);
            }
        }
        self.checkpoint = frame_id;
        self.frames_acked = self.frames_acked.saturating_add(acknowledged.len() as u64);
        acknowledged
    }

    /// Release complete frames reported by the optional CST2 ACK bitvector.
    ///
    /// Bit zero describes `checkpoint + 2`; subsequent bits advance frame IDs
    /// in least-significant-bit-first order. `checkpoint + 1` is deliberately
    /// absent because it is the receiver's oldest incomplete frame.
    pub fn acknowledge_bitvector(
        &mut self,
        checkpoint: FrameId,
        ack: Option<&AckBitvector>,
    ) -> Vec<FrameId> {
        self.last_ack_feedback_count = ack.map(|ack| ack.feedback_count);
        self.last_ack_bitvector_bytes = ack.map(|ack| ack.data.len());

        let Some(ack) = ack else {
            return Vec::new();
        };
        let mut acknowledged = Vec::new();
        for (byte_index, byte) in ack.data.iter().copied().enumerate() {
            for bit_index in 0..8usize {
                if byte & (1 << bit_index) == 0 {
                    continue;
                }
                let offset = 2 + byte_index * 8 + bit_index;
                let frame_id = checkpoint + offset as i64;
                if frame_id >= self.next_frame_id {
                    continue;
                }
                let slot = frame_id.raw() as usize % MAX_UNACKED_FRAMES;
                if self.pending[slot]
                    .as_ref()
                    .is_some_and(|pending| pending.frame_id == frame_id)
                {
                    self.pending[slot] = None;
                    self.cancel_queued_frame(frame_id);
                    acknowledged.push(frame_id);
                }
            }
        }
        self.frames_acked = self.frames_acked.saturating_add(acknowledged.len() as u64);
        acknowledged
    }

    fn cancel_queued_frame(&mut self, frame_id: FrameId) {
        self.send_queue.retain(|queued| queued.frame_id != frame_id);
        self.retransmit_queue
            .retain(|queued| queued.frame_id != frame_id);
    }

    /// Queue retransmissions for one NACK and return the packet count queued.
    pub fn handle_nack(&mut self, frame_id: FrameId, packet_id: u16) -> usize {
        self.nack_count = self.nack_count.saturating_add(1);
        let slot = frame_id.raw() as usize % MAX_UNACKED_FRAMES;
        let Some(pending) = self.pending[slot].as_ref() else {
            return 0;
        };
        if pending.frame_id != frame_id {
            return 0;
        }

        if crate::constants::ALL_PACKETS_LOST == packet_id {
            let count = pending.packets.len();
            self.retransmit_queue.extend(
                pending
                    .packets
                    .iter()
                    .cloned()
                    .map(|packet| QueuedPacket { frame_id, packet }),
            );
            count
        } else if let Some(packet) = pending.packets.get(packet_id as usize) {
            self.retransmit_queue.push_back(QueuedPacket {
                frame_id,
                packet: packet.clone(),
            });
            1
        } else {
            0
        }
    }

    /// Take a retransmission first, then a newly queued packet.
    pub fn next_packet(&mut self) -> Option<OutboundPacket> {
        self.retransmit_queue
            .pop_front()
            .map(|queued| OutboundPacket {
                packet: queued.packet,
                retransmission: true,
            })
            .or_else(|| {
                self.send_queue.pop_front().map(|queued| OutboundPacket {
                    packet: queued.packet,
                    retransmission: false,
                })
            })
    }

    pub fn sender_sync_source(&self) -> SyncSource {
        self.sender_sync_source
    }

    pub fn receiver_sync_source(&self) -> SyncSource {
        self.receiver_sync_source
    }

    pub fn accepts_receiver_sync_source(&self, sync_source: SyncSource) -> bool {
        self.receiver_sync_source == sync_source
    }

    pub fn total_packets_sent(&self) -> u32 {
        self.rtp_packets_sent as u32
    }

    pub fn total_octets_sent(&self) -> u32 {
        self.rtp_octets_sent as u32
    }

    pub fn sender_report_timestamp(&self) -> (NtpTimestamp, u32) {
        let Some(reference_time) = self.last_reference_time else {
            return (NtpTimestamp::now(), self.last_rtp_timestamp);
        };
        let monotonic_now = Instant::now();
        let rtp_timestamp = if reference_time <= monotonic_now {
            self.last_rtp_timestamp.wrapping_add(media_to_rtp_timestamp(
                monotonic_now.duration_since(reference_time),
                self.rtp_timebase,
            ))
        } else {
            self.last_rtp_timestamp.wrapping_sub(media_to_rtp_timestamp(
                reference_time.duration_since(monotonic_now),
                self.rtp_timebase,
            ))
        };
        (NtpTimestamp::now(), rtp_timestamp)
    }

    pub fn record_packet_sent(&mut self, packet_size: usize, retransmission: bool) {
        if retransmission {
            self.packets_retransmitted = self.packets_retransmitted.saturating_add(1);
            return;
        }
        self.rtp_packets_sent = self.rtp_packets_sent.wrapping_add(1);
        let payload_size = packet_size.saturating_sub(RTP_STANDARD_HEADER_SIZE);
        self.rtp_octets_sent = self.rtp_octets_sent.wrapping_add(payload_size as u64);
    }

    pub fn record_sender_report(&mut self, compact_ntp: u32, sent_at: Instant) {
        if self.sender_reports.len() == SENDER_REPORT_HISTORY {
            self.sender_reports.pop_front();
        }
        self.sender_reports.push_back(SenderReportRecord {
            compact_ntp,
            sent_at,
        });
    }

    pub fn update_receiver_report(&mut self, report: &ReceiverReport, received_at: Instant) {
        self.fraction_lost = Some(report.fraction_lost);
        self.cumulative_packets_lost = Some(report.cumulative_lost);
        self.highest_sequence_number = Some(report.highest_sequence);
        self.jitter = Some(report.jitter);
        if report.last_sr_timestamp == 0 {
            return;
        }
        let Some(sent_at) = self
            .sender_reports
            .iter()
            .rev()
            .find(|record| record.compact_ntp == report.last_sr_timestamp)
            .map(|record| record.sent_at)
        else {
            return;
        };
        let receiver_delay = duration_from_ntp_short(report.delay_since_last_sr);
        self.current_rtt = received_at
            .checked_duration_since(sent_at)
            .and_then(|elapsed| elapsed.checked_sub(receiver_delay));
    }

    pub fn update_playout_delay(&mut self, delay_ms: u16) {
        self.receiver_playout_delay = Some(Duration::from_millis(delay_ms.into()));
    }

    pub fn update_receiver_reference_time(&mut self, report: &ReceiverReferenceTime) {
        self.receiver_reference_time = Some(report.ntp_timestamp.to_system_time());
    }

    pub fn statistics(&self) -> StreamStatistics {
        StreamStatistics {
            in_flight_frames: self.in_flight_count(),
            in_flight_media_duration: self.in_flight_media_duration(),
            max_acceptable_in_flight_duration: self.max_acceptable_in_flight_duration(),
            current_rtt: self.current_rtt,
            receiver_playout_delay: self.receiver_playout_delay,
            packets_sent: self.rtp_packets_sent,
            packets_retransmitted: self.packets_retransmitted,
            frames_acked: self.frames_acked,
            frames_dropped_or_skipped: self.frames_dropped_or_skipped,
            nack_count: self.nack_count,
            fraction_lost: self.fraction_lost,
            cumulative_packets_lost: self.cumulative_packets_lost,
            highest_sequence_number: self.highest_sequence_number,
            jitter: self.jitter,
            last_ack_feedback_count: self.last_ack_feedback_count,
            last_ack_bitvector_bytes: self.last_ack_bitvector_bytes,
            receiver_reference_time: self.receiver_reference_time,
        }
    }

    fn max_acceptable_in_flight_duration(&self) -> Duration {
        let playout_delay = self
            .receiver_playout_delay
            .unwrap_or(self.target_playout_delay);
        let maximum = (playout_delay / 3).max(MINIMUM_IN_FLIGHT_MEDIA_DURATION);
        let network_budget = self.current_rtt.unwrap_or(Duration::ZERO).saturating_mul(2);
        network_budget
            .max(MINIMUM_IN_FLIGHT_MEDIA_DURATION)
            .min(maximum)
    }
}

fn media_to_rtp_timestamp(media_timestamp: Duration, timebase: u32) -> u32 {
    let whole_seconds = media_timestamp.as_secs().wrapping_mul(u64::from(timebase)) as u32;
    let fractional =
        u64::from(media_timestamp.subsec_nanos()) * u64::from(timebase) / 1_000_000_000;
    whole_seconds.wrapping_add(fractional as u32)
}

fn duration_from_ntp_short(value: u32) -> Duration {
    let nanos = u64::from(value) * 1_000_000_000 / 65_536;
    Duration::from_nanos(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn make_stream() -> StreamState {
        StreamState::new(StreamParameters {
            sender_sync_source: 12345,
            receiver_sync_source: 54321,
            rtp_payload_type: 101,
            rtp_timebase: 90_000,
            target_playout_delay: Duration::from_millis(400),
            adaptive_playout_delay: true,
            aes_key: [0xAA; 16],
            aes_iv_mask: [0xBB; 16],
            max_packet_size: 1472,
        })
    }

    fn make_frame(dependency: FrameDependency, timestamp_ms: u64) -> EncodedFrame {
        EncodedFrame::new(
            dependency,
            Bytes::from_static(b"test payload data"),
            Duration::from_millis(timestamp_ms),
            Instant::now(),
        )
        .with_duration(Duration::from_millis(10))
    }

    fn has_packets(stream: &StreamState) -> bool {
        !stream.retransmit_queue.is_empty() || !stream.send_queue.is_empty()
    }

    #[test]
    fn enqueue_assigns_sequential_ids() {
        let mut stream = make_stream();
        let first = stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 0))
            .unwrap();
        let second = stream
            .enqueue_frame(make_frame(FrameDependency::Delta, 10))
            .unwrap();
        assert_eq!(first, FrameId::first());
        assert_eq!(second, FrameId(1));
    }

    #[test]
    fn timestamp_drives_rtp_timestamp() {
        let mut stream = make_stream();
        stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 1_000))
            .unwrap();
        assert_eq!(stream.last_rtp_timestamp, 90_000);
    }

    #[test]
    fn playout_delay_update_is_attached_to_exactly_one_frame() {
        let mut stream = make_stream();
        stream.set_target_playout_delay(Duration::from_millis(33));
        stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 0))
            .unwrap();

        let packet = stream.next_packet().unwrap().packet;
        assert_eq!(packet.data[12] & 0x3f, 1);
        assert_eq!(u16::from_be_bytes([packet.data[21], packet.data[22]]), 33);

        stream
            .enqueue_frame(make_frame(FrameDependency::Delta, 10))
            .unwrap();
        let packet = stream.next_packet().unwrap().packet;
        assert_eq!(packet.data[12] & 0x3f, 0);

        stream.set_target_playout_delay(Duration::from_millis(33));
        stream
            .enqueue_frame(make_frame(FrameDependency::Delta, 20))
            .unwrap();
        let packet = stream.next_packet().unwrap().packet;
        assert_eq!(packet.data[12] & 0x3f, 1);
        assert_eq!(u16::from_be_bytes([packet.data[21], packet.data[22]]), 33);
    }

    #[test]
    fn sender_report_projects_media_clock_to_report_time() {
        let mut stream = make_stream();
        let frame = EncodedFrame::new(
            FrameDependency::KeyFrame,
            Bytes::from_static(b"frame"),
            Duration::ZERO,
            Instant::now() - Duration::from_secs(1),
        );
        stream.enqueue_frame(frame).unwrap();

        let (_, rtp_timestamp) = stream.sender_report_timestamp();
        assert!((90_000..92_000).contains(&rtp_timestamp));
    }

    #[test]
    fn rejects_invalid_initial_dependency_and_timestamps() {
        let mut stream = make_stream();
        assert!(matches!(
            stream.enqueue_frame(make_frame(FrameDependency::Delta, 0)),
            Err(EnqueueError::FirstFrameMustBeKeyFrame)
        ));
        stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 10))
            .unwrap();
        assert!(matches!(
            stream.enqueue_frame(make_frame(FrameDependency::Delta, 10)),
            Err(EnqueueError::NonMonotonicMediaTimestamp)
        ));
        assert_eq!(stream.statistics().frames_dropped_or_skipped, 2);
    }

    #[test]
    fn enqueue_produces_packets() {
        let mut stream = make_stream();
        stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 0))
            .unwrap();
        assert!(has_packets(&stream));
        assert!(!stream.next_packet().unwrap().packet.data.is_empty());
    }

    #[test]
    fn acknowledge_clears_pending_and_updates_duration() {
        let mut stream = make_stream();
        let first = stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 0))
            .unwrap();
        stream
            .enqueue_frame(make_frame(FrameDependency::Delta, 10))
            .unwrap();
        assert_eq!(stream.in_flight_media_duration(), Duration::from_millis(20));

        let acknowledged = stream.acknowledge_up_to(first);
        assert_eq!(acknowledged, vec![first]);
        assert_eq!(stream.in_flight_count(), 1);
        assert_eq!(stream.statistics().frames_acked, 1);
    }

    #[test]
    fn ack_bitvector_retires_complete_frames_after_the_gap() {
        let mut stream = make_stream();
        let first = stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 0))
            .unwrap();
        let gap = stream
            .enqueue_frame(make_frame(FrameDependency::Delta, 10))
            .unwrap();
        let second_complete = stream
            .enqueue_frame(make_frame(FrameDependency::Delta, 20))
            .unwrap();
        let third_complete = stream
            .enqueue_frame(make_frame(FrameDependency::Delta, 30))
            .unwrap();

        assert_eq!(stream.acknowledge_up_to(first), vec![first]);
        let acknowledged = stream.acknowledge_bitvector(
            first,
            Some(&AckBitvector {
                feedback_count: 7,
                data: vec![0b0000_0011],
            }),
        );

        assert_eq!(acknowledged, vec![second_complete, third_complete]);
        assert_eq!(stream.in_flight_count(), 1);
        assert_eq!(stream.statistics().frames_acked, 3);
        assert_eq!(stream.statistics().last_ack_feedback_count, Some(7));
        assert!(stream
            .send_queue
            .iter()
            .all(|queued| queued.frame_id == gap));
        assert_eq!(stream.handle_nack(second_complete, 0), 0);
        assert!(stream
            .acknowledge_bitvector(
                first,
                Some(&AckBitvector {
                    feedback_count: 8,
                    data: vec![0b0000_0011],
                }),
            )
            .is_empty());
    }

    #[test]
    fn nack_queues_retransmit() {
        let mut stream = make_stream();
        let id = stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 0))
            .unwrap();
        while stream.next_packet().is_some() {}

        assert_eq!(stream.handle_nack(id, 0), 1);
        let packet = stream.next_packet().unwrap();
        assert!(packet.retransmission);
        assert!(!packet.packet.data.is_empty());
    }

    #[test]
    fn max_in_flight_rejected() {
        let mut stream = make_stream();
        for index in 0..MAX_UNACKED_FRAMES {
            let dependency = if index == 0 {
                FrameDependency::KeyFrame
            } else {
                FrameDependency::Delta
            };
            stream
                .enqueue_frame(make_frame(dependency, index as u64))
                .unwrap();
        }
        assert!(matches!(
            stream.enqueue_frame(make_frame(
                FrameDependency::Delta,
                MAX_UNACKED_FRAMES as u64
            )),
            Err(EnqueueError::ReachedIdSpanLimit)
        ));
    }

    #[test]
    fn retransmits_have_priority_over_new_packets() {
        let mut stream = make_stream();
        let id = stream
            .enqueue_frame(make_frame(FrameDependency::KeyFrame, 0))
            .unwrap();
        while stream.next_packet().is_some() {}

        stream
            .enqueue_frame(make_frame(FrameDependency::Delta, 10))
            .unwrap();
        stream.handle_nack(id, 0);
        let first = stream.next_packet().unwrap();
        assert!(first.retransmission);
        assert_eq!(first.packet.data[13], id.truncate());
    }

    #[test]
    fn sender_report_counts_exclude_retransmissions() {
        let mut stream = make_stream();

        stream.record_packet_sent(100, false);
        stream.record_packet_sent(100, true);

        assert_eq!(stream.total_packets_sent(), 1);
        assert_eq!(
            stream.total_octets_sent(),
            100 - RTP_STANDARD_HEADER_SIZE as u32
        );
        assert_eq!(stream.statistics().packets_sent, 1);
        assert_eq!(stream.statistics().packets_retransmitted, 1);
    }

    #[test]
    fn in_flight_budget_uses_rtt_floor_and_playout_cap() {
        let mut stream = make_stream();
        assert_eq!(
            stream.max_acceptable_in_flight_duration(),
            Duration::from_millis(66)
        );

        stream.current_rtt = Some(Duration::from_millis(50));
        assert_eq!(
            stream.max_acceptable_in_flight_duration(),
            Duration::from_millis(100)
        );

        stream.current_rtt = Some(Duration::from_millis(100));
        assert_eq!(
            stream.max_acceptable_in_flight_duration(),
            Duration::from_millis(400) / 3
        );

        stream.receiver_playout_delay = Some(Duration::from_millis(90));
        assert_eq!(
            stream.max_acceptable_in_flight_duration(),
            Duration::from_millis(66)
        );
    }
}
