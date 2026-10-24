# chromiacast

`chromiacast` is a codec-agnostic, sender-side implementation of Cast
Streaming in Rust. It connects to a Cast receiver, authenticates the device,
launches the mirroring application, negotiates audio and video streams, and
sends already-encoded access units over encrypted Cast RTP.

The crate deliberately does not depend on PipeWire, GStreamer, GNOME, or an
encoder. Those belong in adapters and applications built on top of this
protocol boundary.

## Using the crate

Add the protocol library to a Rust project with:

```sh
cargo add chromiacast
```

The default feature set provides authenticated Cast control and streaming.
Enable `discovery` for mDNS discovery or `android-tv` for Android TV pairing
and remote control:

```toml
chromiacast = { version = "0.3", features = ["discovery", "android-tv"] }
```

The `dangerous-unverified` feature exists only for protocol test doubles. It
must not be enabled as a production fallback. See the
[API documentation](https://docs.rs/chromiacast) for the complete interface.

## Status

The library currently provides:

- Cast V2 framing over TLS and Cast device authentication;
- receiver status, app availability, launch, stop, and unsolicited lifecycle
  events;
- request-correlated authenticated `GET_DEVICE_INFO` and optional
  `eureka_info` product metadata, accepting both deployed `DEVICE_INFO`
  responses and receivers that echo `GET_DEVICE_INFO`;
- a bounded HTTPS setup-endpoint query for deployed receiver manufacturer and
  product metadata;
- heartbeat replies and timeout detection;
- OFFER/ANSWER negotiation for H.264, VP8, VP9, HEVC, AV1, Opus, and AAC
  configurations;
- frame encryption and Cast RTP packetization;
- RTCP Sender Reports, receiver reports, ACKs, NACKs, retransmission, and
  picture-loss handling;
- simultaneous audio and video sender streams;
- optional Android TV Remote Service v2 pairing and control, kept independent
  from the Cast transport and exposed through the `android-tv` feature;
- observable sender failures, receiver timeouts, and flow-control statistics;
- a pluggable datagram `Transport`; and
- optional mDNS discovery that retains stable device IDs and every advertised
  scoped endpoint.

Bitrate selection and congestion control remain application policy. Capture,
encoding, media-pipeline integration, and desktop transaction management are
also intentionally outside this crate.

## Security

`CastConnection::connect` and `CastConnection::connect_address` authenticate
the receiver after TLS. Cast receivers use self-signed TLS certificates, so
authentication is performed by the Cast device-auth challenge: the response's
certificate chain is checked against the bundled Cast trust roots and its
signature is bound to the live TLS certificate and a fresh nonce. Revocation
data is validated when the receiver supplies it.

`connect_unverified_for_testing` and
`connect_address_unverified_for_testing` exist only for protocol test doubles
such as shanocast. They are hidden from generated documentation and only
compiled when the explicitly named `dangerous-unverified` feature is enabled.
They make no claim about the peer's identity and must not be used in production
software.

Authenticated product queries are available only on a verified
`CastConnection`. The control task uses the protocol's distinct `requestId`
and `request_id` spellings, requires the exact platform route and namespace,
bounds every returned string, and distinguishes an unsupported or omitted
`eureka_info` reply from a malformed successful reply. Applications remain
responsible for matching the returned device ID to the device the user
selected; the library does not treat presentation strings as authority.

`CastConnection::get_setup_device_info` queries the selected receiver's local
HTTPS setup service directly. That service uses a self-signed certificate, so
its bounded manufacturer, model, product, and SSDP strings are peer-supplied
presentation metadata rather than Cast-authenticated identity. The client
uses the deployed literal `device_info,name` request, sends the selected IP in
the HTTP `Host` header, does not follow redirects, and stops at the declared
body boundary without waiting for the receiver to close its persistent TLS
connection.

Android TV Remote Service is a separate protocol even when the same television
also implements Google Cast. `AndroidTvRemoteIdentity` contains the persistent
client certificate and private key authorized by the PIN pairing exchange. The
library never persists that identity; applications must place it in an
appropriate credential service and reuse it for later connections. Remote
Service devices also use self-signed TLS certificates. The pairing secret binds
both certificates to the code displayed by the television, but the current
control connection does not pin the device certificate after pairing. Treat the
selected network endpoint and all device metadata as untrusted input.

See [SECURITY.md](SECURITY.md) for the security model and reporting guidance.

## Encoded-frame contract

Each `EncodedFrame` represents exactly one complete encoded access unit:

- `media_timestamp` is monotonic on that stream and uses the shared media
  timeline supplied by the producer;
- `reference_time` is the corresponding local monotonic-clock observation,
  used for Sender Reports and latency measurements;
- `duration` should be supplied when known so callers can measure queued media
  time;
- the first frame is a key frame, and a delta frame references the immediately
  preceding frame;
- reordered frames and B-frames are not supported initially;
- H.264 access units use Annex B byte-stream format and include parameter sets
  where needed for independent decoding; and
- an Opus access unit contains one complete Opus packet, with its duration
  represented by `duration`.

The RTP timestamp is derived from `media_timestamp`, not from the thread's
arrival time. Audio and video produced under one media clock can therefore be
submitted from different tasks without losing their shared timeline.

Applications should watch `SenderEvent::NeedsKeyFrame`, react to terminal
events, and use `SenderSession::statistics` or `StatisticsUpdated` events to
throttle the encoder before `send` reaches the maximum frame-ID span.

`max_acceptable_in_flight_duration` is an advisory encoder budget. It keeps a
66 ms floor, tracks roughly two measured network round trips, and reserves most
of the receiver's playout window by capping the sender at one third of that
window. The library reports this pressure but does not change encoder bitrate
or drop already-accepted frames on the application's behalf.

Until the first frame acknowledgement arrives, a video producer should still
send occasional key-frame probes even when that advisory budget is exhausted.
This lets a receiver recover when its media path becomes ready after the first
key frame was sent. The red-screen example demonstrates this startup policy.

## Examples

Discover the first receiver, authenticate it, and display decodable solid-red
H.264 video:

```sh
cargo run --features discovery --example red_screen
```

Press Ctrl-C to stop the mirroring application and close both sender sessions
cleanly. The example is intentionally small, but it demonstrates key-frame
requests, a media timeline, session negotiation, and deliberate teardown.

For packetization and load diagnostics with deliberately non-decodable data:

```sh
cargo run --example packet_transport_stress -- RECEIVER_ADDRESS
```

An IP address uses port 8009. For an IPv6 link-local receiver, pass a scoped
socket address such as `[fe80::1234%3]:8009`.

Applications which combine Cast streaming with Android TV control should keep
the two connection lifecycles as peers. A Cast connection can continue to own
mirroring and receiver volume while an optional `AndroidTvRemote` supplies
power and Android key injection. Pairing is a consuming two-stage operation:
retain the returned `AndroidTvPairingSession` while the TV shows its
six-hex-digit code, then call `finish` once. Key methods return after a complete
Remote v2 frame has been flushed; the protocol provides no semantic
acknowledgement that the television acted on the key.

For a one-shot interoperability check which generates an ephemeral identity,
pairs, and sends one action:

```sh
cargo run --features android-tv --example android_tv_control -- TV_ADDRESS
```

The diagnostic keeps one connection open for interactive actions but
intentionally does not persist its private key, so it must pair again on every
run. An action may be supplied after the address for a one-shot invocation.
Production applications should use the API directly and store the identity in
their credential service.

Chromiacast does not present input-source selection as a Remote Service
capability. Although Android assigns key codes to an input picker and direct
inputs, a live Google TV ignored those injected codes and the Google remote UI
on the tested setup does not expose them. `AndroidTvKeyCode::from_raw` remains
available for deliberate protocol experiments without promoting an unproven
operation into the typed convenience API.

## Testing

Run the in-process protocol and mock-transport coverage with:

```sh
cargo test --all-features
```

An ignored end-to-end test starts shanocast in Podman, encodes a red H.264
image, sends it through the complete stack, captures the receiver window, and
checks the rendered center pixel:

```sh
cargo test --all-features --test pixel_verify -- --ignored --nocapture
```

That test requires Podman, Xvfb, and ImageMagick's `import` and `convert`
commands. The shanocast container image may be downloaded on its first run.

## Diagnostics

Control-message routes, message types, request/sequence IDs, and RTCP feedback
shapes are instrumented through the `tracing` crate. Encoded media, encryption
keys, certificates, and complete control payloads are not logged. Applications
can install their preferred `tracing` subscriber when collecting an
interoperability trace from a production receiver.

When discovery selects an IPv6 link-local receiver route, pass its complete
scoped `SocketAddr` to `SenderSession::start_address()` and bind the transport
to an IPv6 local address. `SenderSession::start()` remains the convenience API
for callers that already have a routable `IpAddr`.

## Protocol references and provenance

The implementation is original Rust code informed by the published
[Open Screen Cast Streaming protocol](https://chromium.googlesource.com/openscreen/+/refs/heads/main/cast/protocol/streaming_session_protocol.md),
[Cast channel schema](https://chromium.googlesource.com/openscreen/+/refs/heads/main/cast/common/channel/proto/cast_channel.proto),
and [sender authentication reference](https://chromium.googlesource.com/openscreen/+/refs/heads/main/cast/sender/channel/cast_auth_util.cc).
It does not embed Open Screen's GN/C++ runtime. The bundled Cast trust anchors
were sourced from Open Screen; their attribution and license are recorded in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

The optional Android TV implementation is original Rust code informed by
AOSP's published Google TV Pairing Protocol and multiple interoperable Remote
Service v2 clients. It models only the protobuf fields it uses rather than
embedding another project's generated protocol sources. Exact references,
revisions, and licenses are recorded in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## License

The original chromiacast code is available under the MIT License. See
[LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
