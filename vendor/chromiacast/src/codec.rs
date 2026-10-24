use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AudioCodec {
    Opus,
    Aac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum VideoCodec {
    H264,
    Vp8,
    Hevc,
    Vp9,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CastMode {
    Mirroring,
    Remoting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamType {
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Resolution {
    width: u32,
    height: u32,
}

impl<'de> Deserialize<'de> for Resolution {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireResolution {
            width: u32,
            height: u32,
        }

        let resolution = WireResolution::deserialize(deserializer)?;
        Resolution::new(resolution.width, resolution.height).map_err(serde::de::Error::custom)
    }
}

impl Resolution {
    pub fn new(width: u32, height: u32) -> Result<Self, InvalidResolution> {
        if width == 0 || height == 0 {
            return Err(InvalidResolution);
        }
        Ok(Self { width, height })
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("resolution dimensions must be nonzero")]
pub struct InvalidResolution;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framerate {
    numerator: u32,
    denominator: u32,
}

impl Framerate {
    pub fn new(numerator: u32, denominator: u32) -> Result<Self, InvalidFramerate> {
        if numerator == 0 || denominator == 0 {
            return Err(InvalidFramerate);
        }
        Ok(Self {
            numerator,
            denominator,
        })
    }

    pub fn numerator(self) -> u32 {
        self.numerator
    }

    pub fn denominator(self) -> u32 {
        self.denominator
    }

    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("framerate numerator and denominator must be nonzero")]
pub struct InvalidFramerate;

impl Serialize for Framerate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}/{}", self.numerator, self.denominator))
    }
}

impl<'de> Deserialize<'de> for Framerate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        let (numerator, denominator) = match s.split_once('/') {
            Some((numerator, denominator)) => (
                numerator.parse().map_err(serde::de::Error::custom)?,
                denominator.parse().map_err(serde::de::Error::custom)?,
            ),
            None => (s.parse().map_err(serde::de::Error::custom)?, 1),
        };
        if numerator == 0 || denominator == 0 {
            return Err(serde::de::Error::custom(
                "framerate numerator and denominator must be nonzero",
            ));
        }
        Framerate::new(numerator, denominator).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for Framerate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

impl AudioCodec {
    pub fn rtp_payload_type(self) -> u8 {
        match self {
            AudioCodec::Opus => 96,
            AudioCodec::Aac => 97,
        }
    }

    pub fn codec_name(self) -> &'static str {
        match self {
            AudioCodec::Opus => "opus",
            AudioCodec::Aac => "aac",
        }
    }
}

impl VideoCodec {
    pub fn rtp_payload_type(self) -> u8 {
        match self {
            VideoCodec::H264 => 101,
            VideoCodec::Vp8 => 100,
            VideoCodec::Hevc => 102,
            VideoCodec::Vp9 => 103,
            VideoCodec::Av1 => 104,
        }
    }

    pub fn codec_name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Vp8 => "vp8",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Vp9 => "vp9",
            VideoCodec::Av1 => "av1",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_codec_serde_roundtrip() {
        let json = serde_json::to_string(&AudioCodec::Opus).unwrap();
        assert_eq!(json, "\"opus\"");
        let codec: AudioCodec = serde_json::from_str(&json).unwrap();
        assert_eq!(codec, AudioCodec::Opus);
    }

    #[test]
    fn video_codec_serde_roundtrip() {
        let json = serde_json::to_string(&VideoCodec::H264).unwrap();
        assert_eq!(json, "\"h264\"");
        let codec: VideoCodec = serde_json::from_str(&json).unwrap();
        assert_eq!(codec, VideoCodec::H264);
    }

    #[test]
    fn framerate_serde_roundtrip() {
        let fr = Framerate::new(30, 1).unwrap();
        let json = serde_json::to_string(&fr).unwrap();
        assert_eq!(json, "\"30/1\"");
        let parsed: Framerate = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, fr);
    }

    #[test]
    fn framerate_fractional() {
        let fr = Framerate::new(30000, 1001).unwrap();
        assert!((fr.as_f64() - 29.97).abs() < 0.01);
    }

    #[test]
    fn framerate_accepts_protocol_integer_form_and_rejects_zero() {
        assert_eq!(
            serde_json::from_str::<Framerate>("\"60\"").unwrap(),
            Framerate::new(60, 1).unwrap()
        );
        for invalid in ["\"0\"", "\"60/0\""] {
            assert!(serde_json::from_str::<Framerate>(invalid).is_err());
        }
    }

    #[test]
    fn dimensions_and_framerates_reject_zero_components() {
        assert_eq!(Resolution::new(0, 1080), Err(InvalidResolution));
        assert_eq!(Resolution::new(1920, 0), Err(InvalidResolution));
        assert_eq!(Framerate::new(0, 1), Err(InvalidFramerate));
        assert_eq!(Framerate::new(60, 0), Err(InvalidFramerate));
        assert!(serde_json::from_str::<Resolution>(r#"{"width":0,"height":1080}"#).is_err());
    }

    #[test]
    fn cast_mode_serde() {
        assert_eq!(
            serde_json::to_string(&CastMode::Mirroring).unwrap(),
            "\"mirroring\""
        );
    }
}
