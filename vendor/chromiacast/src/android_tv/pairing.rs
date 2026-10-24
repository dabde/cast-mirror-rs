use std::net::{IpAddr, SocketAddr};

use prost::Message;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;

use super::connection::{connect, peer_certificate, EXCHANGE_TIMEOUT};
use super::error::{protocol_error, AndroidTvError};
use super::framing::{FramedReader, FramedWriter};
use super::identity::{rsa_parameters_from_certificate, AndroidTvRemoteIdentity};
use super::proto::{
    PairingConfiguration, PairingConfigurationAck, PairingEncoding, PairingEnvelope,
    PairingOptions, PairingRequest, PairingRequestAck, PairingSecret, PairingSecretAck,
    PAIRING_STATUS_OK, ROLE_INPUT,
};
use super::ANDROID_TV_PAIRING_PORT;

const SERVICE_NAME: &str = "atvremote";
const MAX_PEER_NAME_BYTES: usize = 256;
const MAX_CLIENT_NAME_BYTES: usize = 128;

type PairingReader = FramedReader<ReadHalf<TlsStream<TcpStream>>>;
type PairingWriter = FramedWriter<WriteHalf<TlsStream<TcpStream>>>;

/// An in-progress Android TV pairing exchange waiting for the displayed PIN.
///
/// Dropping this object cancels pairing. [`finish`](Self::finish) consumes it so
/// a PIN cannot accidentally be submitted twice on one protocol transcript.
pub struct AndroidTvPairingSession {
    exchange: PairingExchange<PairingReader, PairingWriter>,
}

impl std::fmt::Debug for AndroidTvPairingSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AndroidTvPairingSession")
            .field("server_name", &self.exchange.server_name)
            .finish_non_exhaustive()
    }
}

impl AndroidTvPairingSession {
    /// Connect to the standard Android TV pairing port at `ip`.
    pub async fn begin(
        ip: IpAddr,
        identity: &AndroidTvRemoteIdentity,
        client_name: &str,
    ) -> Result<Self, AndroidTvError> {
        Self::begin_address(
            SocketAddr::new(ip, ANDROID_TV_PAIRING_PORT),
            identity,
            client_name,
        )
        .await
    }

    /// Connect to an explicit Android TV pairing endpoint.
    pub async fn begin_address(
        address: SocketAddr,
        identity: &AndroidTvRemoteIdentity,
        client_name: &str,
    ) -> Result<Self, AndroidTvError> {
        validate_client_name(client_name)?;
        let stream = connect(address, identity).await?;
        let (server_modulus, server_exponent) =
            rsa_parameters_from_certificate(peer_certificate(&stream)?).map_err(|error| {
                protocol_error(format!(
                    "device TLS certificate is unusable for pairing: {error}"
                ))
            })?;
        let (read_half, write_half) = tokio::io::split(stream);
        let exchange = begin_exchange(
            FramedReader::new(read_half),
            FramedWriter::new(write_half),
            identity.modulus().to_vec(),
            identity.exponent().to_vec(),
            server_modulus,
            server_exponent,
            client_name,
        )
        .await?;
        Ok(Self { exchange })
    }

    /// Name supplied by the device in its pairing acknowledgement, if any.
    pub fn server_name(&self) -> Option<&str> {
        self.exchange.server_name.as_deref()
    }

    /// Validate and submit the six-digit hexadecimal PIN shown by the device.
    pub async fn finish(self, pairing_code: &str) -> Result<(), AndroidTvError> {
        finish_exchange(self.exchange, pairing_code).await
    }
}

struct PairingExchange<R, W> {
    reader: R,
    writer: W,
    client_modulus: Vec<u8>,
    client_exponent: Vec<u8>,
    server_modulus: Vec<u8>,
    server_exponent: Vec<u8>,
    server_name: Option<String>,
}

async fn begin_exchange<R, W>(
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
    client_modulus: Vec<u8>,
    client_exponent: Vec<u8>,
    server_modulus: Vec<u8>,
    server_exponent: Vec<u8>,
    client_name: &str,
) -> Result<PairingExchange<FramedReader<R>, FramedWriter<W>>, AndroidTvError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let request = PairingEnvelope {
        pairing_request: Some(PairingRequest {
            service_name: SERVICE_NAME.into(),
            client_name: Some(client_name.into()),
        }),
        ..PairingEnvelope::ok()
    };
    write_pairing(&mut writer, &request, "pairing request").await?;

    let acknowledgement = read_pairing(&mut reader, "pairing acknowledgement").await?;
    let PairingRequestAck { server_name } = acknowledgement
        .pairing_request_ack
        .ok_or_else(|| protocol_error("expected pairing-request acknowledgement"))?;
    let server_name = server_name
        .map(validate_peer_name)
        .transpose()?
        .filter(|name| !name.is_empty());

    let options = PairingEnvelope {
        options: Some(PairingOptions {
            input_encodings: vec![PairingEncoding::hexadecimal_pin()],
            output_encodings: Vec::new(),
            preferred_role: Some(ROLE_INPUT),
        }),
        ..PairingEnvelope::ok()
    };
    write_pairing(&mut writer, &options, "pairing options").await?;

    let device_options = read_pairing(&mut reader, "device pairing options")
        .await?
        .options
        .ok_or_else(|| protocol_error("expected device pairing options"))?;
    if !device_options
        .output_encodings
        .iter()
        .any(PairingEncoding::is_hexadecimal_pin)
    {
        return Err(AndroidTvError::UnsupportedPairingConfiguration);
    }

    let configuration = PairingEnvelope {
        configuration: Some(PairingConfiguration {
            encoding: Some(PairingEncoding::hexadecimal_pin()),
            client_role: ROLE_INPUT,
        }),
        ..PairingEnvelope::ok()
    };
    write_pairing(&mut writer, &configuration, "pairing configuration").await?;

    let configuration_acknowledgement =
        read_pairing(&mut reader, "pairing configuration acknowledgement").await?;
    let Some(PairingConfigurationAck {}) = configuration_acknowledgement.configuration_ack else {
        return Err(protocol_error(
            "expected pairing-configuration acknowledgement",
        ));
    };

    Ok(PairingExchange {
        reader,
        writer,
        client_modulus,
        client_exponent,
        server_modulus,
        server_exponent,
        server_name,
    })
}

async fn finish_exchange<R, W>(
    mut exchange: PairingExchange<FramedReader<R>, FramedWriter<W>>,
    pairing_code: &str,
) -> Result<(), AndroidTvError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let secret = compute_pairing_secret(
        &exchange.client_modulus,
        &exchange.client_exponent,
        &exchange.server_modulus,
        &exchange.server_exponent,
        pairing_code,
    )?;
    let message = PairingEnvelope {
        secret: Some(PairingSecret { secret }),
        ..PairingEnvelope::ok()
    };
    write_pairing(&mut exchange.writer, &message, "pairing secret").await?;

    let acknowledgement =
        read_pairing(&mut exchange.reader, "pairing-secret acknowledgement").await?;
    let Some(PairingSecretAck { .. }) = acknowledgement.secret_ack else {
        return Err(protocol_error("expected pairing-secret acknowledgement"));
    };
    // Some deployed implementations do not return a verifiable acknowledgement
    // secret. Presence plus STATUS_OK is therefore the interoperable success gate.
    let _ = timeout(EXCHANGE_TIMEOUT, exchange.writer.shutdown()).await;
    Ok(())
}

async fn read_pairing<R>(
    reader: &mut FramedReader<R>,
    operation: &'static str,
) -> Result<PairingEnvelope, AndroidTvError>
where
    R: AsyncRead + Unpin,
{
    let frame = timeout(EXCHANGE_TIMEOUT, reader.read_frame())
        .await
        .map_err(|_| AndroidTvError::TimedOut(operation))??;
    let message = PairingEnvelope::decode(frame)
        .map_err(|error| protocol_error(format!("decode {operation}: {error}")))?;
    if message.status != PAIRING_STATUS_OK {
        return Err(AndroidTvError::PairingRejected(message.status));
    }
    if message.protocol_version == 0 || message.protocol_version > 2 {
        return Err(protocol_error(format!(
            "unsupported pairing protocol version {}",
            message.protocol_version
        )));
    }
    Ok(message)
}

async fn write_pairing<W>(
    writer: &mut FramedWriter<W>,
    message: &PairingEnvelope,
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

fn compute_pairing_secret(
    client_modulus: &[u8],
    client_exponent: &[u8],
    server_modulus: &[u8],
    server_exponent: &[u8],
    pairing_code: &str,
) -> Result<Vec<u8>, AndroidTvError> {
    let code = parse_pairing_code(pairing_code)?;
    let mut digest = Sha256::new();
    digest.update(client_modulus);
    digest.update(client_exponent);
    digest.update(server_modulus);
    digest.update(server_exponent);
    digest.update(&code[1..]);
    let secret = digest.finalize();
    if secret[0] != code[0] {
        return Err(AndroidTvError::InvalidPairingCode);
    }
    Ok(secret.to_vec())
}

fn parse_pairing_code(pairing_code: &str) -> Result<[u8; 3], AndroidTvError> {
    let bytes = pairing_code.as_bytes();
    if bytes.len() != 6 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(AndroidTvError::InvalidPairingCode);
    }
    let mut result = [0_u8; 3];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        result[index] = (hex_value(pair[0]) << 4) | hex_value(pair[1]);
    }
    Ok(result)
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => unreachable!("pairing code was validated as hexadecimal"),
    }
}

fn validate_client_name(client_name: &str) -> Result<(), AndroidTvError> {
    if client_name.is_empty()
        || client_name.len() > MAX_CLIENT_NAME_BYTES
        || client_name.chars().any(char::is_control)
    {
        return Err(AndroidTvError::InvalidClientInfo(
            "pairing client name must be 1..=128 bytes without control characters".into(),
        ));
    }
    Ok(())
}

fn validate_peer_name(peer_name: String) -> Result<String, AndroidTvError> {
    if peer_name.len() > MAX_PEER_NAME_BYTES || peer_name.chars().any(char::is_control) {
        return Err(protocol_error("pairing server name is invalid"));
    }
    Ok(peer_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_MODULUS: [u8; 4] = [0xc0, 0xff, 0xee, 0x01];
    const EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];
    const SERVER_MODULUS: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];
    const PAIRING_CODE: &str = "82ABCD";
    const EXPECTED_SECRET: [u8; 32] = [
        0x82, 0x16, 0xd1, 0xd2, 0x83, 0xf2, 0x74, 0x63, 0x28, 0x0c, 0x6c, 0x6b, 0x00, 0xbe, 0x3b,
        0x67, 0xaa, 0x31, 0xb7, 0xf8, 0xba, 0x0e, 0x1e, 0x0d, 0xa6, 0x9c, 0x4b, 0xd3, 0x25, 0x88,
        0x9c, 0x41,
    ];

    #[test]
    fn secret_computation_matches_the_published_polo_algorithm() {
        assert_eq!(
            compute_pairing_secret(
                &CLIENT_MODULUS,
                &EXPONENT,
                &SERVER_MODULUS,
                &EXPONENT,
                PAIRING_CODE,
            )
            .unwrap(),
            EXPECTED_SECRET
        );
        assert!(compute_pairing_secret(
            &CLIENT_MODULUS,
            &EXPONENT,
            &SERVER_MODULUS,
            &EXPONENT,
            "00ABCD",
        )
        .is_err());
    }

    #[tokio::test]
    async fn complete_transcript_requires_each_expected_acknowledgement() {
        let (client, server) = tokio::io::duplex(4096);
        let (client_reader, client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let mut reader = FramedReader::new(server_reader);
            let mut writer = FramedWriter::new(server_writer);

            let request = PairingEnvelope::decode(reader.read_frame().await.unwrap()).unwrap();
            assert_eq!(request.pairing_request.unwrap().service_name, SERVICE_NAME);
            writer
                .write_message(&PairingEnvelope {
                    pairing_request_ack: Some(PairingRequestAck {
                        server_name: Some("Living Room TV".into()),
                    }),
                    ..PairingEnvelope::ok()
                })
                .await
                .unwrap();

            let options = PairingEnvelope::decode(reader.read_frame().await.unwrap()).unwrap();
            assert!(options.options.unwrap().input_encodings[0].is_hexadecimal_pin());
            writer
                .write_message(&PairingEnvelope {
                    options: Some(PairingOptions {
                        input_encodings: Vec::new(),
                        output_encodings: vec![PairingEncoding::hexadecimal_pin()],
                        preferred_role: None,
                    }),
                    ..PairingEnvelope::ok()
                })
                .await
                .unwrap();

            let configuration =
                PairingEnvelope::decode(reader.read_frame().await.unwrap()).unwrap();
            assert_eq!(configuration.configuration.unwrap().client_role, ROLE_INPUT);
            writer
                .write_message(&PairingEnvelope {
                    configuration_ack: Some(PairingConfigurationAck {}),
                    ..PairingEnvelope::ok()
                })
                .await
                .unwrap();

            let secret = PairingEnvelope::decode(reader.read_frame().await.unwrap()).unwrap();
            assert_eq!(secret.secret.unwrap().secret, EXPECTED_SECRET);
            writer
                .write_message(&PairingEnvelope {
                    secret_ack: Some(PairingSecretAck { secret: Vec::new() }),
                    ..PairingEnvelope::ok()
                })
                .await
                .unwrap();
        });

        let exchange = begin_exchange(
            FramedReader::new(client_reader),
            FramedWriter::new(client_writer),
            CLIENT_MODULUS.to_vec(),
            EXPONENT.to_vec(),
            SERVER_MODULUS.to_vec(),
            EXPONENT.to_vec(),
            "Pronk",
        )
        .await
        .unwrap();
        assert_eq!(exchange.server_name.as_deref(), Some("Living Room TV"));
        finish_exchange(exchange, PAIRING_CODE).await.unwrap();
        server.await.unwrap();
    }
}
