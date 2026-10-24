use bytes::Bytes;
use std::fmt;
use std::ops::{Add, Sub};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameId(pub(crate) i64);

impl FrameId {
    pub const fn first() -> Self {
        FrameId(0)
    }

    pub fn next(self) -> Self {
        FrameId(self.0 + 1)
    }

    pub(crate) fn truncate(self) -> u8 {
        self.0 as u8
    }

    /// Expand a truncated 8-bit wire value to a full FrameId, choosing the
    /// value nearest to `reference`.
    pub(crate) fn expand(truncated: u8, reference: FrameId) -> FrameId {
        let delta = truncated.wrapping_sub(reference.truncate()) as i8;
        FrameId(reference.0 + delta as i64)
    }

    /// Expand choosing the value <= reference.
    pub(crate) fn expand_lte(truncated: u8, reference: FrameId) -> FrameId {
        let expanded = Self::expand(truncated, reference);
        if expanded.0 > reference.0 {
            FrameId(expanded.0 - 256)
        } else {
            expanded
        }
    }

    pub(crate) fn lower_32(self) -> u32 {
        self.0 as u32
    }

    pub fn raw(self) -> i64 {
        self.0
    }
}

impl Add<i64> for FrameId {
    type Output = FrameId;
    fn add(self, rhs: i64) -> FrameId {
        FrameId(self.0 + rhs)
    }
}

impl Sub for FrameId {
    type Output = i64;
    fn sub(self, rhs: FrameId) -> i64 {
        self.0 - rhs.0
    }
}

impl fmt::Debug for FrameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FrameId({})", self.0)
    }
}

impl fmt::Display for FrameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameDependency {
    KeyFrame,
    Delta,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EncodedFrame {
    /// Whether this access unit is independently decodable or references the
    /// immediately preceding frame.
    pub dependency: FrameDependency,

    /// One complete encoded access unit.
    ///
    /// H.264 inputs use Annex-B byte-stream format. Key frames must include
    /// the parameter sets needed to initialize a decoder. Opus inputs contain
    /// exactly one Opus packet.
    pub data: Bytes,

    /// Timestamp on the media pipeline's shared timeline.
    ///
    /// Values must be strictly increasing within each stream. Audio and video
    /// can use the same clock origin even when buffers arrive on different
    /// tasks. Cast RTP timestamps are derived from this value.
    pub media_timestamp: Duration,

    /// Local monotonic time corresponding to `media_timestamp`.
    ///
    /// This supplies the wall-clock/RTP mapping used by Sender Reports and is
    /// separate from the time at which the sender happens to receive a buffer.
    pub reference_time: Instant,

    /// Amount of media represented by the access unit, when known.
    pub duration: Option<Duration>,
}

impl EncodedFrame {
    /// Construct an encoded access unit without a declared duration.
    pub fn new(
        dependency: FrameDependency,
        data: Bytes,
        media_timestamp: Duration,
        reference_time: Instant,
    ) -> Self {
        Self {
            dependency,
            data,
            media_timestamp,
            reference_time,
            duration: None,
        }
    }

    /// Attach the access unit's media duration.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_id_truncation() {
        assert_eq!(FrameId(0).truncate(), 0);
        assert_eq!(FrameId(255).truncate(), 255);
        assert_eq!(FrameId(256).truncate(), 0);
        assert_eq!(FrameId(257).truncate(), 1);
    }

    #[test]
    fn frame_id_expand_forward() {
        let reference = FrameId(250);
        let expanded = FrameId::expand(5, reference);
        assert_eq!(expanded, FrameId(261));
        assert_eq!(expanded.truncate(), 5);
    }

    #[test]
    fn frame_id_expand_backward() {
        let reference = FrameId(260);
        let expanded = FrameId::expand(250, reference);
        assert_eq!(expanded, FrameId(250));
        assert_eq!(expanded.truncate(), 250);
    }

    #[test]
    fn frame_id_expand_exact() {
        let reference = FrameId(100);
        let expanded = FrameId::expand(100, reference);
        assert_eq!(expanded, reference);
    }

    #[test]
    fn frame_id_expand_lte() {
        let reference = FrameId(250);
        // Truncated 5 expands to 261 (> 250), so expand_lte should give 261-256=5
        let expanded = FrameId::expand_lte(5, reference);
        assert_eq!(expanded, FrameId(5));
        assert_eq!(expanded.truncate(), 5);
    }

    #[test]
    fn frame_id_expand_lte_already_below() {
        let reference = FrameId(260);
        let expanded = FrameId::expand_lte(250, reference);
        assert_eq!(expanded, FrameId(250));
    }

    #[test]
    fn frame_id_arithmetic() {
        let a = FrameId(10);
        assert_eq!(a + 5, FrameId(15));
        assert_eq!(FrameId(15) - FrameId(10), 5);
    }

    #[test]
    fn frame_id_ordering() {
        assert!(FrameId(0) < FrameId(1));
        assert!(FrameId(100) > FrameId(99));
    }

    #[test]
    fn lower_32_wraps() {
        let id = FrameId(0x1_0000_0042);
        assert_eq!(id.lower_32(), 0x42);
    }
}
