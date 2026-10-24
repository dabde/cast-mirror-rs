use std::time::Duration;

use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::connection::EXCHANGE_TIMEOUT;
use super::error::{protocol_error, AndroidTvError};
use super::framing::{FramedReader, FramedWriter};
use super::proto::{
    RemoteConfigure, RemoteDeviceInfo, RemoteKeyInject, RemoteMessage, RemotePingResponse,
    RemoteSetActive, RemoteVolume,
};
use super::remote_types::{
    AndroidTvDeviceInfo, AndroidTvKeyAction, AndroidTvKeyCode, AndroidTvRemoteClientInfo,
    AndroidTvRemoteEvent, AndroidTvRemoteFeatures, AndroidTvVolume, IMPLEMENTED_FEATURES,
};

const COMMAND_QUEUE_CAPACITY: usize = 16;
const EVENT_QUEUE_CAPACITY: usize = 16;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HANDSHAKE_MESSAGES: usize = 32;
const MAX_DEVICE_TEXT_BYTES: usize = 256;

pub(super) struct EstablishedRemote {
    pub session: RemoteSession,
    pub device_info: AndroidTvDeviceInfo,
    pub features: AndroidTvRemoteFeatures,
}

pub(super) struct RemoteSession {
    commands: mpsc::Sender<RemoteCommand>,
    events: broadcast::Sender<AndroidTvRemoteEvent>,
    task: Option<JoinHandle<Result<(), AndroidTvError>>>,
    power: watch::Receiver<bool>,
    volume: watch::Receiver<Option<AndroidTvVolume>>,
}

impl RemoteSession {
    pub async fn start<R, W>(
        mut reader: FramedReader<R>,
        mut writer: FramedWriter<W>,
        client_info: &AndroidTvRemoteClientInfo,
    ) -> Result<EstablishedRemote, AndroidTvError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let handshake = timeout(
            HANDSHAKE_TIMEOUT,
            perform_handshake(&mut reader, &mut writer, client_info),
        )
        .await
        .map_err(|_| AndroidTvError::TimedOut("Remote Service handshake"))??;

        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (event_tx, _) = broadcast::channel(EVENT_QUEUE_CAPACITY);
        let (power_tx, power) = watch::channel(handshake.powered_on);
        let (volume_tx, volume) = watch::channel(handshake.volume);
        let task_events = event_tx.clone();
        let task = tokio::spawn(async move {
            let result = run_remote(
                reader,
                writer,
                command_rx,
                &task_events,
                &power_tx,
                &volume_tx,
            )
            .await;
            let _ = task_events.send(AndroidTvRemoteEvent::Disconnected {
                error: result.as_ref().err().map(ToString::to_string),
            });
            result
        });

        Ok(EstablishedRemote {
            session: Self {
                commands: command_tx,
                events: event_tx,
                task: Some(task),
                power,
                volume,
            },
            device_info: handshake.device_info,
            features: handshake.features,
        })
    }

    pub fn powered_on(&self) -> bool {
        *self.power.borrow()
    }

    pub fn volume(&self) -> Option<AndroidTvVolume> {
        *self.volume.borrow()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AndroidTvRemoteEvent> {
        self.events.subscribe()
    }

    pub async fn send_key(
        &self,
        key_code: AndroidTvKeyCode,
        action: AndroidTvKeyAction,
    ) -> Result<(), AndroidTvError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(RemoteCommand::Key {
                key_code,
                action,
                reply: reply_tx,
            })
            .await
            .map_err(|_| AndroidTvError::ConnectionClosed)?;
        reply_rx
            .await
            .map_err(|_| AndroidTvError::ConnectionClosed)?
    }

    pub async fn close(mut self) -> Result<(), AndroidTvError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let sent = self
            .commands
            .send(RemoteCommand::Close { reply: reply_tx })
            .await
            .is_ok();
        let close_result = if sent {
            reply_rx
                .await
                .map_err(|_| AndroidTvError::ConnectionClosed)?
        } else {
            Err(AndroidTvError::ConnectionClosed)
        };
        let task_result = match self.task.take() {
            Some(task) => task.await.map_err(|error| {
                AndroidTvError::Connection(format!("join remote task: {error}"))
            })?,
            None => Ok(()),
        };
        close_result.and(task_result)
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct HandshakeResult {
    device_info: AndroidTvDeviceInfo,
    features: AndroidTvRemoteFeatures,
    powered_on: bool,
    volume: Option<AndroidTvVolume>,
}

async fn perform_handshake<R, W>(
    reader: &mut FramedReader<R>,
    writer: &mut FramedWriter<W>,
    client_info: &AndroidTvRemoteClientInfo,
) -> Result<HandshakeResult, AndroidTvError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut volume = None;
    let configure = wait_for_handshake_message(
        reader,
        writer,
        &mut volume,
        |message| message.configure.take(),
        "configuration",
    )
    .await?;
    let server_features = AndroidTvRemoteFeatures::from_protocol_bits(configure.features as u32);
    let features = server_features & IMPLEMENTED_FEATURES;
    let device_info = validated_device_info(
        configure
            .device_info
            .ok_or_else(|| protocol_error("RemoteConfigure omitted device information"))?,
    )?;

    write_remote(
        writer,
        &RemoteMessage {
            configure: Some(RemoteConfigure {
                features: features.bits() as i32,
                device_info: Some(RemoteDeviceInfo {
                    model: client_info.model.clone(),
                    vendor: client_info.vendor.clone(),
                    unknown_one: 1,
                    unknown_two: "1".into(),
                    package_name: client_info.package_name.clone(),
                    app_version: client_info.app_version.clone(),
                }),
            }),
            ..RemoteMessage::default()
        },
        "configuration reply",
    )
    .await?;

    let _ = wait_for_handshake_message(
        reader,
        writer,
        &mut volume,
        |message| message.set_active.take(),
        "set-active request",
    )
    .await?;
    write_remote(
        writer,
        &RemoteMessage {
            set_active: Some(RemoteSetActive {
                features: features.bits() as i32,
            }),
            ..RemoteMessage::default()
        },
        "set-active reply",
    )
    .await?;

    let start = wait_for_handshake_message(
        reader,
        writer,
        &mut volume,
        |message| message.start.take(),
        "remote-start notification",
    )
    .await?;
    Ok(HandshakeResult {
        device_info,
        features,
        powered_on: start.powered_on,
        volume,
    })
}

async fn wait_for_handshake_message<R, W, T>(
    reader: &mut FramedReader<R>,
    writer: &mut FramedWriter<W>,
    volume: &mut Option<AndroidTvVolume>,
    mut select: impl FnMut(&mut RemoteMessage) -> Option<T>,
    expected: &'static str,
) -> Result<T, AndroidTvError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    for _ in 0..MAX_HANDSHAKE_MESSAGES {
        let mut message = read_remote(reader, expected).await?;
        if message.error.as_ref().is_some_and(|error| error.value) {
            return Err(protocol_error(format!(
                "device returned an error while waiting for {expected}"
            )));
        }
        if let Some(request) = message.ping_request.take() {
            answer_ping(writer, request.value).await?;
        }
        if let Some(remote_volume) = message.volume.take() {
            *volume = Some(validated_volume(remote_volume)?);
        }
        if let Some(value) = select(&mut message) {
            return Ok(value);
        }
        // Deployed Remote Service implementations may publish initial volume,
        // IME, or other state between handshake messages. Known interoperable
        // clients keep reading until the expected message arrives. Retain the
        // finite message count and overall handshake deadline while doing so.
    }
    Err(protocol_error(format!(
        "too many messages while waiting for {expected}"
    )))
}

enum RemoteCommand {
    Key {
        key_code: AndroidTvKeyCode,
        action: AndroidTvKeyAction,
        reply: oneshot::Sender<Result<(), AndroidTvError>>,
    },
    Close {
        reply: oneshot::Sender<Result<(), AndroidTvError>>,
    },
}

async fn run_remote<R, W>(
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
    mut commands: mpsc::Receiver<RemoteCommand>,
    events: &broadcast::Sender<AndroidTvRemoteEvent>,
    power: &watch::Sender<bool>,
    volume: &watch::Sender<Option<AndroidTvVolume>>,
) -> Result<(), AndroidTvError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            frame = reader.read_frame() => {
                let frame = frame?;
                trace_inbound_frame("established session", frame);
                let message = RemoteMessage::decode(frame)
                    .map_err(|error| protocol_error(format!("decode remote message: {error}")))?;
                handle_inbound(message, &mut writer, events, power, volume).await?;
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    let _ = timeout(EXCHANGE_TIMEOUT, writer.shutdown()).await;
                    return Ok(());
                };
                match command {
                    RemoteCommand::Key { key_code, action, reply } => {
                        let result = write_remote(
                            &mut writer,
                            &RemoteMessage {
                                key_inject: Some(RemoteKeyInject {
                                    key_code: key_code.0,
                                    direction: action.protocol_value(),
                                }),
                                ..RemoteMessage::default()
                            },
                            "key injection",
                        ).await;
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        if failed {
                            return Err(AndroidTvError::ConnectionClosed);
                        }
                    }
                    RemoteCommand::Close { reply } => {
                        let result = timeout(EXCHANGE_TIMEOUT, writer.shutdown())
                            .await
                            .map_err(|_| AndroidTvError::TimedOut("connection shutdown"))?;
                        let result = result.map_err(AndroidTvError::Io);
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        return if failed {
                            Err(AndroidTvError::ConnectionClosed)
                        } else {
                            Ok(())
                        };
                    }
                }
            }
        }
    }
}

async fn handle_inbound<W>(
    mut message: RemoteMessage,
    writer: &mut FramedWriter<W>,
    events: &broadcast::Sender<AndroidTvRemoteEvent>,
    power: &watch::Sender<bool>,
    volume: &watch::Sender<Option<AndroidTvVolume>>,
) -> Result<(), AndroidTvError>
where
    W: AsyncWrite + Unpin,
{
    if message.error.as_ref().is_some_and(|error| error.value) {
        return Err(protocol_error("device returned RemoteError"));
    }
    if let Some(ping) = message.ping_request.take() {
        answer_ping(writer, ping.value).await?;
    }
    if let Some(start) = message.start.take() {
        power.send_replace(start.powered_on);
        let _ = events.send(AndroidTvRemoteEvent::PowerChanged {
            powered_on: start.powered_on,
        });
    }
    if let Some(remote_volume) = message.volume.take() {
        let current = validated_volume(remote_volume)?;
        volume.send_replace(Some(current));
        let _ = events.send(AndroidTvRemoteEvent::VolumeChanged(current));
    }
    Ok(())
}

async fn answer_ping<W>(writer: &mut FramedWriter<W>, value: i32) -> Result<(), AndroidTvError>
where
    W: AsyncWrite + Unpin,
{
    write_remote(
        writer,
        &RemoteMessage {
            ping_response: Some(RemotePingResponse { value }),
            ..RemoteMessage::default()
        },
        "ping response",
    )
    .await
}

async fn read_remote<R>(
    reader: &mut FramedReader<R>,
    operation: &'static str,
) -> Result<RemoteMessage, AndroidTvError>
where
    R: AsyncRead + Unpin,
{
    let frame = timeout(EXCHANGE_TIMEOUT, reader.read_frame())
        .await
        .map_err(|_| AndroidTvError::TimedOut(operation))??;
    trace_inbound_frame(operation, frame);
    RemoteMessage::decode(frame)
        .map_err(|error| protocol_error(format!("decode {operation}: {error}")))
}

async fn write_remote<W>(
    writer: &mut FramedWriter<W>,
    message: &RemoteMessage,
    operation: &'static str,
) -> Result<(), AndroidTvError>
where
    W: AsyncWrite + Unpin,
{
    timeout(EXCHANGE_TIMEOUT, writer.write_message(message))
        .await
        .map_err(|_| AndroidTvError::TimedOut(operation))??;
    Ok(())
}

fn validated_device_info(info: RemoteDeviceInfo) -> Result<AndroidTvDeviceInfo, AndroidTvError> {
    for (field, value) in [
        ("model", info.model.as_str()),
        ("vendor", info.vendor.as_str()),
        ("service version", info.app_version.as_str()),
    ] {
        if value.len() > MAX_DEVICE_TEXT_BYTES || value.chars().any(char::is_control) {
            return Err(protocol_error(format!("device {field} is invalid")));
        }
    }
    Ok(AndroidTvDeviceInfo {
        model: info.model,
        vendor: info.vendor,
        service_version: info.app_version,
    })
}

fn validated_volume(volume: RemoteVolume) -> Result<AndroidTvVolume, AndroidTvError> {
    if volume.maximum == 0 || volume.level > volume.maximum {
        return Err(protocol_error("device volume state is invalid"));
    }
    Ok(AndroidTvVolume {
        level: volume.level,
        maximum: volume.maximum,
        muted: volume.muted,
    })
}

fn trace_inbound_frame(operation: &'static str, frame: &[u8]) {
    tracing::trace!(
        target: "chromiacast::android_tv::remote::wire",
        operation,
        frame_bytes = frame.len(),
        top_level_fields = ?top_level_field_numbers(frame),
        "received Remote Service message"
    );
}

fn top_level_field_numbers(mut bytes: &[u8]) -> Option<Vec<u32>> {
    let mut fields = Vec::new();
    while !bytes.is_empty() {
        let key = take_varint(&mut bytes)?;
        let field = u32::try_from(key >> 3).ok()?;
        if field == 0 {
            return None;
        }
        fields.push(field);
        match key & 0x07 {
            0 => {
                take_varint(&mut bytes)?;
            }
            1 => bytes = bytes.get(8..)?,
            2 => {
                let length = usize::try_from(take_varint(&mut bytes)?).ok()?;
                bytes = bytes.get(length..)?;
            }
            5 => bytes = bytes.get(4..)?,
            _ => return None,
        }
    }
    Some(fields)
}

fn take_varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..=63).step_by(7) {
        let byte = *bytes.first()?;
        *bytes = bytes.get(1..)?;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::proto::{RemotePingRequest, RemoteStart};
    use super::*;

    #[test]
    fn diagnostic_field_scanner_reports_fields_without_payloads() {
        let frame = [
            0x42, 0x02, 0x08, 0x49, // field 8: ping request
            0xa2, 0x01, 0x03, 0x0a, 0x01, b'x', // field 20: IME/app state
            0x95, 0x06, 0, 0, 0, 0, // field 98: unknown fixed32 extension
        ];
        assert_eq!(top_level_field_numbers(&frame), Some(vec![8, 20, 98]));
        assert_eq!(top_level_field_numbers(&[0x82]), None);
        assert_eq!(top_level_field_numbers(&[0]), None);
    }

    fn server_configure(features: AndroidTvRemoteFeatures) -> RemoteMessage {
        RemoteMessage {
            configure: Some(RemoteConfigure {
                features: features.bits() as i32,
                device_info: Some(RemoteDeviceInfo {
                    model: "55A3G".into(),
                    vendor: "TCL".into(),
                    unknown_one: 0,
                    unknown_two: String::new(),
                    package_name: "com.google.android.tv.remote.service".into(),
                    app_version: "8".into(),
                }),
            }),
            ..RemoteMessage::default()
        }
    }

    async fn send<W: AsyncWrite + Unpin>(writer: &mut FramedWriter<W>, message: RemoteMessage) {
        writer.write_message(&message).await.unwrap();
    }

    async fn receive<R: AsyncRead + Unpin>(reader: &mut FramedReader<R>) -> RemoteMessage {
        RemoteMessage::decode(reader.read_frame().await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn handshake_masks_features_and_preserves_initial_volume() {
        let (client, server) = tokio::io::duplex(4096);
        let (client_reader, client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let unimplemented_voice = AndroidTvRemoteFeatures::from_protocol_bits(1 << 3);
        let server = tokio::spawn(async move {
            let mut reader = FramedReader::new(server_reader);
            let mut writer = FramedWriter::new(server_writer);
            send(
                &mut writer,
                server_configure(IMPLEMENTED_FEATURES | unimplemented_voice),
            )
            .await;
            let configure = receive(&mut reader).await.configure.unwrap();
            assert_eq!(configure.features as u32, IMPLEMENTED_FEATURES.bits());
            assert_eq!(
                configure.device_info.unwrap().package_name,
                "io.github.pronkproject.chromiacast"
            );

            send(
                &mut writer,
                RemoteMessage {
                    set_active: Some(RemoteSetActive { features: -1 }),
                    ..RemoteMessage::default()
                },
            )
            .await;
            assert_eq!(
                receive(&mut reader).await.set_active.unwrap().features as u32,
                IMPLEMENTED_FEATURES.bits()
            );
            send(
                &mut writer,
                RemoteMessage {
                    ping_request: Some(RemotePingRequest {
                        value: 73,
                        auxiliary_value: 0,
                    }),
                    ..RemoteMessage::default()
                },
            )
            .await;
            assert_eq!(receive(&mut reader).await.ping_response.unwrap().value, 73);
            send(
                &mut writer,
                RemoteMessage {
                    volume: Some(RemoteVolume {
                        maximum: 100,
                        level: 31,
                        muted: false,
                    }),
                    ..RemoteMessage::default()
                },
            )
            .await;
            send(
                &mut writer,
                RemoteMessage {
                    start: Some(RemoteStart { powered_on: true }),
                    ..RemoteMessage::default()
                },
            )
            .await;
        });

        let mut reader = FramedReader::new(client_reader);
        let mut writer = FramedWriter::new(client_writer);
        let result = perform_handshake(&mut reader, &mut writer, &Default::default())
            .await
            .unwrap();
        assert_eq!(result.device_info.vendor(), "TCL");
        assert!(result.features.contains(AndroidTvRemoteFeatures::KEY));
        assert!(result.powered_on);
        assert_eq!(
            result.volume,
            Some(AndroidTvVolume {
                level: 31,
                maximum: 100,
                muted: false,
            })
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ready_session_flushes_keys_and_tracks_later_state() {
        let (client, server) = tokio::io::duplex(4096);
        let (client_reader, client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let mut reader = FramedReader::new(server_reader);
            let mut writer = FramedWriter::new(server_writer);
            send(&mut writer, server_configure(IMPLEMENTED_FEATURES)).await;
            let _ = receive(&mut reader).await;
            send(
                &mut writer,
                RemoteMessage {
                    set_active: Some(RemoteSetActive {
                        features: IMPLEMENTED_FEATURES.bits() as i32,
                    }),
                    ..RemoteMessage::default()
                },
            )
            .await;
            let _ = receive(&mut reader).await;
            send(
                &mut writer,
                RemoteMessage {
                    start: Some(RemoteStart { powered_on: true }),
                    ..RemoteMessage::default()
                },
            )
            .await;

            let key = receive(&mut reader).await.key_inject.unwrap();
            assert_eq!(key.key_code, 3);
            assert_eq!(key.direction, AndroidTvKeyAction::Press.protocol_value());
            send(
                &mut writer,
                RemoteMessage {
                    ping_request: Some(RemotePingRequest {
                        value: 99,
                        auxiliary_value: 0,
                    }),
                    ..RemoteMessage::default()
                },
            )
            .await;
            assert_eq!(receive(&mut reader).await.ping_response.unwrap().value, 99);
            send(
                &mut writer,
                RemoteMessage {
                    start: Some(RemoteStart { powered_on: false }),
                    ..RemoteMessage::default()
                },
            )
            .await;
            send(
                &mut writer,
                RemoteMessage {
                    volume: Some(RemoteVolume {
                        maximum: 100,
                        level: 31,
                        muted: false,
                    }),
                    ..RemoteMessage::default()
                },
            )
            .await;
            let _ = reader.read_frame().await;
        });

        let EstablishedRemote { session, .. } = RemoteSession::start(
            FramedReader::new(client_reader),
            FramedWriter::new(client_writer),
            &Default::default(),
        )
        .await
        .unwrap();
        let mut events = session.subscribe();
        session
            .send_key(AndroidTvKeyCode::HOME, AndroidTvKeyAction::Press)
            .await
            .unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            AndroidTvRemoteEvent::PowerChanged { powered_on: false }
        );
        assert!(!session.powered_on());
        let expected_volume = AndroidTvVolume {
            level: 31,
            maximum: 100,
            muted: false,
        };
        assert_eq!(
            events.recv().await.unwrap(),
            AndroidTvRemoteEvent::VolumeChanged(expected_volume)
        );
        assert_eq!(session.volume(), Some(expected_volume));
        session.close().await.unwrap();
        server.await.unwrap();
    }

    #[test]
    fn rejects_incoherent_volume_state() {
        assert!(validated_volume(RemoteVolume {
            maximum: 0,
            level: 0,
            muted: false,
        })
        .is_err());
        assert!(validated_volume(RemoteVolume {
            maximum: 100,
            level: 101,
            muted: false,
        })
        .is_err());
    }
}
