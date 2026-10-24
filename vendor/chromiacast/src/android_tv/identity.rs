use std::fmt;

use rand::rngs::OsRng;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, SerialNumber};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use rustls::pki_types::PrivatePkcs8KeyDer;
use x509_parser::prelude::{FromDer, X509Certificate};
use x509_parser::public_key::PublicKey;
use zeroize::Zeroizing;

use super::error::{crypto_error, invalid_identity, AndroidTvError};

const RSA_BITS: usize = 2048;
const MAX_COMMON_NAME_BYTES: usize = 128;
const MAX_CERTIFICATE_DER_BYTES: usize = 64 * 1024;
const MAX_PRIVATE_KEY_DER_BYTES: usize = 64 * 1024;

/// Persistent client certificate and private key used by Android TV Remote.
///
/// Pairing authorizes the certificate, so callers must reuse the same identity
/// for later control connections and store the private-key DER in an
/// appropriate credential service. The type never writes credentials itself.
pub struct AndroidTvRemoteIdentity {
    certificate_der: Vec<u8>,
    private_key_pkcs8_der: Zeroizing<Vec<u8>>,
    modulus: Vec<u8>,
    exponent: Vec<u8>,
}

impl AndroidTvRemoteIdentity {
    /// Generate a 2048-bit RSA identity and self-signed client certificate.
    pub fn generate(common_name: &str) -> Result<Self, AndroidTvError> {
        validate_common_name(common_name)?;
        let private_key = RsaPrivateKey::new(&mut OsRng, RSA_BITS)
            .map_err(|error| crypto_error(format!("generate RSA key: {error}")))?;
        let private_key_der = private_key
            .to_pkcs8_der()
            .map_err(|error| crypto_error(format!("encode RSA key: {error}")))?;

        let rcgen_key = KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(private_key_der.as_bytes()),
            &rcgen::PKCS_RSA_SHA256,
        )
        .map_err(|error| crypto_error(format!("load RSA key for certificate: {error}")))?;
        let mut parameters = CertificateParams::new(vec!["chromiacast.local".into()])
            .map_err(|error| crypto_error(format!("build certificate parameters: {error}")))?;
        parameters.distinguished_name = rcgen::DistinguishedName::new();
        parameters
            .distinguished_name
            .push(DnType::CommonName, common_name);
        parameters.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        parameters.serial_number = Some(SerialNumber::from(1000_u64));
        parameters.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        parameters.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(3_650);
        let certificate = parameters
            .self_signed(&rcgen_key)
            .map_err(|error| crypto_error(format!("sign client certificate: {error}")))?;

        Self::from_der(
            certificate.der().as_ref().to_vec(),
            private_key_der.as_bytes().to_vec(),
        )
    }

    /// Import a DER X.509 certificate and matching unencrypted PKCS#8 RSA key.
    pub fn from_der(
        certificate_der: Vec<u8>,
        private_key_pkcs8_der: Vec<u8>,
    ) -> Result<Self, AndroidTvError> {
        let private_key_pkcs8_der = Zeroizing::new(private_key_pkcs8_der);
        if private_key_pkcs8_der.is_empty()
            || private_key_pkcs8_der.len() > MAX_PRIVATE_KEY_DER_BYTES
        {
            return Err(invalid_identity("PKCS#8 private key must be 1..=64 KiB"));
        }
        let (modulus, exponent) = rsa_parameters_from_certificate(&certificate_der)?;
        if unsigned_bits(&modulus) < RSA_BITS {
            return Err(invalid_identity("RSA key is smaller than 2048 bits"));
        }

        let private_key = RsaPrivateKey::from_pkcs8_der(private_key_pkcs8_der.as_slice())
            .map_err(|error| invalid_identity(format!("parse PKCS#8 RSA key: {error}")))?;
        private_key
            .validate()
            .map_err(|error| invalid_identity(format!("validate RSA key: {error}")))?;
        let public_key = RsaPublicKey::from(&private_key);
        if public_key.n().to_bytes_be() != modulus || public_key.e().to_bytes_be() != exponent {
            return Err(invalid_identity(
                "certificate public key does not match the private key",
            ));
        }

        Ok(Self {
            certificate_der,
            private_key_pkcs8_der,
            modulus,
            exponent,
        })
    }

    /// DER-encoded X.509 certificate suitable for credential persistence.
    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    /// Unencrypted PKCS#8 private-key DER suitable for credential persistence.
    pub fn private_key_pkcs8_der(&self) -> &[u8] {
        &self.private_key_pkcs8_der
    }

    pub(crate) fn modulus(&self) -> &[u8] {
        &self.modulus
    }

    pub(crate) fn exponent(&self) -> &[u8] {
        &self.exponent
    }
}

impl fmt::Debug for AndroidTvRemoteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AndroidTvRemoteIdentity")
            .field("certificate_der_bytes", &self.certificate_der.len())
            .field("rsa_bits", &unsigned_bits(&self.modulus))
            .finish_non_exhaustive()
    }
}

pub(crate) fn rsa_parameters_from_certificate(
    certificate_der: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), AndroidTvError> {
    if certificate_der.is_empty() || certificate_der.len() > MAX_CERTIFICATE_DER_BYTES {
        return Err(invalid_identity("X.509 certificate must be 1..=64 KiB"));
    }
    let (remainder, certificate) = X509Certificate::from_der(certificate_der)
        .map_err(|error| invalid_identity(format!("parse X.509 certificate: {error}")))?;
    if !remainder.is_empty() {
        return Err(invalid_identity(
            "X.509 certificate contains trailing bytes",
        ));
    }
    let public_key = certificate
        .public_key()
        .parsed()
        .map_err(|error| invalid_identity(format!("parse certificate public key: {error}")))?;
    let PublicKey::RSA(public_key) = public_key else {
        return Err(invalid_identity("certificate public key is not RSA"));
    };
    let modulus = minimal_unsigned(public_key.modulus);
    let exponent = minimal_unsigned(public_key.exponent);
    if modulus.is_empty() || exponent.is_empty() {
        return Err(invalid_identity(
            "certificate contains an empty RSA parameter",
        ));
    }
    Ok((modulus, exponent))
}

fn minimal_unsigned(bytes: &[u8]) -> Vec<u8> {
    let first_nonzero = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len());
    bytes[first_nonzero..].to_vec()
}

fn unsigned_bits(bytes: &[u8]) -> usize {
    bytes
        .first()
        .map(|first| bytes.len() * 8 - first.leading_zeros() as usize)
        .unwrap_or(0)
}

fn validate_common_name(common_name: &str) -> Result<(), AndroidTvError> {
    if common_name.is_empty()
        || common_name.len() > MAX_COMMON_NAME_BYTES
        || common_name.chars().any(char::is_control)
    {
        return Err(AndroidTvError::InvalidClientInfo(
            "certificate common name must be 1..=128 bytes without control characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_identity_round_trips_through_der() {
        let generated = AndroidTvRemoteIdentity::generate("Pronk test remote").unwrap();
        let imported = AndroidTvRemoteIdentity::from_der(
            generated.certificate_der().to_vec(),
            generated.private_key_pkcs8_der().to_vec(),
        )
        .unwrap();
        assert_eq!(generated.modulus(), imported.modulus());
        assert_eq!(generated.exponent(), [1, 0, 1]);
    }

    #[test]
    fn a_certificate_cannot_be_combined_with_another_private_key() {
        let first = AndroidTvRemoteIdentity::generate("first").unwrap();
        let second = AndroidTvRemoteIdentity::generate("second").unwrap();
        let error = AndroidTvRemoteIdentity::from_der(
            first.certificate_der().to_vec(),
            second.private_key_pkcs8_der().to_vec(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn debug_never_contains_key_material() {
        let identity = AndroidTvRemoteIdentity::generate("debug").unwrap();
        let debug = format!("{identity:?}");
        assert!(!debug.contains(&format!("{:?}", &identity.private_key_pkcs8_der()[..16])));
        assert!(debug.contains("rsa_bits"));
    }

    #[test]
    fn imported_certificate_must_be_one_complete_der_value() {
        let identity = AndroidTvRemoteIdentity::generate("trailing DER").unwrap();
        let mut certificate = identity.certificate_der().to_vec();
        certificate.push(0);
        let error = AndroidTvRemoteIdentity::from_der(
            certificate,
            identity.private_key_pkcs8_der().to_vec(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("trailing bytes"));
    }
}
