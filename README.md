# cast-mirror

Mirror the macOS desktop to a Cast receiver over **Cast Streaming** (the protocol Chrome's
"Cast screen" uses): ScreenCaptureKit capture, VideoToolbox hardware H.264/HEVC, Opus audio,
encrypted Cast RTP with NACK/retransmission. No Chrome, no Default Media Receiver buffering.

```sh
cargo build --release
./target/release/cast-mirror scan                 # list receivers (name, model, id, address)
./target/release/cast-mirror launch dactv         # start the mirroring app (0F5096E8) and exit
./target/release/cast-mirror pattern dactv        # solid colour changing every second
./target/release/cast-mirror dactv --stats        # mirror display 0 with audio
./target/release/cast-mirror dactv --window brave # only that window (and only its app's audio)
```

Options: `--fps 30 --bitrate 6M --width 1920 --delay 100 --no-audio --display N --window TEXT --stats`,
`--codecs h264,hevc,vp8` (offer order = preference, the receiver picks),
`--spec-payload-types` (H.264 101 / Opus 96 instead of Chrome's 96 / 127).
DEVICE is a name substring, Cast id, IP, or `ip:port`; default is the first receiver found.
`--window` picks the largest visible window whose "app: title" contains TEXT (any case); without a match it
lists the open windows. The capture size is fixed at the start, so resize the window before casting.
Ctrl-C sends STOP and closes the channel. A lost receiver is reconnected once after 2 s; a second loss within 30 s stops with a hint. If another sender takes the receiver over (or it is stopped on the TV), mirroring stops instead of grabbing it back.

Needs Screen Recording permission for the terminal (System Settings → Privacy & Security).
`--features vp8` adds a software VP8 encoder (needs `brew install libvpx pkgconf`).

## Menu bar app

`scripts/bundle-app.sh` builds `~/Applications/CastMirror.app` (log: `~/Library/Logs/CastMirror.log`).
A click on its menu bar icon opens a small window (a web view on the system's WebKit; `src/ui.html` is compiled
into the binary): **Screen** (a list when several displays are connected) or **Window…** (the macOS window
picker) to start, **Stop** while casting, the receivers (a TV with the app installed and network ADB authorized
shows up a second time as "<name> · TV app" for direct mode), a latency profile, **Send audio** and **Mute Mac
while casting** (ScreenCaptureKit records the apps' audio before the output volume, so the receiver still gets all
of it). Picking another source while casting switches to it. A right click offers **Restart app** and **Quit**;
after an update, macOS may ask for the Screen Recording permission again: grant it, then use Restart app.
Icons are SF Symbols rendered from the system at runtime, so no Apple artwork ships with the app.

### Signing

Ad-hoc signed builds lose the Screen Recording permission on every rebuild. `bundle-app.sh` signs with a
code-signing identity named "CastMirror Local Signing" when the keychain has one; a self-signed one is enough
(it need not be trusted):

```sh
printf '[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=CastMirror Local Signing\n[ext]\nbasicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=critical,codeSigning\n' > /tmp/cs.cnf
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config /tmp/cs.cnf -keyout /tmp/cs.key -out /tmp/cs.pem
openssl pkcs12 -export -inkey /tmp/cs.key -in /tmp/cs.pem -name "CastMirror Local Signing" -out /tmp/cs.p12 -passout pass:x
security import /tmp/cs.p12 -k ~/Library/Keychains/login.keychain-db -P x -T /usr/bin/codesign
rm /tmp/cs.cnf /tmp/cs.key /tmp/cs.pem /tmp/cs.p12
```

Grant the permission once more after the first signed build; later builds keep it.

## Layout

- `src/capture.rs`: ScreenCaptureKit, NV12 frames (latest-only, never queued) and 48 kHz stereo audio
- `src/video.rs`: VideoToolbox H.264/HEVC → Annex B with parameter sets on key frames
- `src/vp8.rs`: libvpx VP8 fallback (feature `vp8`)
- `src/audio.rs`: 10 ms Opus packets on the shared A/V timeline
- `src/mirror.rs`: session loop, backpressure frame dropping, AIMD bitrate control, `--stats`
- `vendor/chromiacast`: Cast control channel, device auth, OFFER/ANSWER, frame crypto, RTP/RTCP,
  retransmission ([chromiacast](https://crates.io/crates/chromiacast) with local patches, see
  `vendor/chromiacast/PATCHES.md`)

## Tests

```sh
cargo test --workspace                       # unit tests, openscreen RTCP vectors, crypto KAT
cargo test -- --include-ignored              # plus hardware encode and real screen capture
```

### Against openscreen's reference receiver (no TV needed)

openscreen's `cast_receiver` decodes and displays the stream in a window. It uses a self-signed
certificate, so Cast device auth is skipped with a test-only feature:

```sh
# in an openscreen checkout (fetch openscreen; gclient sync), see cast/docs/external_libraries.md
gn gen out/av --args='is_debug=false have_ffmpeg=true have_libsdl2=true have_libopus=true ...'
autoninja -C out/av cast_receiver
./out/av/cast_receiver -g en0                          # once: writes generated_root_cast_receiver.{key,crt}
./out/av/cast_receiver -p generated_root_cast_receiver.key -d generated_root_cast_receiver.crt en0

cargo run --release --features test-receiver -- 192.168.x.y:8010 --unverified --stats
```

## Latency tuning (measured 2026-10-24, Philips TPM191E, Wi-Fi)

Lag wheel on the Mac next to Photo Booth showing the TV, 6 screenshots per setting (`scripts/lag-sweep.sh`).
Photo Booth's own ~145 ms is subtracted (calibrated with a phone photo); medians, roughly ±40 ms.

**Chromecast dongle on HDMI 4, TV in Game mode** (honours the playout delay almost 1:1 down to ~60 ms;
the default 100 keeps room for a Wi-Fi retransmit, `--delay 60` is the fastest useful setting):

| Setting | End-to-end |
|---|---|
| `--delay 200` | ~210 ms |
| `--delay 100` (default) | ~95 ms |
| `--delay 60` | the floor: 40, 30, 20 and 10 measure the same (~190 ms with Photo Booth, i.e. roughly 50-100 ms real) |
| `--delay 60` at 1670x1080, 834x540, 556x360 | all the same (204-213 ms with Photo Booth): resolution doesn't matter |
| `--delay 60`, `--bitrate` 2 to 25M | latency the same at every level; above ~8 Mbit/s the encoder doesn't use more at 1080p30 |

**TV's built-in Cast app** (keeps its own ~150 ms buffer, Game mode only exists on HDMI):

| Setting | End-to-end |
|---|---|
| `--delay 200` | ~310-330 ms |
| `--delay 150` | ~200 ms at any size: 1670x1080, 1112x720, 834x540, 556x360 all measure the same |
| `--delay 100`, `--fps 60`, H.264 Main, motion smoothing off | no consistent gain |

4K (`--width 3840`, 3340x2160 from a MacBook) shows on the TV's built-in app; the dongle accepted and ACKed it, picture unconfirmed.
Neither receiver accepts HEVC for mirroring (both pick H.264 when offered `hevc,h264`).
Frames above 1080 lines (1920x1240 before the 16:9 fit) cost ~90 ms on the TV app and show black on the dongle.

Chrome to the built-in app: ~300 ms (offers targetDelay 200, H.264 Baseline). CPU 2-10 % of one core.
Capture is fitted into a 16:9 box (1920 → 1670x1080 on a MacBook): the dongle shows a black screen above
1080 lines even though its ANSWER sends no constraints.

## Direct mode: own Android TV app (prototype, video only)

The TV's Cast app holds frames for at least ~150 ms. `android/` is a minimal receiver app (plain Java, no
Gradle) that feeds UDP frames straight into the hardware decoder and renders each one at once.

```sh
android/build.sh install 192.168.177.56        # build, install over adb, start
cast-mirror 192.168.177.56 --direct --codecs hevc --fps 60
```

Measured on the Philips (same Photo Booth setup, medians, ~145 ms Photo Booth subtracted):

| Path | End-to-end |
|---|---|
| Built-in Cast, `--delay 150` | ~240 ms |
| Direct, H.264, video plane | ~165 ms |
| Direct, HEVC, video plane | ~130-155 ms (the MediaTek HEVC decoder buffers less; Cast never accepted HEVC) |
| **Direct, HEVC, 60 fps, GPU plane (default)** | **~70 ms**, on par with the Chromecast dongle in Game mode |

The decoder's output goes through a TextureView, so the GPU composites it on the graphics plane like a game.
The TV's video plane runs its picture processing (~85 ms); `--ez texture false` brings that path back.
Declaring the app a game (`GAME=1 ./build.sh`) makes no difference on this Philips. The decoder rejects
Android's KEY_LOW_LATENCY but accepts MediaTek's `vdec-lowlatency`.

**Encryption (protocol v3):** every packet in both directions is AES-128-GCM sealed: payload encrypted,
header authenticated, a counter per packet as nonce, so resends get fresh nonces. The key is random per session
and reaches the app with the start intent over ADB, which the TV authenticated when it accepted this Mac; that is
why `--direct` now starts the app itself. Started by hand without a key, the app only asks to cast from the Mac. `--no-encryption` (in the
menu bar app: switch off "Encrypt the stream", which then shows a warning) sends v2 packets in the clear, e.g. to
inspect them in Wireshark; anyone on the network can then watch the stream and inject frames.
Cost: 24 bytes per packet (payload 1376 instead of 1400); fps unchanged in a 2 % loss test.

**Loss and congestion:** the app NACKs missing packets (and whole missing frames) and the sender
resends them from a 120-frame history; a frame is only given up after 60 ms without progress while a later one is
complete, then a key frame is requested (at most one per 500 ms). Every 100 ms the app reports received/expected
packets and the minimum one-way delay. The sender cuts the bitrate when the queue grows (> 30 ms, down to 85 % of
what got through) or loss over 1 s exceeds 10 %, holds at moderate loss (NACK repairs it), and climbs back 2 % per
report after a calm second. `--auto` (tray profile "Auto") also steps the picture: 1080p60 ≥ 3.5 Mbit/s,
1080p30 ≥ 2, 720p30 ≥ 1.2, else 540p30, starting on the rung `--bitrate` carries; down at once, up after 5 s
with 20 % headroom. A step up that fails within 10 s doubles that wait (up to 60 s): a static screen leaves the link
idle, so the bitrate climbs and would otherwise flip between two rungs every few seconds.

Tested without touching the network: `CAST_MIRROR_DROP=0.05` (5 % loss: stays at 1080p60, ~30 NACK repairs/s)
and `CAST_MIRROR_CAP_KBPS=3000` (queue stays under ~40 ms, settles on 720p30 at ~1.9 Mbit/s).

The TV's graphics plane is 1920x1080 (`wm size` override on a 3840x2160 panel), so the fast GPU path is
1080p at most. The tray restarts the app on the 4K video plane for profiles wider than 1920 (Presentation):
sharp, but ~85 ms slower.

In the menu bar app, picking "<name> · TV app" as receiver starts the app over ADB and streams HEVC. Not yet: audio. The Chromecast dongle
runs classic Cast firmware and can't install apps.
