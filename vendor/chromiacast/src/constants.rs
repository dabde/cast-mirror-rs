use std::time::Duration;

pub const DEFAULT_TARGET_PLAYOUT_DELAY: Duration = Duration::from_millis(400);
pub const ADAPTIVE_PLAYOUT_DELAY_EXTENSION: &str = "adaptive_playout_delay";

pub const RTCP_REPORT_INTERVAL: Duration = Duration::from_millis(500);
pub const STATISTICS_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(100);
pub const BURST_INTERVAL: Duration = Duration::from_millis(10);

pub const MAX_UNACKED_FRAMES: usize = 120;

pub const DEFAULT_VIDEO_MAX_BIT_RATE: u32 = 10_000_000;
// H.264 IDR frames can be substantially larger than the stream's average
// bitrate. Keep enough peak headroom to deliver a 720p60 key frame inside a
// low-latency Cast playout window instead of spreading it across ~100 ms of
// 10 ms pacer ticks. This remains a pacing ceiling; the encoder's negotiated
// bitrate controls sustained traffic.
pub const MAX_BURST_BITRATE: u32 = 128 << 20;

pub const DEFAULT_AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_AUDIO_CHANNELS: u8 = 2;

pub const RTP_VIDEO_TIMEBASE: u32 = 90_000;
pub const DEFAULT_FRAME_RATE: u32 = 30;

pub const MAX_RTP_PACKET_SIZE_IPV4: usize = 1472;
pub const MAX_RTP_PACKET_SIZE_IPV6: usize = 1452;

pub const RTP_STANDARD_HEADER_SIZE: usize = 12;
const CAST_HEADER_WITH_REF_SIZE: usize = 7;
pub const RTP_BASE_HEADER_SIZE: usize = RTP_STANDARD_HEADER_SIZE + CAST_HEADER_WITH_REF_SIZE;
pub const ADAPTIVE_LATENCY_EXT_SIZE: usize = 4;
pub const RTP_MAX_HEADER_SIZE: usize = RTP_BASE_HEADER_SIZE + ADAPTIVE_LATENCY_EXT_SIZE;
pub const RTP_REQUIRED_FIRST_BYTE: u8 = 0x80;

pub const RTCP_SENDER_REPORT: u8 = 200;
pub const RTCP_RECEIVER_REPORT: u8 = 201;
pub const RTCP_PAYLOAD_SPECIFIC: u8 = 206;
pub const RTCP_EXTENDED_REPORTS: u8 = 207;

pub const RTCP_SUBTYPE_PICTURE_LOSS: u8 = 1;
pub const RTCP_SUBTYPE_FEEDBACK: u8 = 15;

pub const CAST_FEEDBACK_MAGIC: u32 = 0x4341_5354; // "CAST"
pub const CAST_ACK_MAGIC: u32 = 0x4353_5432; // "CST2"

pub const ALL_PACKETS_LOST: u16 = 0xFFFF;

pub const MINIMUM_AUDIO_SYNC_SOURCE: u32 = 1;
pub const MAXIMUM_AUDIO_SYNC_SOURCE: u32 = 50_000;
pub const MINIMUM_VIDEO_SYNC_SOURCE: u32 = 50_001;
pub const MAXIMUM_VIDEO_SYNC_SOURCE: u32 = 100_000;

pub const NTP_EPOCH_OFFSET_SECS: u64 = 2_208_988_800;
