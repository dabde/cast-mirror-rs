use super::error::AndroidTvError;

const MAX_CLIENT_TEXT_BYTES: usize = 128;

const DIRECTION_DOWN: i32 = 1;
const DIRECTION_UP: i32 = 2;
const DIRECTION_PRESS: i32 = 3;

/// Android TV Remote Service capabilities implemented by this client.
///
/// Values returned by [`AndroidTvRemote::features`](super::remote::AndroidTvRemote::features)
/// have already been intersected with the device's advertised capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AndroidTvRemoteFeatures(u32);

impl AndroidTvRemoteFeatures {
    pub const PING: Self = Self(1 << 0);
    pub const KEY: Self = Self(1 << 1);
    pub const POWER: Self = Self(1 << 5);
    pub const VOLUME: Self = Self(1 << 6);

    pub(crate) const fn from_protocol_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, feature: Self) -> bool {
        self.0 & feature.0 == feature.0
    }
}

impl std::ops::BitOr for AndroidTvRemoteFeatures {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl std::ops::BitAnd for AndroidTvRemoteFeatures {
    type Output = Self;

    fn bitand(self, right: Self) -> Self::Output {
        Self(self.0 & right.0)
    }
}

pub(crate) const IMPLEMENTED_FEATURES: AndroidTvRemoteFeatures =
    AndroidTvRemoteFeatures::from_protocol_bits(
        AndroidTvRemoteFeatures::PING.0
            | AndroidTvRemoteFeatures::KEY.0
            | AndroidTvRemoteFeatures::POWER.0
            | AndroidTvRemoteFeatures::VOLUME.0,
    );

/// Presentation information supplied during the Remote Service handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidTvRemoteClientInfo {
    pub(crate) model: String,
    pub(crate) vendor: String,
    pub(crate) package_name: String,
    pub(crate) app_version: String,
}

impl AndroidTvRemoteClientInfo {
    pub fn new(
        model: impl Into<String>,
        vendor: impl Into<String>,
        package_name: impl Into<String>,
        app_version: impl Into<String>,
    ) -> Result<Self, AndroidTvError> {
        let result = Self {
            model: model.into(),
            vendor: vendor.into(),
            package_name: package_name.into(),
            app_version: app_version.into(),
        };
        for (field, value) in [
            ("model", result.model.as_str()),
            ("vendor", result.vendor.as_str()),
            ("package name", result.package_name.as_str()),
            ("application version", result.app_version.as_str()),
        ] {
            validate_client_text(field, value)?;
        }
        Ok(result)
    }
}

impl Default for AndroidTvRemoteClientInfo {
    fn default() -> Self {
        Self {
            model: "chromiacast".into(),
            vendor: "pronkproject".into(),
            package_name: "io.github.pronkproject.chromiacast".into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// Device metadata advertised by Android TV Remote Service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidTvDeviceInfo {
    pub(crate) model: String,
    pub(crate) vendor: String,
    pub(crate) service_version: String,
}

impl AndroidTvDeviceInfo {
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn vendor(&self) -> &str {
        &self.vendor
    }

    pub fn service_version(&self) -> &str {
        &self.service_version
    }
}

/// Volume state reported asynchronously by the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AndroidTvVolume {
    pub level: u32,
    pub maximum: u32,
    pub muted: bool,
}

/// Asynchronous state observed on an established remote connection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AndroidTvRemoteEvent {
    PowerChanged { powered_on: bool },
    VolumeChanged(AndroidTvVolume),
    Disconnected { error: Option<String> },
}

/// Raw Android key code used by Remote Service v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AndroidTvKeyCode(pub(crate) i32);

impl AndroidTvKeyCode {
    pub const UNKNOWN: Self = Self(0);
    pub const HOME: Self = Self(3);
    pub const BACK: Self = Self(4);
    pub const DPAD_UP: Self = Self(19);
    pub const DPAD_DOWN: Self = Self(20);
    pub const DPAD_LEFT: Self = Self(21);
    pub const DPAD_RIGHT: Self = Self(22);
    pub const DPAD_CENTER: Self = Self(23);
    pub const VOLUME_UP: Self = Self(24);
    pub const VOLUME_DOWN: Self = Self(25);
    pub const POWER: Self = Self(26);
    pub const MENU: Self = Self(82);
    pub const MEDIA_PLAY_PAUSE: Self = Self(85);
    pub const MEDIA_STOP: Self = Self(86);
    pub const MEDIA_NEXT: Self = Self(87);
    pub const MEDIA_PREVIOUS: Self = Self(88);
    pub const MEDIA_REWIND: Self = Self(89);
    pub const MEDIA_FAST_FORWARD: Self = Self(90);
    pub const VOLUME_MUTE: Self = Self(164);
    pub const SETTINGS: Self = Self(176);
    pub const TV_POWER: Self = Self(177);
    pub const SLEEP: Self = Self(223);
    pub const WAKEUP: Self = Self(224);

    /// Construct a key code for deliberate use of an Android value which has
    /// no named constant here.
    pub const fn from_raw(value: u16) -> Self {
        Self(value as i32)
    }

    /// Return the Android key-code value carried on the wire.
    pub const fn raw(self) -> u16 {
        self.0 as u16
    }
}

/// Key action encoded by Android TV Remote Service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndroidTvKeyAction {
    Down,
    Up,
    Press,
}

impl AndroidTvKeyAction {
    pub(crate) const fn protocol_value(self) -> i32 {
        match self {
            Self::Down => DIRECTION_DOWN,
            Self::Up => DIRECTION_UP,
            Self::Press => DIRECTION_PRESS,
        }
    }
}

fn validate_client_text(field: &'static str, value: &str) -> Result<(), AndroidTvError> {
    if value.is_empty()
        || value.len() > MAX_CLIENT_TEXT_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(AndroidTvError::InvalidClientInfo(format!(
            "{field} must be 1..=128 bytes without control characters"
        )));
    }
    Ok(())
}
