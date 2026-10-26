# Performance

End to end means glass to glass: from a change on the Mac's screen to the same change on the receiver, over Wi-Fi.

| Receiver | End to end |
|---|---|
| Browser on an iPad, one window | ~40 ms |
| Browser on an iPad, whole screen | 50-80 ms |
| TV app on an Android TV, HEVC 60 fps | 70-90 ms |
| Chromecast, TV in Game mode, `--delay 60` | 95-110 ms |
| A TV's built-in Cast app, `--delay 150` | 200-240 ms |
| Chrome's "Cast screen", for comparison | ~300 ms |

The Mac uses 2-10 % of one CPU core; encoding runs on the hardware encoder.

## How it was measured

A lag wheel (a page with a running millisecond clock) on the Mac, the receiver next to it, and a phone photo of
both: the difference between the two clocks is the lag. `--stats` and the menu bar window show the per-stage
times live: round trip, transfer, decoder and queue.

## Findings

- **Cast receivers** honour the playout delay (`--delay`) down to about 60 ms. A TV's built-in Cast app keeps
  its own ~150 ms buffer whatever the sender asks for. Resolution and bitrate barely change the latency.
- **The TV app** gets most of its gain from drawing through the GPU (the TV's video plane adds ~85 ms of
  picture processing) and from HEVC, whose decoder holds back less. What's left is mostly after the decoder:
  Android's compositor and the panel take about four refreshes.
- **Browsers** get the picture over a WebRTC data channel (UDP). Over the WebSocket (TCP), bursts piled up and
  round trips reached 200 ms. Large key frames still cause a short hitch now and then.
- **Sharpness**: the picture is sent at the receiver's exact size and drawn pixel for pixel; even a small stretch
  blurs text. Extend renders a second monitor at that size, so nothing gets scaled.
- **Loss**: with 5 % packet loss the TV app stays at 1080p60, as lost packets are resent. On a link capped at
  3 Mbit/s, Auto settles on 720p30 with the queue under ~40 ms. Encryption costs 24 bytes per packet and no fps.
