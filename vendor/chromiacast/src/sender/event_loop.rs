use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant as StdInstant};

use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval_at, Instant, Interval, MissedTickBehavior};

use crate::codec::StreamType;
use crate::constants::{
    BURST_INTERVAL, MAX_BURST_BITRATE, RTCP_REPORT_INTERVAL, STATISTICS_DEBOUNCE_INTERVAL,
};
use crate::error::{EnqueueError, PlayoutDelayError, SenderError};
use crate::frame::{EncodedFrame, FrameId};
use crate::rtcp::{self, CompoundRtcpPacket, SenderReportBuilder};
use crate::rtp;
use crate::sync_source;
use crate::transport::Transport;

use super::stats::SessionStatistics;
use super::stream::StreamState;

const INITIAL_RECEIVER_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(10);
const RECEIVER_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(5);
const RECEIVER_TIMEOUT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RESERVED_TERMINAL_EVENT_SLOTS: usize = 2;

#[derive(Default)]
struct AcknowledgementWatchdog {
    waiting_since: Option<tokio::time::Instant>,
    has_acknowledged: bool,
}

impl AcknowledgementWatchdog {
    fn packet_sent(&mut self) {
        self.waiting_since
            .get_or_insert_with(tokio::time::Instant::now);
    }

    fn acknowledgement_advanced(&mut self, frames_still_in_flight: bool) {
        self.has_acknowledged = true;
        self.waiting_since = frames_still_in_flight.then(tokio::time::Instant::now);
    }

    fn timed_out(&mut self, frames_in_flight: bool) -> bool {
        if !frames_in_flight {
            self.waiting_since = None;
            return false;
        }
        let timeout = if self.has_acknowledged {
            RECEIVER_ACKNOWLEDGEMENT_TIMEOUT
        } else {
            INITIAL_RECEIVER_ACKNOWLEDGEMENT_TIMEOUT
        };
        self.waiting_since
            .is_some_and(|waiting_since| waiting_since.elapsed() >= timeout)
    }
}

#[derive(Default)]
struct AcknowledgementProgress {
    audio: bool,
    video: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamFeedbackState {
    pressure_limited: bool,
    max_acceptable_in_flight_duration: Duration,
    current_rtt: Option<Duration>,
    receiver_playout_delay: Option<Duration>,
    nack_count: u64,
    frames_dropped_or_skipped: u64,
    fraction_lost: Option<u8>,
}

struct BurstBudget {
    capacity_bytes: usize,
    available_bytes: usize,
    fractional_byte_nanos: u128,
    last_refill: Instant,
}

impl BurstBudget {
    fn new() -> Self {
        let bytes_per_second = u128::from(MAX_BURST_BITRATE / 8);
        let capacity_bytes = usize::try_from(
            bytes_per_second.saturating_mul(BURST_INTERVAL.as_nanos()) / 1_000_000_000,
        )
        .unwrap_or(usize::MAX)
        .max(1);
        Self {
            capacity_bytes,
            available_bytes: capacity_bytes,
            fractional_byte_nanos: 0,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed_nanos = now.duration_since(self.last_refill).as_nanos();
        self.last_refill = now;
        if self.available_bytes == self.capacity_bytes {
            self.fractional_byte_nanos = 0;
            return;
        }
        let scaled_bytes = elapsed_nanos
            .saturating_mul(u128::from(MAX_BURST_BITRATE / 8))
            .saturating_add(self.fractional_byte_nanos);
        let added_bytes = scaled_bytes / 1_000_000_000;
        self.fractional_byte_nanos = scaled_bytes % 1_000_000_000;
        self.available_bytes = self
            .available_bytes
            .saturating_add(usize::try_from(added_bytes).unwrap_or(usize::MAX))
            .min(self.capacity_bytes);
        if self.available_bytes == self.capacity_bytes {
            self.fractional_byte_nanos = 0;
        }
    }

    fn has_credit(&self) -> bool {
        self.available_bytes != 0
    }

    fn consume(&mut self, bytes: usize) {
        self.available_bytes = self.available_bytes.saturating_sub(bytes);
    }
}

/// Events emitted by a running media sender.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SenderEvent {
    NeedsKeyFrame {
        stream: StreamType,
    },
    FrameAcked {
        stream: StreamType,
        frame_id: FrameId,
    },
    PictureLoss {
        stream: StreamType,
    },
    PacketRetransmission {
        stream: StreamType,
        frame_id: FrameId,
        packet_id: u16,
        packets_queued: usize,
    },
    StatisticsUpdated(Box<SessionStatistics>),
    MalformedPacket {
        description: String,
    },
    /// The receiver did not establish frame acknowledgements during startup,
    /// or stopped advancing acknowledgements after streaming began.
    ReceiverTimedOut,
    FatalError(SenderError),
}

pub(crate) enum StreamCommand {
    EnqueueFrame {
        stream: StreamType,
        frame: EncodedFrame,
        result_sender: oneshot::Sender<Result<FrameId, EnqueueError>>,
    },
    Statistics {
        result_sender: oneshot::Sender<SessionStatistics>,
    },
    SetTargetPlayoutDelay {
        delay: Duration,
        result_sender: oneshot::Sender<Result<(), PlayoutDelayError>>,
    },
    Shutdown {
        result_sender: oneshot::Sender<()>,
    },
}

/// The single select loop that owns all sender state.
pub(crate) async fn run<T: Transport>(
    transport: T,
    remote_address: SocketAddr,
    mut audio_stream: Option<StreamState>,
    mut video_stream: Option<StreamState>,
    mut command_receiver: mpsc::Receiver<StreamCommand>,
    event_sender: mpsc::Sender<SenderEvent>,
) -> Result<(), SenderError> {
    let mut burst_timer = make_timer(BURST_INTERVAL);
    let mut rtcp_timer = make_timer(RTCP_REPORT_INTERVAL);
    let statistics_timer = tokio::time::sleep(STATISTICS_DEBOUNCE_INTERVAL);
    tokio::pin!(statistics_timer);
    let mut statistics_pending = false;
    let mut statistics_trigger_count = 0u64;
    let mut timeout_timer = make_timer(RECEIVER_TIMEOUT_POLL_INTERVAL);
    let mut receive_buffer = vec![0u8; 2048];
    let mut audio_watchdog = AcknowledgementWatchdog::default();
    let mut video_watchdog = AcknowledgementWatchdog::default();
    let mut burst_budget = BurstBudget::new();

    loop {
        tokio::select! {
            command = command_receiver.recv() => {
                let mut publish_statistics = false;
                let mut command_stream = None;
                let flush = match command {
                    Some(StreamCommand::EnqueueFrame { stream, frame, result_sender }) => {
                        command_stream = Some(stream);
                        let feedback_before =
                            stream_feedback_state(stream, &audio_stream, &video_stream);
                        let result = match stream {
                            StreamType::Audio => audio_stream.as_mut().map(|stream| stream.enqueue_frame(frame)),
                            StreamType::Video => video_stream.as_mut().map(|stream| stream.enqueue_frame(frame)),
                        };
                        let flush = result.as_ref().is_some_and(Result::is_ok);
                        publish_statistics = result.as_ref().is_some_and(Result::is_err)
                            || feedback_before
                                != stream_feedback_state(stream, &audio_stream, &video_stream);
                        let _ = result_sender.send(result.unwrap_or(Err(EnqueueError::SessionClosed)));
                        flush
                    }
                    Some(StreamCommand::Statistics { result_sender }) => {
                        let _ = result_sender.send(session_statistics(&audio_stream, &video_stream));
                        false
                    }
                    Some(StreamCommand::SetTargetPlayoutDelay { delay, result_sender }) => {
                        let feedback_before = session_feedback_state(&audio_stream, &video_stream);
                        let result = set_target_playout_delay(
                            &mut audio_stream,
                            &mut video_stream,
                            delay,
                        );
                        publish_statistics = result.is_ok()
                            && feedback_before
                                != session_feedback_state(&audio_stream, &video_stream);
                        let _ = result_sender.send(result);
                        false
                    }
                    Some(StreamCommand::Shutdown { result_sender }) => {
                        let _ = result_sender.send(());
                        return Ok(());
                    }
                    None => return Ok(()),
                };
                if flush {
                    // Like openscreen's SenderImpl: the receiver drops RTP that arrives before its
                    // first Sender Report, so lead with one until a Receiver Report gives an RTT.
                    let enqueued = match command_stream {
                        Some(StreamType::Audio) => audio_stream.as_mut(),
                        Some(StreamType::Video) => video_stream.as_mut(),
                        None => None,
                    };
                    if let Some(stream) = enqueued.filter(|s| s.awaiting_receiver_report()) {
                        if let Err(error) = send_sender_report(
                            &transport,
                            remote_address,
                            stream,
                            "send leading RTCP Sender Report",
                        ).await {
                            emit_fatal(&event_sender, &error);
                            return Err(error);
                        }
                    }
                    if let Err(error) = flush_queued_packets(
                        &transport,
                        remote_address,
                        &mut audio_stream,
                        &mut video_stream,
                        &mut audio_watchdog,
                        &mut video_watchdog,
                        &mut burst_budget,
                    ).await {
                        emit_fatal(&event_sender, &error);
                        return Err(error);
                    }
                }
                if publish_statistics {
                    statistics_trigger_count = statistics_trigger_count.saturating_add(1);
                    if !statistics_pending {
                        statistics_pending = true;
                        statistics_timer
                            .as_mut()
                            .reset(Instant::now() + STATISTICS_DEBOUNCE_INTERVAL);
                    }
                }
            }

            _ = burst_timer.tick() => {
                if let Err(error) = flush_queued_packets(
                    &transport,
                    remote_address,
                    &mut audio_stream,
                    &mut video_stream,
                    &mut audio_watchdog,
                    &mut video_watchdog,
                    &mut burst_budget,
                ).await {
                    emit_fatal(&event_sender, &error);
                    return Err(error);
                }
            }

            received = transport.recv_from(&mut receive_buffer) => {
                let (length, address) = match received {
                    Ok(received) => received,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let error = transport_error("receive RTCP packet", error);
                        emit_fatal(&event_sender, &error);
                        return Err(error);
                    }
                };
                if !is_expected_receiver_source(address, remote_address) {
                    continue;
                }

                let data = &receive_buffer[..length];
                if !matches!(rtp::classify_packet(data), rtp::PacketType::Rtcp(_)) {
                    continue;
                }

                match rtcp::parse_compound(data) {
                    Ok(compound) if contains_receiver_feedback(&compound) => {
                        tracing::trace!(
                            target: "chromiacast::sender",
                            source = %address,
                            length,
                            receiver_reports = compound.receiver_reports.len(),
                            cast_feedbacks = compound.cast_feedbacks.len(),
                            picture_loss_reports = compound.picture_loss.len(),
                            reference_time_reports = compound.receiver_reference_times.len(),
                            "received compound RTCP feedback",
                        );
                        let received_at = StdInstant::now();
                        let feedback_before =
                            session_feedback_state(&audio_stream, &video_stream);
                        let progress = process_feedback(
                            compound,
                            received_at,
                            &mut audio_stream,
                            &mut video_stream,
                            &event_sender,
                        );
                        if progress.audio {
                            audio_watchdog.acknowledgement_advanced(
                                audio_stream
                                    .as_ref()
                                    .is_some_and(|stream| stream.in_flight_count() != 0),
                            );
                        }
                        if progress.video {
                            video_watchdog.acknowledgement_advanced(
                                video_stream
                                    .as_ref()
                                    .is_some_and(|stream| stream.in_flight_count() != 0),
                            );
                        }
                        if let Err(error) = flush_queued_packets(
                            &transport,
                            remote_address,
                            &mut audio_stream,
                            &mut video_stream,
                            &mut audio_watchdog,
                            &mut video_watchdog,
                            &mut burst_budget,
                        ).await {
                            emit_fatal(&event_sender, &error);
                            return Err(error);
                        }
                        if feedback_before != session_feedback_state(&audio_stream, &video_stream) {
                            statistics_trigger_count = statistics_trigger_count.saturating_add(1);
                            if !statistics_pending {
                                statistics_pending = true;
                                statistics_timer
                                    .as_mut()
                                    .reset(Instant::now() + STATISTICS_DEBOUNCE_INTERVAL);
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        emit_nonterminal(&event_sender, SenderEvent::MalformedPacket {
                            description: error.to_string(),
                        });
                    }
                }
            }

            _ = rtcp_timer.tick() => {
                if let Some(stream) = audio_stream.as_mut() {
                    if let Err(error) = send_sender_report(
                        &transport,
                        remote_address,
                        stream,
                        "send audio RTCP Sender Report",
                    ).await {
                        emit_fatal(&event_sender, &error);
                        return Err(error);
                    }
                }
                if let Some(stream) = video_stream.as_mut() {
                    if let Err(error) = send_sender_report(
                        &transport,
                        remote_address,
                        stream,
                        "send video RTCP Sender Report",
                    ).await {
                        emit_fatal(&event_sender, &error);
                        return Err(error);
                    }
                }
            }

            _ = &mut statistics_timer, if statistics_pending => {
                tracing::debug!(
                    target: "chromiacast::sender",
                    debounce_milliseconds = STATISTICS_DEBOUNCE_INTERVAL.as_millis(),
                    triggers = statistics_trigger_count,
                    coalesced_triggers = statistics_trigger_count.saturating_sub(1),
                    "publishing debounced sender statistics",
                );
                statistics_pending = false;
                statistics_trigger_count = 0;
                emit_nonterminal(&event_sender, SenderEvent::StatisticsUpdated(Box::new(
                    session_statistics(&audio_stream, &video_stream),
                )));
            }

            _ = timeout_timer.tick() => {
                let audio_timed_out = audio_watchdog.timed_out(
                    audio_stream
                        .as_ref()
                        .is_some_and(|stream| stream.in_flight_count() != 0),
                );
                let video_timed_out = video_watchdog.timed_out(
                    video_stream
                        .as_ref()
                        .is_some_and(|stream| stream.in_flight_count() != 0),
                );
                if audio_timed_out || video_timed_out {
                    tracing::warn!(
                        target: "chromiacast::sender",
                        audio_timed_out,
                        video_timed_out,
                        "receiver acknowledgement timeout",
                    );
                    let _ = event_sender.try_send(SenderEvent::ReceiverTimedOut);
                    return Err(SenderError::ReceiverTimedOut);
                }
            }
        }
    }
}

async fn flush_queued_packets<T: Transport>(
    transport: &T,
    remote_address: SocketAddr,
    audio_stream: &mut Option<StreamState>,
    video_stream: &mut Option<StreamState>,
    audio_watchdog: &mut AcknowledgementWatchdog,
    video_watchdog: &mut AcknowledgementWatchdog,
    budget: &mut BurstBudget,
) -> Result<(), SenderError> {
    budget.refill();
    flush_stream_packets(
        transport,
        remote_address,
        audio_stream.as_mut(),
        audio_watchdog,
        budget,
        "send audio RTP packet",
    )
    .await?;
    flush_stream_packets(
        transport,
        remote_address,
        video_stream.as_mut(),
        video_watchdog,
        budget,
        "send video RTP packet",
    )
    .await
}

async fn flush_stream_packets<T: Transport>(
    transport: &T,
    remote_address: SocketAddr,
    mut stream: Option<&mut StreamState>,
    watchdog: &mut AcknowledgementWatchdog,
    budget: &mut BurstBudget,
    operation: &'static str,
) -> Result<(), SenderError> {
    while budget.has_credit() {
        let Some(stream) = stream.as_mut() else {
            break;
        };
        let Some(outbound) = stream.next_packet() else {
            break;
        };
        let length = outbound.packet.data.len();
        send_datagram(transport, &outbound.packet.data, remote_address, operation).await?;
        stream.record_packet_sent(length, outbound.retransmission);
        watchdog.packet_sent();
        budget.consume(length);
    }
    Ok(())
}

fn set_target_playout_delay(
    audio: &mut Option<StreamState>,
    video: &mut Option<StreamState>,
    delay: Duration,
) -> Result<(), PlayoutDelayError> {
    if audio
        .iter()
        .chain(video.iter())
        .any(|stream| !stream.supports_adaptive_playout_delay())
    {
        return Err(PlayoutDelayError::Unsupported);
    }
    if let Some(audio) = audio {
        audio.set_target_playout_delay(delay);
    }
    if let Some(video) = video {
        video.set_target_playout_delay(delay);
    }
    Ok(())
}

fn process_feedback(
    compound: CompoundRtcpPacket,
    received_at: StdInstant,
    audio: &mut Option<StreamState>,
    video: &mut Option<StreamState>,
    event_sender: &mpsc::Sender<SenderEvent>,
) -> AcknowledgementProgress {
    let mut progress = AcknowledgementProgress::default();
    for report in &compound.receiver_reports {
        if let Some(stream) = find_stream_by_sender_sync_source(report.sync_source, audio, video).1
        {
            if !stream.accepts_receiver_sync_source(report.receiver_sync_source) {
                continue;
            }
            stream.update_receiver_report(report, received_at);
        }
    }

    for feedback in &compound.cast_feedbacks {
        let (stream_type, stream) =
            find_stream_by_sender_sync_source(feedback.sender_sync_source, audio, video);
        let Some(stream) = stream else {
            continue;
        };
        if !stream.accepts_receiver_sync_source(feedback.receiver_sync_source) {
            continue;
        }
        stream.update_playout_delay(feedback.playout_delay_ms);
        let checkpoint =
            FrameId::expand_lte(feedback.checkpoint_frame_id, stream.next_frame_id() + -1);
        let mut acknowledged = stream.acknowledge_up_to(checkpoint);
        acknowledged
            .extend(stream.acknowledge_bitvector(checkpoint, feedback.ack_bitvector.as_ref()));
        if !acknowledged.is_empty() {
            match stream_type {
                StreamType::Audio => progress.audio = true,
                StreamType::Video => progress.video = true,
            }
        }
        for frame_id in acknowledged {
            emit_nonterminal(
                event_sender,
                SenderEvent::FrameAcked {
                    stream: stream_type,
                    frame_id,
                },
            );
        }

        for nack in &feedback.nacks {
            let frame_id = FrameId::expand(nack.frame_id.truncate(), checkpoint);
            let packets_queued = stream.handle_nack(frame_id, nack.packet_id);
            emit_nonterminal(
                event_sender,
                SenderEvent::PacketRetransmission {
                    stream: stream_type,
                    frame_id,
                    packet_id: nack.packet_id,
                    packets_queued,
                },
            );
        }
    }

    for loss in &compound.picture_loss {
        let (stream_type, stream) =
            find_stream_by_sender_sync_source(loss.sender_sync_source, audio, video);
        if stream
            .is_some_and(|stream| stream.accepts_receiver_sync_source(loss.receiver_sync_source))
        {
            emit_nonterminal(
                event_sender,
                SenderEvent::PictureLoss {
                    stream: stream_type,
                },
            );
            emit_nonterminal(
                event_sender,
                SenderEvent::NeedsKeyFrame {
                    stream: stream_type,
                },
            );
        }
    }

    for reference_time in &compound.receiver_reference_times {
        if let Some(stream) =
            find_stream_by_receiver_sync_source(reference_time.receiver_sync_source, audio, video)
        {
            stream.update_receiver_reference_time(reference_time);
        }
    }
    progress
}

fn contains_receiver_feedback(compound: &CompoundRtcpPacket) -> bool {
    !compound.receiver_reports.is_empty()
        || !compound.cast_feedbacks.is_empty()
        || !compound.picture_loss.is_empty()
        || !compound.receiver_reference_times.is_empty()
}

fn is_expected_receiver_source(source: SocketAddr, destination: SocketAddr) -> bool {
    match (source, destination) {
        (SocketAddr::V4(source), SocketAddr::V4(destination)) => source.ip() == destination.ip(),
        (SocketAddr::V6(source), SocketAddr::V6(destination)) => {
            source.ip() == destination.ip()
                && (destination.scope_id() == 0 || source.scope_id() == destination.scope_id())
        }
        _ => false,
    }
}

async fn send_datagram<T: Transport>(
    transport: &T,
    data: &[u8],
    remote_address: SocketAddr,
    operation: &'static str,
) -> Result<(), SenderError> {
    let written = transport
        .send_to(data, remote_address)
        .await
        .map_err(|error| transport_error(operation, error))?;
    if written != data.len() {
        return Err(SenderError::Transport {
            operation,
            message: format!(
                "short datagram write: wrote {written} of {} bytes",
                data.len()
            ),
        });
    }
    Ok(())
}

async fn send_sender_report<T: Transport>(
    transport: &T,
    remote_address: SocketAddr,
    stream: &mut StreamState,
    operation: &'static str,
) -> Result<(), SenderError> {
    let (ntp_timestamp, rtp_timestamp) = stream.sender_report_timestamp();
    let report = SenderReportBuilder::build(
        stream.sender_sync_source(),
        ntp_timestamp,
        rtp_timestamp,
        stream.total_packets_sent(),
        stream.total_octets_sent(),
    );
    send_datagram(transport, &report, remote_address, operation).await?;
    stream.record_sender_report(ntp_timestamp.middle_32(), StdInstant::now());
    Ok(())
}

fn session_statistics(
    audio: &Option<StreamState>,
    video: &Option<StreamState>,
) -> SessionStatistics {
    SessionStatistics::new(
        audio.as_ref().map(StreamState::statistics),
        video.as_ref().map(StreamState::statistics),
    )
}

fn stream_feedback_state(
    stream_type: StreamType,
    audio: &Option<StreamState>,
    video: &Option<StreamState>,
) -> Option<StreamFeedbackState> {
    let stream = match stream_type {
        StreamType::Audio => audio.as_ref(),
        StreamType::Video => video.as_ref(),
    }?;
    let statistics = stream.statistics();
    Some(StreamFeedbackState {
        pressure_limited: !statistics.max_acceptable_in_flight_duration.is_zero()
            && statistics.in_flight_media_duration >= statistics.max_acceptable_in_flight_duration,
        max_acceptable_in_flight_duration: statistics.max_acceptable_in_flight_duration,
        current_rtt: statistics.current_rtt,
        receiver_playout_delay: statistics.receiver_playout_delay,
        nack_count: statistics.nack_count,
        frames_dropped_or_skipped: statistics.frames_dropped_or_skipped,
        fraction_lost: statistics.fraction_lost,
    })
}

fn session_feedback_state(
    audio: &Option<StreamState>,
    video: &Option<StreamState>,
) -> (Option<StreamFeedbackState>, Option<StreamFeedbackState>) {
    (
        stream_feedback_state(StreamType::Audio, audio, video),
        stream_feedback_state(StreamType::Video, audio, video),
    )
}

fn find_stream_by_sender_sync_source<'a>(
    sync_source: u32,
    audio: &'a mut Option<StreamState>,
    video: &'a mut Option<StreamState>,
) -> (StreamType, Option<&'a mut StreamState>) {
    if audio
        .as_ref()
        .is_some_and(|stream| stream.sender_sync_source() == sync_source)
    {
        return (StreamType::Audio, audio.as_mut());
    }
    if video
        .as_ref()
        .is_some_and(|stream| stream.sender_sync_source() == sync_source)
    {
        return (StreamType::Video, video.as_mut());
    }
    if sync_source::is_audio(sync_source) {
        (StreamType::Audio, None)
    } else {
        (StreamType::Video, None)
    }
}

fn find_stream_by_receiver_sync_source<'a>(
    sync_source: u32,
    audio: &'a mut Option<StreamState>,
    video: &'a mut Option<StreamState>,
) -> Option<&'a mut StreamState> {
    if audio
        .as_ref()
        .is_some_and(|stream| stream.receiver_sync_source() == sync_source)
    {
        return audio.as_mut();
    }
    if video
        .as_ref()
        .is_some_and(|stream| stream.receiver_sync_source() == sync_source)
    {
        return video.as_mut();
    }
    None
}

fn transport_error(operation: &'static str, error: io::Error) -> SenderError {
    SenderError::Transport {
        operation,
        message: error.to_string(),
    }
}

fn emit_fatal(event_sender: &mpsc::Sender<SenderEvent>, error: &SenderError) {
    tracing::error!(target: "chromiacast::sender", %error, "sender session terminated");
    let _ = event_sender.try_send(SenderEvent::FatalError(error.clone()));
}

fn emit_nonterminal(event_sender: &mpsc::Sender<SenderEvent>, event: SenderEvent) {
    if event_sender.capacity() > RESERVED_TERMINAL_EVENT_SLOTS {
        let _ = event_sender.try_send(event);
    }
}

fn make_timer(period: Duration) -> Interval {
    let mut timer = interval_at(tokio::time::Instant::now() + period, period);
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    timer
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    #[tokio::test(start_paused = true)]
    async fn burst_budget_starts_full_and_refills_at_the_peak_rate() {
        let mut budget = BurstBudget::new();
        let capacity = budget.capacity_bytes;
        assert_eq!(budget.available_bytes, capacity);

        budget.consume(capacity);
        assert!(!budget.has_credit());

        tokio::time::advance(BURST_INTERVAL / 2).await;
        budget.refill();
        assert!(budget.available_bytes.abs_diff(capacity / 2) <= 1);

        budget.consume(budget.available_bytes);
        tokio::time::advance(BURST_INTERVAL).await;
        budget.refill();
        assert_eq!(budget.available_bytes, capacity);
    }

    #[tokio::test(start_paused = true)]
    async fn acknowledgement_watchdog_allows_receiver_startup_grace() {
        let mut watchdog = AcknowledgementWatchdog::default();
        watchdog.packet_sent();

        tokio::time::advance(RECEIVER_ACKNOWLEDGEMENT_TIMEOUT).await;
        assert!(!watchdog.timed_out(true));

        tokio::time::advance(
            INITIAL_RECEIVER_ACKNOWLEDGEMENT_TIMEOUT - RECEIVER_ACKNOWLEDGEMENT_TIMEOUT,
        )
        .await;
        assert!(watchdog.timed_out(true));
    }

    #[tokio::test(start_paused = true)]
    async fn acknowledgement_watchdog_uses_shorter_deadline_after_feedback() {
        let mut watchdog = AcknowledgementWatchdog::default();
        watchdog.packet_sent();
        watchdog.acknowledgement_advanced(true);

        tokio::time::advance(RECEIVER_ACKNOWLEDGEMENT_TIMEOUT).await;
        assert!(watchdog.timed_out(true));
    }

    #[test]
    fn receiver_feedback_source_port_may_differ_from_media_port() {
        let destination = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 5), 5000));
        let source = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 5), 61234));

        assert!(is_expected_receiver_source(source, destination));
    }

    #[test]
    fn receiver_feedback_must_come_from_receiver_ip() {
        let destination = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 5), 5000));
        let source = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 6), 5000));

        assert!(!is_expected_receiver_source(source, destination));
    }

    #[test]
    fn link_local_receiver_feedback_preserves_interface_scope() {
        let ip = "fe80::1234".parse::<Ipv6Addr>().unwrap();
        let destination = SocketAddr::V6(SocketAddrV6::new(ip, 5000, 0, 7));
        let matching = SocketAddr::V6(SocketAddrV6::new(ip, 61234, 0, 7));
        let wrong_interface = SocketAddr::V6(SocketAddrV6::new(ip, 61234, 0, 8));

        assert!(is_expected_receiver_source(matching, destination));
        assert!(!is_expected_receiver_source(wrong_interface, destination));
    }
}
