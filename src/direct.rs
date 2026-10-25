//! `--direct`: frames over UDP to the cast-mirror Android TV app (android/), no Cast buffering.
//!
//! Packet (v3): "CM" | 3 | flags (bit0 key, bit1 HEVC, bit2 retransmit) | frame u32 | index u16 | count u16 |
//!   seq u32 | send ms u32 | counter u64 | AES-128-GCM(payload) with a 16-byte tag over everything before it.
//! From the app, as counter u64 | AES-128-GCM(message): 'K' key frame request; 'N' frame u32, n u16,
//!   n x index u16 (missing packets; n = 0xffff: the whole frame); 'R' every 100 ms: received u32,
//!   expected u32, minimum one-way delay ms i32, frames given up u32, average first-packet-to-complete µs u32,
//!   average decoder µs u32; 'E' echoes a ping. Ping: "CP" | 3 | 0 | counter | sealed send time µs u64.
//! The nonce is direction (0: to the app, 1: from it) | 0 0 0 | counter. A fresh random key per session
//! reaches the app over ADB, which the TV authenticated when it accepted this Mac.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM};
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::capture::{self, Capture, LatestFrame};
use crate::mirror::{Control, VideoEncoder};
use crate::{Codec, MirrorArgs};

pub const PORT: u16 = 7010;
const HEADER: usize = 20;
const COUNTER: usize = 8;
const TAG: usize = 16;
// With header, counter and tag this stays under a 1500-byte MTU with IP/UDP headers.
const PAYLOAD: usize = 1376;
// Frames kept for retransmission: ~2 s at 60 fps, far beyond the app's 60 ms give-up deadline.
const HISTORY: usize = 120;
const MIN_BITRATE: u32 = 400_000;
const KEY_INTERVAL: Duration = Duration::from_millis(500);
// Wi-Fi round trips: the app echoes each ping at once.
const PING_INTERVAL: Duration = Duration::from_millis(100);
// A ping is "CP" | version | 0, then the send time in µs u64.
const PING: usize = 4;

/// Auto ladder, best first: (max width, fps, bitrate needed to stay on this rung). Sized for HEVC,
/// which direct mode uses; H.264 needs roughly 40 % more for the same picture.
const LADDER: [(u32, u32, u32); 4] = [
    (1920, 60, 3_500_000),
    (1920, 30, 2_000_000),
    (1280, 30, 1_200_000),
    (960, 30, 0),
];

pub async fn run(args: MirrorArgs, control: Control) -> Result<()> {
    capture::ensure_permission()?;
    // A second monitor instead of a mirror: 1920x1080 at 1x, crisp at TV size. It lives as long as this run.
    let mut args = args;
    let _extended = if args.extend {
        let (width, height) = crate::virtual_display::scaled((1920, 1080), args.scale);
        let display = crate::virtual_display::VirtualDisplay::create("Cast Mirror", width, height, false)?;
        args.display = capture::display_index(display.id)?;
        Some(display)
    } else {
        None
    };

    let device = args.device.as_deref().context("--direct needs the TV address")?;
    let addr: SocketAddr = device
        .parse()
        .or_else(|_| format!("{device}:{PORT}").parse())
        .with_context(|| format!("not an address: {device}"))?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(addr).await?;
    // The TV's graphics plane is 1080p: above that, use the (slower) 4K video plane.
    let gpu = args.width <= 1920;
    let host = addr.ip().to_string();
    let key = if args.no_encryption {
        warn!("direct mode UNENCRYPTED: anyone on the network can watch the stream and inject frames");
        (control.status)("Starting the TV app over ADB…".into());
        start_tv_app(&host, gpu, true).await?;
        None
    } else {
        // The key always comes from the pairing handshake: network ADB is unencrypted, so it only starts the app.
        use crate::pairing::{session, NoSession, Store};
        let store = Store::load();
        let pair_hint = format!("pick it in the menu bar window, or run `cast-mirror pair {host}`");
        let key = match session(&socket, &store).await {
            Err(NoSession::NoAnswer) => {
                (control.status)("Starting the TV app over ADB…".into());
                start_tv_app(&host, gpu, false).await.map_err(|adb| {
                    anyhow::anyhow!("the app is not open on the device, and ADB can't start it ({adb:#})")
                })?;
                session(&socket, &store).await
            }
            other => other,
        };
        let key = key.map_err(|e| anyhow::anyhow!("{e}. Pair it once: {pair_hint}"))?;
        info!("session agreed over the network");
        Some(key)
    };
    let mut link = Link::new(socket, key.map(|k| Crypto::new(&k)));

    let codec = args.codecs.first().copied().unwrap_or(Codec::H264);
    let hevc = codec == Codec::Hevc;
    let mut rate = RateControl::new(args.bitrate);
    let mut ladder = Ladder::starting_at(rate.bitrate);
    let (width, fps) = if args.auto { (LADDER[ladder.rung].0, LADDER[ladder.rung].1) } else { (args.width, args.fps) };
    let latest = Arc::new(LatestFrame::default());
    let mut cap = Capture::start(args.display, args.window.as_deref(), capture::box_16_9(width), fps, latest.clone(), None)?;
    let mut encoder = VideoEncoder::new(codec, cap.width, cap.height, fps, rate.bitrate)?;
    encoder.set_max_qp(args.max_qp);
    let describe = |cap: &Capture, fps: u32, bitrate: u32| {
        format!("Direct {codec:?} {}x{}@{fps}, {:.1} Mbit/s", cap.width, cap.height, bitrate as f64 / 1e6)
    };
    info!("{} to {addr}", describe(&cap, fps, rate.bitrate));
    (control.status)(describe(&cap, fps, rate.bitrate));

    let origin = Instant::now();
    let mut history: VecDeque<(u32, Vec<Vec<u8>>)> = VecDeque::with_capacity(HISTORY);
    // A random start lets the app tell a restarted sender from late packets of the old one.
    let start_id = std::time::SystemTime::UNIX_EPOCH.elapsed().map_or(0, |d| d.subsec_nanos());
    let (mut id, mut seq, mut need_key, mut last) = (start_id, 0u32, true, None::<capture::Frame>);
    // On a static screen keep sending the last frame at full rate: unchanged frames cost a few bytes,
    // and the TV's decoder only releases the frames it holds when more come in.
    let mut refresh = Duration::from_secs(1) / fps;
    let mut last_pts = Duration::ZERO;
    let mut last_key = None::<Instant>;
    let mut stats = Stats::default();
    let mut report = tokio::time::interval(Duration::from_secs(1));
    let mut ping = tokio::time::interval(PING_INTERVAL);
    let mut feedback = [0u8; 2048];
    let mut stop = control.stop.clone();
    let stopped = async move {
        let _ = stop.wait_for(|v| *v).await;
    };
    tokio::pin!(stopped);

    loop {
        let frame = tokio::select! {
            _ = &mut stopped => return Ok(()),
            r = link.socket.recv(&mut feedback) => {
                // ECONNREFUSED here just means the app isn't listening yet.
                let Ok(n) = r else { continue };
                // Anything that doesn't decrypt is not from our app (or was altered on the way).
                let msg = match &link.crypto {
                    Some(crypto) => match crypto.open(&mut feedback[..n], 0, FROM_APP) {
                        Some(msg) => msg,
                        None => continue,
                    },
                    None => &feedback[..n],
                };
                let n = msg.len();
                match *msg {
                    // The app repeats 'K' every 200 ms until a key frame lands; one per 500 ms is enough,
                    // or a slow link ends up sending nothing but key frames.
                    [b'K', ..] => need_key |= last_key.is_none_or(|t| t.elapsed() >= KEY_INTERVAL),
                    [b'N', ..] if n >= 7 => {
                        let frame_id = u32::from_be_bytes(msg[1..5].try_into().unwrap());
                        let whole = msg[5..7] == [0xff, 0xff];
                        if let Some((_, packets)) = history.iter().find(|(f, _)| *f == frame_id) {
                            let indices: Vec<usize> = if whole {
                                (0..packets.len()).collect()
                            } else {
                                msg[7..n].as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c) as usize).collect()
                            };
                            for i in indices.into_iter().filter(|&i| i < packets.len()) {
                                let mut p = packets[i].clone();
                                p[3] |= 4;
                                link.send(&p).await;
                                stats.retransmits += 1;
                            }
                        }
                        stats.nacks += 1;
                    }
                    [b'E', ..] if n >= 9 => {
                        let sent = u64::from_be_bytes(msg[1..9].try_into().unwrap());
                        stats.rtt_us.push((origin.elapsed().as_micros() as u64).saturating_sub(sent));
                    }
                    [b'R', ..] if n >= 17 => {
                        let field = |o: usize| u32::from_be_bytes(msg[o..o + 4].try_into().unwrap());
                        let fb = Report { received: field(1), expected: field(5), owd_ms: field(9) as i32, given_up: field(13) };
                        // Older app versions send 17 bytes; 0 means no frame in that report.
                        if n >= 25 {
                            stats.transfer_us.extend([field(17)].into_iter().filter(|&us| us > 0).map(u64::from));
                            stats.decode_us.extend([field(21)].into_iter().filter(|&us| us > 0).map(u64::from));
                        }
                        stats.loss = fb.loss();
                        let now = Instant::now();
                        let changed = rate.on_report(now, &fb).is_some();
                        stats.queue_ms = rate.queue_ms;
                        // Every report, not only on a change: pinned at MIN_BITRATE the rate stops changing.
                        // An empty report says nothing about the link, so it must not count toward a step up.
                        let empty = fb.received == 0 && fb.expected == 0;
                        let switched = args.auto && !empty && ladder.update(rate.bitrate, now);
                        if switched {
                            let (w, f, _) = LADDER[ladder.rung];
                            drop(cap);
                            (cap, encoder) = tokio::task::block_in_place(|| -> Result<_> {
                                let cap = Capture::start(args.display, args.window.as_deref(), capture::box_16_9(w), f, latest.clone(), None)?;
                                let encoder = VideoEncoder::new(codec, cap.width, cap.height, f, rate.bitrate)?;
                                Ok((cap, encoder))
                            })?;
                            encoder.set_max_qp(args.max_qp);
                            refresh = Duration::from_secs(1) / f;
                            last = None;
                            // A NACK for a frame of the old size would feed it to the new decoder.
                            history.clear();
                            need_key = true;
                            info!("auto: {}", describe(&cap, f, rate.bitrate));
                        } else if changed {
                            encoder.set_bitrate(rate.bitrate)?;
                        }
                        if changed || switched {
                            let f = if args.auto { LADDER[ladder.rung].1 } else { fps };
                            (control.status)(describe(&cap, f, rate.bitrate));
                        }
                    }
                    _ => {}
                }
                continue;
            }
            _ = ping.tick() => {
                let mut p = vec![b'C', b'P', 3, 0];
                p.extend_from_slice(&(origin.elapsed().as_micros() as u64).to_be_bytes());
                link.send_with(&p, PING).await;
                continue;
            }
            _ = report.tick() => {
                if args.stats {
                    stats.print(rate.bitrate, args.auto.then_some(ladder.rung));
                }
                (control.stats)(stats.summary());
                stats = Stats { queue_ms: rate.queue_ms, loss: stats.loss, ..Stats::default() };
                continue;
            }
            _ = latest.ready.notified() => match latest.take_sized(cap.width, cap.height) {
                Some(f) => (f, true),
                None => continue,
            },
            _ = tokio::time::sleep(refresh), if last.is_some() => (last.clone().unwrap(), false),
        };
        // Refreshed frames (static screen) are not new: they don't count for the Mac-side timings.
        let (frame, fresh) = frame;
        let handed_over = frame.captured_at;
        // Refreshed frames reuse the capture time; VideoToolbox needs increasing timestamps.
        let pts = frame.captured_at.saturating_duration_since(origin).max(last_pts + Duration::from_millis(1));
        last_pts = pts;
        let force = std::mem::take(&mut need_key);
        let encode_start = Instant::now();
        let encoded = tokio::task::block_in_place(|| encoder.encode(frame.image(), pts, force))?;
        if fresh {
            stats.encode_us.push(encode_start.elapsed().as_micros() as u64);
            stats.capture_us.extend(frame.capture_age.map(|a| a.as_micros() as u64));
        }
        let Some(encoded) = encoded else {
            need_key |= force;
            continue;
        };
        last = Some(frame);
        id = id.wrapping_add(1);
        let flags = u8::from(encoded.key_frame) | (u8::from(hevc) << 1);
        let sent_ms = origin.elapsed().as_millis() as u32;
        let packets = packetize(id, flags, &mut seq, sent_ms, &encoded.data);
        for p in &packets {
            link.send(p).await;
        }
        if fresh {
            stats.mac_us.push(handed_over.elapsed().as_micros() as u64);
        }
        if history.len() == HISTORY {
            history.pop_front();
        }
        history.push_back((id, packets));
        stats.frames += 1;
        stats.bytes += encoded.data.len();
        stats.keys += u32::from(encoded.key_frame);
        if encoded.key_frame {
            last_key = Some(Instant::now());
        }
    }
}

fn packetize(id: u32, flags: u8, seq: &mut u32, sent_ms: u32, data: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = data.chunks(PAYLOAD).collect();
    let count = chunks.len() as u16;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            *seq = seq.wrapping_add(1);
            let mut p = Vec::with_capacity(HEADER + chunk.len());
            p.extend_from_slice(&[b'C', b'M', 3, flags]);
            p.extend_from_slice(&id.to_be_bytes());
            p.extend_from_slice(&(i as u16).to_be_bytes());
            p.extend_from_slice(&count.to_be_bytes());
            p.extend_from_slice(&seq.to_be_bytes());
            p.extend_from_slice(&sent_ms.to_be_bytes());
            p.extend_from_slice(chunk);
            p
        })
        .collect()
}

/// The UDP socket plus two test hooks that simulate a bad network without admin rights:
/// CAST_MIRROR_DROP=0.05 drops 5 % of packets; CAST_MIRROR_CAP_KBPS=3000 drains a queue at 3 Mbit/s, like a
/// router in front of a slow link, while capture and encoding go on.
struct Link {
    socket: Arc<UdpSocket>,
    // None with --no-encryption.
    crypto: Option<Crypto>,
    drop: f64,
    paced: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    rng: u64,
}

impl Link {
    fn new(socket: UdpSocket, crypto: Option<Crypto>) -> Self {
        let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let socket = Arc::new(socket);
        let cap_bps = env("CAST_MIRROR_CAP_KBPS") * 1000.0;
        let paced = (cap_bps > 0.0).then(|| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let out = socket.clone();
            tokio::spawn(async move {
                let mut free_at = Instant::now();
                while let Some(p) = rx.recv().await {
                    free_at = free_at.max(Instant::now()) + Duration::from_secs_f64(p.len() as f64 * 8.0 / cap_bps);
                    tokio::time::sleep_until(free_at.into()).await;
                    let _ = out.send(&p).await;
                }
            });
            tx
        });
        Self { socket, crypto, drop: env("CAST_MIRROR_DROP"), paced, rng: 0x9e37_79b9_7f4a_7c15 }
    }

    async fn send(&mut self, packet: &[u8]) {
        self.send_with(packet, HEADER).await;
    }

    /// Encrypts on every send, resends included: their flags differ, so they need a fresh nonce.
    /// `clear`: the readable prefix (HEADER for video, PING for pings).
    async fn send_with(&mut self, packet: &[u8], clear: usize) {
        let packet = &wire(self.crypto.as_mut(), packet, clear)[..];
        if self.drop > 0.0 {
            // xorshift: cheap and good enough for a loss simulation.
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            if (self.rng % 10_000) as f64 / 10_000.0 < self.drop {
                return;
            }
        }
        match &self.paced {
            Some(queue) => {
                let _ = queue.send(packet.to_vec());
            }
            None => {
                let _ = self.socket.send(packet).await;
            }
        }
    }
}

/// What goes on the wire: sealed (v3), or with `--no-encryption` the packet as it is, marked v2.
fn wire(crypto: Option<&mut Crypto>, packet: &[u8], clear: usize) -> Vec<u8> {
    match crypto {
        Some(crypto) => crypto.seal(packet, clear, TO_APP),
        None => {
            let mut plain = packet.to_vec();
            plain[2] = 2;
            plain
        }
    }
}

const TO_APP: u8 = 0;
const FROM_APP: u8 = 1;

/// AES-128-GCM in both directions; a counter per sent message makes each nonce unique.
// ponytail: no replay window; a replayed video packet is a duplicate the app drops anyway, a replayed
// NACK costs one resend. Add a window if feedback ever carries something that must not repeat.
struct Crypto {
    key: LessSafeKey,
    sent: u64,
}

impl Crypto {
    fn new(key: &[u8; 16]) -> Self {
        Self { key: LessSafeKey::new(UnboundKey::new(&AES_128_GCM, key).expect("16-byte key")), sent: 0 }
    }

    fn nonce(direction: u8, counter: u64) -> Nonce {
        let mut n = [0u8; 12];
        n[0] = direction;
        n[4..].copy_from_slice(&counter.to_be_bytes());
        Nonce::assume_unique_for_key(n)
    }

    /// `message[..clear]` stays readable, then comes the counter; both are authenticated.
    fn seal(&mut self, message: &[u8], clear: usize, direction: u8) -> Vec<u8> {
        let counter = self.sent;
        self.sent += 1;
        let mut out = Vec::with_capacity(message.len() + COUNTER + TAG);
        out.extend_from_slice(&message[..clear]);
        out.extend_from_slice(&counter.to_be_bytes());
        let mut body = message[clear..].to_vec();
        self.key
            .seal_in_place_append_tag(Self::nonce(direction, counter), Aad::from(&out[..]), &mut body)
            .expect("AES-GCM seal");
        out.extend_from_slice(&body);
        out
    }

    /// The decrypted message, or `None` if it doesn't authenticate.
    fn open<'a>(&self, message: &'a mut [u8], clear: usize, direction: u8) -> Option<&'a [u8]> {
        if message.len() < clear + COUNTER + TAG {
            return None;
        }
        let (aad, body) = message.split_at_mut(clear + COUNTER);
        let counter = u64::from_be_bytes(aad[clear..].try_into().unwrap());
        self.key.open_in_place(Self::nonce(direction, counter), Aad::from(&*aad), body).ok().map(|p| &*p)
    }
}

/// (Re)starts the receiver app (android/) over ADB; network debugging must be enabled on the TV.
/// `gpu`: draw on the graphics plane (fast, 1080p max) instead of the video plane (4K, ~85 ms slower).
/// `plain`: unencrypted (`--no-encryption`); otherwise the app waits for a paired Mac's session.
pub async fn start_tv_app(host: &str, gpu: bool, plain: bool) -> Result<()> {
    let serial = format!("{host}:5555");
    let ten = Duration::from_secs(10);
    adb(&["connect", &serial], ten).await.context("running adb (brew install --cask android-platform-tools)")?;
    // -S restarts the activity, so a changed plane choice takes effect.
    let texture = gpu.to_string();
    let activity = format!("{TV_APP}/.MainActivity");
    let plain = plain.to_string();
    let args = ["-s", &serial, "shell", "am", "start", "-S", "-n", &activity, "--ez", "texture", &texture, "--ez", "plain", &plain];
    let out = adb(&args, ten).await?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || text.contains("Error") {
        bail!("TV app not started on {host}: {}", text.trim());
    }
    // Give the app a moment to open its socket.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    Ok(())
}

pub const TV_APP: &str = "de.tisth.castmirror";

/// Runs adb, which can hang on an unreachable TV, for at most `limit`.
pub async fn adb(args: &[&str], limit: Duration) -> Result<std::process::Output> {
    // Finder-launched apps get a minimal PATH, so look where Homebrew puts adb too.
    let path = ["/opt/homebrew/bin/adb", "/usr/local/bin/adb"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("adb");
    let out = tokio::process::Command::new(path).args(args).kill_on_drop(true).output();
    Ok(tokio::time::timeout(limit, out).await.context("adb timed out")??)
}

struct Report {
    received: u32,
    expected: u32,
    owd_ms: i32,
    given_up: u32,
}

impl Report {
    fn loss(&self) -> f64 {
        if self.expected == 0 { 0.0 } else { (1.0 - self.received as f64 / self.expected as f64).max(0.0) }
    }
}

/// Delay-gradient + loss controller, a much simplified cousin of WebRTC's GCC.
struct RateControl {
    max: u32,
    bitrate: u32,
    // One-way delay samples of the last 10 s; their minimum is the empty-queue baseline.
    owd: VecDeque<i32>,
    // (received, expected) of the last 10 reports: one 100 ms report holds too few packets to judge loss.
    counts: VecDeque<(u32, u32)>,
    queue_ms: i32,
    last_cut: Option<Instant>,
    calm_since: Option<Instant>,
}

impl RateControl {
    fn new(max: u32) -> Self {
        Self {
            max,
            bitrate: max,
            owd: VecDeque::new(),
            counts: VecDeque::new(),
            queue_ms: 0,
            last_cut: None,
            calm_since: None,
        }
    }

    /// Returns the new bitrate when it changed.
    fn on_report(&mut self, now: Instant, r: &Report) -> Option<u32> {
        if r.received == 0 && r.expected == 0 {
            return None;
        }
        if r.received > 0 {
            if self.owd.len() == 100 {
                self.owd.pop_front();
            }
            self.owd.push_back(r.owd_ms);
            self.queue_ms = r.owd_ms - self.owd.iter().min().unwrap();
        }
        if self.counts.len() == 10 {
            self.counts.pop_front();
        }
        self.counts.push_back((r.received, r.expected));
        let (got, want) = self.counts.iter().fold((0, 0), |(g, w), (r, e)| (g + r, w + e));
        let loss = Report { received: got, expected: want, owd_ms: 0, given_up: 0 }.loss();
        // Like GCC: NACK repairs random loss, so only heavy loss or a growing queue means congestion.
        let factor = if loss > 0.10 {
            Some(0.7)
        } else if self.queue_ms > 30 || r.given_up > 0 {
            Some(0.85)
        } else {
            None
        };
        // What actually got through in the last (up to) 0.5 s, at 100 ms per report; most packets are full.
        let recent = self.counts.len().min(5) as u64;
        let delivered_bps = self.counts.iter().rev().take(5).map(|(r, _)| *r as u64).sum::<u64>()
            * PAYLOAD as u64 * 8 * 10 / recent;
        let old = self.bitrate;
        match factor {
            // At most one cut per 300 ms: the reports right after a cut still describe the old rate.
            Some(f) if self.last_cut.is_none_or(|t| now - t >= Duration::from_millis(300)) => {
                // A growing queue means we send more than the link carries: drop below what it delivered
                // (as GCC does), not just by a fixed step, or the queue keeps growing for seconds.
                let cut = (self.bitrate as f64 * f) as u32;
                let cut = if self.queue_ms > 30 && delivered_bps > 0 { cut.min((delivered_bps as f64 * 0.85) as u32) } else { cut };
                self.bitrate = cut.max(MIN_BITRATE);
                self.last_cut = Some(now);
                self.calm_since = None;
            }
            Some(_) => self.calm_since = None,
            None if self.queue_ms < 10 && loss < 0.02 => {
                let calm = *self.calm_since.get_or_insert(now);
                if now - calm >= Duration::from_secs(1) {
                    self.bitrate = (self.bitrate + self.max / 50).min(self.max);
                }
            }
            None => {}
        }
        (self.bitrate != old).then_some(self.bitrate)
    }
}

/// Position on LADDER: down as soon as the bitrate no longer carries a rung, up after a hold time
/// with 20 % headroom.
struct Ladder {
    rung: usize,
    ok_since: Option<Instant>,
    hold: Duration,
    last_up: Option<Instant>,
}

impl Ladder {
    /// The best rung this bitrate carries.
    fn starting_at(bitrate: u32) -> Self {
        let rung = LADDER.iter().position(|r| bitrate >= r.2).unwrap_or(LADDER.len() - 1);
        Self { rung, ok_since: None, hold: Duration::from_secs(5), last_up: None }
    }

    /// Returns whether the rung changed.
    fn update(&mut self, bitrate: u32, now: Instant) -> bool {
        if bitrate < LADDER[self.rung].2 && self.rung < LADDER.len() - 1 {
            // A static screen leaves the link idle, so the bitrate climbs and the ladder steps up until
            // the next key frame burst proves otherwise; back off instead of flipping every few seconds.
            // ponytail: the hold never shrinks again, add a decay if long sessions need it.
            if self.last_up.take().is_some_and(|t| now - t < Duration::from_secs(10)) {
                self.hold = (self.hold * 2).min(Duration::from_secs(60));
            }
            self.rung += 1;
            self.ok_since = None;
            return true;
        }
        if self.rung > 0 && bitrate as f64 >= LADDER[self.rung - 1].2 as f64 * 1.2 {
            if now - *self.ok_since.get_or_insert(now) >= self.hold {
                self.rung -= 1;
                self.ok_since = None;
                self.last_up = Some(now);
                return true;
            }
        } else {
            self.ok_since = None;
        }
        false
    }
}

#[derive(Default)]
struct Stats {
    frames: u32,
    bytes: usize,
    keys: u32,
    nacks: u32,
    retransmits: u32,
    loss: f64,
    queue_ms: i32,
    // Measured this second: ping round trips, and the app's first-packet-to-complete and decoder times.
    rtt_us: Vec<u64>,
    transfer_us: Vec<u64>,
    decode_us: Vec<u64>,
    // Mac side, new frames only: frame age at hand-over from ScreenCaptureKit, encoding, and hand-over
    // until the last packet is sent (waiting for the encoder included).
    capture_us: Vec<u64>,
    encode_us: Vec<u64>,
    mac_us: Vec<u64>,
}

impl Stats {
    fn ms(samples: &[u64]) -> Option<f64> {
        (!samples.is_empty()).then(|| samples.iter().sum::<u64>() as f64 / samples.len() as f64 / 1000.0)
    }

    /// One line for the menu bar window, e.g. "Wi-Fi RTT 4 ms (3–9) · transfer 6 ms · decoder 11 ms · loss 0.3 %".
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let (Some(c), Some(e), Some(m)) = (Self::ms(&self.capture_us), Self::ms(&self.encode_us), Self::ms(&self.mac_us)) {
            parts.push(format!("Mac: capture {c:.0} · encode {e:.0} · out {m:.0} ms"));
        }
        if let Some(avg) = Self::ms(&self.rtt_us) {
            let min = *self.rtt_us.iter().min().unwrap() as f64 / 1000.0;
            let max = *self.rtt_us.iter().max().unwrap() as f64 / 1000.0;
            parts.push(format!("Wi-Fi RTT {avg:.0} ms ({min:.0}–{max:.0})"));
        }
        if let Some(t) = Self::ms(&self.transfer_us) {
            parts.push(format!("transfer {t:.0} ms"));
        }
        if let Some(d) = Self::ms(&self.decode_us) {
            parts.push(format!("decoder {d:.0} ms"));
        }
        parts.push(format!("loss {:.1} %", self.loss * 100.0));
        parts.join(" · ")
    }

    fn print(&self, target: u32, rung: Option<usize>) {
        let rung = rung.map_or(String::new(), |r| format!("  rung {r}"));
        let ms = |s: &[u64]| Self::ms(s).map_or("-".into(), |v| format!("{v:.1}"));
        let timing = format!(
            "  cap {} enc {} out {} ms  rtt {} ms  xfer {} ms  dec {} ms",
            ms(&self.capture_us),
            ms(&self.encode_us),
            ms(&self.mac_us),
            ms(&self.rtt_us),
            ms(&self.transfer_us),
            ms(&self.decode_us)
        );
        eprintln!(
            "fps {:>3}  sent {:>6} kbit/s  target {:>5} kbit/s  loss {:>4.1}%  queue {:>3} ms  nacks {:>3}  retx {:>4}  keys {}{timing}{rung}",
            self.frames,
            self.bytes * 8 / 1000,
            target / 1000,
            self.loss * 100.0,
            self.queue_ms,
            self.nacks,
            self.retransmits,
            self.keys,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_carry_v3_header_and_split_payload() {
        let data = vec![7u8; PAYLOAD + 10];
        let mut seq = 41;
        let packets = packetize(0x01020304, 3, &mut seq, 0x0a0b0c0d, &data);
        assert_eq!(packets.len(), 2);
        assert_eq!(&packets[0][..HEADER], &[b'C', b'M', 3, 3, 1, 2, 3, 4, 0, 0, 0, 2, 0, 0, 0, 42, 10, 11, 12, 13]);
        assert_eq!(&packets[1][8..16], &[0, 1, 0, 2, 0, 0, 0, 43]);
        assert_eq!(packets[0].len() + packets[1].len() - 2 * HEADER, data.len());
    }

    #[test]
    fn unencrypted_packets_go_out_unchanged_as_v2() {
        let packet = packetize(1, 0, &mut 0, 0, &[5u8; 10]).remove(0);
        let plain = wire(None, &packet, HEADER);
        assert_eq!(plain[2], 2);
        assert_eq!(plain[3..], packet[3..]);
        assert_eq!(wire(Some(&mut Crypto::new(&[1; 16])), &packet, HEADER)[2], 3);
    }

    #[test]
    fn summary_reports_round_trip_spread_and_app_timings() {
        let stats = Stats { rtt_us: vec![3_000, 4_000, 9_000], transfer_us: vec![6_000], decode_us: vec![11_000], loss: 0.003, ..Stats::default() };
        assert_eq!(stats.summary(), "Wi-Fi RTT 5 ms (3–9) · transfer 6 ms · decoder 11 ms · loss 0.3 %");
        assert_eq!(Stats::default().summary(), "loss 0.0 %");
    }

    #[test]
    fn sealed_packets_fit_the_mtu_open_once_and_reject_tampering() {
        let key = [9u8; 16];
        let (mut sender, app) = (Crypto::new(&key), Crypto::new(&key));
        let packet = packetize(1, 0, &mut 0, 0, &vec![5u8; PAYLOAD]).remove(0);
        let mut sealed = sender.seal(&packet, HEADER, TO_APP);
        assert!(sealed.len() + 28 <= 1500, "{} bytes on the wire", sealed.len() + 28);
        assert_eq!(&sealed[..HEADER], &packet[..HEADER], "header stays readable");
        assert_eq!(app.open(&mut sealed.clone(), HEADER, TO_APP).unwrap(), &packet[HEADER..]);
        assert!(app.open(&mut sealed.clone(), HEADER, FROM_APP).is_none(), "direction is part of the nonce");
        sealed[3] |= 4; // A forged retransmit flag in the readable header.
        assert!(app.open(&mut sealed, HEADER, TO_APP).is_none());
        let resend = sender.seal(&packet, HEADER, TO_APP);
        assert_ne!(resend[HEADER..HEADER + COUNTER], sealed[HEADER..HEADER + COUNTER], "fresh counter per send");
    }

    fn report(received: u32, expected: u32, owd_ms: i32) -> Report {
        Report { received, expected, owd_ms, given_up: 0 }
    }

    #[test]
    fn rate_cuts_on_queue_and_sustained_loss_then_recovers() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut rc = RateControl::new(6_000_000);
        assert_eq!(rc.on_report(at(0), &report(100, 100, 50)), None, "baseline, already at max");
        assert_eq!(rc.on_report(at(100), &report(100, 100, 100)), Some(5_100_000), "50 ms queue, link delivers plenty");
        assert_eq!(rc.on_report(at(200), &report(100, 100, 100)), None, "no second cut within 300 ms");
        assert_eq!(rc.on_report(at(500), &report(80, 100, 50)), None, "one lossy report in a clean second: hold");
        assert_eq!(rc.on_report(at(600), &report(50, 100, 50)), Some(3_570_000), "sustained loss > 10 %");
        for ms in (700..1700).step_by(100) {
            rc.on_report(at(ms), &report(100, 100, 50));
        }
        assert_eq!(rc.bitrate, 3_570_000, "loss still in the 1 s window, then a calm second");
        assert_eq!(rc.on_report(at(2800), &report(100, 100, 50)), Some(3_690_000), "+2 % of max");
    }

    #[test]
    fn queue_cut_drops_to_delivered_rate() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(6_000_000);
        // 20 packets per 100 ms get through: 2.2 Mbit/s on a link we feed with 6 Mbit/s.
        rc.on_report(t0, &report(20, 20, 50));
        let cut = rc.on_report(t0 + Duration::from_millis(100), &report(20, 20, 300));
        assert_eq!(cut, Some(1_871_360), "85 % of the delivered 2.2 Mbit/s, not just -15 %");
    }

    #[test]
    fn ladder_steps_down_at_once_and_up_after_five_seconds() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut l = Ladder::starting_at(6_000_000);
        assert!(l.update(3_000_000, t0) && l.rung == 1);
        assert!(!l.update(5_500_000, t0), "headroom, but not for 5 s yet");
        assert!(l.update(5_500_000, at(5)) && l.rung == 0);
        for _ in 0..5 {
            l.update(100_000, at(6));
        }
        assert_eq!(l.rung, 3, "one rung per report, bottom rung stays");
        assert_eq!(Ladder::starting_at(1_000_000).rung, 3, "--bitrate 1M starts low, no restarts");
    }

    #[test]
    fn ladder_waits_longer_after_a_failed_step_up() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut l = Ladder::starting_at(2_500_000);
        l.update(5_500_000, t0);
        assert!(l.update(5_500_000, at(5)) && l.rung == 0);
        assert!(l.update(3_000_000, at(8)) && l.rung == 1, "step up failed within 10 s");
        l.update(1_000_000, at(8));
        assert_eq!(l.hold, Duration::from_secs(10), "walking further down doubles only once");
        l.update(5_500_000, at(9));
        assert!(!l.update(5_500_000, at(14)), "now holds 10 s");
        assert!(l.update(5_500_000, at(19)) && l.rung == 1);
    }
}
