use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::ClientConfig;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::tls::SelfSignedCertificateVerifier;

use super::error::AndroidTvError;
use super::identity::AndroidTvRemoteIdentity;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn connect(
    address: SocketAddr,
    identity: &AndroidTvRemoteIdentity,
) -> Result<TlsStream<TcpStream>, AndroidTvError> {
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| AndroidTvError::TimedOut("TCP connection"))?
        .map_err(|error| AndroidTvError::Connection(format!("connect to {address}: {error}")))?;
    tcp.set_nodelay(true).map_err(AndroidTvError::Io)?;

    let verifier = Arc::new(SelfSignedCertificateVerifier::new());
    let certificate = CertificateDer::from(identity.certificate_der().to_vec());
    let private_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(
        identity.private_key_pkcs8_der().to_vec(),
    ));
    let configuration = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![certificate], private_key)
        .map_err(|error| {
            AndroidTvError::InvalidIdentity(format!("configure mutual TLS: {error}"))
        })?;
    let connector = TlsConnector::from(Arc::new(configuration));
    let server_name = ServerName::IpAddress(address.ip().into());
    let stream = timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| AndroidTvError::TimedOut("TLS handshake"))?
        .map_err(|error| AndroidTvError::Connection(format!("TLS with {address}: {error}")))?;

    let _ = peer_certificate(&stream)?;
    Ok(stream)
}

pub(crate) fn peer_certificate(stream: &TlsStream<TcpStream>) -> Result<&[u8], AndroidTvError> {
    stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .map(|certificate| certificate.as_ref())
        .ok_or_else(|| {
            AndroidTvError::Connection("device did not present a TLS certificate".into())
        })
}
