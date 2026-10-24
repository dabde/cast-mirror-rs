mod auth;
mod device_info;
pub(crate) mod framing;
pub(crate) mod proto;

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use serde_json::Value;
use tokio::io::{self, AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};
use tokio_rustls::TlsConnector;

use crate::answer::Answer;
use crate::error::Error;
use crate::offer::Offer;
use crate::tls::SelfSignedCertificateVerifier;

use self::auth::AuthChallenge;
pub use self::auth::DeviceIdentity;
pub use self::device_info::{AuthenticatedDeviceInfo, AuthenticatedEurekaInfo, EurekaInfoOutcome};
use self::framing::{FramedReader, FramedWriter};
use self::proto::{CastMessage, Payload};

/// Standard Cast V2 control-channel port.
pub const CAST_PORT: u16 = 8009;

/// App ID for the built-in mirroring receiver.
pub const APP_MIRRORING: &str = "0F5096E8";

const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const NS_DEVICE_AUTH: &str = "urn:x-cast:com.google.cast.tp.deviceauth";
const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
const NS_RECEIVER_DISCOVERY: &str = "urn:x-cast:com.google.cast.receiver.discovery";
const NS_SETUP: &str = "urn:x-cast:com.google.cast.setup";
const NS_WEBRTC: &str = "urn:x-cast:com.google.cast.webrtc";

const SENDER_ID: &str = "sender-0";
const RECEIVER_ID: &str = "receiver-0";

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A Cast application running on the receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastApp {
    app_id: String,
    transport_id: String,
    session_id: String,
    display_name: Option<String>,
}

impl CastApp {
    /// Cast application ID.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Virtual-connection destination used for app messages.
    pub fn transport_id(&self) -> &str {
        &self.transport_id
    }

    /// Receiver session ID used by STOP requests.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Receiver-provided display name, when present.
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }
}

/// Parsed platform receiver state.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverStatus {
    applications: Vec<CastApp>,
    volume_level: Option<f64>,
    muted: Option<bool>,
}

impl ReceiverStatus {
    /// Applications currently reported by the receiver.
    pub fn applications(&self) -> &[CastApp] {
        &self.applications
    }

    /// Receiver volume in the range reported by the Cast protocol.
    pub fn volume_level(&self) -> Option<f64> {
        self.volume_level
    }

    /// Receiver mute state, when included in the status.
    pub fn is_muted(&self) -> Option<bool> {
        self.muted
    }
}

/// Result of a Cast application-availability query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppAvailability {
    /// The receiver reports that it can launch the application.
    Available,
    /// The receiver reports that the application is unavailable.
    Unavailable,
}

/// Why the control task terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlCloseReason {
    /// The local caller intentionally closed the connection.
    Requested,
    /// The receiver closed the platform virtual connection or the TLS stream.
    PeerClosed,
    /// No Cast heartbeat arrived before the deadline.
    HeartbeatTimeout,
    /// The underlying transport or protocol failed.
    Error(String),
}

/// Unsolicited control-channel state changes.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ControlEvent {
    /// The receiver published a new platform status.
    ReceiverStatus(ReceiverStatus),
    /// A previously reported application disappeared or closed its transport.
    ApplicationStopped(CastApp),
    /// The control task terminated.
    Closed(ControlCloseReason),
}

type JsonResponseSender = oneshot::Sender<Result<Value, Error>>;

enum ReceiverOperation {
    Availability { app_id: String },
    Launch { app_id: String },
    SetVolume,
    Status,
    Stop,
}

impl ReceiverOperation {
    fn payload(&self, request_id: u32) -> Value {
        match self {
            Self::Availability { app_id } => serde_json::json!({
                "type": "GET_APP_AVAILABILITY",
                "appId": [app_id],
                "requestId": request_id,
            }),
            Self::Launch { app_id } => serde_json::json!({
                "type": "LAUNCH",
                "appId": app_id,
                "requestId": request_id,
            }),
            Self::Status => serde_json::json!({
                "type": "GET_STATUS",
                "requestId": request_id,
            }),
            Self::SetVolume | Self::Stop => {
                unreachable!("receiver operation requires a custom payload")
            }
        }
    }
}

enum ControlCommand {
    ReceiverRequest {
        operation: ReceiverOperation,
        payload: Option<Value>,
        response_sender: JsonResponseSender,
    },
    ProductInfoRequest {
        operation: ProductInfoOperation,
        response_sender: JsonResponseSender,
    },
    SendOffer {
        destination: String,
        offer: Value,
        response_sender: JsonResponseSender,
    },
    SendFire {
        namespace: String,
        destination: String,
        payload: Value,
        response_sender: oneshot::Sender<Result<(), Error>>,
    },
    Close {
        response_sender: oneshot::Sender<()>,
    },
}

struct PendingReceiverRequest {
    operation: ReceiverOperation,
    response_sender: JsonResponseSender,
}

struct PendingOffer {
    destination: String,
    response_sender: JsonResponseSender,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProductInfoOperation {
    DeviceInfo,
    EurekaInfo,
}

impl ProductInfoOperation {
    fn namespace(self) -> &'static str {
        match self {
            Self::DeviceInfo => NS_RECEIVER_DISCOVERY,
            Self::EurekaInfo => NS_SETUP,
        }
    }

    fn message_type(self) -> &'static str {
        match self {
            Self::DeviceInfo => "GET_DEVICE_INFO",
            Self::EurekaInfo => "eureka_info",
        }
    }

    fn request_id_field(self) -> &'static str {
        match self {
            Self::DeviceInfo => "requestId",
            Self::EurekaInfo => "request_id",
        }
    }

    fn accepts_response_type(self, message_type: &str) -> bool {
        match self {
            // Production Cast receivers use both response spellings. The
            // authenticated channel, namespace, route, and request ID still
            // provide the trust boundary and exact correlation.
            Self::DeviceInfo => matches!(message_type, "DEVICE_INFO" | "GET_DEVICE_INFO"),
            Self::EurekaInfo => message_type == "eureka_info",
        }
    }

    fn from_namespace(namespace: &str) -> Option<Self> {
        match namespace {
            NS_RECEIVER_DISCOVERY => Some(Self::DeviceInfo),
            NS_SETUP => Some(Self::EurekaInfo),
            _ => None,
        }
    }
}

struct PendingProductInfoRequest {
    operation: ProductInfoOperation,
    response_sender: JsonResponseSender,
}

/// Authenticated connection to a receiver's Cast V2 control channel.
///
/// [`connect`](Self::connect) completes Cast device authentication before this
/// value is returned. The connection also owns heartbeat, request correlation,
/// and application lifecycle handling.
pub struct CastConnection {
    command_sender: mpsc::Sender<ControlCommand>,
    event_sender: broadcast::Sender<ControlEvent>,
    remote_address: SocketAddr,
    device_identity: Option<DeviceIdentity>,
    task: JoinHandle<Result<(), Error>>,
}

impl fmt::Debug for CastConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CastConnection")
            .field("remote_address", &self.remote_address)
            .field("authenticated", &self.device_identity.is_some())
            .finish_non_exhaustive()
    }
}

impl CastConnection {
    /// Connect to and authenticate a receiver on the standard Cast port.
    pub async fn connect(ip: IpAddr) -> Result<Self, Error> {
        Self::connect_address(SocketAddr::new(ip, CAST_PORT)).await
    }

    /// Connect to and authenticate a receiver at a specific endpoint.
    pub async fn connect_address(address: SocketAddr) -> Result<Self, Error> {
        Self::connect_address_inner(address, true).await
    }

    /// Connect without Cast device authentication.
    ///
    /// This exists only for protocol test doubles such as shanocast. Production
    /// code must use [`connect`](Self::connect) or
    /// [`connect_address`](Self::connect_address).
    #[cfg(feature = "dangerous-unverified")]
    #[doc(hidden)]
    pub async fn connect_unverified_for_testing(ip: IpAddr) -> Result<Self, Error> {
        Self::connect_address_unverified_for_testing(SocketAddr::new(ip, CAST_PORT)).await
    }

    /// Connect to a test endpoint without Cast device authentication.
    #[cfg(feature = "dangerous-unverified")]
    #[doc(hidden)]
    pub async fn connect_address_unverified_for_testing(
        address: SocketAddr,
    ) -> Result<Self, Error> {
        Self::connect_address_inner(address, false).await
    }

    async fn connect_address_inner(
        address: SocketAddr,
        authenticate_device: bool,
    ) -> Result<Self, Error> {
        let tcp = timeout(REQUEST_TIMEOUT, TcpStream::connect(address))
            .await
            .map_err(|_| connection_error(format!("TCP connect to {address} timed out")))?
            .map_err(|error| connection_error(format!("TCP connect to {address}: {error}")))?;

        let verifier = Arc::new(SelfSignedCertificateVerifier::new());
        let tls_config = Arc::new(
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth(),
        );
        let connector = TlsConnector::from(tls_config);
        let domain = ServerName::IpAddress(address.ip().into());
        let tls_stream = timeout(REQUEST_TIMEOUT, connector.connect(domain, tcp))
            .await
            .map_err(|_| connection_error(format!("TLS handshake with {address} timed out")))?
            .map_err(|error| connection_error(format!("TLS handshake with {address}: {error}")))?;

        let peer_certificate = tls_stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certificates| certificates.first())
            .map(|certificate| certificate.as_ref().to_vec())
            .ok_or_else(|| connection_error("receiver did not present a TLS certificate"))?;

        let (read_half, write_half) = io::split(tls_stream);
        let mut reader = FramedReader::new(read_half);
        let mut writer = FramedWriter::new(write_half);

        let device_identity = if authenticate_device {
            Some(authenticate_receiver(&mut reader, &mut writer, &peer_certificate).await?)
        } else {
            None
        };

        write_json_message_timed(
            &mut writer,
            SENDER_ID,
            RECEIVER_ID,
            NS_CONNECTION,
            &serde_json::json!({"type": "CONNECT"}),
        )
        .await
        .map_err(|error| connection_error(format!("initial CONNECT: {error}")))?;

        let (command_sender, command_receiver) = mpsc::channel(32);
        let (event_sender, _) = broadcast::channel(32);
        let task_event_sender = event_sender.clone();
        let task = tokio::spawn(control_loop(
            reader,
            writer,
            command_receiver,
            task_event_sender,
        ));

        Ok(Self {
            command_sender,
            event_sender,
            remote_address: address,
            device_identity,
            task,
        })
    }

    /// Endpoint selected for this control connection.
    pub fn remote_address(&self) -> SocketAddr {
        self.remote_address
    }

    /// IP address of the connected receiver.
    pub fn remote_ip(&self) -> IpAddr {
        self.remote_address.ip()
    }

    /// Authenticated device identity.
    ///
    /// This is `None` only for connections made through the explicitly
    /// unverified testing constructor.
    pub fn device_identity(&self) -> Option<&DeviceIdentity> {
        self.device_identity.as_ref()
    }

    /// Subscribe to unsolicited receiver status and closure notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<ControlEvent> {
        self.event_sender.subscribe()
    }

    /// Query identity and capability metadata asserted over this authenticated
    /// Cast control channel.
    pub async fn get_device_info(&self) -> Result<AuthenticatedDeviceInfo, Error> {
        self.require_authenticated_product_info()?;
        let value = self
            .product_info_request(ProductInfoOperation::DeviceInfo, false)
            .await?
            .ok_or_else(|| Error::ProtocolError("GET_DEVICE_INFO returned no response".into()))?;
        device_info::parse_device_info(&value)
    }

    /// Query optional hardware product metadata over this authenticated Cast
    /// control channel.
    ///
    /// Older receivers may reject or omit this operation. A correlated
    /// rejection or a bounded response timeout returns
    /// [`EurekaInfoOutcome::Unsupported`]; malformed successful replies remain
    /// protocol errors.
    pub async fn get_eureka_info(&self) -> Result<EurekaInfoOutcome, Error> {
        self.require_authenticated_product_info()?;
        let Some(value) = self
            .product_info_request(ProductInfoOperation::EurekaInfo, true)
            .await?
        else {
            return Ok(EurekaInfoOutcome::Unsupported);
        };
        device_info::parse_eureka_info(&value)
    }

    /// Query hardware presentation metadata from the selected receiver's
    /// local HTTPS setup endpoint.
    ///
    /// The request is routed directly to the IP address used by this control
    /// connection and is bounded by a short timeout and response-size limit.
    /// Unlike [`get_device_info`](Self::get_device_info), the returned strings
    /// are not covered by Cast device authentication. Callers should correlate
    /// the returned SSDP UDN with the authenticated device ID when available.
    pub async fn get_setup_device_info(&self) -> Result<crate::SetupInfoOutcome, Error> {
        self.require_authenticated_product_info()?;
        crate::setup::query_device_info(self.remote_address).await
    }

    /// Ask whether a receiver supports a Cast application ID.
    pub async fn get_app_availability(&self, app_id: &str) -> Result<AppAvailability, Error> {
        let value = self
            .receiver_request(
                ReceiverOperation::Availability {
                    app_id: app_id.to_owned(),
                },
                None,
                "application availability",
            )
            .await?;

        let availability = value
            .get("availability")
            .and_then(|entries| entries.get(app_id))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::ProtocolError(format!(
                    "availability response omitted application {app_id}"
                ))
            })?;

        match availability {
            "APP_AVAILABLE" => Ok(AppAvailability::Available),
            "APP_UNAVAILABLE" => Ok(AppAvailability::Unavailable),
            other => Err(Error::ProtocolError(format!(
                "unknown application availability {other}"
            ))),
        }
    }

    /// Query the current platform receiver status.
    pub async fn status(&self) -> Result<ReceiverStatus, Error> {
        let value = self
            .receiver_request(ReceiverOperation::Status, None, "receiver status")
            .await?;
        parse_receiver_status(&value)
    }

    /// Set the receiver's platform volume level in the inclusive 0.0..=1.0
    /// range and return the acknowledged receiver status.
    pub async fn set_volume_level(&self, level: f64) -> Result<ReceiverStatus, Error> {
        if !level.is_finite() || !(0.0..=1.0).contains(&level) {
            return Err(Error::ProtocolError(
                "receiver volume level must be finite and in 0.0..=1.0".into(),
            ));
        }
        self.set_volume(serde_json::json!({"level": level})).await
    }

    /// Set the receiver's platform mute state and return the acknowledged
    /// receiver status.
    pub async fn set_muted(&self, muted: bool) -> Result<ReceiverStatus, Error> {
        self.set_volume(serde_json::json!({"muted": muted})).await
    }

    async fn set_volume(&self, volume: Value) -> Result<ReceiverStatus, Error> {
        let value = self
            .receiver_request(
                ReceiverOperation::SetVolume,
                Some(serde_json::json!({
                    "type": "SET_VOLUME",
                    "volume": volume,
                })),
                "receiver volume update",
            )
            .await?;
        parse_receiver_status(&value)
    }

    /// Launch a Cast application by app ID.
    ///
    /// Use [`APP_MIRRORING`] for screen or tab mirroring.
    pub async fn launch(&self, app_id: &str) -> Result<CastApp, Error> {
        let value = self
            .receiver_request(
                ReceiverOperation::Launch {
                    app_id: app_id.to_owned(),
                },
                None,
                "application launch",
            )
            .await?;
        let status = parse_receiver_status(&value)?;
        let app = status
            .applications
            .into_iter()
            .find(|app| app.app_id == app_id)
            .ok_or_else(|| {
                Error::AppLaunchFailed(format!(
                    "application {app_id} was absent from RECEIVER_STATUS"
                ))
            })?;

        self.send_fire(
            NS_CONNECTION,
            app.transport_id(),
            serde_json::json!({"type": "CONNECT"}),
        )
        .await?;
        Ok(app)
    }

    /// Stop a launched application and close its virtual connection.
    pub async fn stop(&self, app: &CastApp) -> Result<(), Error> {
        self.send_fire(
            NS_CONNECTION,
            app.transport_id(),
            serde_json::json!({"type": "CLOSE"}),
        )
        .await?;

        let payload = serde_json::json!({
            "type": "STOP",
            "sessionId": app.session_id(),
        });
        self.receiver_request(ReceiverOperation::Stop, Some(payload), "application stop")
            .await?;
        Ok(())
    }

    /// Exchange a streaming OFFER with a launched receiver application.
    ///
    /// Every call receives a distinct sequence number and can run concurrently
    /// with other control operations.
    pub async fn exchange_offer(&self, offer: &Offer, app: &CastApp) -> Result<Answer, Error> {
        if self
            .device_identity
            .as_ref()
            .is_some_and(DeviceIdentity::is_audio_only)
            && offer.has_video()
        {
            return Err(Error::NegotiationFailed(
                "authenticated receiver credential is restricted to audio-only use".into(),
            ));
        }
        let offer = serde_json::to_value(offer)
            .map_err(|error| Error::ProtocolError(format!("serialize OFFER: {error}")))?;
        let (response_sender, response_receiver) = oneshot::channel();
        self.command_sender
            .send(ControlCommand::SendOffer {
                destination: app.transport_id.clone(),
                offer,
                response_sender,
            })
            .await
            .map_err(|_| control_closed_error())?;

        let response = await_json_response(response_receiver, "streaming OFFER").await?;
        if response.get("result").and_then(Value::as_str) == Some("error") {
            let description = response
                .get("error")
                .and_then(|error| error.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("unknown receiver error");
            return Err(Error::NegotiationFailed(format!(
                "receiver rejected OFFER: {description}"
            )));
        }

        let answer = response
            .get("answer")
            .ok_or_else(|| Error::ProtocolError("ANSWER omitted the answer field".into()))?;
        serde_json::from_value(answer.clone())
            .map_err(|error| Error::ProtocolError(format!("parse ANSWER: {error}")))
    }

    /// Stop known applications and close the control channel gracefully.
    ///
    /// The returned error is the terminal result of the control task.
    pub async fn close(self) -> Result<(), Error> {
        let (response_sender, response_receiver) = oneshot::channel();
        if self
            .command_sender
            .send(ControlCommand::Close { response_sender })
            .await
            .is_ok()
        {
            let _ = timeout(REQUEST_TIMEOUT, response_receiver).await;
        }

        self.task
            .await
            .map_err(|error| connection_error(format!("control task failed: {error}")))?
    }

    async fn receiver_request(
        &self,
        operation: ReceiverOperation,
        payload: Option<Value>,
        description: &'static str,
    ) -> Result<Value, Error> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.command_sender
            .send(ControlCommand::ReceiverRequest {
                operation,
                payload,
                response_sender,
            })
            .await
            .map_err(|_| control_closed_error())?;
        await_json_response(response_receiver, description).await
    }

    fn require_authenticated_product_info(&self) -> Result<(), Error> {
        if self.device_identity.is_none() {
            return Err(Error::AuthenticationFailed(
                "product information requires an authenticated receiver".into(),
            ));
        }
        Ok(())
    }

    async fn product_info_request(
        &self,
        operation: ProductInfoOperation,
        timeout_is_unsupported: bool,
    ) -> Result<Option<Value>, Error> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.command_sender
            .send(ControlCommand::ProductInfoRequest {
                operation,
                response_sender,
            })
            .await
            .map_err(|_| control_closed_error())?;
        match timeout(REQUEST_TIMEOUT, response_receiver).await {
            Ok(Ok(result)) => result.map(Some),
            Ok(Err(_)) => Err(control_closed_error()),
            Err(_) if timeout_is_unsupported => Ok(None),
            Err(_) => Err(Error::ProtocolError(format!(
                "{} timed out",
                operation.message_type()
            ))),
        }
    }

    async fn send_fire(
        &self,
        namespace: &str,
        destination: &str,
        payload: Value,
    ) -> Result<(), Error> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.command_sender
            .send(ControlCommand::SendFire {
                namespace: namespace.to_owned(),
                destination: destination.to_owned(),
                payload,
                response_sender,
            })
            .await
            .map_err(|_| control_closed_error())?;
        timeout(REQUEST_TIMEOUT, response_receiver)
            .await
            .map_err(|_| Error::ProtocolError("control write timed out".into()))?
            .map_err(|_| control_closed_error())?
    }
}

async fn await_json_response(
    receiver: oneshot::Receiver<Result<Value, Error>>,
    operation: &'static str,
) -> Result<Value, Error> {
    timeout(REQUEST_TIMEOUT, receiver)
        .await
        .map_err(|_| Error::ProtocolError(format!("{operation} timed out")))?
        .map_err(|_| control_closed_error())?
}

async fn authenticate_receiver<R, W>(
    reader: &mut FramedReader<R>,
    writer: &mut FramedWriter<W>,
    tls_peer_certificate: &[u8],
) -> Result<DeviceIdentity, Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let AuthChallenge { nonce, payload } = auth::create_challenge();
    timeout(
        REQUEST_TIMEOUT,
        writer.write_message(&CastMessage {
            source_id: SENDER_ID.into(),
            destination_id: RECEIVER_ID.into(),
            namespace: NS_DEVICE_AUTH.into(),
            payload: Payload::Binary(payload),
        }),
    )
    .await
    .map_err(|_| Error::AuthenticationFailed("send challenge timed out".into()))?
    .map_err(|error| {
        Error::AuthenticationFailed(format!("send authentication challenge: {error}"))
    })?;

    let message = timeout(REQUEST_TIMEOUT, reader.read_message())
        .await
        .map_err(|_| Error::AuthenticationFailed("challenge response timed out".into()))?
        .map_err(|error| {
            Error::AuthenticationFailed(format!("read challenge response: {error}"))
        })?;

    if message.namespace != NS_DEVICE_AUTH
        || message.source_id != RECEIVER_ID
        || message.destination_id != SENDER_ID
    {
        return Err(Error::AuthenticationFailed(format!(
            "unexpected challenge response namespace or route: {} {} -> {}",
            message.namespace, message.source_id, message.destination_id
        )));
    }
    let Payload::Binary(payload) = message.payload else {
        return Err(Error::AuthenticationFailed(
            "challenge response was not a binary protobuf".into(),
        ));
    };

    auth::verify_challenge_reply(&payload, &nonce, tls_peer_certificate)
}

async fn control_loop<R, W>(
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
    mut command_receiver: mpsc::Receiver<ControlCommand>,
    event_sender: broadcast::Sender<ControlEvent>,
) -> Result<(), Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut next_request_id = 1u32;
    let mut next_product_info_request_id = 1u32;
    let mut next_sequence_number = 1u32;
    let mut pending_receiver = HashMap::<u32, PendingReceiverRequest>::new();
    let mut pending_product_info = HashMap::<u32, PendingProductInfoRequest>::new();
    let mut pending_offers = HashMap::<u32, PendingOffer>::new();
    let mut known_apps = HashMap::<String, CastApp>::new();
    let mut last_heartbeat = Instant::now();
    let mut heartbeat = interval_at(last_heartbeat + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let (result, close_reason) = loop {
        pending_receiver.retain(|_, pending| !pending.response_sender.is_closed());
        pending_product_info.retain(|_, pending| !pending.response_sender.is_closed());
        pending_offers.retain(|_, pending| !pending.response_sender.is_closed());

        tokio::select! {
            incoming = reader.read_message() => {
                let message = match incoming {
                    Ok(message) => message,
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                        break (
                            Err(connection_error("receiver closed the TLS stream")),
                            ControlCloseReason::PeerClosed,
                        );
                    }
                    Err(error) => {
                        let detail = format!("read Cast message: {error}");
                        break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                    }
                };
                let incoming_type = cast_message_type(&message);
                tracing::trace!(
                    target: "chromiacast::control",
                    direction = "inbound",
                    namespace = %message.namespace,
                    source = %message.source_id,
                    destination = %message.destination_id,
                    message_type = incoming_type.as_deref().unwrap_or(match &message.payload {
                        Payload::Binary(_) => "binary",
                        Payload::String(_) => "unknown",
                    }),
                    "Cast control message",
                );

                if message.namespace == NS_HEARTBEAT {
                    let is_ping = message_is_type(&message, "PING");
                    let is_pong = message_is_type(&message, "PONG");
                    if is_ping || is_pong {
                        last_heartbeat = Instant::now();
                    }
                    if is_ping {
                        if let Err(error) = write_json_message_timed(
                            &mut writer,
                            SENDER_ID,
                            &message.source_id,
                            NS_HEARTBEAT,
                            &serde_json::json!({"type": "PONG"}),
                        ).await {
                            let detail = format!("send heartbeat PONG: {error}");
                            break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                        }
                    }
                    continue;
                }

                if message.namespace == NS_CONNECTION && message_is_type(&message, "CLOSE") {
                    if message.source_id == RECEIVER_ID {
                        break (
                            Err(connection_error("receiver closed the platform connection")),
                            ControlCloseReason::PeerClosed,
                        );
                    }
                    if let Some(app) = take_app_by_transport(&mut known_apps, &message.source_id) {
                        let _ = event_sender.send(ControlEvent::ApplicationStopped(app));
                    }
                    fail_offers_for_destination(
                        &mut pending_offers,
                        &message.source_id,
                        "application transport closed",
                    );
                    continue;
                }

                let dispatched = if ProductInfoOperation::from_namespace(&message.namespace).is_some() {
                    dispatch_product_info_message(message, &mut pending_product_info)
                } else {
                    dispatch_json_message(
                        message,
                        &mut pending_receiver,
                        &mut pending_offers,
                        &mut known_apps,
                        &event_sender,
                    )
                };
                if let Err(error) = dispatched {
                    let detail = error.to_string();
                    break (Err(error), ControlCloseReason::Error(detail));
                }
            }

            command = command_receiver.recv() => {
                match command {
                    Some(ControlCommand::ReceiverRequest {
                        operation,
                        payload,
                        response_sender,
                    }) => {
                        let request_id = allocate_id(&mut next_request_id, &pending_receiver);
                        let mut payload = payload.unwrap_or_else(|| operation.payload(request_id));
                        payload["requestId"] = Value::from(request_id);
                        pending_receiver.insert(request_id, PendingReceiverRequest {
                            operation,
                            response_sender,
                        });
                        if let Err(error) = write_json_message_timed(
                            &mut writer,
                            SENDER_ID,
                            RECEIVER_ID,
                            NS_RECEIVER,
                            &payload,
                        ).await {
                            let detail = format!("send receiver request: {error}");
                            break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                        }
                    }
                    Some(ControlCommand::ProductInfoRequest {
                        operation,
                        response_sender,
                    }) => {
                        let request_id = allocate_id(
                            &mut next_product_info_request_id,
                            &pending_product_info,
                        );
                        let mut payload = serde_json::json!({
                            "type": operation.message_type(),
                        });
                        payload[operation.request_id_field()] = Value::from(request_id);
                        pending_product_info.insert(request_id, PendingProductInfoRequest {
                            operation,
                            response_sender,
                        });
                        if let Err(error) = write_json_message_timed(
                            &mut writer,
                            SENDER_ID,
                            RECEIVER_ID,
                            operation.namespace(),
                            &payload,
                        ).await {
                            let detail = format!("send product-info request: {error}");
                            break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                        }
                    }
                    Some(ControlCommand::SendOffer {
                        destination,
                        offer,
                        response_sender,
                    }) => {
                        let sequence_number = allocate_id(&mut next_sequence_number, &pending_offers);
                        let payload = serde_json::json!({
                            "type": "OFFER",
                            "seqNum": sequence_number,
                            "offer": offer,
                        });
                        pending_offers.insert(sequence_number, PendingOffer {
                            destination: destination.clone(),
                            response_sender,
                        });
                        if let Err(error) = write_json_message_timed(
                            &mut writer,
                            SENDER_ID,
                            &destination,
                            NS_WEBRTC,
                            &payload,
                        ).await {
                            let detail = format!("send streaming OFFER: {error}");
                            break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                        }
                    }
                    Some(ControlCommand::SendFire {
                        namespace,
                        destination,
                        payload,
                        response_sender,
                    }) => {
                        match write_json_message_timed(
                            &mut writer,
                            SENDER_ID,
                            &destination,
                            &namespace,
                            &payload,
                        ).await {
                            Ok(()) => {
                                let _ = response_sender.send(Ok(()));
                            }
                            Err(error) => {
                                let detail = format!("send Cast message: {error}");
                                let _ = response_sender.send(Err(connection_error(&detail)));
                                break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                            }
                        }
                    }
                    Some(ControlCommand::Close { response_sender }) => {
                        let teardown = send_teardown(&mut writer, known_apps.values()).await;
                        let _ = response_sender.send(());
                        match teardown {
                            Ok(()) => break (Ok(()), ControlCloseReason::Requested),
                            Err(error) => {
                                let detail = format!("send Cast teardown: {error}");
                                break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                            }
                        }
                    }
                    None => {
                        let teardown = send_teardown(&mut writer, known_apps.values()).await;
                        match teardown {
                            Ok(()) => break (Ok(()), ControlCloseReason::Requested),
                            Err(error) => {
                                let detail = format!("send Cast teardown: {error}");
                                break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                            }
                        }
                    }
                }
            }

            _ = heartbeat.tick() => {
                if last_heartbeat.elapsed() >= HEARTBEAT_TIMEOUT {
                    break (
                        Err(connection_error("receiver heartbeat timed out")),
                        ControlCloseReason::HeartbeatTimeout,
                    );
                }
                if let Err(error) = write_json_message_timed(
                    &mut writer,
                    SENDER_ID,
                    RECEIVER_ID,
                    NS_HEARTBEAT,
                    &serde_json::json!({"type": "PING"}),
                ).await {
                    let detail = format!("send heartbeat PING: {error}");
                    break (Err(connection_error(&detail)), ControlCloseReason::Error(detail));
                }
            }
        }
    };

    let failure_detail = match &result {
        Ok(()) => "control channel closed".to_owned(),
        Err(error) => error.to_string(),
    };
    fail_all_pending(
        &mut pending_receiver,
        &mut pending_product_info,
        &mut pending_offers,
        &failure_detail,
    );
    let _ = event_sender.send(ControlEvent::Closed(close_reason));
    result
}

fn dispatch_product_info_message(
    message: CastMessage,
    pending: &mut HashMap<u32, PendingProductInfoRequest>,
) -> Result<(), Error> {
    let operation = ProductInfoOperation::from_namespace(&message.namespace)
        .expect("caller classified the product-info namespace");
    if message.source_id != RECEIVER_ID || message.destination_id != SENDER_ID {
        return Ok(());
    }
    let Payload::String(payload) = &message.payload else {
        return Err(Error::ProtocolError(format!(
            "non-JSON response on {}",
            message.namespace
        )));
    };
    let value: Value = serde_json::from_str(payload).map_err(|error| {
        Error::ProtocolError(format!(
            "invalid JSON on namespace {}: {error}",
            message.namespace
        ))
    })?;
    let Some(request_id) = json_u32(&value, operation.request_id_field()) else {
        return Ok(());
    };
    if !pending
        .get(&request_id)
        .is_some_and(|request| request.operation == operation)
    {
        return Ok(());
    }

    let message_type = json_message_type(&value).unwrap_or("");
    let response =
        if operation.accepts_response_type(message_type) || message_type == "INVALID_REQUEST" {
            Ok(value)
        } else {
            Err(Error::ProtocolError(format!(
                "correlated {} response has unexpected type {message_type:?}",
                operation.message_type()
            )))
        };
    let request = pending
        .remove(&request_id)
        .expect("product-info request disappeared after lookup");
    let _ = request.response_sender.send(response);
    Ok(())
}

fn dispatch_json_message(
    message: CastMessage,
    pending_receiver: &mut HashMap<u32, PendingReceiverRequest>,
    pending_offers: &mut HashMap<u32, PendingOffer>,
    known_apps: &mut HashMap<String, CastApp>,
    event_sender: &broadcast::Sender<ControlEvent>,
) -> Result<(), Error> {
    if message.namespace != NS_RECEIVER && message.namespace != NS_WEBRTC {
        tracing::trace!(
            target: "chromiacast::control",
            namespace = %message.namespace,
            "ignored message on unsupported namespace",
        );
        return Ok(());
    }
    let Payload::String(payload) = &message.payload else {
        return Ok(());
    };
    let value: Value = serde_json::from_str(payload).map_err(|error| {
        Error::ProtocolError(format!(
            "invalid JSON on namespace {}: {error}",
            message.namespace
        ))
    })?;
    let message_type = json_message_type(&value).unwrap_or("");

    if message.namespace == NS_RECEIVER {
        if message.source_id != RECEIVER_ID {
            return Ok(());
        }
        match message_type {
            "RECEIVER_STATUS" => {
                let status = parse_receiver_status(&value)?;
                publish_receiver_status(status, known_apps, event_sender);
                complete_receiver_status(&value, pending_receiver);
            }
            "GET_APP_AVAILABILITY" => {
                complete_receiver_request(&value, pending_receiver, |operation| {
                    matches!(operation, ReceiverOperation::Availability { .. })
                });
            }
            "LAUNCH_ERROR" | "INVALID_REQUEST" => {
                let detail = receiver_error_description(&value, message_type);
                fail_receiver_request(&value, pending_receiver, &detail);
            }
            _ => {}
        }
    } else if message.namespace == NS_WEBRTC {
        match message_type {
            "ANSWER" => complete_offer(&value, &message.source_id, pending_offers),
            "INVALID_REQUEST" => {
                let detail = receiver_error_description(&value, message_type);
                fail_offer(&value, &message.source_id, pending_offers, &detail);
            }
            _ => {}
        }
    }
    Ok(())
}

fn complete_receiver_status(value: &Value, pending: &mut HashMap<u32, PendingReceiverRequest>) {
    if let Some(request_id) = json_u32(value, "requestId") {
        let complete = pending
            .get(&request_id)
            .is_some_and(|request| match &request.operation {
                ReceiverOperation::Launch { app_id } => receiver_status_contains_app(value, app_id),
                ReceiverOperation::SetVolume
                | ReceiverOperation::Status
                | ReceiverOperation::Stop => true,
                ReceiverOperation::Availability { .. } => false,
            });
        if complete {
            if let Some(request) = pending.remove(&request_id) {
                let _ = request.response_sender.send(Ok(value.clone()));
            }
            return;
        }
    }

    // Some production receivers omit requestId on launch status updates. Only
    // complete launches whose requested app is visibly present.
    let matching: Vec<_> = pending
        .iter()
        .filter_map(|(request_id, request)| {
            let ReceiverOperation::Launch { app_id } = &request.operation else {
                return None;
            };
            receiver_status_contains_app(value, app_id).then_some(*request_id)
        })
        .collect();
    for request_id in matching {
        if let Some(request) = pending.remove(&request_id) {
            let _ = request.response_sender.send(Ok(value.clone()));
        }
    }
}

fn complete_receiver_request(
    value: &Value,
    pending: &mut HashMap<u32, PendingReceiverRequest>,
    expected: impl FnOnce(&ReceiverOperation) -> bool,
) {
    let Some(request_id) = json_u32(value, "requestId") else {
        return;
    };
    if pending
        .get(&request_id)
        .is_some_and(|request| expected(&request.operation))
    {
        if let Some(request) = pending.remove(&request_id) {
            let _ = request.response_sender.send(Ok(value.clone()));
        }
    }
}

fn complete_offer(value: &Value, source: &str, pending: &mut HashMap<u32, PendingOffer>) {
    let Some(sequence_number) = json_u32(value, "seqNum") else {
        return;
    };
    if pending
        .get(&sequence_number)
        .is_some_and(|request| request.destination == source)
    {
        let request = pending
            .remove(&sequence_number)
            .expect("pending OFFER disappeared after lookup");
        let _ = request.response_sender.send(Ok(value.clone()));
    }
}

fn fail_receiver_request(
    value: &Value,
    receiver_requests: &mut HashMap<u32, PendingReceiverRequest>,
    detail: &str,
) {
    let request_id = json_u32(value, "requestId").or_else(|| {
        let reported_app_id = value.get("appId").and_then(Value::as_str);
        let mut matching = receiver_requests
            .iter()
            .filter_map(
                |(request_id, request)| match (&request.operation, reported_app_id) {
                    (ReceiverOperation::Launch { app_id }, Some(reported))
                        if app_id == reported =>
                    {
                        Some(*request_id)
                    }
                    (_, None) => Some(*request_id),
                    _ => None,
                },
            );
        let first = matching.next()?;
        matching.next().is_none().then_some(first)
    });
    if let Some(request_id) = request_id {
        if let Some(request) = receiver_requests.remove(&request_id) {
            let error = match request.operation {
                ReceiverOperation::Launch { .. } => Error::AppLaunchFailed(detail.into()),
                _ => Error::ProtocolError(detail.into()),
            };
            let _ = request.response_sender.send(Err(error));
        }
    }
}

fn fail_offer(value: &Value, source: &str, offers: &mut HashMap<u32, PendingOffer>, detail: &str) {
    let sequence_number = json_u32(value, "seqNum").or_else(|| {
        let mut matching = offers
            .iter()
            .filter_map(|(sequence, request)| (request.destination == source).then_some(*sequence));
        let first = matching.next()?;
        matching.next().is_none().then_some(first)
    });
    if let Some(sequence_number) = sequence_number {
        if offers
            .get(&sequence_number)
            .is_some_and(|request| request.destination == source)
        {
            let request = offers
                .remove(&sequence_number)
                .expect("pending OFFER disappeared after lookup");
            let _ = request
                .response_sender
                .send(Err(Error::NegotiationFailed(detail.into())));
        }
    }
}

fn publish_receiver_status(
    status: ReceiverStatus,
    known_apps: &mut HashMap<String, CastApp>,
    event_sender: &broadcast::Sender<ControlEvent>,
) {
    let new_apps: HashMap<_, _> = status
        .applications
        .iter()
        .cloned()
        .map(|app| (app.transport_id.clone(), app))
        .collect();
    for (transport_id, app) in known_apps.iter() {
        if !new_apps.contains_key(transport_id) {
            let _ = event_sender.send(ControlEvent::ApplicationStopped(app.clone()));
        }
    }
    *known_apps = new_apps;
    let _ = event_sender.send(ControlEvent::ReceiverStatus(status));
}

fn parse_receiver_status(value: &Value) -> Result<ReceiverStatus, Error> {
    let status = value
        .get("status")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::ProtocolError("RECEIVER_STATUS omitted status".into()))?;

    let applications = status
        .get("applications")
        .and_then(Value::as_array)
        .map(|applications| applications.iter().filter_map(parse_cast_app).collect())
        .unwrap_or_default();
    let volume = status.get("volume").and_then(Value::as_object);
    Ok(ReceiverStatus {
        applications,
        volume_level: volume
            .and_then(|volume| volume.get("level"))
            .and_then(Value::as_f64),
        muted: volume
            .and_then(|volume| volume.get("muted"))
            .and_then(Value::as_bool),
    })
}

fn parse_cast_app(value: &Value) -> Option<CastApp> {
    Some(CastApp {
        app_id: value.get("appId")?.as_str()?.to_owned(),
        transport_id: value.get("transportId")?.as_str()?.to_owned(),
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        display_name: value
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn receiver_status_contains_app(status: &Value, app_id: &str) -> bool {
    status
        .get("status")
        .and_then(|status| status.get("applications"))
        .and_then(Value::as_array)
        .is_some_and(|applications| {
            applications
                .iter()
                .any(|app| app.get("appId").and_then(Value::as_str) == Some(app_id))
        })
}

fn receiver_error_description(value: &Value, fallback: &str) -> String {
    value
        .get("reason")
        .or_else(|| value.get("error"))
        .and_then(|error| {
            error
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .unwrap_or(fallback)
        .to_owned()
}

fn message_is_type(message: &CastMessage, expected: &str) -> bool {
    cast_message_type(message).as_deref() == Some(expected)
}

fn cast_message_type(message: &CastMessage) -> Option<String> {
    let Payload::String(payload) = &message.payload else {
        return None;
    };
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| json_message_type(&value).map(str::to_owned))
}

fn json_message_type(value: &Value) -> Option<&str> {
    value
        .get("type")
        .or_else(|| value.get("responseType"))
        .and_then(Value::as_str)
}

fn json_u32(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn allocate_id<T>(next: &mut u32, pending: &HashMap<u32, T>) -> u32 {
    loop {
        let candidate = *next;
        *next = next.wrapping_add(1).max(1);
        if candidate != 0 && !pending.contains_key(&candidate) {
            return candidate;
        }
    }
}

fn take_app_by_transport(
    applications: &mut HashMap<String, CastApp>,
    transport_id: &str,
) -> Option<CastApp> {
    applications.remove(transport_id)
}

fn fail_offers_for_destination(
    pending: &mut HashMap<u32, PendingOffer>,
    destination: &str,
    detail: &str,
) {
    let failed: Vec<_> = pending
        .iter()
        .filter_map(|(sequence, request)| (request.destination == destination).then_some(*sequence))
        .collect();
    for sequence in failed {
        if let Some(request) = pending.remove(&sequence) {
            let _ = request
                .response_sender
                .send(Err(Error::NegotiationFailed(detail.into())));
        }
    }
}

fn fail_all_pending(
    receiver_requests: &mut HashMap<u32, PendingReceiverRequest>,
    product_info_requests: &mut HashMap<u32, PendingProductInfoRequest>,
    offers: &mut HashMap<u32, PendingOffer>,
    detail: &str,
) {
    for (_, request) in receiver_requests.drain() {
        let _ = request
            .response_sender
            .send(Err(connection_error(detail.to_owned())));
    }
    for (_, request) in product_info_requests.drain() {
        let _ = request
            .response_sender
            .send(Err(connection_error(detail.to_owned())));
    }
    for (_, request) in offers.drain() {
        let _ = request
            .response_sender
            .send(Err(connection_error(detail.to_owned())));
    }
}

async fn send_teardown<'a, W>(
    writer: &mut FramedWriter<W>,
    applications: impl Iterator<Item = &'a CastApp>,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    for app in applications {
        write_json_message_timed(
            writer,
            SENDER_ID,
            app.transport_id(),
            NS_CONNECTION,
            &serde_json::json!({"type": "CLOSE"}),
        )
        .await?;
        if !app.session_id().is_empty() {
            write_json_message_timed(
                writer,
                SENDER_ID,
                RECEIVER_ID,
                NS_RECEIVER,
                &serde_json::json!({
                    "type": "STOP",
                    "sessionId": app.session_id(),
                    "requestId": 0,
                }),
            )
            .await?;
        }
    }
    write_json_message_timed(
        writer,
        SENDER_ID,
        RECEIVER_ID,
        NS_CONNECTION,
        &serde_json::json!({"type": "CLOSE"}),
    )
    .await
}

async fn write_json_message<W>(
    writer: &mut FramedWriter<W>,
    source: &str,
    destination: &str,
    namespace: &str,
    payload: &Value,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    tracing::trace!(
        target: "chromiacast::control",
        direction = "outbound",
        namespace,
        source,
        destination,
        message_type = payload
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown"),
        request_id = json_u32(payload, "requestId")
            .or_else(|| json_u32(payload, "request_id")),
        sequence_number = json_u32(payload, "seqNum"),
        "Cast control message",
    );
    writer
        .write_message(&CastMessage {
            source_id: source.to_owned(),
            destination_id: destination.to_owned(),
            namespace: namespace.to_owned(),
            payload: Payload::String(payload.to_string()),
        })
        .await
}

async fn write_json_message_timed<W>(
    writer: &mut FramedWriter<W>,
    source: &str,
    destination: &str,
    namespace: &str,
    payload: &Value,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    timeout(
        REQUEST_TIMEOUT,
        write_json_message(writer, source, destination, namespace, payload),
    )
    .await
    .map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "Cast control write timed out")
    })?
}

fn connection_error(detail: impl Into<String>) -> Error {
    Error::ConnectionFailed(detail.into())
}

fn control_closed_error() -> Error {
    connection_error("control channel closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(request_id: Option<u32>, apps: Value) -> Value {
        let mut value = serde_json::json!({
            "type": "RECEIVER_STATUS",
            "status": {
                "applications": apps,
                "volume": {"level": 0.5, "muted": false},
            },
        });
        if let Some(request_id) = request_id {
            value["requestId"] = request_id.into();
        }
        value
    }

    #[test]
    fn parses_receiver_status() {
        let value = status(
            Some(7),
            serde_json::json!([{
                "appId": APP_MIRRORING,
                "transportId": "transport-1",
                "sessionId": "session-1",
                "displayName": "Screen mirroring",
            }]),
        );
        let parsed = parse_receiver_status(&value).unwrap();
        assert_eq!(parsed.volume_level(), Some(0.5));
        assert_eq!(parsed.is_muted(), Some(false));
        assert_eq!(parsed.applications()[0].app_id(), APP_MIRRORING);
        assert_eq!(parsed.applications()[0].transport_id(), "transport-1");
    }

    #[test]
    fn concurrent_launches_are_correlated_by_request_id() {
        let (first_sender, mut first_receiver) = oneshot::channel();
        let (second_sender, mut second_receiver) = oneshot::channel();
        let mut pending = HashMap::from([
            (
                1,
                PendingReceiverRequest {
                    operation: ReceiverOperation::Launch {
                        app_id: "one".into(),
                    },
                    response_sender: first_sender,
                },
            ),
            (
                2,
                PendingReceiverRequest {
                    operation: ReceiverOperation::Launch {
                        app_id: "two".into(),
                    },
                    response_sender: second_sender,
                },
            ),
        ]);
        let value = status(
            Some(1),
            serde_json::json!([{
                "appId": "one",
                "transportId": "transport-1",
            }]),
        );
        complete_receiver_status(&value, &mut pending);

        assert!(first_receiver.try_recv().unwrap().is_ok());
        assert!(second_receiver.try_recv().is_err());
        assert!(pending.contains_key(&2));
    }

    #[test]
    fn status_without_request_id_completes_every_visible_launch() {
        let (first_sender, mut first_receiver) = oneshot::channel();
        let (second_sender, mut second_receiver) = oneshot::channel();
        let mut pending = HashMap::from([
            (
                1,
                PendingReceiverRequest {
                    operation: ReceiverOperation::Launch {
                        app_id: "one".into(),
                    },
                    response_sender: first_sender,
                },
            ),
            (
                2,
                PendingReceiverRequest {
                    operation: ReceiverOperation::Launch {
                        app_id: "two".into(),
                    },
                    response_sender: second_sender,
                },
            ),
        ]);
        let value = status(
            None,
            serde_json::json!([
                {"appId": "one", "transportId": "transport-1"},
                {"appId": "two", "transportId": "transport-2"},
            ]),
        );

        complete_receiver_status(&value, &mut pending);

        assert!(first_receiver.try_recv().unwrap().is_ok());
        assert!(second_receiver.try_recv().unwrap().is_ok());
        assert!(pending.is_empty());
    }

    #[test]
    fn launch_error_completes_immediately() {
        let (sender, mut receiver) = oneshot::channel();
        let mut pending = HashMap::from([(
            9,
            PendingReceiverRequest {
                operation: ReceiverOperation::Launch {
                    app_id: "bad".into(),
                },
                response_sender: sender,
            },
        )]);
        fail_receiver_request(
            &serde_json::json!({"appId": "bad"}),
            &mut pending,
            "not found",
        );
        assert!(matches!(
            receiver.try_recv().unwrap(),
            Err(Error::AppLaunchFailed(_))
        ));
    }

    #[test]
    fn accepts_receiver_response_type_field() {
        let (sender, mut receiver) = oneshot::channel();
        let mut pending = HashMap::from([(
            12,
            PendingReceiverRequest {
                operation: ReceiverOperation::Availability {
                    app_id: APP_MIRRORING.into(),
                },
                response_sender: sender,
            },
        )]);
        let response = serde_json::json!({
            "responseType": "GET_APP_AVAILABILITY",
            "requestId": 12,
            "availability": {APP_MIRRORING: "APP_AVAILABLE"},
        });
        let message = CastMessage {
            source_id: RECEIVER_ID.into(),
            destination_id: SENDER_ID.into(),
            namespace: NS_RECEIVER.into(),
            payload: Payload::String(response.to_string()),
        };
        let (events, _) = broadcast::channel(1);

        dispatch_json_message(
            message,
            &mut pending,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
        )
        .unwrap();

        assert_eq!(
            receiver.try_recv().unwrap().unwrap()["availability"][APP_MIRRORING],
            "APP_AVAILABLE"
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn ignores_non_json_payload_on_unknown_namespace() {
        let message = CastMessage {
            source_id: "receiver-app".into(),
            destination_id: SENDER_ID.into(),
            namespace: "urn:x-cast:example.binary-protocol".into(),
            payload: Payload::String("not JSON".into()),
        };
        let (events, _) = broadcast::channel(1);

        dispatch_json_message(
            message,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
        )
        .unwrap();
    }

    #[test]
    fn rejects_non_json_payload_on_implemented_namespace() {
        let message = CastMessage {
            source_id: RECEIVER_ID.into(),
            destination_id: SENDER_ID.into(),
            namespace: NS_RECEIVER.into(),
            payload: Payload::String("not JSON".into()),
        };
        let (events, _) = broadcast::channel(1);

        assert!(matches!(
            dispatch_json_message(
                message,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &events,
            ),
            Err(Error::ProtocolError(_))
        ));
    }

    #[tokio::test]
    async fn answers_receiver_ping_with_pong() {
        let (client, server) = tokio::io::duplex(4096);
        let (client_read, client_write) = io::split(client);
        let (server_read, server_write) = io::split(server);
        let (command_sender, command_receiver) = mpsc::channel(4);
        let (event_sender, _) = broadcast::channel(4);
        let task = tokio::spawn(control_loop(
            FramedReader::new(client_read),
            FramedWriter::new(client_write),
            command_receiver,
            event_sender,
        ));
        let mut reader = FramedReader::new(server_read);
        let mut writer = FramedWriter::new(server_write);
        write_json_message(
            &mut writer,
            RECEIVER_ID,
            SENDER_ID,
            NS_HEARTBEAT,
            &serde_json::json!({"type": "PING"}),
        )
        .await
        .unwrap();

        let response = timeout(Duration::from_secs(1), reader.read_message())
            .await
            .unwrap()
            .unwrap();
        assert!(message_is_type(&response, "PONG"));

        let (response_sender, response_receiver) = oneshot::channel();
        command_sender
            .send(ControlCommand::Close { response_sender })
            .await
            .unwrap();
        response_receiver.await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn set_volume_uses_receiver_request_correlation() {
        let (client, server) = tokio::io::duplex(4096);
        let (client_read, client_write) = io::split(client);
        let (server_read, server_write) = io::split(server);
        let (command_sender, command_receiver) = mpsc::channel(4);
        let (event_sender, _) = broadcast::channel(4);
        let task = tokio::spawn(control_loop(
            FramedReader::new(client_read),
            FramedWriter::new(client_write),
            command_receiver,
            event_sender,
        ));
        let mut reader = FramedReader::new(server_read);
        let mut writer = FramedWriter::new(server_write);
        let (response_sender, response_receiver) = oneshot::channel();
        command_sender
            .send(ControlCommand::ReceiverRequest {
                operation: ReceiverOperation::SetVolume,
                payload: Some(serde_json::json!({
                    "type": "SET_VOLUME",
                    "volume": {"level": 0.75},
                })),
                response_sender,
            })
            .await
            .unwrap();

        let request = reader.read_message().await.unwrap();
        assert_eq!(request.source_id, SENDER_ID);
        assert_eq!(request.destination_id, RECEIVER_ID);
        assert_eq!(request.namespace, NS_RECEIVER);
        let Payload::String(request_payload) = request.payload else {
            panic!("SET_VOLUME request was not JSON");
        };
        let request_payload: Value = serde_json::from_str(&request_payload).unwrap();
        let request_id = json_u32(&request_payload, "requestId").unwrap();
        assert_eq!(request_payload["type"], "SET_VOLUME");
        assert_eq!(request_payload["volume"]["level"], 0.75);

        write_json_message(
            &mut writer,
            RECEIVER_ID,
            SENDER_ID,
            NS_RECEIVER,
            &status(Some(request_id), serde_json::json!([])),
        )
        .await
        .unwrap();
        let response = response_receiver.await.unwrap().unwrap();
        assert_eq!(json_u32(&response, "requestId"), Some(request_id));

        let (close_sender, close_receiver) = oneshot::channel();
        command_sender
            .send(ControlCommand::Close {
                response_sender: close_sender,
            })
            .await
            .unwrap();
        close_receiver.await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn closes_when_receiver_heartbeats_stop() {
        let (client, _server) = tokio::io::duplex(4096);
        let (client_read, client_write) = io::split(client);
        let (_command_sender, command_receiver) = mpsc::channel(1);
        let (event_sender, _) = broadcast::channel(4);
        let mut events = event_sender.subscribe();
        let task = tokio::spawn(control_loop(
            FramedReader::new(client_read),
            FramedWriter::new(client_write),
            command_receiver,
            event_sender,
        ));

        tokio::time::advance(HEARTBEAT_TIMEOUT + Duration::from_secs(1)).await;

        assert!(matches!(
            events.recv().await.unwrap(),
            ControlEvent::Closed(ControlCloseReason::HeartbeatTimeout)
        ));
        assert!(matches!(
            task.await.unwrap(),
            Err(Error::ConnectionFailed(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_offers_use_distinct_sequences_and_routes() {
        let (client, server) = tokio::io::duplex(8192);
        let (client_read, client_write) = io::split(client);
        let (server_read, server_write) = io::split(server);
        let (command_sender, command_receiver) = mpsc::channel(4);
        let (event_sender, _) = broadcast::channel(4);
        let task = tokio::spawn(control_loop(
            FramedReader::new(client_read),
            FramedWriter::new(client_write),
            command_receiver,
            event_sender,
        ));
        let mut reader = FramedReader::new(server_read);
        let mut writer = FramedWriter::new(server_write);
        let (first_sender, mut first_receiver) = oneshot::channel();
        let (second_sender, second_receiver) = oneshot::channel();
        command_sender
            .send(ControlCommand::SendOffer {
                destination: "app-one".into(),
                offer: serde_json::json!({"streams": []}),
                response_sender: first_sender,
            })
            .await
            .unwrap();
        command_sender
            .send(ControlCommand::SendOffer {
                destination: "app-two".into(),
                offer: serde_json::json!({"streams": []}),
                response_sender: second_sender,
            })
            .await
            .unwrap();

        let first_message = reader.read_message().await.unwrap();
        let second_message = reader.read_message().await.unwrap();
        let Payload::String(first_payload) = first_message.payload else {
            panic!("OFFER was not JSON");
        };
        let Payload::String(second_payload) = second_message.payload else {
            panic!("OFFER was not JSON");
        };
        let first_payload: Value = serde_json::from_str(&first_payload).unwrap();
        let second_payload: Value = serde_json::from_str(&second_payload).unwrap();
        let first_sequence = json_u32(&first_payload, "seqNum").unwrap();
        let second_sequence = json_u32(&second_payload, "seqNum").unwrap();
        assert_ne!(first_sequence, second_sequence);

        write_json_message(
            &mut writer,
            "wrong-app",
            SENDER_ID,
            NS_WEBRTC,
            &serde_json::json!({"type": "ANSWER", "seqNum": first_sequence}),
        )
        .await
        .unwrap();
        tokio::task::yield_now().await;
        assert!(first_receiver.try_recv().is_err());

        write_json_message(
            &mut writer,
            "app-two",
            SENDER_ID,
            NS_WEBRTC,
            &serde_json::json!({"type": "ANSWER", "seqNum": second_sequence}),
        )
        .await
        .unwrap();
        assert_eq!(
            json_u32(&second_receiver.await.unwrap().unwrap(), "seqNum"),
            Some(second_sequence)
        );
        assert!(first_receiver.try_recv().is_err());

        write_json_message(
            &mut writer,
            "app-one",
            SENDER_ID,
            NS_WEBRTC,
            &serde_json::json!({"type": "ANSWER", "seqNum": first_sequence}),
        )
        .await
        .unwrap();
        assert_eq!(
            json_u32(&first_receiver.await.unwrap().unwrap(), "seqNum"),
            Some(first_sequence)
        );

        let (response_sender, response_receiver) = oneshot::channel();
        command_sender
            .send(ControlCommand::Close { response_sender })
            .await
            .unwrap();
        response_receiver.await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn product_info_requests_use_namespace_specific_correlations() {
        let (client, server) = tokio::io::duplex(8192);
        let (client_read, client_write) = io::split(client);
        let (server_read, server_write) = io::split(server);
        let (command_sender, command_receiver) = mpsc::channel(4);
        let (event_sender, _) = broadcast::channel(4);
        let task = tokio::spawn(control_loop(
            FramedReader::new(client_read),
            FramedWriter::new(client_write),
            command_receiver,
            event_sender,
        ));
        let mut reader = FramedReader::new(server_read);
        let mut writer = FramedWriter::new(server_write);
        let (device_sender, mut device_receiver) = oneshot::channel();
        let (eureka_sender, mut eureka_receiver) = oneshot::channel();
        command_sender
            .send(ControlCommand::ProductInfoRequest {
                operation: ProductInfoOperation::DeviceInfo,
                response_sender: device_sender,
            })
            .await
            .unwrap();
        command_sender
            .send(ControlCommand::ProductInfoRequest {
                operation: ProductInfoOperation::EurekaInfo,
                response_sender: eureka_sender,
            })
            .await
            .unwrap();

        let first = reader.read_message().await.unwrap();
        let second = reader.read_message().await.unwrap();
        let messages = [first, second];
        let device_request = messages
            .iter()
            .find(|message| message.namespace == NS_RECEIVER_DISCOVERY)
            .unwrap();
        let eureka_request = messages
            .iter()
            .find(|message| message.namespace == NS_SETUP)
            .unwrap();
        let Payload::String(device_payload) = &device_request.payload else {
            panic!("device-info request was not JSON");
        };
        let Payload::String(eureka_payload) = &eureka_request.payload else {
            panic!("eureka-info request was not JSON");
        };
        let device_payload: Value = serde_json::from_str(device_payload).unwrap();
        let eureka_payload: Value = serde_json::from_str(eureka_payload).unwrap();
        let device_id = json_u32(&device_payload, "requestId").unwrap();
        let eureka_id = json_u32(&eureka_payload, "request_id").unwrap();
        assert!(device_payload.get("request_id").is_none());
        assert!(eureka_payload.get("requestId").is_none());

        write_json_message(
            &mut writer,
            RECEIVER_ID,
            SENDER_ID,
            NS_RECEIVER_DISCOVERY,
            &serde_json::json!({
                "type": "GET_DEVICE_INFO",
                "requestId": device_id.wrapping_add(1),
                "deviceId": "wrong",
            }),
        )
        .await
        .unwrap();
        write_json_message(
            &mut writer,
            "wrong-receiver",
            SENDER_ID,
            NS_SETUP,
            &serde_json::json!({
                "type": "eureka_info",
                "request_id": eureka_id,
                "response_code": 200,
            }),
        )
        .await
        .unwrap();
        tokio::task::yield_now().await;
        assert!(eureka_receiver.try_recv().is_err());
        write_json_message(
            &mut writer,
            RECEIVER_ID,
            SENDER_ID,
            NS_SETUP,
            &serde_json::json!({
                "type": "eureka_info",
                "request_id": eureka_id,
                "response_code": 200,
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            json_u32(&eureka_receiver.await.unwrap().unwrap(), "request_id"),
            Some(eureka_id)
        );
        assert!(device_receiver.try_recv().is_err());

        write_json_message(
            &mut writer,
            RECEIVER_ID,
            SENDER_ID,
            NS_RECEIVER_DISCOVERY,
            &serde_json::json!({
                "type": "DEVICE_INFO",
                "requestId": device_id,
                "deviceId": "00112233445566778899aabbccddeeff",
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            json_u32(&device_receiver.await.unwrap().unwrap(), "requestId"),
            Some(device_id)
        );

        let (response_sender, response_receiver) = oneshot::channel();
        command_sender
            .send(ControlCommand::Close { response_sender })
            .await
            .unwrap();
        response_receiver.await.unwrap();
        task.await.unwrap().unwrap();
    }
}
