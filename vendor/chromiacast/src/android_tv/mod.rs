//! Android TV Remote Service support.
//!
//! This protocol is independent from Google Cast.  Keeping it behind a
//! separate feature and module lets applications combine both capabilities
//! for devices which implement both without coupling either connection's
//! lifecycle to the other.
//!
//! Pairing is intentionally split into two phases.  Start with
//! [`AndroidTvPairingSession::begin`], display the code entry UI requested by
//! the TV, then consume the session with
//! [`AndroidTvPairingSession::finish`].  The caller owns persistence of the
//! [`AndroidTvRemoteIdentity`]; the library never writes credentials to disk.
//!
//! Capabilities which share a ready Remote Service connection belong behind
//! [`AndroidTvRemote`]. A future streaming capability such as voice input
//! should return its own bounded session handle while the existing private
//! actor continues to own and multiplex the TLS stream; it should not turn key
//! injection into an unbounded media command path.

mod connection;
mod error;
mod framing;
mod identity;
mod pairing;
mod proto;
mod remote;
mod remote_session;
mod remote_types;

pub use error::AndroidTvError;
pub use identity::AndroidTvRemoteIdentity;
pub use pairing::AndroidTvPairingSession;
pub use remote::AndroidTvRemote;
pub use remote_types::{
    AndroidTvDeviceInfo, AndroidTvKeyAction, AndroidTvKeyCode, AndroidTvRemoteClientInfo,
    AndroidTvRemoteEvent, AndroidTvRemoteFeatures, AndroidTvVolume,
};

/// Default TCP port for the Android TV Remote Service.
pub const ANDROID_TV_REMOTE_PORT: u16 = 6466;

/// Default TCP port for the Android TV pairing service.
pub const ANDROID_TV_PAIRING_PORT: u16 = 6467;
