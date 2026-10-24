use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid packet: {0}")]
    InvalidPacket(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("negotiation failed: {0}")]
    NegotiationFailed(String),

    #[error("no streams accepted by receiver")]
    NoAcceptedStreams,

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("device authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("setup endpoint query failed: {0}")]
    SetupFailed(String),

    #[error("app launch failed: {0}")]
    AppLaunchFailed(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EnqueueError {
    #[error("too many unacknowledged frames in flight")]
    ReachedIdSpanLimit,

    #[error("the first frame on a stream must be independently decodable")]
    FirstFrameMustBeKeyFrame,

    #[error("media timestamps must be strictly increasing within a stream")]
    NonMonotonicMediaTimestamp,

    #[error("encoded frames must not be empty")]
    EmptyFrame,

    #[error("session is closed or shutting down")]
    SessionClosed,
}

/// Non-terminal rejection of a target playout-delay update.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PlayoutDelayError {
    #[error("target playout delay must be a whole number of milliseconds between 1 and 65535")]
    InvalidDelay,

    #[error("receiver did not negotiate adaptive playout-delay updates for every active stream")]
    Unsupported,

    #[error("session is closed or shutting down")]
    SessionClosed,
}

/// Terminal failure from a streaming sender task.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SenderError {
    #[error("sender session is closed")]
    SessionClosed,

    #[error("receiver stopped acknowledging media")]
    ReceiverTimedOut,

    #[error("sender task failed: {0}")]
    TaskFailed(String),

    #[error("{operation} failed: {message}")]
    Transport {
        operation: &'static str,
        message: String,
    },
}
