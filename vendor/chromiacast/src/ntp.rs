use crate::constants::NTP_EPOCH_OFFSET_SECS;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 64-bit NTP timestamp: upper 32 bits are seconds since 1900-01-01,
/// lower 32 bits are fractional seconds (1 unit = 2^-32 seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NtpTimestamp(pub u64);

impl NtpTimestamp {
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    pub fn from_system_time(time: SystemTime) -> Self {
        let duration = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let ntp_secs = duration.as_secs() + NTP_EPOCH_OFFSET_SECS;
        let fraction = ((duration.subsec_nanos() as u64) << 32) / 1_000_000_000;
        NtpTimestamp((ntp_secs << 32) | fraction)
    }

    pub fn to_system_time(self) -> SystemTime {
        let secs = self.seconds().saturating_sub(NTP_EPOCH_OFFSET_SECS);
        let nanos = ((self.fraction() as u64) * 1_000_000_000) >> 32;
        UNIX_EPOCH + Duration::new(secs, nanos as u32)
    }

    pub fn seconds(self) -> u64 {
        self.0 >> 32
    }

    pub fn fraction(self) -> u32 {
        self.0 as u32
    }

    /// The upper 32 bits, used as a compact identifier in RTCP Sender Reports.
    pub fn upper_32(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// The middle 32 bits (lower 16 of seconds + upper 16 of fraction),
    /// used in RTCP "last SR" fields.
    pub fn middle_32(self) -> u32 {
        (self.0 >> 16) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_system_time() {
        let now = SystemTime::now();
        let ntp = NtpTimestamp::from_system_time(now);
        let back = ntp.to_system_time();
        let diff = now.duration_since(back).unwrap_or_else(|e| e.duration());
        assert!(diff < Duration::from_micros(1));
    }

    #[test]
    fn unix_epoch() {
        let ntp = NtpTimestamp::from_system_time(UNIX_EPOCH);
        assert_eq!(ntp.seconds(), NTP_EPOCH_OFFSET_SECS);
        assert_eq!(ntp.fraction(), 0);
    }

    #[test]
    fn seconds_and_fraction() {
        let ntp = NtpTimestamp((100 << 32) | 0x8000_0000);
        assert_eq!(ntp.seconds(), 100);
        assert_eq!(ntp.fraction(), 0x8000_0000);
    }

    #[test]
    fn middle_32() {
        // seconds = 0xAABB_CCDD, fraction = 0x1122_3344
        let ntp = NtpTimestamp((0xAABB_CCDD_u64 << 32) | 0x1122_3344);
        assert_eq!(ntp.middle_32(), 0xCCDD_1122);
    }
}
