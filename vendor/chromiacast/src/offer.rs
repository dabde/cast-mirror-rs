use std::time::Duration;

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use thiserror::Error;

use crate::codec::{AudioCodec, CastMode, Framerate, Resolution, VideoCodec};
use crate::constants::{ADAPTIVE_PLAYOUT_DELAY_EXTENSION, DEFAULT_TARGET_PLAYOUT_DELAY};
use crate::crypto::FrameCrypto;
use crate::sync_source;

/// Configuration for an audio stream in an OFFER.
#[derive(Debug, Clone)]
pub struct AudioStreamConfig {
    pub codec: AudioCodec,
    pub bit_rate: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub target_delay: Duration,
}

/// Configuration for a video stream in an OFFER.
#[derive(Debug, Clone)]
pub struct VideoStreamConfig {
    pub codec: VideoCodec,
    pub max_bit_rate: u32,
    pub max_frame_rate: Framerate,
    pub resolutions: Vec<Resolution>,
    pub target_delay: Duration,
}

/// A generated OFFER ready for serialization and sending over the control channel.
///
/// Contains all negotiation parameters including the AES keys (the receiver
/// needs them). Pass this to `SenderSession::start()` along with the receiver's
/// `Answer` to begin streaming.
#[derive(Debug, Clone)]
pub struct Offer {
    pub cast_mode: CastMode,
    pub(crate) streams: Vec<StreamOffer>,
}

#[derive(Debug, Clone)]
pub(crate) struct StreamOffer {
    pub index: usize,
    pub stream_type: StreamOfferType,
    pub channels: u8,
    pub rtp_payload_type: u8,
    pub sync_source: u32,
    pub target_delay: Duration,
    pub aes_key: [u8; 16],
    pub aes_iv_mask: [u8; 16],
    pub rtp_timebase: u32,
    pub codec_name: String,
    pub rtp_extensions: Vec<String>,
    pub audio_extra: Option<AudioExtra>,
    pub video_extra: Option<VideoExtra>,
}

#[derive(Debug, Clone)]
pub(crate) struct AudioExtra {
    pub bit_rate: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct VideoExtra {
    pub max_frame_rate: Framerate,
    pub max_bit_rate: u32,
    pub resolutions: Vec<Resolution>,
}

impl Offer {
    pub fn builder() -> OfferBuilder {
        OfferBuilder {
            cast_mode: CastMode::Mirroring,
            android_rtp_hack: false,
            audio: Vec::new(),
            video: Vec::new(),
        }
    }

    pub(crate) fn stream_aes_key(&self, index: usize) -> Option<&[u8; 16]> {
        self.streams
            .iter()
            .find(|s| s.index == index)
            .map(|s| &s.aes_key)
    }

    pub(crate) fn stream_aes_iv_mask(&self, index: usize) -> Option<&[u8; 16]> {
        self.streams
            .iter()
            .find(|s| s.index == index)
            .map(|s| &s.aes_iv_mask)
    }

    pub(crate) fn stream_sync_source(&self, index: usize) -> Option<u32> {
        self.streams
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.sync_source)
    }

    pub(crate) fn stream_rtp_timebase(&self, index: usize) -> Option<u32> {
        self.streams
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.rtp_timebase)
    }

    pub(crate) fn stream_rtp_payload_type(&self, index: usize) -> Option<u8> {
        self.streams
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.rtp_payload_type)
    }

    pub(crate) fn stream_target_delay(&self, index: usize) -> Option<Duration> {
        self.streams
            .iter()
            .find(|stream| stream.index == index)
            .map(|stream| stream.target_delay)
    }

    pub(crate) fn stream_type(&self, index: usize) -> Option<crate::codec::StreamType> {
        self.streams
            .iter()
            .find(|s| s.index == index)
            .map(|s| match s.stream_type {
                StreamOfferType::AudioSource => crate::codec::StreamType::Audio,
                StreamOfferType::VideoSource => crate::codec::StreamType::Video,
            })
    }

    pub(crate) fn has_video(&self) -> bool {
        self.streams
            .iter()
            .any(|stream| stream.stream_type == StreamOfferType::VideoSource)
    }
}

pub struct OfferBuilder {
    cast_mode: CastMode,
    audio: Vec<AudioStreamConfig>,
    video: Vec<VideoStreamConfig>,
    android_rtp_hack: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum OfferError {
    #[error("an offer must contain at least one stream")]
    NoStreams,

    #[error("audio stream {index} is invalid: {reason}")]
    InvalidAudioStream { index: usize, reason: &'static str },

    #[error("video stream {index} is invalid: {reason}")]
    InvalidVideoStream { index: usize, reason: &'static str },
}

impl OfferBuilder {
    pub fn cast_mode(mut self, mode: CastMode) -> Self {
        self.cast_mode = mode;
        self
    }

    /// Use payload types 127 (audio) / 96 (video) like Chrome; some Android TV receivers need them.
    pub fn android_rtp_hack(mut self, enabled: bool) -> Self {
        self.android_rtp_hack = enabled;
        self
    }

    pub fn audio(mut self, config: AudioStreamConfig) -> Self {
        self.audio.push(config);
        self
    }

    pub fn video(mut self, config: VideoStreamConfig) -> Self {
        self.video.push(config);
        self
    }

    pub fn build(self) -> Result<Offer, OfferError> {
        if self.audio.is_empty() && self.video.is_empty() {
            return Err(OfferError::NoStreams);
        }

        for (index, audio) in self.audio.iter().enumerate() {
            let reason = if audio.bit_rate == 0 {
                Some("bit rate must be nonzero")
            } else if audio.sample_rate == 0 {
                Some("sample rate must be nonzero")
            } else if audio.channels == 0 {
                Some("channel count must be nonzero")
            } else if !valid_target_delay(audio.target_delay) {
                Some("target delay must be a whole number of milliseconds between 1 and 4294967295")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(OfferError::InvalidAudioStream { index, reason });
            }
        }

        for (index, video) in self.video.iter().enumerate() {
            let reason = if video.max_bit_rate == 0 {
                Some("maximum bit rate must be nonzero")
            } else if video.resolutions.is_empty() {
                Some("at least one resolution is required")
            } else if !valid_target_delay(video.target_delay) {
                Some("target delay must be a whole number of milliseconds between 1 and 4294967295")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(OfferError::InvalidVideoStream { index, reason });
            }
        }

        let mut streams = Vec::new();
        let mut index = 0;

        for audio in self.audio {
            streams.push(StreamOffer {
                index,
                stream_type: StreamOfferType::AudioSource,
                channels: audio.channels,
                rtp_payload_type: if self.android_rtp_hack { 127 } else { audio.codec.rtp_payload_type() },
                sync_source: sync_source::generate_audio(),
                target_delay: audio.target_delay,
                aes_key: FrameCrypto::generate_key(),
                aes_iv_mask: FrameCrypto::generate_iv_mask(),
                rtp_timebase: audio.sample_rate,
                codec_name: audio.codec.codec_name().to_string(),
                rtp_extensions: vec![ADAPTIVE_PLAYOUT_DELAY_EXTENSION.to_string()],
                audio_extra: Some(AudioExtra {
                    bit_rate: audio.bit_rate,
                }),
                video_extra: None,
            });
            index += 1;
        }

        for video in self.video {
            streams.push(StreamOffer {
                index,
                stream_type: StreamOfferType::VideoSource,
                channels: 1,
                rtp_payload_type: if self.android_rtp_hack { 96 } else { video.codec.rtp_payload_type() },
                sync_source: sync_source::generate_video(),
                target_delay: video.target_delay,
                aes_key: FrameCrypto::generate_key(),
                aes_iv_mask: FrameCrypto::generate_iv_mask(),
                rtp_timebase: crate::constants::RTP_VIDEO_TIMEBASE,
                codec_name: video.codec.codec_name().to_string(),
                rtp_extensions: vec![ADAPTIVE_PLAYOUT_DELAY_EXTENSION.to_string()],
                audio_extra: None,
                video_extra: Some(VideoExtra {
                    max_frame_rate: video.max_frame_rate,
                    max_bit_rate: video.max_bit_rate,
                    resolutions: video.resolutions,
                }),
            });
            index += 1;
        }

        Ok(Offer {
            cast_mode: self.cast_mode,
            streams,
        })
    }
}

fn valid_target_delay(delay: Duration) -> bool {
    let milliseconds = delay.as_millis();
    (1..=u128::from(u32::MAX)).contains(&milliseconds)
        && delay == Duration::from_millis(milliseconds as u64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamOfferType {
    AudioSource,
    VideoSource,
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

impl Serialize for Offer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Offer", 2)?;
        state.serialize_field("castMode", &self.cast_mode)?;
        state.serialize_field("supportedStreams", &self.streams)?;
        state.end()
    }
}

impl Serialize for StreamOffer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;

        map.serialize_entry("index", &self.index)?;
        let type_str = match self.stream_type {
            StreamOfferType::AudioSource => "audio_source",
            StreamOfferType::VideoSource => "video_source",
        };
        map.serialize_entry("type", type_str)?;
        map.serialize_entry("codecName", &self.codec_name)?;
        map.serialize_entry("rtpProfile", "cast")?;
        map.serialize_entry("rtpPayloadType", &self.rtp_payload_type)?;
        map.serialize_entry("ssrc", &self.sync_source)?;

        let timebase = format!("1/{}", self.rtp_timebase);
        map.serialize_entry("timeBase", &timebase)?;

        map.serialize_entry("targetDelay", &(self.target_delay.as_millis() as u32))?;
        map.serialize_entry("aesKey", &hex_encode(&self.aes_key))?;
        map.serialize_entry("aesIvMask", &hex_encode(&self.aes_iv_mask))?;
        map.serialize_entry("channels", &self.channels)?;
        map.serialize_entry("rtpExtensions", &self.rtp_extensions)?;

        if let Some(ref audio) = self.audio_extra {
            map.serialize_entry("bitRate", &audio.bit_rate)?;
        }

        if let Some(ref video) = self.video_extra {
            map.serialize_entry("maxFrameRate", &video.max_frame_rate)?;
            map.serialize_entry("maxBitRate", &video.max_bit_rate)?;
            map.serialize_entry("resolutions", &video.resolutions)?;
        }

        map.end()
    }
}

impl Default for AudioStreamConfig {
    fn default() -> Self {
        Self {
            codec: AudioCodec::Opus,
            bit_rate: 128_000,
            sample_rate: crate::constants::DEFAULT_AUDIO_SAMPLE_RATE,
            channels: crate::constants::DEFAULT_AUDIO_CHANNELS,
            target_delay: DEFAULT_TARGET_PLAYOUT_DELAY,
        }
    }
}

impl Default for VideoStreamConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::H264,
            max_bit_rate: crate::constants::DEFAULT_VIDEO_MAX_BIT_RATE,
            max_frame_rate: Framerate::new(crate::constants::DEFAULT_FRAME_RATE, 1)
                .expect("default framerate is valid"),
            resolutions: vec![Resolution::new(1920, 1080).expect("default resolution is valid")],
            target_delay: DEFAULT_TARGET_PLAYOUT_DELAY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_offer_with_audio_and_video() {
        let offer = Offer::builder()
            .audio(AudioStreamConfig::default())
            .video(VideoStreamConfig::default())
            .build()
            .unwrap();

        assert_eq!(offer.streams.len(), 2);
        assert_eq!(offer.streams[0].index, 0);
        assert_eq!(offer.streams[0].stream_type, StreamOfferType::AudioSource);
        assert_eq!(offer.streams[1].index, 1);
        assert_eq!(offer.streams[1].stream_type, StreamOfferType::VideoSource);
    }

    #[test]
    fn offer_serializes_to_valid_json() {
        let offer = Offer::builder()
            .audio(AudioStreamConfig::default())
            .video(VideoStreamConfig::default())
            .build()
            .unwrap();

        let json = serde_json::to_value(&offer).unwrap();
        assert_eq!(json["castMode"], "mirroring");

        let streams = json["supportedStreams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);

        let audio = &streams[0];
        assert_eq!(audio["type"], "audio_source");
        assert_eq!(audio["codecName"], "opus");
        assert_eq!(audio["rtpProfile"], "cast");
        assert_eq!(audio["channels"], 2);
        assert_eq!(audio["bitRate"], 128_000);
        assert_eq!(audio["timeBase"], "1/48000");
        assert_eq!(audio["targetDelay"], 400);
        assert!(audio["aesKey"].as_str().unwrap().len() == 32);
        assert!(audio["aesIvMask"].as_str().unwrap().len() == 32);

        let video = &streams[1];
        assert_eq!(video["type"], "video_source");
        assert_eq!(video["codecName"], "h264");
        assert_eq!(video["maxFrameRate"], "30/1");
        assert_eq!(video["maxBitRate"], 10_000_000);
        assert!(video["resolutions"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn offer_generates_unique_keys() {
        let o1 = Offer::builder()
            .audio(AudioStreamConfig::default())
            .build()
            .unwrap();
        let o2 = Offer::builder()
            .audio(AudioStreamConfig::default())
            .build()
            .unwrap();
        assert_ne!(o1.streams[0].aes_key, o2.streams[0].aes_key);
    }

    #[test]
    fn stream_accessors() {
        let offer = Offer::builder()
            .audio(AudioStreamConfig::default())
            .build()
            .unwrap();

        assert!(offer.stream_aes_key(0).is_some());
        assert!(offer.stream_aes_key(99).is_none());
        assert_eq!(offer.stream_rtp_timebase(0), Some(48_000));
    }

    #[test]
    fn offer_rejects_invalid_stream_configuration() {
        assert!(matches!(
            Offer::builder().build(),
            Err(OfferError::NoStreams)
        ));

        let audio = AudioStreamConfig {
            sample_rate: 0,
            ..AudioStreamConfig::default()
        };
        assert!(matches!(
            Offer::builder().audio(audio).build(),
            Err(OfferError::InvalidAudioStream { .. })
        ));

        let mut video = VideoStreamConfig::default();
        video.resolutions.clear();
        assert!(matches!(
            Offer::builder().video(video).build(),
            Err(OfferError::InvalidVideoStream { .. })
        ));

        let video = VideoStreamConfig {
            target_delay: Duration::from_nanos(1),
            ..VideoStreamConfig::default()
        };
        assert!(matches!(
            Offer::builder().video(video).build(),
            Err(OfferError::InvalidVideoStream { .. })
        ));
    }
}

#[cfg(test)]
mod android_rtp_hack_tests {
    use super::*;
    use crate::codec::{Framerate, Resolution};

    #[test]
    fn hack_uses_chrome_payload_types() {
        let video = VideoStreamConfig {
            codec: VideoCodec::H264,
            max_bit_rate: 1_000_000,
            max_frame_rate: Framerate::new(30, 1).unwrap(),
            resolutions: vec![Resolution::new(1280, 720).unwrap()],
            target_delay: Duration::from_millis(250),
        };
        let audio = AudioStreamConfig {
            codec: AudioCodec::Opus,
            bit_rate: 128_000,
            sample_rate: 48_000,
            channels: 2,
            target_delay: Duration::from_millis(250),
        };
        let hacked = Offer::builder().android_rtp_hack(true).audio(audio.clone()).video(video.clone()).build().unwrap();
        assert_eq!(hacked.stream_rtp_payload_type(0), Some(127));
        assert_eq!(hacked.stream_rtp_payload_type(1), Some(96));
        let spec = Offer::builder().audio(audio).video(video).build().unwrap();
        assert_eq!(spec.stream_rtp_payload_type(0), Some(96));
        assert_eq!(spec.stream_rtp_payload_type(1), Some(101));
    }
}
