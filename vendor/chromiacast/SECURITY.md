# Security policy

## Receiver authentication

Production callers must create control connections with
`CastConnection::connect` or `CastConnection::connect_address`. These methods
complete the Cast device-authentication exchange before returning, including
certificate-chain, nonce, TLS-certificate binding, signature, and optional
revocation checks.

The explicitly named `connect_unverified_for_testing` variants are provided
for receiver emulators that do not implement device authentication. They are
only compiled by the `dangerous-unverified` Cargo feature. They accept any peer
identity and therefore permit an on-path attacker on the local network to
impersonate a receiver. Do not expose them as a production fallback or select
them from ordinary user configuration.

The library authenticates the receiver; it does not authorize which local
users may discover, connect to, or cast to a device. Applications remain
responsible for local-user policy, consent, and protecting encoded media
before it reaches this crate.

`CastConnection::get_setup_device_info` is deliberately outside that
authentication boundary. It queries a self-signed HTTPS service at the same
selected receiver address and returns bounded presentation metadata. Callers
must not treat those manufacturer, model, product, or SSDP strings as a
credential or authorization decision.

## Android TV pairing credentials

The optional `android-tv` feature generates an RSA client key and certificate.
The television authorizes that certificate during the six-hex-digit Polo
pairing exchange, so callers must reuse the same `AndroidTvRemoteIdentity` for
subsequent Remote Service connections. The library returns DER bytes for
caller-managed persistence and never writes them itself. Store the unencrypted
PKCS#8 key in a desktop credential service, never in discovery metadata,
ordinary configuration, logs, crash diagnostics, or command-line arguments.

The pairing secret binds the live client and device certificates to the code
displayed by the television. Remote Service uses a self-signed device
certificate, and the current post-pairing connection checks TLS signatures but
does not retain or pin that certificate. Consequently, endpoint selection is a
local-network trust decision and Remote Service metadata is presentation data,
not authenticated Cast identity. Applications that need a stronger boundary
should retain a device certificate or fingerprint from pairing and add pinning
before exposing control on a hostile network.

Android key injection has no semantic acknowledgement. A successful
`send_key` or `press_key` result means only that the complete bounded protocol
frame was flushed to the live TLS session.

## Supported versions

Security fixes are applied to the current development branch and the most
recent published release. Earlier 0.x releases are not supported after a
successor is published.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through the repository's
[GitHub security advisory form](https://github.com/pronkproject/chromiacast/security/advisories/new).
Include the affected revision, a description of the impact, and reproduction
details when possible. Avoid filing a public issue until a fix is available.
