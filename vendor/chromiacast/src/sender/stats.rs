use std::time::{Duration, SystemTime};

/// Read-only pressure and network measurements for one media stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct StreamStatistics {
    /// Frames accepted but not yet acknowledged by the receiver.
    pub in_flight_frames: usize,
    /// Media-time span covered by unacknowledged frames.
    pub in_flight_media_duration: Duration,
    /// Recommended upper bound for queued media, derived from the target
    /// playout window and measured network round-trip time.
    ///
    /// Producers should normally throttle before their in-flight media exceeds
    /// this value instead of waiting for enqueue rejection.
    pub max_acceptable_in_flight_duration: Duration,
    /// Most recent round-trip time derived from an RTCP Receiver Report.
    pub current_rtt: Option<Duration>,
    /// Most recent Cast feedback playout delay.
    pub receiver_playout_delay: Option<Duration>,
    /// Original RTP packets successfully written.
    pub packets_sent: u64,
    /// RTP packet retransmissions successfully written.
    pub packets_retransmitted: u64,
    /// Frames acknowledged by cumulative or selective Cast feedback.
    pub frames_acked: u64,
    /// Frames rejected before transmission because they violated the stream
    /// contract or exceeded the in-flight frame window.
    pub frames_dropped_or_skipped: u64,
    /// Individual missing-packet entries received through Cast NACK feedback.
    pub nack_count: u64,
    /// Most recent RTCP loss fraction, in units of 1/256.
    pub fraction_lost: Option<u8>,
    /// Most recent cumulative packet-loss count from RTCP.
    pub cumulative_packets_lost: Option<i32>,
    /// Most recent extended RTP sequence number from RTCP.
    pub highest_sequence_number: Option<u32>,
    /// Most recent RTCP interarrival jitter in RTP timestamp ticks.
    pub jitter: Option<u32>,
    /// Feedback sequence from the latest optional Cast ACK bitvector.
    pub last_ack_feedback_count: Option<u8>,
    /// Byte length of the latest optional Cast ACK bitvector.
    pub last_ack_bitvector_bytes: Option<usize>,
    /// Receiver wall-clock sample from its latest reference-time report.
    pub receiver_reference_time: Option<SystemTime>,
}

/// Snapshot of all accepted streams in a sender session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SessionStatistics {
    pub audio: Option<StreamStatistics>,
    pub video: Option<StreamStatistics>,
}

impl SessionStatistics {
    pub(crate) fn new(audio: Option<StreamStatistics>, video: Option<StreamStatistics>) -> Self {
        Self { audio, video }
    }
}
