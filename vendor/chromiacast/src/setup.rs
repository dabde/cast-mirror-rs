use std::net::{SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::error::Error;
use crate::tls::SelfSignedCertificateVerifier;

/// Standard HTTPS port for the Cast setup API.
pub const SETUP_PORT: u16 = 8443;

const SETUP_PATH: &str = "/setup/eureka_info?params=device_info,name";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_MANUFACTURER_BYTES: usize = 256;
const MAX_MODEL_NAME_BYTES: usize = 256;
const MAX_PRODUCT_NAME_BYTES: usize = 256;
const MAX_SSDP_UDN_BYTES: usize = 256;

/// Hardware metadata reported by a receiver's local setup endpoint.
///
/// These presentation strings are supplied by the network peer. The setup
/// endpoint uses a self-signed certificate and does not inherit Cast device
/// authentication from the control channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupDeviceInfo {
    manufacturer: Option<String>,
    model_name: Option<String>,
    product_name: Option<String>,
    ssdp_udn: Option<String>,
}

impl SetupDeviceInfo {
    /// Free-form hardware manufacturer, when supplied.
    pub fn manufacturer(&self) -> Option<&str> {
        self.manufacturer.as_deref()
    }

    /// Platform model name, when supplied.
    pub fn model_name(&self) -> Option<&str> {
        self.model_name.as_deref()
    }

    /// Platform product name, when supplied.
    pub fn product_name(&self) -> Option<&str> {
        self.product_name.as_deref()
    }

    /// SSDP device UUID, when supplied.
    pub fn ssdp_udn(&self) -> Option<&str> {
        self.ssdp_udn.as_deref()
    }
}

/// Result of the optional local setup-endpoint operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetupInfoOutcome {
    /// The endpoint returned a successful, validated response.
    Available(SetupDeviceInfo),
    /// The endpoint does not expose this operation.
    Unsupported,
}

#[derive(Debug, Deserialize)]
struct SetupEnvelope {
    device_info: Option<RawSetupDeviceInfo>,
}

#[derive(Debug, Deserialize)]
struct RawSetupDeviceInfo {
    manufacturer: Option<String>,
    model_name: Option<String>,
    product_name: Option<String>,
    ssdp_udn: Option<String>,
}

pub(crate) async fn query_device_info(
    cast_endpoint: SocketAddr,
) -> Result<SetupInfoOutcome, Error> {
    let setup_endpoint = setup_endpoint(cast_endpoint);
    timeout(REQUEST_TIMEOUT, query_device_info_inner(setup_endpoint))
        .await
        .map_err(|_| setup_error(format!("request to {setup_endpoint} timed out")))?
}

async fn query_device_info_inner(setup_endpoint: SocketAddr) -> Result<SetupInfoOutcome, Error> {
    let tcp = TcpStream::connect(setup_endpoint)
        .await
        .map_err(|error| setup_error(format!("connect to {setup_endpoint}: {error}")))?;
    let verifier = Arc::new(SelfSignedCertificateVerifier::new());
    let tls_config = Arc::new(
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    );
    let connector = TlsConnector::from(tls_config);
    let server_name = ServerName::IpAddress(setup_endpoint.ip().into());
    let mut stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|error| setup_error(format!("TLS handshake with {setup_endpoint}: {error}")))?;

    let host = host_header(setup_endpoint);
    let request = format!(
        "GET {SETUP_PATH} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| setup_error(format!("write request: {error}")))?;

    let response = read_http_response(&mut stream).await?;
    parse_http_response(&response)
}

async fn read_http_response<R>(stream: &mut R) -> Result<Vec<u8>, Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut response = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = stream
            .read(&mut buffer)
            .await
            .map_err(|error| setup_error(format!("read response: {error}")))?;
        if count == 0 {
            return Ok(response);
        }
        if response.len().saturating_add(count) > MAX_HEADER_BYTES + MAX_RESPONSE_BYTES {
            return Err(setup_error("HTTP response exceeds the size limit"));
        }
        response.extend_from_slice(&buffer[..count]);
        if let Some(length) = response_extent(&response)? {
            if response.len() >= length {
                response.truncate(length);
                return Ok(response);
            }
        }
    }
}

fn setup_endpoint(cast_endpoint: SocketAddr) -> SocketAddr {
    match cast_endpoint {
        SocketAddr::V4(endpoint) => SocketAddr::new((*endpoint.ip()).into(), SETUP_PORT),
        SocketAddr::V6(endpoint) => SocketAddr::V6(SocketAddrV6::new(
            *endpoint.ip(),
            SETUP_PORT,
            endpoint.flowinfo(),
            endpoint.scope_id(),
        )),
    }
}

fn host_header(endpoint: SocketAddr) -> String {
    match endpoint {
        SocketAddr::V4(endpoint) => endpoint.to_string(),
        SocketAddr::V6(endpoint) => format!("[{}]:{}", endpoint.ip(), endpoint.port()),
    }
}

fn response_extent(response: &[u8]) -> Result<Option<usize>, Error> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_length = match parsed
        .parse(response)
        .map_err(|error| setup_error(format!("parse HTTP response: {error}")))?
    {
        httparse::Status::Complete(length) if length <= MAX_HEADER_BYTES => length,
        httparse::Status::Complete(_) => return Err(setup_error("HTTP headers exceed the limit")),
        httparse::Status::Partial if response.len() <= MAX_HEADER_BYTES => return Ok(None),
        httparse::Status::Partial => return Err(setup_error("HTTP headers exceed the limit")),
    };
    if header_value(&parsed, "transfer-encoding").is_some_and(|value| {
        value
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
    }) {
        return chunked_body_length(&response[header_length..])
            .map(|length| length.map(|length| header_length + length));
    }
    if let Some(value) = header_value(&parsed, "content-length") {
        let length = value
            .trim()
            .parse::<usize>()
            .map_err(|_| setup_error("invalid Content-Length header"))?;
        if length > MAX_RESPONSE_BYTES {
            return Err(setup_error("response body exceeds the size limit"));
        }
        return Ok(Some(header_length + length));
    }
    Ok(None)
}

fn chunked_body_length(encoded: &[u8]) -> Result<Option<usize>, Error> {
    let mut offset = 0usize;
    loop {
        let Some(line_end) = encoded[offset..]
            .windows(2)
            .position(|window| window == b"\r\n")
        else {
            return Ok(None);
        };
        if line_end > 128 {
            return Err(setup_error("chunk-size line exceeds the limit"));
        }
        let size = std::str::from_utf8(&encoded[offset..offset + line_end])
            .ok()
            .and_then(|line| line.split(';').next())
            .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())
            .ok_or_else(|| setup_error("invalid chunk size"))?;
        offset = offset.saturating_add(line_end + 2);
        if size == 0 {
            let trailers = &encoded[offset..];
            if trailers.starts_with(b"\r\n") {
                return Ok(Some(offset + 2));
            }
            return Ok(trailers
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|length| offset + length + 4));
        }
        if size > MAX_RESPONSE_BYTES {
            return Err(setup_error("response body exceeds the size limit"));
        }
        if offset.saturating_add(size + 2) > encoded.len() {
            return Ok(None);
        }
        if encoded.get(offset + size..offset + size + 2) != Some(b"\r\n") {
            return Err(setup_error("response chunk omitted its terminator"));
        }
        offset = offset.saturating_add(size + 2);
        if offset > MAX_RESPONSE_BYTES {
            return Err(setup_error("response body exceeds the size limit"));
        }
    }
}

fn parse_http_response(response: &[u8]) -> Result<SetupInfoOutcome, Error> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Response::new(&mut headers);
    let header_length = match parsed
        .parse(response)
        .map_err(|error| setup_error(format!("parse HTTP response: {error}")))?
    {
        httparse::Status::Complete(length) if length <= MAX_HEADER_BYTES => length,
        httparse::Status::Complete(_) => return Err(setup_error("HTTP headers exceed the limit")),
        httparse::Status::Partial => return Err(setup_error("incomplete HTTP response headers")),
    };
    let status = parsed
        .code
        .ok_or_else(|| setup_error("HTTP response omitted its status"))?;
    if matches!(status, 404 | 405 | 501) {
        return Ok(SetupInfoOutcome::Unsupported);
    }
    if !(200..300).contains(&status) {
        return Err(setup_error(format!("receiver returned HTTP {status}")));
    }

    let body = response_body(&parsed, &response[header_length..])?;
    parse_device_info(&body).map(SetupInfoOutcome::Available)
}

fn response_body(parsed: &httparse::Response<'_, '_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let chunked = header_value(parsed, "transfer-encoding")
        .map(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        })
        .unwrap_or(false);
    if chunked {
        return decode_chunked_body(body);
    }

    let body = if let Some(value) = header_value(parsed, "content-length") {
        let length = value
            .trim()
            .parse::<usize>()
            .map_err(|_| setup_error("invalid Content-Length header"))?;
        if length > MAX_RESPONSE_BYTES {
            return Err(setup_error("response body exceeds the size limit"));
        }
        body.get(..length)
            .ok_or_else(|| setup_error("truncated HTTP response body"))?
    } else {
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(setup_error("response body exceeds the size limit"));
        }
        body
    };
    Ok(body.to_vec())
}

fn header_value<'a>(parsed: &'a httparse::Response<'_, '_>, name: &str) -> Option<&'a str> {
    parsed.headers.iter().find_map(|header| {
        header
            .name
            .eq_ignore_ascii_case(name)
            .then(|| std::str::from_utf8(header.value).ok())
            .flatten()
    })
}

fn decode_chunked_body(mut encoded: &[u8]) -> Result<Vec<u8>, Error> {
    let mut decoded = Vec::new();
    loop {
        let line_end = encoded
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| setup_error("incomplete chunk-size line"))?;
        if line_end > 128 {
            return Err(setup_error("chunk-size line exceeds the limit"));
        }
        let size = std::str::from_utf8(&encoded[..line_end])
            .ok()
            .and_then(|line| line.split(';').next())
            .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())
            .ok_or_else(|| setup_error("invalid chunk size"))?;
        encoded = &encoded[line_end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        if decoded.len().saturating_add(size) > MAX_RESPONSE_BYTES {
            return Err(setup_error("response body exceeds the size limit"));
        }
        let chunk = encoded
            .get(..size)
            .ok_or_else(|| setup_error("truncated HTTP response chunk"))?;
        if encoded.get(size..size + 2) != Some(b"\r\n") {
            return Err(setup_error("response chunk omitted its terminator"));
        }
        decoded.extend_from_slice(chunk);
        encoded = &encoded[size + 2..];
    }
}

fn parse_device_info(body: &[u8]) -> Result<SetupDeviceInfo, Error> {
    let envelope: SetupEnvelope = serde_json::from_slice(body)
        .map_err(|error| setup_error(format!("parse response JSON: {error}")))?;
    let Some(info) = envelope.device_info else {
        return Ok(SetupDeviceInfo {
            manufacturer: None,
            model_name: None,
            product_name: None,
            ssdp_udn: None,
        });
    };
    Ok(SetupDeviceInfo {
        manufacturer: bounded_text("manufacturer", info.manufacturer, MAX_MANUFACTURER_BYTES)?,
        model_name: bounded_text("model_name", info.model_name, MAX_MODEL_NAME_BYTES)?,
        product_name: bounded_text("product_name", info.product_name, MAX_PRODUCT_NAME_BYTES)?,
        ssdp_udn: bounded_text("ssdp_udn", info.ssdp_udn, MAX_SSDP_UDN_BYTES)?,
    })
}

fn bounded_text(
    field: &'static str,
    value: Option<String>,
    maximum: usize,
) -> Result<Option<String>, Error> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > maximum || value.chars().any(char::is_control) {
        return Err(setup_error(format!(
            "{field} is too long or contains a control character"
        )));
    }
    Ok(Some(value.to_owned()))
}

fn setup_error(message: impl Into<String>) -> Error {
    Error::SetupFailed(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_deployed_path_and_endpoint_host() {
        assert_eq!(SETUP_PATH, "/setup/eureka_info?params=device_info,name");
        assert_eq!(
            host_header("192.0.2.8:8443".parse().unwrap()),
            "192.0.2.8:8443"
        );
        assert_eq!(
            host_header("[fe80::1234%7]:8443".parse().unwrap()),
            "[fe80::1234]:8443"
        );
    }

    #[test]
    fn parses_bounded_tcl_product_information() {
        let info = parse_device_info(
            br#"{
                "name": "Apartment Living Room TV",
                "device_info": {
                    "manufacturer": " TCL ",
                    "model_name": "Smart TV",
                    "product_name": "G08",
                    "ssdp_udn": "70ba349a-7a14-0f3d-3808-9c7b5f2a5eb6"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(info.manufacturer(), Some("TCL"));
        assert_eq!(info.model_name(), Some("Smart TV"));
        assert_eq!(info.product_name(), Some("G08"));
        assert_eq!(
            info.ssdp_udn(),
            Some("70ba349a-7a14-0f3d-3808-9c7b5f2a5eb6")
        );
    }

    #[test]
    fn parses_content_length_and_chunked_http_responses() {
        let body = br#"{"device_info":{"manufacturer":"TCL","product_name":"G08"}}"#;
        let response = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes(),
            body.to_vec(),
        ]
        .concat();
        let SetupInfoOutcome::Available(info) = parse_http_response(&response).unwrap() else {
            panic!("expected setup information");
        };
        assert_eq!(info.manufacturer(), Some("TCL"));

        let first = &body[..20];
        let second = &body[20..];
        let chunked = [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec(),
            format!("{:x}\r\n", first.len()).into_bytes(),
            first.to_vec(),
            b"\r\n".to_vec(),
            format!("{:x}\r\n", second.len()).into_bytes(),
            second.to_vec(),
            b"\r\n0\r\n\r\n".to_vec(),
        ]
        .concat();
        let SetupInfoOutcome::Available(info) = parse_http_response(&chunked).unwrap() else {
            panic!("expected chunked setup information");
        };
        assert_eq!(info.product_name(), Some("G08"));
    }

    #[tokio::test]
    async fn content_length_completes_without_waiting_for_eof() {
        let body = br#"{"device_info":{"manufacturer":"TCL"}}"#;
        let response = [
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();
        let (mut reader, mut writer) = tokio::io::duplex(4096);
        writer.write_all(&response).await.unwrap();

        let received = timeout(Duration::from_millis(50), read_http_response(&mut reader))
            .await
            .expect("reader waited for the still-open writer")
            .unwrap();
        assert_eq!(received, response);
        drop(writer);
    }

    #[test]
    fn distinguishes_unsupported_and_rejects_unbounded_responses() {
        assert_eq!(
            parse_http_response(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").unwrap(),
            SetupInfoOutcome::Unsupported
        );
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        );
        assert!(matches!(
            parse_http_response(oversized.as_bytes()),
            Err(Error::SetupFailed(_))
        ));
    }

    #[test]
    fn missing_device_information_is_an_empty_success() {
        let info = parse_device_info(br#"{"name":"Living Room"}"#).unwrap();
        assert_eq!(
            info,
            SetupDeviceInfo {
                manufacturer: None,
                model_name: None,
                product_name: None,
                ssdp_udn: None,
            }
        );
    }

    #[test]
    fn rejects_malformed_or_unbounded_identity() {
        assert!(matches!(
            parse_device_info(br#"{"device_info":{"manufacturer":7}}"#),
            Err(Error::SetupFailed(_))
        ));
        let oversized = serde_json::json!({
            "device_info": {"product_name": "x".repeat(MAX_PRODUCT_NAME_BYTES + 1)},
        });
        assert!(matches!(
            parse_device_info(&serde_json::to_vec(&oversized).unwrap()),
            Err(Error::SetupFailed(_))
        ));
    }

    #[test]
    fn setup_endpoint_preserves_ipv6_scope() {
        let endpoint = setup_endpoint("[fe80::1234%7]:8009".parse().unwrap());
        assert_eq!(endpoint, "[fe80::1234%7]:8443".parse().unwrap());
    }
}
