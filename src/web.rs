//! `web`: any browser with WebCodecs as receiver (an iPad, a laptop). The Mac serves the page over HTTPS
//! (self-signed: the browser asks once). Video goes over a WebRTC data channel (UDP), the WebSocket on the
//! same port carries the access check, the connection setup and, until the channel is open, the video too.
//!
//! The page link carries a random token in its fragment (never sent in an HTTP request); the page sends it
//! as its first WebSocket message, then a WebRTC offer. Frames: flags u8 (bit0 key) | frame u32 |
//! timestamp µs u64 | piece u16 | pieces u16 | H.264 Annex B (that piece of it).
//! Browsers take data channel messages up to 256 KB, and a sharp key frame is bigger: frames go out in
//! pieces of at most CHUNK bytes, which the page puts back together. The page opens the channel unordered with at most 100 ms of
//! retransmissions, so a lost piece costs a retry instead of holding up later frames (TCP did that: round
//! trips up to 216 ms on Wi-Fi); a frame still missing is given up and the page asks for a key frame ("k").

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use str0m::change::SdpOffer;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tracing::{info, warn};

use crate::capture::{self, Capture, LatestFrame};
use crate::mirror::{Control, VideoEncoder};
use crate::{Codec, MirrorArgs};

pub const PORT: u16 = 7443;
const PAGE: &str = include_str!("web.html");
const MIN_BITRATE: u32 = 1_000_000;
// More than this waiting in the data channel means the link is behind: skip rather than queue. Room for a
// key frame (over 300 KB at 30 Mbit/s) plus a few more, or each key frame would cause the next skip.
const CHANNEL_BACKLOG: usize = 1024 * 1024;
const HEADER: usize = 17;
const CHUNK: usize = 60 * 1024;
// Forced key frames at most this often: each one is large, and a skip right after it would ask again.
const KEY_INTERVAL: Duration = Duration::from_millis(500);
// Pings carry µs since this instant; echoes turn into round-trip times.
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn now_us() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

type Socket = TlsStream<TcpStream>;

/// What the page (over either transport) and the WebRTC task tell the streaming loop.
enum Request {
    KeyFrame,
    Echo(u64),
    Offer(String),
    // The page's visible area changed (fullscreen, rotation): capture to fit the new one.
    Resize(u32, u32),
    ChannelOpen,
    // The data channel couldn't take a frame.
    Skipped,
    Gone,
}

/// A browser that sent the right token.
struct Viewer {
    peer: SocketAddr,
    // Its screen in pixels, landscape: the picture is captured to fit it, so nothing is scaled twice.
    screen: Option<(u32, u32)>,
    writer: tokio::io::WriteHalf<Socket>,
    requests: mpsc::Receiver<Request>,
    requests_tx: mpsc::Sender<Request>,
}

pub async fn run(args: MirrorArgs, control: Control) -> Result<()> {
    capture::ensure_permission()?;
    let (tls, token, ip) = identity()?;
    let url = format!("https://{ip}:{PORT}/#{token}");
    info!("open {url} on the receiving device (accept the certificate warning once)");
    (control.status)(format!("Waiting for a browser: {url}"));

    let listener = TcpListener::bind(("0.0.0.0", PORT)).await.with_context(|| format!("port {PORT}"))?;
    let (viewers_tx, mut viewers) = mpsc::channel::<Viewer>(1);
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let token = Arc::new(token);
    let accepting = tokio::spawn(async move {
        loop {
            let Ok((tcp, peer)) = listener.accept().await else { continue };
            let (acceptor, token, viewers) = (acceptor.clone(), token.clone(), viewers_tx.clone());
            tokio::spawn(async move {
                if let Err(e) = serve(acceptor, tcp, peer, &token, viewers).await {
                    tracing::debug!("{peer}: {e:#}");
                }
            });
        }
    });

    let lan: IpAddr = ip.parse().unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let mut stop = control.stop.clone();
    // One viewer at a time: a browser that connects while another watches takes over.
    let mut next: Option<Viewer> = None;
    loop {
        let viewer = match next.take() {
            Some(v) => v,
            None => tokio::select! {
                _ = stop.wait_for(|s| *s) => break,
                v = viewers.recv() => match v {
                    Some(v) => v,
                    None => break,
                },
            },
        };
        info!("browser {} connected", viewer.peer);
        (control.status)(format!("Casting to the browser on {}", viewer.peer.ip()));
        match stream(&args, &control, viewer, &mut viewers, lan).await {
            Ok(newcomer) => next = newcomer,
            Err(e) => warn!("browser session: {e:#}"),
        }
        if *control.stop.borrow() {
            break;
        }
        if next.is_none() {
            (control.status)(format!("Waiting for a browser: {url}"));
        }
    }
    accepting.abort();
    Ok(())
}

/// One HTTPS connection: the page, or a WebSocket that becomes the viewer once its token checks out.
async fn serve(
    acceptor: tokio_rustls::TlsAcceptor,
    tcp: TcpStream,
    peer: SocketAddr,
    token: &str,
    viewers: mpsc::Sender<Viewer>,
) -> Result<()> {
    let mut tls = acceptor.accept(tcp).await?;
    let request = read_request(&mut tls).await?;
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    if path != "/ws" {
        let page = PAGE.replace("__PAGE_VERSION__", &page_version());
        let body = page.as_bytes();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tls.write_all(head.as_bytes()).await?;
        tls.write_all(body).await?;
        return Ok(());
    }
    let key = header(&request, "sec-websocket-key").context("no WebSocket key")?;
    let accept = {
        use base64::Engine;
        let digest = ring::digest::digest(
            &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
            format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
        );
        base64::engine::general_purpose::STANDARD.encode(digest.as_ref())
    };
    tls.write_all(
        format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .as_bytes(),
    )
    .await?;
    let (mut reader, writer) = tokio::io::split(tls);
    let (opcode, hello) = read_frame(&mut reader).await?;
    let hello: serde_json::Value = serde_json::from_slice(&hello).unwrap_or_default();
    if opcode != 1 || hello["token"].as_str() != Some(token) {
        bail!("wrong token");
    }
    // A page that reconnected on its own after an update still runs the old code: it reloads on a mismatch.
    if hello["version"].as_str() != Some(page_version().as_str()) {
        let mut writer = writer;
        writer.write_all(&frame_bytes(1, format!("v{}", page_version()).as_bytes())).await?;
        bail!("page out of date, told it to reload");
    }
    let pixels = |key: &str| hello[key].as_u64().map(|v| v as u32).filter(|v| (320..=7680).contains(v));
    let screen = pixels("width").zip(pixels("height"));
    let (requests_tx, requests) = mpsc::channel(32);
    let tx = requests_tx.clone();
    tokio::spawn(async move {
        while let Ok((opcode, payload)) = read_frame(&mut reader).await {
            if opcode == 8 {
                break;
            }
            if opcode == 1
                && let Some(request) = parse_text(&payload, peer)
            {
                let _ = tx.send(request).await;
            }
        }
        let _ = tx.send(Request::Gone).await;
    });
    viewers.send(Viewer { peer, screen, writer, requests, requests_tx }).await?;
    Ok(())
}

/// A text message from the page: "k" key frame, "e<µs>" ping echo, "s…" its stats, or {"offer": sdp}.
fn parse_text(payload: &[u8], peer: SocketAddr) -> Option<Request> {
    match payload.first()? {
        b'k' => Some(Request::KeyFrame),
        b'e' => std::str::from_utf8(&payload[1..]).ok()?.parse().ok().map(Request::Echo),
        b's' => {
            info!("browser {peer}: {}", String::from_utf8_lossy(&payload[1..]));
            None
        }
        b'{' => {
            let message: serde_json::Value = serde_json::from_slice(payload).ok()?;
            if let Some(offer) = message["offer"].as_str() {
                return Some(Request::Offer(offer.to_string()));
            }
            let pixels = |key: &str| message[key].as_u64().map(|v| v as u32).filter(|v| (320..=7680).contains(v));
            Some(Request::Resize(pixels("width")?, pixels("height")?))
        }
        _ => None,
    }
}

/// Answers the page's offer and runs its data channel on a UDP socket; returns the answer SDP and a sender
/// for frames. As ICE-lite on the LAN address the Mac only answers the browser's connectivity checks.
async fn webrtc(
    offer: &str,
    lan: IpAddr,
    peer: SocketAddr,
    requests: mpsc::Sender<Request>,
) -> Result<(String, mpsc::Sender<Vec<u8>>)> {
    static CRYPTO: std::sync::Once = std::sync::Once::new();
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
    let socket = UdpSocket::bind((lan, 0)).await?;
    let local = socket.local_addr()?;
    // str0m refuses any message beyond 128 KB of buffer by default, and key frames are bigger: each was
    // skipped, and the page waited for the next key frame. CHANNEL_BACKLOG still caps what may queue.
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_sctp_max_buffered_amount(2 * 1024 * 1024)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    let answer = rtc.sdp_api().accept_offer(SdpOffer::from_sdp_string(offer)?)?;
    // Room while a big key frame goes out (hundreds of UDP packets); CHANNEL_BACKLOG decides about skipping.
    let (frames_tx, mut frames) = mpsc::channel::<Vec<u8>>(8);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2000];
        let mut channel = None;
        loop {
            let timeout = loop {
                match rtc.poll_output() {
                    Ok(Output::Timeout(t)) => break t,
                    Ok(Output::Transmit(t)) => {
                        let _ = socket.send_to(&t.contents, t.destination).await;
                    }
                    Ok(Output::Event(event)) => match event {
                        Event::ChannelOpen(id, _) => {
                            channel = Some(id);
                            let _ = requests.send(Request::ChannelOpen).await;
                        }
                        Event::ChannelData(data) => {
                            if !data.binary
                                && let Some(request) = parse_text(&data.data, peer)
                            {
                                let _ = requests.send(request).await;
                            }
                        }
                        Event::IceConnectionStateChange(IceConnectionState::Disconnected) | Event::ChannelClose(_) => {
                            return;
                        }
                        _ => {}
                    },
                    Err(e) => {
                        warn!("WebRTC: {e}");
                        return;
                    }
                }
            };
            let wait = timeout.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
            let input = tokio::select! {
                r = socket.recv_from(&mut buf) => {
                    let Ok((n, source)) = r else { continue };
                    let Ok(contents) = buf[..n].try_into() else { continue };
                    Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents })
                }
                frame = frames.recv() => {
                    let Some(frame) = frame else { return };
                    let Some(mut ch) = channel.and_then(|id| rtc.channel(id)) else { continue };
                    // Text (pings) is marked by a leading 0xff; video frames start with their flags (0 or 1).
                    if let Some((0xff, text)) = frame.split_first() {
                        let _ = ch.write(false, text);
                        continue;
                    }
                    if ch.buffered_amount() > CHANNEL_BACKLOG {
                        let _ = requests.try_send(Request::Skipped);
                        continue;
                    }
                    let pieces = frame[HEADER..].chunks(CHUNK).collect::<Vec<_>>();
                    for (i, piece) in pieces.iter().enumerate() {
                        let mut message = Vec::with_capacity(HEADER + piece.len());
                        message.extend_from_slice(&frame[..13]);
                        message.extend_from_slice(&(i as u16).to_be_bytes());
                        message.extend_from_slice(&(pieces.len() as u16).to_be_bytes());
                        message.extend_from_slice(piece);
                        if !ch.write(true, &message).unwrap_or(false) {
                            let _ = requests.try_send(Request::Skipped);
                            break;
                        }
                    }
                    continue;
                }
                _ = tokio::time::sleep(wait) => Input::Timeout(Instant::now()),
            };
            if rtc.handle_input(input).is_err() {
                return;
            }
        }
    });
    Ok((answer.to_sdp_string(), frames_tx))
}

/// Streams to one viewer until it leaves, the session stops, or another viewer connects (returned).
async fn stream(
    args: &MirrorArgs,
    control: &Control,
    viewer: Viewer,
    viewers: &mut mpsc::Receiver<Viewer>,
    lan: IpAddr,
) -> Result<Option<Viewer>> {
    let Viewer { mut writer, mut requests, requests_tx, peer, screen } = viewer;
    // The WebSocket writer runs apart from encoding: a full queue means the link is behind, so skip.
    let (ws_tx, mut ws_rx) = mpsc::channel::<Vec<u8>>(2);
    let writing = tokio::spawn(async move {
        while let Some(frame) = ws_rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    // Set once the data channel is open; from then on video goes over UDP.
    let mut channel: Option<mpsc::Sender<Vec<u8>>> = None;
    let mut pending_channel: Option<mpsc::Sender<Vec<u8>>> = None;

    let fps = args.fps;
    let latest = Arc::new(LatestFrame::default());
    let mut bounds = screen.unwrap_or(capture::box_16_9(args.width));
    // A second monitor at the view's exact size; Retina (half the points) for a close-up screen.
    let hidpi = |(width, _): (u32, u32)| width >= 1600;
    let extended = match args.extend {
        true => Some(crate::virtual_display::VirtualDisplay::create("Cast Mirror", bounds.0, bounds.1, hidpi(bounds))?),
        false => None,
    };
    let display = match &extended {
        Some(d) => capture::display_index(d.id)?,
        None => args.display,
    };
    let mut cap = Capture::start(display, args.window.as_deref(), bounds, fps, latest.clone(), None)?;
    info!("browser {peer}: screen {bounds:?}, sending {}x{}", cap.width, cap.height);
    let mut bitrate = args.bitrate;
    let mut encoder = VideoEncoder::new(Codec::H264High, cap.width, cap.height, fps, bitrate)?;
    encoder.set_max_qp(args.max_qp);
    let origin = Instant::now();
    let (mut need_key, mut last, mut last_pts, mut frame_id) = (true, None::<capture::Frame>, Duration::ZERO, 0u32);
    let (mut sent, mut dropped, mut bytes, mut largest) = (0u32, 0u32, 0usize, 0usize);
    let mut rtts: Vec<u64> = Vec::new();
    let mut calm_since = Instant::now();
    let mut last_key = None::<Instant>;
    let mut report = tokio::time::interval(Duration::from_secs(1));
    let mut ping = tokio::time::interval(Duration::from_millis(200));
    let refresh = Duration::from_secs(1) / fps;
    let mut stop = control.stop.clone();

    let next = loop {
        let frame = tokio::select! {
            _ = stop.wait_for(|s| *s) => break None,
            v = viewers.recv() => break v,
            r = requests.recv() => {
                match r {
                    Some(Request::KeyFrame) => need_key = true,
                    Some(Request::Echo(sent_at)) => rtts.push(now_us().saturating_sub(sent_at)),
                    Some(Request::Resize(width, height)) if (width, height) != bounds => {
                        bounds = (width, height);
                        if let Some(d) = &extended {
                            // Same display in a new mode, so the windows on it stay.
                            d.set_mode(width, height, hidpi(bounds))?;
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                        drop(cap);
                        (cap, encoder) = tokio::task::block_in_place(|| -> Result<_> {
                            let cap = Capture::start(display, args.window.as_deref(), bounds, fps, latest.clone(), None)?;
                            let mut encoder = VideoEncoder::new(Codec::H264High, cap.width, cap.height, fps, bitrate)?;
                            encoder.set_max_qp(args.max_qp);
                            Ok((cap, encoder))
                        })?;
                        info!("browser {peer}: view {bounds:?}, sending {}x{}", cap.width, cap.height);
                        // The page starts over from the next key frame, decoder included (new size).
                        let reset = b"r".to_vec();
                        match &channel {
                            Some(c) => {
                                let _ = c.try_send([&[0xff], &reset[..]].concat());
                            }
                            None => {
                                let _ = ws_tx.try_send(frame_bytes(1, &reset));
                            }
                        }
                        last = None;
                        need_key = true;
                        last_key = None;
                    }
                    Some(Request::Resize(..)) => {}
                    Some(Request::Offer(sdp)) => match webrtc(&sdp, lan, peer, requests_tx.clone()).await {
                        Ok((answer, frames)) => {
                            pending_channel = Some(frames);
                            let reply = serde_json::json!({"answer": answer}).to_string();
                            let _ = ws_tx.send(frame_bytes(1, reply.as_bytes())).await;
                        }
                        Err(e) => warn!("WebRTC offer from {peer}: {e:#}"),
                    },
                    Some(Request::ChannelOpen) => {
                        info!("browser {peer}: video over the WebRTC data channel (UDP)");
                        channel = pending_channel.take();
                        need_key = true;
                    }
                    Some(Request::Skipped) => {
                        dropped += 1;
                        need_key = true;
                        calm_since = Instant::now();
                    }
                    Some(Request::Gone) | None => {
                        info!("browser {peer} left");
                        break None;
                    }
                }
                continue;
            }
            _ = ping.tick() => {
                let text = format!("p{}", now_us());
                match &channel {
                    Some(c) => {
                        let _ = c.try_send([&[0xff], text.as_bytes()].concat());
                    }
                    None => {
                        let _ = ws_tx.try_send(frame_bytes(1, text.as_bytes()));
                    }
                }
                continue;
            }
            _ = report.tick() => {
                let rtt = if rtts.is_empty() {
                    String::from("-")
                } else {
                    let avg = rtts.iter().sum::<u64>() / rtts.len() as u64;
                    format!("{}/{}/{} ms", rtts.iter().min().unwrap() / 1000, avg / 1000, rtts.iter().max().unwrap() / 1000)
                };
                let line = format!(
                    "{} · {sent} fps · {:.1} Mbit/s · largest {} KB · target {:.1} · skipped {dropped} · round trip {rtt}",
                    if channel.is_some() { "UDP" } else { "TCP" },
                    bytes as f64 * 8.0 / 1e6,
                    largest / 1024,
                    bitrate as f64 / 1e6
                );
                if args.stats {
                    eprintln!("mac: {line}");
                }
                (control.stats)(line);
                // Lower the rate after skipped frames, raise it again after a few calm seconds.
                // Single skips come with key frames; several a second mean the link can't keep up.
                if dropped >= 3 && bitrate > MIN_BITRATE {
                    bitrate = ((bitrate as f64 * 0.8) as u32).max(MIN_BITRATE);
                    encoder.set_bitrate(bitrate)?;
                } else if dropped == 0 && calm_since.elapsed() > Duration::from_secs(3) && bitrate < args.bitrate {
                    bitrate = (bitrate + args.bitrate / 20).min(args.bitrate);
                    encoder.set_bitrate(bitrate)?;
                }
                (sent, dropped, bytes, largest) = (0, 0, 0, 0);
                rtts.clear();
                continue;
            }
            _ = latest.ready.notified() => match latest.take_sized(cap.width, cap.height) {
                Some(f) => f,
                None => continue,
            },
            _ = tokio::time::sleep(refresh), if last.is_some() => last.clone().unwrap(),
        };
        let pts = frame.captured_at.saturating_duration_since(origin).max(last_pts + Duration::from_millis(1));
        last_pts = pts;
        let force = need_key && last_key.is_none_or(|t: Instant| t.elapsed() >= KEY_INTERVAL);
        need_key &= !force;
        let Some(encoded) = tokio::task::block_in_place(|| encoder.encode(frame.image(), pts, force))? else {
            need_key |= force;
            continue;
        };
        last = Some(frame);
        if encoded.key_frame {
            last_key = Some(Instant::now());
        }
        frame_id = frame_id.wrapping_add(1);
        let mut message = Vec::with_capacity(encoded.data.len() + HEADER);
        message.push(u8::from(encoded.key_frame));
        message.extend_from_slice(&frame_id.to_be_bytes());
        message.extend_from_slice(&(pts.as_micros() as u64).to_be_bytes());
        // One piece: the WebSocket takes any size; the data channel task splits it.
        message.extend_from_slice(&0u16.to_be_bytes());
        message.extend_from_slice(&1u16.to_be_bytes());
        message.extend_from_slice(&encoded.data);
        let size = message.len();
        let queued = match &channel {
            Some(c) => c.try_send(message).map_err(|e| matches!(e, mpsc::error::TrySendError::Closed(_))),
            None => ws_tx.try_send(frame_bytes(2, &message)).map_err(|e| matches!(e, mpsc::error::TrySendError::Closed(_))),
        };
        match queued {
            Ok(()) => {
                sent += 1;
                bytes += size;
                largest = largest.max(size);
            }
            // The data channel closed: back to the WebSocket.
            Err(true) if channel.is_some() => {
                warn!("browser {peer}: data channel closed, back to the WebSocket");
                channel = None;
                need_key = true;
            }
            Err(true) => break None,
            // Behind: the decoder can't use later delta frames without this one, so restart from a key frame.
            Err(false) => {
                dropped += 1;
                need_key = true;
                calm_since = Instant::now();
            }
        }
    };
    writing.abort();
    drop(cap);
    Ok(next)
}

/// Changes whenever the page does (FNV-1a of its source).
fn page_version() -> String {
    let hash = PAGE.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    format!("{hash:016x}")
}

/// The link the receiving device opens (creates the certificate and token on first use).
pub fn link() -> Result<String> {
    let (_, token, ip) = identity()?;
    Ok(format!("https://{ip}:{PORT}/#{token}"))
}

/// The TLS config with a self-signed certificate for this Mac's names, the access token, and the LAN IP.
/// Both are kept in Application Support so the browser's exception and bookmarked links stay valid; the
/// certificate is renewed when the IP changes.
fn identity() -> Result<(Arc<rustls::ServerConfig>, String, String)> {
    let dir = std::env::home_dir().unwrap_or_default().join("Library/Application Support/CastMirror");
    std::fs::create_dir_all(&dir)?;
    let ip = lan_ip().unwrap_or_else(|| "127.0.0.1".into());
    let local = format!("{}.local", local_host_name());
    let names = vec![ip.clone(), local, "localhost".into()];
    let (cert_path, key_path, names_path, token_path) =
        (dir.join("web-cert.der"), dir.join("web-key.der"), dir.join("web-names.txt"), dir.join("web-token.txt"));
    if std::fs::read_to_string(&names_path).ok().as_deref() != Some(names.join(" ").as_str()) {
        let certified = rcgen::generate_simple_self_signed(names.clone())?;
        std::fs::write(&cert_path, certified.cert.der())?;
        write_private(&key_path, &certified.key_pair.serialize_der())?;
        std::fs::write(&names_path, names.join(" "))?;
    }
    let token = match std::fs::read_to_string(&token_path) {
        Ok(t) if t.trim().len() == 32 => t.trim().to_string(),
        _ => {
            let mut raw = [0u8; 16];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut raw)
                .map_err(|_| anyhow::anyhow!("no system randomness"))?;
            let token = crate::pairing::hex(&raw);
            write_private(&token_path, token.as_bytes())?;
            token
        }
    };
    let cert = rustls::pki_types::CertificateDer::from(std::fs::read(&cert_path)?);
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(std::fs::read(&key_path)?.into());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    Ok((Arc::new(config), token, ip))
}

fn write_private(path: &PathBuf, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path)?;
    std::io::Write::write_all(&mut file, data)?;
    Ok(())
}

/// The address other devices on the LAN reach this Mac at (no packet is sent).
fn lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn local_host_name() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "mac".into())
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() > 8192 || stream.read(&mut byte).await? == 0 {
            bail!("bad request");
        }
        buf.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A server-to-browser WebSocket frame (servers don't mask).
fn frame_bytes(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => out.push(n as u8),
        n if n <= 0xffff => {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// Reads one browser-to-server frame and unmasks it; returns (opcode, payload).
async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    let opcode = head[0] & 0x0f;
    let mut len = u64::from(head[1] & 0x7f);
    if len == 126 {
        len = u64::from(stream.read_u16().await?);
    } else if len == 127 {
        len = stream.read_u64().await?;
    }
    if len > 1 << 16 {
        bail!("frame too large");
    }
    let mut mask = [0u8; 4];
    if head[1] & 0x80 != 0 {
        stream.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Ok((opcode, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_through_the_masking_reader() {
        // What a browser sends: masked text "k".
        let mask = [1u8, 2, 3, 4];
        let masked = [0x81, 0x80 | 1, mask[0], mask[1], mask[2], mask[3], b'k' ^ mask[0]];
        let (opcode, payload) = read_frame(&mut &masked[..]).await.unwrap();
        assert_eq!((opcode, payload.as_slice()), (1, &b"k"[..]));
        // What we send: unmasked binary, 16-bit length above 125 bytes.
        let frame = frame_bytes(2, &[7u8; 300]);
        assert_eq!(&frame[..4], &[0x82, 126, 1, 44]);
        assert_eq!(frame.len(), 4 + 300);
    }

    #[test]
    fn headers_match_case_insensitively() {
        let request = "GET /ws HTTP/1.1\r\nHost: x\r\nSec-WebSocket-Key: abc==\r\n\r\n";
        assert_eq!(header(request, "sec-websocket-key"), Some("abc=="));
    }
}
