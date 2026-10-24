// Cast device authentication is intentionally kept internal to the control
// connection. Production callers cannot obtain a connection until this module
// has authenticated the receiver credential and its binding to the TLS peer
// certificate.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use ring::{digest, signature};
use rustls::pki_types::{
    alg_id, AlgorithmIdentifier, CertificateDer, InvalidSignature, SignatureVerificationAlgorithm,
    TrustAnchor, UnixTime,
};
use webpki::{
    anchor_from_trusted_cert, EndEntityCert, ExtendedKeyUsageValidator, KeyPurposeIdIter,
};
use x509_parser::extensions::ParsedExtension;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::{parse_x509_certificate, X509Certificate};

use crate::error::Error;

const NONCE_SIZE: usize = 16;
const MAX_TLS_CERTIFICATE_LIFETIME: Duration = Duration::from_secs(4 * 24 * 60 * 60);
const AUDIO_ONLY_POLICY_OID: &str = "1.3.6.1.4.1.11129.2.5.2";

const CAST_ROOT_CA_PEM: &[u8] = include_bytes!("cast_root_ca.pem");
const EUREKA_ROOT_CA_PEM: &[u8] = include_bytes!("eureka_root_ca.pem");
const CAST_CRL_ROOT_CA_PEM: &[u8] = include_bytes!("cast_crl_root_ca.pem");

/// Result of the Cast device-authentication exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    certificate_sha256: [u8; 32],
    audio_only: bool,
    revocation_checked: bool,
}

impl DeviceIdentity {
    /// SHA-256 fingerprint of the authenticated device credential.
    pub fn certificate_sha256(&self) -> &[u8; 32] {
        &self.certificate_sha256
    }

    /// Whether the credential chain restricts this receiver to audio-only use.
    pub fn is_audio_only(&self) -> bool {
        self.audio_only
    }

    /// Whether receiver-supplied revocation information was present and checked.
    pub fn revocation_checked(&self) -> bool {
        self.revocation_checked
    }
}

pub(crate) struct AuthChallenge {
    pub nonce: [u8; NONCE_SIZE],
    pub payload: Vec<u8>,
}

pub(crate) fn create_challenge() -> AuthChallenge {
    let mut nonce = [0; NONCE_SIZE];
    rand::thread_rng().fill_bytes(&mut nonce);

    // DeviceAuthMessage { challenge: AuthChallenge {
    //   signature_algorithm: RSASSA_PKCS1v15,
    //   sender_nonce: nonce,
    //   hash_algorithm: SHA256,
    // }}
    let mut challenge = Vec::with_capacity(NONCE_SIZE + 8);
    encode_varint_field(&mut challenge, 1, SignatureAlgorithm::Pkcs1V15 as u64);
    encode_bytes_field(&mut challenge, 2, &nonce);
    encode_varint_field(&mut challenge, 3, HashAlgorithm::Sha256 as u64);

    let mut payload = Vec::with_capacity(challenge.len() + 3);
    encode_bytes_field(&mut payload, 1, &challenge);

    AuthChallenge { nonce, payload }
}

pub(crate) fn verify_challenge_reply(
    payload: &[u8],
    nonce: &[u8; NONCE_SIZE],
    tls_peer_certificate: &[u8],
) -> Result<DeviceIdentity, Error> {
    verify_tls_certificate_lifetime(tls_peer_certificate)?;

    let response = parse_device_auth_message(payload)?;
    verify_sender_nonce(response.sender_nonce.as_deref(), nonce)?;

    let verified = verify_device_credential_chain(
        &response.client_auth_certificate,
        &response.intermediate_certificates,
    )?;

    let signature_input = auth_signature_input(nonce, tls_peer_certificate);
    verify_auth_signature(&response, &verified.leaf_public_key, &signature_input)?;

    let revocation_checked = match response.crl.as_deref() {
        Some(crl) if !crl.is_empty() => {
            let revocations = parse_and_verify_crl(crl)?;
            revocations.check_chain(&verified.certificates)?;
            true
        }
        _ => false,
    };

    let fingerprint = digest::digest(&digest::SHA256, &response.client_auth_certificate);
    let mut certificate_sha256 = [0; 32];
    certificate_sha256.copy_from_slice(fingerprint.as_ref());

    Ok(DeviceIdentity {
        certificate_sha256,
        audio_only: verified.audio_only,
        revocation_checked,
    })
}

fn verify_tls_certificate_lifetime(der: &[u8]) -> Result<(), Error> {
    let now = unix_time_seconds()?;
    verify_tls_certificate_lifetime_at(der, now)
}

fn verify_tls_certificate_lifetime_at(der: &[u8], now: i64) -> Result<(), Error> {
    let cert = parse_certificate(der, "TLS peer certificate")?;
    let not_before = cert.validity().not_before.timestamp();
    let not_after = cert.validity().not_after.timestamp();

    if now < not_before {
        return Err(auth_error("TLS peer certificate is not valid yet"));
    }
    if now > not_after {
        return Err(auth_error("TLS peer certificate has expired"));
    }

    let remaining = u64::try_from(not_after - now)
        .map(Duration::from_secs)
        .map_err(|_| auth_error("invalid TLS peer certificate lifetime"))?;
    if remaining > MAX_TLS_CERTIFICATE_LIFETIME {
        return Err(auth_error(
            "TLS peer certificate remains valid for longer than four days",
        ));
    }
    Ok(())
}

fn verify_sender_nonce(received: Option<&[u8]>, expected: &[u8]) -> Result<(), Error> {
    if received != Some(expected) {
        return Err(auth_error("receiver returned a different sender nonce"));
    }
    Ok(())
}

fn auth_signature_input(nonce: &[u8], tls_peer_certificate: &[u8]) -> Vec<u8> {
    let mut input = Vec::with_capacity(nonce.len() + tls_peer_certificate.len());
    input.extend_from_slice(nonce);
    input.extend_from_slice(tls_peer_certificate);
    input
}

struct AuthResponse {
    signature: Vec<u8>,
    client_auth_certificate: Vec<u8>,
    intermediate_certificates: Vec<Vec<u8>>,
    signature_algorithm: SignatureAlgorithm,
    sender_nonce: Option<Vec<u8>>,
    hash_algorithm: HashAlgorithm,
    crl: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum SignatureAlgorithm {
    Pkcs1V15 = 1,
    Pss = 2,
}

impl SignatureAlgorithm {
    fn from_wire(value: u64) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::Pkcs1V15),
            2 => Ok(Self::Pss),
            _ => Err(auth_error(format!(
                "unsupported device signature algorithm {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum HashAlgorithm {
    Sha1 = 0,
    Sha256 = 1,
}

impl HashAlgorithm {
    fn from_wire(value: u64) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::Sha1),
            1 => Ok(Self::Sha256),
            _ => Err(auth_error(format!(
                "unsupported device hash algorithm {value}"
            ))),
        }
    }
}

fn parse_device_auth_message(data: &[u8]) -> Result<AuthResponse, Error> {
    let fields = parse_fields(data)?;
    if let Some(error) = fields.bytes(3) {
        let error_fields = parse_fields(error)?;
        let error_type = error_fields.varint(1).unwrap_or(0);
        return Err(auth_error(format!(
            "receiver rejected authentication challenge with error {error_type}"
        )));
    }

    let response = fields
        .bytes(2)
        .ok_or_else(|| auth_error("device-auth reply contains no response"))?;
    let fields = parse_fields(response)?;

    Ok(AuthResponse {
        signature: fields
            .bytes(1)
            .ok_or_else(|| auth_error("device-auth response has no signature"))?
            .to_vec(),
        client_auth_certificate: fields
            .bytes(2)
            .ok_or_else(|| auth_error("device-auth response has no device certificate"))?
            .to_vec(),
        intermediate_certificates: fields.bytes_all(3).map(<[u8]>::to_vec).collect(),
        signature_algorithm: SignatureAlgorithm::from_wire(fields.varint(4).unwrap_or(1))?,
        sender_nonce: fields.bytes(5).map(<[u8]>::to_vec),
        hash_algorithm: HashAlgorithm::from_wire(fields.varint(6).unwrap_or(0))?,
        crl: fields.bytes(7).map(<[u8]>::to_vec),
    })
}

struct VerifiedCredential {
    leaf_public_key: Vec<u8>,
    certificates: Vec<Vec<u8>>,
    audio_only: bool,
}

fn verify_device_credential_chain(
    leaf: &[u8],
    intermediates: &[Vec<u8>],
) -> Result<VerifiedCredential, Error> {
    let roots = [
        pem_contents(CAST_ROOT_CA_PEM)?,
        pem_contents(EUREKA_ROOT_CA_PEM)?,
    ];
    verify_chain(leaf, intermediates, &roots, "device credential")
}

fn verify_chain(
    leaf: &[u8],
    intermediates: &[Vec<u8>],
    root_der: &[Vec<u8>],
    description: &str,
) -> Result<VerifiedCredential, Error> {
    let leaf_der = CertificateDer::from(leaf);
    let end_entity = EndEntityCert::try_from(&leaf_der)
        .map_err(|e| auth_error(format!("invalid {description} certificate: {e}")))?;

    let root_certs: Vec<CertificateDer<'_>> = root_der
        .iter()
        .map(|der| CertificateDer::from(der.as_slice()))
        .collect();
    let roots: Vec<TrustAnchor<'_>> = root_certs
        .iter()
        .map(anchor_from_trusted_cert)
        .collect::<Result<_, _>>()
        .map_err(|e| auth_error(format!("invalid built-in trust anchor: {e}")))?;
    let intermediate_der: Vec<CertificateDer<'_>> = intermediates
        .iter()
        .map(|der| CertificateDer::from(der.as_slice()))
        .collect();

    let mut algorithms = webpki::ALL_VERIFICATION_ALGS.to_vec();
    algorithms.push(&RSA_PKCS1_SHA1_FOR_LEGACY_CAST_CERTS);
    let now = UnixTime::since_unix_epoch(Duration::from_secs(
        u64::try_from(unix_time_seconds()?)
            .map_err(|_| auth_error("system clock predates the Unix epoch"))?,
    ));

    let path = end_entity
        .verify_for_usage(
            &algorithms,
            &roots,
            &intermediate_der,
            now,
            AnyExtendedKeyUsage,
            None,
            None,
        )
        .map_err(|e| auth_error(format!("untrusted {description} certificate chain: {e}")))?;

    let leaf_x509 = parse_certificate(leaf, description)?;
    validate_signing_key(&leaf_x509, description)?;

    let mut certificates = vec![leaf.to_vec()];
    certificates.extend(
        path.intermediate_certificates()
            .map(|cert| cert.der().as_ref().to_vec()),
    );

    // `VerifiedPath::anchor()` refers to one of the exact `TrustAnchor`
    // values supplied above.  Match that value directly instead of comparing
    // its subjectPublicKeyInfo with x509-parser's `SubjectPublicKeyInfo::raw`:
    // those APIs expose different DER slices for some certificates even though
    // they describe the same key.
    let trusted_root = trusted_root_der(path.anchor(), &roots, root_der)?;
    certificates.push(trusted_root.to_vec());

    let audio_only = certificates.iter().try_fold(false, |restricted, der| {
        let cert = parse_certificate(der, description)?;
        Ok::<_, Error>(restricted || has_audio_only_policy(&cert))
    })?;

    Ok(VerifiedCredential {
        leaf_public_key: leaf_x509.public_key().subject_public_key.data.to_vec(),
        certificates,
        audio_only,
    })
}

fn trusted_root_der<'a>(
    verified_anchor: &TrustAnchor<'_>,
    roots: &[TrustAnchor<'_>],
    root_der: &'a [Vec<u8>],
) -> Result<&'a [u8], Error> {
    roots
        .iter()
        .position(|root| root == verified_anchor)
        .and_then(|index| root_der.get(index))
        .map(Vec::as_slice)
        .ok_or_else(|| auth_error("verified path used an unknown trust anchor"))
}

fn validate_signing_key(cert: &X509Certificate<'_>, description: &str) -> Result<(), Error> {
    let key = cert
        .public_key()
        .parsed()
        .map_err(|e| auth_error(format!("invalid {description} public key: {e}")))?;
    if key.key_size() < 2048 {
        return Err(auth_error(format!(
            "{description} RSA public key is smaller than 2048 bits"
        )));
    }
    if let Some(usage) = cert
        .key_usage()
        .map_err(|e| auth_error(format!("invalid {description} key usage: {e}")))?
    {
        if !usage.value.digital_signature() {
            return Err(auth_error(format!(
                "{description} certificate cannot sign authentication data"
            )));
        }
    }
    Ok(())
}

fn has_audio_only_policy(cert: &X509Certificate<'_>) -> bool {
    cert.extensions().iter().any(|extension| {
        matches!(
            extension.parsed_extension(),
            ParsedExtension::CertificatePolicies(policies)
                if policies.iter().any(|policy| policy.policy_id.to_id_string() == AUDIO_ONLY_POLICY_OID)
        )
    })
}

fn verify_auth_signature(
    response: &AuthResponse,
    public_key: &[u8],
    signature_input: &[u8],
) -> Result<(), Error> {
    let algorithm: &dyn signature::VerificationAlgorithm =
        match (response.signature_algorithm, response.hash_algorithm) {
            (SignatureAlgorithm::Pkcs1V15, HashAlgorithm::Sha1) => {
                &signature::RSA_PKCS1_1024_8192_SHA1_FOR_LEGACY_USE_ONLY
            }
            (SignatureAlgorithm::Pkcs1V15, HashAlgorithm::Sha256) => {
                &signature::RSA_PKCS1_2048_8192_SHA256
            }
            (SignatureAlgorithm::Pss, HashAlgorithm::Sha256) => {
                &signature::RSA_PSS_2048_8192_SHA256
            }
            (SignatureAlgorithm::Pss, HashAlgorithm::Sha1) => {
                return Err(auth_error("RSA-PSS with SHA-1 is unsupported"));
            }
        };

    signature::UnparsedPublicKey::new(algorithm, public_key)
        .verify(signature_input, &response.signature)
        .map_err(|_| auth_error("device credential did not sign the TLS certificate and nonce"))
}

struct Revocations {
    not_before: u64,
    not_after: u64,
    revoked_public_keys: HashSet<Vec<u8>>,
    revoked_serial_ranges: HashMap<Vec<u8>, Vec<(u64, u64)>>,
}

impl Revocations {
    fn check_chain(&self, chain: &[Vec<u8>]) -> Result<(), Error> {
        let now = u64::try_from(unix_time_seconds()?)
            .map_err(|_| auth_error("system clock predates the Unix epoch"))?;
        self.check_chain_at(chain, now)
    }

    fn check_chain_at(&self, chain: &[Vec<u8>], now: u64) -> Result<(), Error> {
        if now < self.not_before || now > self.not_after {
            return Err(auth_error("receiver supplied an expired revocation list"));
        }

        for index in (0..chain.len()).rev() {
            let cert = parse_certificate(&chain[index], "credential chain")?;
            let issuer_hash = digest::digest(&digest::SHA256, cert.public_key().raw);
            if self.revoked_public_keys.contains(issuer_hash.as_ref()) {
                return Err(auth_error(
                    "receiver credential chain contains a revoked key",
                ));
            }

            if index > 0 {
                if let Some(ranges) = self.revoked_serial_ranges.get(issuer_hash.as_ref()) {
                    if let Some(serial) = serial_number_u64(&parse_certificate(
                        &chain[index - 1],
                        "credential chain",
                    )?) {
                        if ranges
                            .iter()
                            .any(|(first, last)| *first <= serial && serial <= *last)
                        {
                            return Err(auth_error(
                                "receiver credential certificate serial number is revoked",
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn parse_and_verify_crl(data: &[u8]) -> Result<Revocations, Error> {
    let bundle = parse_fields(data)?;
    let crl_root = pem_contents(CAST_CRL_ROOT_CA_PEM)?;

    let mut last_error = auth_error("revocation bundle contained no supported CRL");
    for crl_data in bundle.bytes_all(1) {
        match parse_and_verify_one_crl(crl_data, &crl_root) {
            Ok(crl) => return Ok(crl),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn parse_and_verify_one_crl(data: &[u8], crl_root: &[u8]) -> Result<Revocations, Error> {
    let fields = parse_fields(data)?;
    let tbs = fields
        .bytes(1)
        .ok_or_else(|| auth_error("revocation list has no signed body"))?;
    let signer = fields
        .bytes(2)
        .ok_or_else(|| auth_error("revocation list has no signer certificate"))?;
    let crl_signature = fields
        .bytes(3)
        .ok_or_else(|| auth_error("revocation list has no signature"))?;

    let verified_signer =
        verify_chain(signer, &[], &[crl_root.to_vec()], "revocation-list signer")?;
    signature::UnparsedPublicKey::new(
        &signature::RSA_PKCS1_2048_8192_SHA256,
        &verified_signer.leaf_public_key,
    )
    .verify(tbs, crl_signature)
    .map_err(|_| auth_error("invalid revocation-list signature"))?;

    let signer_x509 = parse_certificate(signer, "revocation-list signer")?;
    let tbs_fields = parse_fields(tbs)?;
    if tbs_fields.varint(1).unwrap_or(0) != 0 {
        return Err(auth_error("unsupported revocation-list version"));
    }
    let not_before = tbs_fields.varint(2).unwrap_or(0);
    let mut not_after = tbs_fields.varint(3).unwrap_or(0);
    not_after = not_after.min(
        u64::try_from(signer_x509.validity().not_after.timestamp())
            .map_err(|_| auth_error("revocation-list signer has an invalid expiration time"))?,
    );

    let revoked_public_keys = tbs_fields.bytes_all(4).map(<[u8]>::to_vec).collect();
    let mut revoked_serial_ranges: HashMap<Vec<u8>, Vec<(u64, u64)>> = HashMap::new();
    for range_data in tbs_fields.bytes_all(5) {
        let range = parse_fields(range_data)?;
        let issuer = range
            .bytes(1)
            .ok_or_else(|| auth_error("revoked serial range has no issuer hash"))?
            .to_vec();
        let first = range.varint(2).unwrap_or(0);
        let last = range.varint(3).unwrap_or(0);
        if last < first {
            return Err(auth_error(
                "revocation list contains an inverted serial range",
            ));
        }
        revoked_serial_ranges
            .entry(issuer)
            .or_default()
            .push((first, last));
    }

    Ok(Revocations {
        not_before,
        not_after,
        revoked_public_keys,
        revoked_serial_ranges,
    })
}

fn serial_number_u64(cert: &X509Certificate<'_>) -> Option<u64> {
    let bytes = cert.raw_serial();
    if bytes.is_empty() || bytes.len() > 8 || bytes[0] & 0x80 != 0 {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0, |value, byte| (value << 8) | u64::from(*byte)),
    )
}

#[derive(Debug)]
struct AnyExtendedKeyUsage;

impl ExtendedKeyUsageValidator for AnyExtendedKeyUsage {
    fn validate(&self, values: KeyPurposeIdIter<'_, '_>) -> Result<(), webpki::Error> {
        for value in values {
            value?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct LegacyCastSha1Algorithm;

impl SignatureVerificationAlgorithm for LegacyCastSha1Algorithm {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature_bytes: &[u8],
    ) -> Result<(), InvalidSignature> {
        signature::UnparsedPublicKey::new(
            &signature::RSA_PKCS1_1024_8192_SHA1_FOR_LEGACY_USE_ONLY,
            public_key,
        )
        .verify(message, signature_bytes)
        .map_err(|_| InvalidSignature)
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::RSA_ENCRYPTION
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        // sha1WithRSAEncryption with explicit NULL parameters.
        AlgorithmIdentifier::from_slice(&[
            0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05, 0x05,
            0x00,
        ])
    }
}

static RSA_PKCS1_SHA1_FOR_LEGACY_CAST_CERTS: LegacyCastSha1Algorithm = LegacyCastSha1Algorithm;

fn parse_certificate<'a>(der: &'a [u8], description: &str) -> Result<X509Certificate<'a>, Error> {
    let (remaining, cert) = parse_x509_certificate(der)
        .map_err(|e| auth_error(format!("invalid {description}: {e}")))?;
    if !remaining.is_empty() {
        return Err(auth_error(format!("trailing data after {description}")));
    }
    Ok(cert)
}

fn pem_contents(data: &[u8]) -> Result<Vec<u8>, Error> {
    let (remaining, pem) = parse_x509_pem(data)
        .map_err(|e| auth_error(format!("invalid built-in trust anchor: {e}")))?;
    if !remaining.iter().all(u8::is_ascii_whitespace) {
        return Err(auth_error("trailing data after built-in trust anchor"));
    }
    Ok(pem.contents)
}

fn unix_time_seconds() -> Result<i64, Error> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| auth_error("system clock predates the Unix epoch"))?;
    i64::try_from(duration.as_secs()).map_err(|_| auth_error("system clock is out of range"))
}

fn auth_error(message: impl Into<String>) -> Error {
    Error::AuthenticationFailed(message.into())
}

#[derive(Clone, Copy)]
enum WireValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

struct Fields<'a>(Vec<(u32, WireValue<'a>)>);

impl<'a> Fields<'a> {
    fn bytes(&self, number: u32) -> Option<&'a [u8]> {
        self.0.iter().find_map(|(field, value)| match value {
            WireValue::Bytes(bytes) if *field == number => Some(*bytes),
            _ => None,
        })
    }

    fn bytes_all(&'a self, number: u32) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.0.iter().filter_map(move |(field, value)| match value {
            WireValue::Bytes(bytes) if *field == number => Some(*bytes),
            _ => None,
        })
    }

    fn varint(&self, number: u32) -> Option<u64> {
        self.0.iter().find_map(|(field, value)| match value {
            WireValue::Varint(value) if *field == number => Some(*value),
            _ => None,
        })
    }
}

fn parse_fields(mut data: &[u8]) -> Result<Fields<'_>, Error> {
    let mut fields = Vec::new();
    while !data.is_empty() {
        let tag = take_varint(&mut data)?;
        let number = u32::try_from(tag >> 3).map_err(|_| auth_error("protobuf field overflow"))?;
        if number == 0 {
            return Err(auth_error("protobuf field number is zero"));
        }
        let value = match tag & 7 {
            0 => WireValue::Varint(take_varint(&mut data)?),
            1 => {
                take(&mut data, 8)?;
                continue;
            }
            2 => {
                let length = usize::try_from(take_varint(&mut data)?)
                    .map_err(|_| auth_error("protobuf length overflow"))?;
                WireValue::Bytes(take(&mut data, length)?)
            }
            5 => {
                take(&mut data, 4)?;
                continue;
            }
            wire => return Err(auth_error(format!("unsupported protobuf wire type {wire}"))),
        };
        fields.push((number, value));
    }
    Ok(Fields(fields))
}

fn take<'a>(data: &mut &'a [u8], length: usize) -> Result<&'a [u8], Error> {
    if data.len() < length {
        return Err(auth_error("truncated protobuf field"));
    }
    let (head, tail) = data.split_at(length);
    *data = tail;
    Ok(head)
}

fn take_varint(data: &mut &[u8]) -> Result<u64, Error> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *take(data, 1)?
            .first()
            .ok_or_else(|| auth_error("truncated protobuf varint"))?;
        if shift == 63 && byte > 1 {
            return Err(auth_error("protobuf varint overflow"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(auth_error("protobuf varint overflow"))
}

fn encode_varint(buffer: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buffer.push(byte);
        if value == 0 {
            return;
        }
    }
}

fn encode_varint_field(buffer: &mut Vec<u8>, field: u32, value: u64) {
    encode_varint(buffer, u64::from(field) << 3);
    encode_varint(buffer, value);
}

fn encode_bytes_field(buffer: &mut Vec<u8>, field: u32, value: &[u8]) {
    encode_varint(buffer, (u64::from(field) << 3) | 2);
    encode_varint(buffer, value.len() as u64);
    buffer.extend_from_slice(value);
}

impl fmt::Debug for AuthResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthResponse")
            .field("signature_len", &self.signature.len())
            .field("certificate_len", &self.client_auth_certificate.len())
            .field("intermediates", &self.intermediate_certificates.len())
            .field("signature_algorithm", &self.signature_algorithm)
            .field("hash_algorithm", &self.hash_algorithm)
            .field("has_crl", &self.crl.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_uses_nonce_and_sha256() {
        let challenge = create_challenge();
        assert_eq!(challenge.nonce.len(), NONCE_SIZE);

        let envelope = parse_fields(&challenge.payload).unwrap();
        let inner = parse_fields(envelope.bytes(1).unwrap()).unwrap();
        assert_eq!(inner.varint(1), Some(1));
        assert_eq!(inner.bytes(2), Some(challenge.nonce.as_slice()));
        assert_eq!(inner.varint(3), Some(1));
    }

    #[test]
    fn parses_device_auth_response() {
        let mut response = Vec::new();
        encode_bytes_field(&mut response, 1, b"signature");
        encode_bytes_field(&mut response, 2, b"certificate");
        encode_bytes_field(&mut response, 3, b"intermediate-1");
        encode_bytes_field(&mut response, 3, b"intermediate-2");
        encode_varint_field(&mut response, 4, 1);
        encode_bytes_field(&mut response, 5, b"0123456789abcdef");
        encode_varint_field(&mut response, 6, 1);

        let mut envelope = Vec::new();
        encode_bytes_field(&mut envelope, 2, &response);
        let parsed = parse_device_auth_message(&envelope).unwrap();

        assert_eq!(parsed.signature, b"signature");
        assert_eq!(parsed.client_auth_certificate, b"certificate");
        assert_eq!(parsed.intermediate_certificates.len(), 2);
        assert_eq!(parsed.hash_algorithm, HashAlgorithm::Sha256);
    }

    #[test]
    fn rejects_auth_error() {
        let mut error = Vec::new();
        encode_varint_field(&mut error, 1, 2);
        let mut envelope = Vec::new();
        encode_bytes_field(&mut envelope, 3, &error);

        assert!(parse_device_auth_message(&envelope).is_err());
    }

    #[test]
    fn built_in_roots_parse() {
        for pem in [CAST_ROOT_CA_PEM, EUREKA_ROOT_CA_PEM, CAST_CRL_ROOT_CA_PEM] {
            let der = pem_contents(pem).unwrap();
            let cert = parse_certificate(&der, "root").unwrap();
            assert!(cert.public_key().parsed().unwrap().key_size() >= 2048);
        }
    }

    #[test]
    fn verified_anchor_maps_back_to_its_exact_root_certificate() {
        let root_der = vec![
            pem_contents(CAST_ROOT_CA_PEM).unwrap(),
            pem_contents(EUREKA_ROOT_CA_PEM).unwrap(),
        ];
        let root_certs: Vec<CertificateDer<'_>> = root_der
            .iter()
            .map(|der| CertificateDer::from(der.as_slice()))
            .collect();
        let roots: Vec<TrustAnchor<'_>> = root_certs
            .iter()
            .map(anchor_from_trusted_cert)
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(
            trusted_root_der(&roots[1], &roots, &root_der).unwrap(),
            root_der[1]
        );
    }

    #[test]
    fn tls_certificate_may_not_outlive_the_authentication_window() {
        let der = pem_contents(CAST_ROOT_CA_PEM).unwrap();
        let cert = parse_certificate(&der, "test certificate").unwrap();
        let not_after = cert.validity().not_after.timestamp();
        let maximum_lifetime = i64::try_from(MAX_TLS_CERTIFICATE_LIFETIME.as_secs()).unwrap();

        verify_tls_certificate_lifetime_at(&der, not_after - maximum_lifetime).unwrap();
        assert!(
            verify_tls_certificate_lifetime_at(&der, not_after - maximum_lifetime - 1).is_err()
        );
        assert!(verify_tls_certificate_lifetime_at(&der, not_after + 1).is_err());
    }

    #[test]
    fn sender_nonce_must_match_exactly() {
        let expected = b"0123456789abcdef";

        verify_sender_nonce(Some(expected), expected).unwrap();
        assert!(verify_sender_nonce(Some(b"0123456789abcdeg"), expected).is_err());
        assert!(verify_sender_nonce(None, expected).is_err());
    }

    #[test]
    fn authentication_signature_binds_nonce_before_tls_certificate() {
        assert_eq!(
            auth_signature_input(b"nonce", b"certificate"),
            b"noncecertificate"
        );
    }

    #[test]
    fn device_chain_does_not_accept_an_unrelated_root() {
        let cast_root = pem_contents(CAST_ROOT_CA_PEM).unwrap();
        let eureka_root = pem_contents(EUREKA_ROOT_CA_PEM).unwrap();

        assert!(verify_chain(&cast_root, &[], &[eureka_root], "test device credential").is_err());
    }

    #[test]
    fn revocation_list_rejects_revoked_keys_and_invalid_time_windows() {
        let root = pem_contents(CAST_ROOT_CA_PEM).unwrap();
        let cert = parse_certificate(&root, "test credential").unwrap();
        let key_hash = digest::digest(&digest::SHA256, cert.public_key().raw)
            .as_ref()
            .to_vec();
        let revoked = Revocations {
            not_before: 100,
            not_after: 200,
            revoked_public_keys: HashSet::from([key_hash]),
            revoked_serial_ranges: HashMap::new(),
        };

        assert!(revoked
            .check_chain_at(std::slice::from_ref(&root), 150)
            .is_err());
        assert!(revoked
            .check_chain_at(std::slice::from_ref(&root), 99)
            .is_err());
        assert!(revoked
            .check_chain_at(std::slice::from_ref(&root), 201)
            .is_err());
    }

    #[test]
    fn unsigned_revocation_bundle_is_rejected() {
        let mut unsigned_crl = Vec::new();
        encode_bytes_field(&mut unsigned_crl, 1, b"body");
        encode_bytes_field(&mut unsigned_crl, 2, b"signer");
        let mut bundle = Vec::new();
        encode_bytes_field(&mut bundle, 1, &unsigned_crl);

        assert!(parse_and_verify_crl(&bundle).is_err());
    }

    #[test]
    fn malformed_protobuf_is_rejected() {
        assert!(parse_fields(&[0x12, 0x80]).is_err());
        assert!(parse_fields(&[0]).is_err());
    }
}
