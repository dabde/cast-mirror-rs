use std::collections::HashSet;

use serde::Deserialize;

use crate::codec::{Framerate, Resolution};

/// Answer from a Cast receiver in response to an OFFER.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Answer {
    /// UDP port the receiver is listening on for RTP/RTCP packets.
    #[serde(rename = "udpPort")]
    pub udp_port: u16,

    /// Indexes into the OFFER's `supportedStreams` that the receiver accepted.
    #[serde(rename = "sendIndexes")]
    pub send_indexes: Vec<usize>,

    /// Receiver sync sources (one per accepted stream, parallel to `send_indexes`).
    #[serde(rename = "ssrcs")]
    pub sync_sources: Vec<u32>,

    /// Optional receiver constraints on encoding.
    #[serde(default)]
    pub constraints: Option<Constraints>,

    /// Optional display description.
    #[serde(default)]
    pub display: Option<DisplayDescription>,

    /// RTP extensions accepted per stream (parallel to `send_indexes`).
    #[serde(default, rename = "rtpExtensions")]
    pub rtp_extensions: Option<Vec<Vec<String>>>,
}

#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Constraints {
    #[serde(default)]
    pub audio: Option<AudioConstraints>,
    #[serde(default)]
    pub video: Option<VideoConstraints>,
}

#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct AudioConstraints {
    #[serde(default, rename = "maxSampleRate")]
    pub max_sample_rate: Option<u32>,
    #[serde(default, rename = "maxChannels")]
    pub max_channels: Option<u8>,
    #[serde(default, rename = "minBitRate")]
    pub min_bit_rate: Option<u32>,
    #[serde(default, rename = "maxBitRate")]
    pub max_bit_rate: Option<u32>,
    #[serde(default, rename = "maxDelay")]
    pub max_delay: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct VideoConstraints {
    #[serde(default, rename = "maxPixelsPerSecond")]
    pub max_pixels_per_second: Option<f64>,
    #[serde(default, rename = "minResolution")]
    pub min_resolution: Option<Resolution>,
    #[serde(default, rename = "maxDimensions")]
    pub max_dimensions: Option<Dimensions>,
    #[serde(default, rename = "minBitRate")]
    pub min_bit_rate: Option<u32>,
    #[serde(default, rename = "maxBitRate")]
    pub max_bit_rate: Option<u32>,
    #[serde(default, rename = "maxDelay")]
    pub max_delay: Option<u32>,
}

/// Width, height, and optional rate ceiling from a Cast receiver ANSWER.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
    #[serde(default, rename = "frameRate")]
    pub frame_rate: Option<Framerate>,
}

#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct DisplayDescription {
    #[serde(default)]
    pub dimensions: Option<Dimensions>,
    #[serde(default, rename = "aspectRatio")]
    pub aspect_ratio: Option<String>,
    #[serde(default)]
    pub scaling: Option<String>,
}

impl Answer {
    pub fn validate(&self) -> Result<(), String> {
        if self.send_indexes.is_empty() {
            return Err("no streams accepted".to_string());
        }
        if self.send_indexes.len() != self.sync_sources.len() {
            return Err(format!(
                "sendIndexes length ({}) != sync_sources length ({})",
                self.send_indexes.len(),
                self.sync_sources.len()
            ));
        }
        if self
            .rtp_extensions
            .as_ref()
            .is_some_and(|extensions| extensions.len() != self.send_indexes.len())
        {
            return Err(format!(
                "rtpExtensions length ({}) != sendIndexes length ({})",
                self.rtp_extensions.as_ref().map_or(0, Vec::len),
                self.send_indexes.len()
            ));
        }
        if self.udp_port == 0 {
            return Err("udpPort is 0".to_string());
        }
        if self.sync_sources.contains(&0) {
            return Err("receiver SSRC must not be zero".to_string());
        }
        if self
            .send_indexes
            .iter()
            .copied()
            .collect::<HashSet<_>>()
            .len()
            != self.send_indexes.len()
        {
            return Err("sendIndexes contains a duplicate stream".to_string());
        }
        if self
            .sync_sources
            .iter()
            .copied()
            .collect::<HashSet<_>>()
            .len()
            != self.sync_sources.len()
        {
            return Err("ssrcs contains a duplicate receiver SSRC".to_string());
        }
        if self
            .constraints
            .as_ref()
            .and_then(|constraints| constraints.video.as_ref())
            .and_then(|video| video.max_dimensions)
            .is_some_and(|dimensions| dimensions.width == 0 || dimensions.height == 0)
        {
            return Err("video maxDimensions contains a zero dimension".to_string());
        }
        if self
            .display
            .as_ref()
            .and_then(|display| display.dimensions)
            .is_some_and(|dimensions| dimensions.width == 0 || dimensions.height == 0)
        {
            return Err("display dimensions contains a zero dimension".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_answer() {
        let json = r#"{
            "udpPort": 2344,
            "sendIndexes": [0, 1],
            "ssrcs": [60000, 30000]
        }"#;
        let answer: Answer = serde_json::from_str(json).unwrap();
        assert_eq!(answer.udp_port, 2344);
        assert_eq!(answer.send_indexes, vec![0, 1]);
        assert_eq!(answer.sync_sources, vec![60000, 30000]);
        assert!(answer.constraints.is_none());
        assert!(answer.display.is_none());
        answer.validate().unwrap();
    }

    #[test]
    fn deserialize_answer_with_constraints() {
        let json = r#"{
            "udpPort": 2344,
            "sendIndexes": [0, 1],
            "ssrcs": [60000, 30000],
            "constraints": {
                "audio": {
                    "maxSampleRate": 48000,
                    "maxChannels": 2,
                    "maxBitRate": 256000
                },
                "video": {
                    "maxDimensions": {"width": 1920, "height": 1080, "frameRate": "60"},
                    "maxBitRate": 5000000,
                    "maxDelay": 1500
                }
            },
            "display": {
                "dimensions": {"width": 3840, "height": 2160, "frameRate": "60000/1001"},
                "aspectRatio": "16:9",
                "scaling": "sender"
            }
        }"#;
        let answer: Answer = serde_json::from_str(json).unwrap();
        let constraints = answer.constraints.as_ref().unwrap();
        let audio = constraints.audio.as_ref().unwrap();
        assert_eq!(audio.max_sample_rate, Some(48000));

        let video = constraints.video.as_ref().unwrap();
        assert_eq!(video.max_bit_rate, Some(5_000_000));
        assert_eq!(video.max_delay, Some(1500));
        assert_eq!(
            video.max_dimensions.unwrap().frame_rate,
            Some(Framerate::new(60, 1).unwrap())
        );

        let display = answer.display.as_ref().unwrap();
        assert_eq!(display.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(display.dimensions.unwrap().width, 3840);
        assert_eq!(
            display.dimensions.unwrap().frame_rate,
            Some(Framerate::new(60_000, 1_001).unwrap())
        );
    }

    #[test]
    fn validate_empty_indexes() {
        let answer = Answer {
            udp_port: 2344,
            send_indexes: vec![],
            sync_sources: vec![],
            constraints: None,
            display: None,
            rtp_extensions: None,
        };
        assert!(answer.validate().is_err());
    }

    #[test]
    fn validate_mismatched_lengths() {
        let answer = Answer {
            udp_port: 2344,
            send_indexes: vec![0, 1],
            sync_sources: vec![1000],
            constraints: None,
            display: None,
            rtp_extensions: None,
        };
        assert!(answer.validate().is_err());
    }

    #[test]
    fn validate_rejects_duplicate_streams_and_sync_sources() {
        let mut answer: Answer = serde_json::from_value(serde_json::json!({
            "udpPort": 2344,
            "sendIndexes": [0, 0],
            "ssrcs": [1000, 2000]
        }))
        .unwrap();
        assert!(answer.validate().is_err());

        answer.send_indexes = vec![0, 1];
        answer.sync_sources = vec![1000, 1000];
        assert!(answer.validate().is_err());
    }

    #[test]
    fn validate_rejects_misaligned_rtp_extensions() {
        let answer: Answer = serde_json::from_value(serde_json::json!({
            "udpPort": 2344,
            "sendIndexes": [0, 1],
            "ssrcs": [1000, 2000],
            "rtpExtensions": [["adaptive_playout_delay"]]
        }))
        .unwrap();

        assert!(answer.validate().is_err());
    }
}
