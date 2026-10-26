# Technical notes

Mirror the macOS desktop to a Cast receiver over **Cast Streaming** (the protocol Chrome's
"Cast screen" uses): ScreenCaptureKit capture, VideoToolbox hardware H.264/HEVC, Opus audio,
encrypted Cast RTP with NACK/retransmission. No Chrome, no Default Media Receiver buffering.

```sh
cargo build --release
./target/release/cast-mirror scan                 # list receivers (name, model, id, address)
./target/release/cast-mirror launch tv         # start the mirroring app (0F5096E8) and exit
./target/release/cast-mirror pattern tv        # solid colour changing every second
./target/release/cast-mirror tv --stats        # mirror display 0 with audio
./target/release/cast-mirror tv --window safari # only that window (and only its app's audio)
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

## Browser receiver

`cast-mirror web --fps 60 --bitrate 20M` serves a receiver page any browser with WebCodecs can open (an iPad, a
laptop): it prints a link `https://<this Mac>:7443/#<token>`. The page has a fullscreen button (iPad, desktop browsers; Safari on the iPhone allows fullscreen only for video,
so there it suggests Add to Home Screen). Entering fullscreen or rotating changes the view; the page reports the new
size and the Mac captures for it, the decoder starting over from the next key frame.
**Extend** (menu bar window, or `--extend`) streams a new virtual display instead of mirroring one: a second
monitor at the receiver's exact size, so macOS renders text natively for it and nothing gets scaled (sharp on
the iPad). Drag windows onto it as onto a real monitor; when casting stops, macOS moves them back. For the
browser it is Retina-sized to the view and follows fullscreen/rotation; for the TV and Cast it is 1920x1080.
It uses CoreGraphics' private `CGVirtualDisplay` (as BetterDisplay and DeskPad do), which a macOS update could
break. macOS renders Retina only from about 530 points of height, so a lower view (an iPhone in landscape)
gets a slightly larger display that the capture scales down a few percent.
**Extend size** in the window (`--scale`) sets the display relative to the receiver: Larger text (0.8), Default
(1.0, sharpest) or More space (1.25); the picture is scaled to the receiver either way. The page keeps the
picture clear of a notch or the iPhone's island (the safe area) and reports that area as its view.
In the menu bar app, pick **Browser (iPad, laptop…)** as receiver: the window shows the link and a QR code to scan
with the iPad camera, and Screen or Window… starts serving (60 fps, 20 Mbit/s). The page reports its screen in pixels and the Mac
captures to fit exactly that, so the picture isn't scaled twice. The certificate is self-signed (the browser asks
once); the token in the link is the access check.

Video goes over a WebRTC data channel (UDP, via str0m on the Mac as ICE-lite peer), opened unordered with at most
100 ms of retransmissions, so a lost piece costs one retry instead of holding up later frames. The page decodes
H.264 High in a worker (WebCodecs, hardware) and draws to a canvas; frames carry a number so it can put them back
in order, and a frame still missing after 120 ms (or once a newer key frame is there) is given up for a key frame.

Measurements: [performance.md](performance.md).

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

## Direct mode: own Android TV app (prototype, video only)

The TV's Cast app holds frames for at least ~150 ms. `android/` is a minimal receiver app (plain Java, no
Gradle) that feeds UDP frames straight into the hardware decoder and renders each one at once.

```sh
android/build.sh install 192.168.1.50        # build, install over adb, start
cast-mirror 192.168.1.50 --direct --codecs hevc --fps 60
```

Measurements: [performance.md](performance.md).

**Encryption (protocol v3):** every packet in both directions is AES-128-GCM sealed: payload encrypted,
header authenticated, a counter per packet as nonce, so resends get fresh nonces. The session key always comes
from the pairing handshake below: network ADB (`adb tcpip`) is unencrypted, so ADB only starts the app and never
carries a key. `--no-encryption` (in the menu bar app: switch off "Encrypt the stream", which then shows a warning)
sends v2 packets in the clear, e.g. to inspect them in Wireshark; anyone on the network can then watch the stream
and inject frames, and the TV shows a red "Unencrypted" banner the whole time.
Cost: 24 bytes per packet (payload 1376 instead of 1400); fps unchanged in a 2 % loss test.

**Without ADB:** the app announces itself on the network (`_castmirror._udp`) while it is open, so it also
shows up in the menu bar window when no ADB connection exists (e.g. a phone after a restart). Pairing happens
once: casting to an unpaired device asks for the code it shows (or `cast-mirror pair <ip>` in a terminal). Both
sides do an ECDH P-256 exchange and mix the code into the result (PBKDF2, 100 000 rounds), so a man in the middle
has to guess the code; each stores the resulting key (Mac: `~/Library/Application Support/CastMirror/paired.json`,
mode 600; app: its preferences). Every later start proves the key both ways and derives a fresh session key in
~0.1 s, without any input. If the app isn't open, `--direct` starts it over ADB and then does the same; without
ADB it has to be opened by hand, Android does not let the Mac start it. The app answers at most one pairing
request per 2 s and none during a session. Details in `src/pairing.rs`.

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
