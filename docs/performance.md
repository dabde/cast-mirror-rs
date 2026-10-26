# Performance

End to end means glass to glass: from a change on the Mac's screen to the same change on the receiver.
All on Wi-Fi, MacBook Pro sending.

| Receiver | End to end | Notes |
|---|---|---|
| TV app, HEVC 60 fps (Philips TPM191E) | **~70-90 ms** | own Android app, encrypted |
| Chromecast dongle, TV in Game mode, `--delay 60` | ~95-110 ms | Cast Streaming |
| Browser, iPad Pro 11" (2018) | 50-80 ms | WebRTC data channel, 57 fps at 60 |
| TV's built-in Cast app, `--delay 150` | ~200-240 ms | keeps its own ~150 ms buffer |
| Chrome "Cast screen" to the built-in app | ~300 ms | the baseline to beat |
| VLC `screen://` → Chromecast | 17-23 s | Default Media Receiver buffering |

CPU on the Mac: 2-10 % of one core (2.4 % mean at 1920x1240@30 with audio); encoding is VideoToolbox hardware.

## Method

A lag wheel (a page with a running millisecond clock) on the Mac, the receiver next to it:

- **Photo Booth**: the Mac's camera films the TV, 6 screenshots per setting (`scripts/lag-sweep.sh`). Photo
  Booth's own ~145 ms is subtracted (calibrated with a phone photo); medians, roughly ±40 ms.
- **Phone photo** of both screens at once: no camera delay to subtract, 3 photos per setting, averaged.
- **Per stage**: `--stats` and the menu bar window show round trip, transfer, decoder and queue times live.

## Cast receivers (2026-10-24, Philips TPM191E)

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

## TV app (direct mode)

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

**Per stage** (`--stats` and the menu bar window show these live): the sender pings the app every 100 ms, and
the app reports how long a frame takes from its first packet to complete (transfer) and through the decoder.
Measured at 60 fps over Wi-Fi: round trip 6-16 ms (so ~3-8 ms one way), transfer 0-7 ms depending on frame
size, decoder 23 ms. The decoder number was one frame interval plus ~6 ms (39.5 ms at 30 fps): the MediaTek
decoder holds each frame until the next one starts. The app now appends an access unit delimiter to every
frame, which marks that boundary at once: decoder 6 ms at 30 and 60 fps, so ~17 ms less end to end at 60 fps
and ~33 ms at 30 fps. The H.264 decoder ignores the delimiter and keeps holding the frame (24 ms at 60 fps).

On the Mac, per new frame at 60 fps: frame age when ScreenCaptureKit hands it over 0.5 ms, HEVC encoding 8.5 ms,
hand-over to the last packet sent 8.7 ms. With the TV side that is ~20-25 ms from capture to decoded frame.

Lag wheel, phone photos of both screens (2026-10-25, 3 each, averaged): TV app (HEVC, 60 fps, GPU plane) ~90 ms,
Chromecast dongle (`--delay 60`) ~110 ms. So ~65 ms of the TV app's path lie after the decoder: TextureView,
Android's compositor and the panel, about four 60 Hz refreshes. The compositor alone takes ~24 ms from a ready
buffer to the screen (`dumpsys SurfaceFlinger --latency`, 1.5 refreshes). Drawing each frame with an own OpenGL
thread onto a SurfaceView (instead of TextureView waiting for the UI thread) stayed on the graphics plane but made
no difference, neither in the compositor (23.8 vs 24.5 ms) nor on the lag wheel, so it was dropped.
Android offers apps on this TV a single 60 Hz mode (`dumpsys display`: no alternative refresh rates, no ALLM,
no minimal post-processing), so ~90 ms is about the floor of the app path here; lower needs a receiver on an
HDMI input in Game mode, e.g. an Android TV box running the same app.

The app runs on Android 9 phones too (`--direct` to the phone's IP, network ADB via `adb tcpip 5555`). On a
Huawei P10 (Kirin 960, `OMX.hisi.video.decoder.hevc`) the decoder takes 33 ms at 60 fps and 51 ms at 30 fps:
one held frame plus ~16 ms of decoding. The access unit delimiter does not help there, nor does HiSilicon's
low-latency scene key (`vendor.hisi-ext-low-latency-video-dec…`); H.264 is worse (52 ms at 60 fps).

MJPEG was checked and dropped: the TV has a hardware decoder for it (`OMX.MTK.VIDEO.DECODER.MJPEG`) and
VideoToolbox encodes JPEG in hardware in ~2 ms, but a 1670x1080 desktop frame is 137 KB at quality 0.5 (67 Mbit/s
at 60 fps) against ~12 KB for HEVC. Frame transfer grows with size (HEVC: ~4.5 ms for 12 KB), so the ~6 ms saved
in encoding would be lost several times over on the air.

## Browser

Measured on an iPad Pro 11" (2018) over Wi-Fi: 50-80 ms glass to glass, 57 fps on average at 60 fps, round trip
4-18 ms. The first version used the WebSocket (TCP) for video: round trips up to 216 ms, and every burst piled up
in Safari's decoder, which releases a frame only when the next one comes in. Two things mattered on the way:
str0m's default 128 KB send buffer rejected every larger frame (key frames reach 230 KB), and at 6 Mbit/s busy
pictures look blocky. At native resolution 20 Mbit/s works (57 fps, round trip 11-18 ms at full rate; the
network carried 30 Mbit/s too); key frames then reach 400 KB and still cause about four short hitches a minute.
The page draws the picture pixel for pixel (a 3 % stretch to the view blurred every edge). Text still looks soft,
also when zooming in: the Mac scales its 3456 pixels down to ~2300, and that detail is gone before encoding
(`--max-qp 24` doesn't change it). Sending more pixels failed: 4112x2658 is beyond H.264 (VideoToolbox refuses),
3564x2304 is beyond the iPad's hardware decoder (Safari fell back to software, under 1 fps), and 3340x2160 came
out of the encoder at 2 fps. Casting a single window (`--window`) at most the iPad's size avoids the downscale.

On an iPhone 17 (Safari, home screen app): 2250x954 inside the safe area, 57 fps. **Extend** at Retina size
(1396x540 points, 2792x1080 pixels): about 45 fps with 0 lost frames; the Mac only sends a frame when the screen
changes, so a static desktop sends fewer.

## Loss and congestion (TV app)

Tested without touching the network:

- `CAST_MIRROR_DROP=0.05` (5 % packet loss): stays at 1080p60, ~30 NACK repairs per second.
- `CAST_MIRROR_CAP_KBPS=3000` (3 Mbit/s cap): the queue stays under ~40 ms, settles on 720p30 at ~1.9 Mbit/s.
- Encryption costs 24 bytes per packet (payload 1376 instead of 1400); fps unchanged in a 2 % loss test.
