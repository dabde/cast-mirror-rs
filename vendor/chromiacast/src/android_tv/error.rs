use std::io;

use thiserror::Error;

/// Failure while pairing with or controlling an Android TV device.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AndroidTvError {
    #[error("invalid Android TV client identity: {0}")]
    InvalidIdentity(String),

    #[error("invalid Android TV client information: {0}")]
    InvalidClientInfo(String),

    #[error("pairing code must contain exactly six hexadecimal digits")]
    InvalidPairingCode,

    #[error("Android TV rejected pairing with protocol status {0}")]
    PairingRejected(i32),

    #[error("Android TV does not offer six-digit hexadecimal pairing")]
    UnsupportedPairingConfiguration,

    #[error("Android TV does not support the {0} remote capability")]
    UnsupportedFeature(&'static str),

    #[error("Android TV connection closed")]
    ConnectionClosed,

    #[error("Android TV {0} timed out")]
    TimedOut(&'static str),

    #[error("Android TV connection failed: {0}")]
    Connection(String),

    #[error("Android TV protocol error: {0}")]
    Protocol(String),

    #[error("Android TV cryptographic operation failed: {0}")]
    Crypto(String),

    #[error(transparent)]
    Io(#[from] io::Error),
}

pub(crate) fn invalid_identity(detail: impl Into<String>) -> AndroidTvError {
    AndroidTvError::InvalidIdentity(detail.into())
}

pub(crate) fn protocol_error(detail: impl Into<String>) -> AndroidTvError {
    AndroidTvError::Protocol(detail.into())
}

pub(crate) fn crypto_error(detail: impl Into<String>) -> AndroidTvError {
    AndroidTvError::Crypto(detail.into())
}
