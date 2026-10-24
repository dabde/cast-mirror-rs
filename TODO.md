# cast-mirror-rs: low-latency Mac → Chromecast mirroring in Rust

Goal: mirror the macOS desktop to a Cast receiver with Chrome-like latency (~300 ms) using
**Cast Streaming** (the mirroring protocol), hardware encoding via VideoToolbox, no Chrome.

Reference implementation: Google **Open Screen** (`openscreen`, BSD licence), the C++ code Chrome
uses. Port from it module by module and keep its unit tests as test vectors.
Paths below are from memory: confirm each against the current source before porting.

Baseline numbers to beat (measured with the lag wheel page, one photo of both screens):
- Chrome "Cast screen": ~300 ms
- VLC `screen://` → `#chromecast`: 17–23 s (Default Media Receiver buffering), software x264

Test target: Philips TPM191E (Cast built in, `dactv`, 192.168.177.56), plus the Chromecast if it is on.

**Status 2026-10-24:** everything up to milestone 6 is implemented and verified against openscreen's own
`cast_receiver` (FFmpeg/SDL build, decodes and displays): scan, launch, OFFER/ANSWER, H.264/HEVC/VP8 + Opus,
test pattern on screen, full desktop mirroring at 30 fps / 6 Mbit/s with 0 retransmits, reconnect, clean STOP.
Update 10:21: all items done on the TV too (the TV's clock was stuck in 2025, which broke Cast TLS until fixed);
measured ~300-340 ms end-to-end, about 2.4 % CPU. See README.md for usage and the local receiver setup.

---

## 0. Groundwork

- [x] Clone openscreen; read `cast/streaming/` (sender side) and `cast/standalone_sender/`
  (`~/git/ext/openscreen`; full gclient checkout in `~/git/ext/osbuild/openscreen`)
- [x] Optional: build a known-good reference via depot_tools: built `cast_receiver` instead (more useful without
      the TV): `out/rel` (dummy player) and `out/av` (FFmpeg 9 + SDL2 + Opus + libvpx, decodes and displays)
- [x] Capture a Chrome mirroring session in Wireshark (TLS 8009 + UDP) for message shapes and packet rates
      (2026-10-24, Chrome → dactv, `scripts/capture-chrome.sh`, channel decrypted via SSLKEYLOGFILE):
  - Flow: CONNECT, GET_APP_AVAILABILITY, LAUNCH 0F5096E8, RECEIVER_STATUS, CONNECT to app, OFFER/ANSWER,
    GET_CAPABILITIES, PING/PONG every 5 s, STOP, CLOSE. Same flow as cast-mirror.
  - OFFER: targetDelay 200 ms both streams; H.264 `avc1.420028` (Baseline L4.0), maxBitRate 5 Mbit/s, 30 fps,
    1920x1080, PT 96; Opus 128 kbit/s 48 kHz stereo, PT 127; receiverRtcpEventLog true; no rtpExtensions.
  - ANSWER: both streams, no constraints/display; TV capabilities h264, vp8, vp9, aac, opus, remoting 2 (no HEVC).
  - Media: audio 100 pkt/s (10 ms Opus), video ~13 fps on a static page (send on change), max packet 1472 B,
    Sender Report every 500 ms per stream; TV sends ~120 RTCP/s (XR ref time, Cast ACK/NACK, event log, RR).
  - Takeaways applied: default `--delay 200`; PT 96/127 and 10 ms Opus already matched.
- [x] Search crates.io / GitHub for an existing Cast Streaming sender before writing one: **chromiacast 0.3.1**
      (MIT) does control, device auth, OFFER/ANSWER, frame crypto, RTP/RTCP, NACK/retransmit, PLI. Packet layout,
      nonce and sender model match openscreen byte for byte; reused for milestones 1 to 3 (vendored with 2 patches,
      see `vendor/chromiacast/PATCHES.md`). `mac-screen-cast` served as a reference for SCK + VideoToolbox.
- [x] `cargo new cast-mirror-rs`. Layout differs from the plan below: one binary crate (`src/capture.rs`,
      `video.rs`, `vp8.rs`, `audio.rs`, `mirror.rs`, `main.rs`) + `vendor/chromiacast` instead of the two protocol
      crates. Planned layout, for reference:
  - `cast-control` (discovery, TLS channel, protobuf, app launch, OFFER/ANSWER)
  - `cast-streaming` (RTP packetizer, frame crypto, RTCP, sender state machine)
  - `capture` (ScreenCaptureKit), `encode` (VideoToolbox H.264, Opus)
  - `cast-mirror` (CLI binary)
- Crates: `tokio`, `mdns-sd`, `prost` (+ `cast_channel.proto`), `rustls` (custom verifier: receivers use
  self-signed certs), `serde_json`, `aes` + `ctr`, `bytes`, `screencapturekit`,
  `objc2-video-toolbox` / `objc2-core-media` (or raw FFI), `opus`, `clap`, `tracing`

## 1. Discovery and control channel  (days)

- [x] mDNS browse `_googlecast._tcp.local`, read TXT `fn` (friendly name), `md` (model), `id` (`cast-mirror scan`)
- [x] TLS to port 8009; frames = 4-byte big-endian length + `CastMessage` protobuf (chromiacast)
- [x] Namespaces: `tp.connection` (CONNECT), `tp.heartbeat` (PING/PONG every 5 s), `receiver` (GET_STATUS, LAUNCH)
- [x] Launch the **mirroring receiver app**; take its app ID from openscreen (do not guess),
      then CONNECT to the app's `transportId`: `0F5096E8` (audio+video), `85CDB22F` (audio only)
- [x] Device authentication (`deviceauth` namespace): chromiacast verifies the receiver against the Cast roots;
      `--features test-receiver` + `--unverified` skips it for openscreen's dev receiver only
- Done when: `cast-mirror scan` lists the TV, `cast-mirror launch dactv` starts the mirroring app on screen
  - [x] verified against openscreen `cast_receiver` (scan lists it, LAUNCH → LAUNCH_STATUS USER_ALLOWED)
  - [x] Chromecast "Living Room": scan lists it, device auth against the Cast roots passes, `launch` starts Chrome Mirroring
  - [x] on the TV (2026-10-24 10:00): `scan` lists `dactv` (TPM191E), device auth passes, `launch` starts Chrome Mirroring.
        The earlier TLS resets were the TV's clock stuck in 2025 (dead CMOS battery, no NTP): its Cast certs were not yet valid

## 2. Session negotiation (OFFER / ANSWER)  (days)

Namespace `urn:x-cast:com.google.cast.webrtc` (confirm in `cast/streaming/offer_messages.cc`, `answer_messages.cc`).

- [x] Build OFFER: `castMode: "mirroring"`, `seqNum`, `supportedStreams[]` (PT 96/127 like Chrome by default)
  - video: `type: video_source`, `codecName: h264`, `rtpProfile: cast`, `rtpPayloadType: 96`, `ssrc`,
    `timeBase: 1/90000`, `targetDelay` (ms, start 200–400), `maxFrameRate`, `maxBitRate`,
    `resolutions: [{width, height}]`, `aesKey` + `aesIvMask` (16 random bytes each, hex)
  - audio: `type: audio_source`, `codecName: opus`, `rtpPayloadType: 127`, `ssrc`, `timeBase: 1/48000`,
    `channels: 2`, `bitRate`, own `aesKey` / `aesIvMask`
- [x] Parse ANSWER: chosen stream indices, receiver UDP port, receiver SSRCs, `rtcpReporting`, receiver constraints
      (constraints clamp bitrate, fps and resolution; the whole ANSWER is logged at info level)
- [x] Open the UDP socket to the receiver's port; one socket for RTP + RTCP of all streams
- Done when: the TV accepts the OFFER and the logs show a valid ANSWER
  - [x] openscreen receiver: ANSWER ok (H.264, HEVC, VP8, Opus; multi-codec offers pick correctly)
  - [x] Chromecast: ANSWER ok, udpPort 10008, `rtpExtensions: [["adaptive_playout_delay"]]`, no constraints sent
  - [x] on the TV: ANSWER ok, udpPort 39118, adaptive_playout_delay, no constraints; audio + video both accepted

## 3. Media transport (Cast RTP / RTCP)  (2–4 weeks, the core)

Port from `cast/streaming/`: `rtp_packetizer`, `frame_crypto`, `sender`, `sender_packet_router`,
`compound_rtcp_parser`, `sender_report_builder`, `rtcp_session`, `packet_util`.

- [x] **Frame crypto:** AES-128-CTR per encoded frame; nonce = `aesIvMask` XOR frame ID (check byte
      order and placement in `frame_crypto.cc`); unit-test against openscreen's vectors: frame ID big-endian
      in nonce bytes 8..12. openscreen's tests use random keys, so the KAT is checked against `openssl enc`
- [x] **Packetizer:** RTP header (V=2, PT, seq, 90 kHz / 48 kHz timestamp, SSRC) + Cast header
      (key-frame flag, frame ID, packet ID, max packet ID, reference frame ID, extensions such as
      adaptive latency); payload ≤ ~1400 B so nothing fragments at IP level
- [x] **Sender state:** frame IDs, frames in flight (cap it), last ACKed frame, playout deadline per frame
- [x] **RTCP out:** Sender Report about once a second (NTP ↔ RTP timestamp mapping for A/V sync); every 500 ms,
      plus one ahead of each frame until the first Receiver Report (patched in; fixed ~300 ms of dropped startup video)
- [x] **RTCP in:** receiver report (loss, jitter), Cast feedback (checkpoint frame ID, ACK bitmap,
      NACK list of frame/packet IDs, target delay), picture-loss indication → keyframe request
- [x] **Retransmission:** resend NACKed packets while still before their deadline; kick-start by resending the
      last packet when no feedback arrives; drop frames that can no longer make their deadline
- [x] Round-trip time from RTCP; log it (`--stats`)
- [x] Unit tests: packetize → depacketize round trip, crypto vectors, RTCP parse of captured Chrome packets:
      no Chrome capture yet, so the RTCP vectors are the 19 packets openscreen's receiver-side builder emits
- Done when: a synthetic test pattern (solid colour switching every second) shows on the TV
  - [x] `cast-mirror pattern` shows on openscreen's receiver window (H.264, HEVC, VP8)
  - [x] Chromecast: colours flash on screen (confirmed by eye), 30 fps, RTT 3 to 6 ms, 0 retransmits, all frames ACKed
  - [x] on the TV (colours seen on screen): sender side verified 10:01 (H.264 High 1920x1080@30, RTT 2-3 ms, 0 retransmits, all frames ACKed);
        confirmed by eye

## 4. Capture and hardware encoding  (about 1 week)

### Screen capture (ScreenCaptureKit; avfoundation delivers no frames on macOS 27)
- [x] `SCStream` on the main display, `SCStreamConfiguration`: output 1920×1080 (or scaled to the display's
      aspect, even dimensions), `minimumFrameInterval` 1/30 or 1/60, pixel format `420v` (NV12) so
      VideoToolbox takes the IOSurface without conversion, `showsCursor = true`, `queueDepth` 3
- [x] Skip idle frames (SCStream only delivers on change); still send a frame at least every ~1 s
      (note: `frame_status()` reads None on macOS 27; the image buffer pointer from screencapturekit is +1
      retained and must be adopted, or SCK stalls after queueDepth frames)
- [x] Screen Recording permission: check `CGPreflightScreenCaptureAccess`, explain if missing

### Video: H.264 via VideoToolbox (`VTCompressionSession`)
- [x] Encoder spec: `RequireHardwareAcceleratedVideoEncoder = true`,
      `EnableLowLatencyRateControl = true` (low-latency mode, macOS 11.3+)
- [x] Properties (MaxKeyFrameIntervalDuration 10 s; key frames forced at start and on PLI):
  - `RealTime = true`
  - `ProfileLevel = H264_Main_AutoLevel` (High if the receiver accepts it; Main is the safe start)
  - `AllowFrameReordering = false`: no B-frames, otherwise frames wait for later ones
  - `ExpectedFrameRate` = capture fps
  - `AverageBitRate` 4–8 Mbit/s at 1080p30; `DataRateLimits` as a 1-second cap (~1.5× average)
  - `MaxKeyFrameInterval` large (e.g. 10 s) or off; keyframes **on demand** with
    `kVTEncodeFrameOptionKey_ForceKeyFrame` on receiver PLI / at session start
  - `H264EntropyMode = CABAC`
- [x] Output conversion: AVCC (length-prefixed NAL units) → **Annex B** start codes; prepend SPS/PPS from the
      format description to every keyframe (confirm the payload format the receiver expects in openscreen)
- [x] Bitrate changes at runtime from the congestion controller (`AverageBitRate` can be updated live)
- [x] Later: offer VP8 as a fallback for receivers without H.264 (libvpx, software), and HEVC/VP9 where the
      ANSWER allows: `--codecs h264,h264-high,hevc,vp8` (VP8 behind `--features vp8`). VP9 skipped: no hardware
      encoder on the Mac and VP8 already covers the software fallback

### Audio: Opus
- [x] ScreenCaptureKit audio (`capturesAudio = true`, 48 kHz stereo, exclude own process)
- [x] Opus encoder: 48 kHz, stereo, 10 or 20 ms frames, ~128 kbit/s, `OPUS_APPLICATION_RESTRICTED_LOWDELAY` (10 ms)
- [x] RTP timestamps in 48 kHz units; A/V sync through the Sender Reports (shared media timeline from one origin)

## 5. Latency and robustness  (weeks)

- [x] Target playout delay: start ~250 ms, make it a CLI flag; check what Chrome offers: Chrome's OFFER to the TV
      has targetDelay 200 ms (captured), so `--delay` defaults to 200
- [x] Congestion control: adapt bitrate from loss / RTT / ACK timing: AIMD on RTCP loss, retransmit ratio and
      in-flight backlog (×0.8 / +5 % per second). openscreen's BandwidthEstimator is not ported: it needs per-burst
      byte counts from inside chromiacast's pacer. Revisit if AIMD oscillates on the TV
- [x] Frame dropping at the capture side when the encoder or network falls behind (never queue up)
- [x] Reconnect after Wi-Fi drops; clean STOP on Ctrl-C (receiver namespace STOP, close TLS): verified by
      killing and restarting the receiver mid-stream
- [x] Measure with the lag wheel page: goal ≤ 400 ms end-to-end; CPU should stay low with hardware encoding.
      CPU done (2026-10-24): 2.4 % mean / 2.8 % max of one core, desktop + audio 1920x1240@30 to dactv, RTT 3 ms.
      Latency (photo 2026-10-24 10:21, `--delay 200`, H.264 High 1920x1240@30): Mac 06.3xx s (hand ~330 ms),
      TV 06.037 s, so ~300-340 ms end-to-end. Goal met; on par with Chrome (~300 ms), vs VLC 17-23 s.

## 6. CLI

- [x] `cast-mirror scan` (+ `cast-mirror stop DEVICE` to end a mirroring app left behind by a killed sender)
- [x] `cast-mirror [DEVICE] --fps 30 --bitrate 6M --width 1920 --delay 250 [--no-audio] [--display N]`
- [x] `--stats`: live fps, bitrate, RTT, retransmits, dropped frames

---

## Effort (rough)

| Milestone | Effort |
|---|---|
| Scan + launch mirroring app (1) | days |
| OFFER/ANSWER accepted (2) | days |
| Test pattern on the TV (3) | 2–4 weeks |
| Desktop at low latency, video only (4) | +1 week |
| Audio, sync, adaptive bitrate, reconnects (5) | 1–2 months more |

## Open questions

- [x] Exact mirroring receiver app ID and namespace strings: read them from openscreen: `0F5096E8` / `85CDB22F`;
      `urn:x-cast:com.google.cast.{tp.connection,tp.heartbeat,tp.deviceauth,receiver,webrtc}`
- [x] Which H.264 profiles/levels and max resolution the Philips receiver accepts (see its ANSWER): picks H.264 High
      over Main, takes 1920x1080 and 1920x1240, sends no constraints/display block; rejects HEVC (picked H.264 from
      `hevc,h264`). Default could move to `--codecs h264-high,h264`
