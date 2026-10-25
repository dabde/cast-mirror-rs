//! `web`: any browser with WebCodecs as receiver (an iPad, a laptop). The Mac serves the page over HTTPS
//! (self-signed: the browser asks once) and streams H.264 frames over a WebSocket on the same port.
//!
//! The page link carries a random token in its fragment (never sent in an HTTP request); the page sends it
//! as its first WebSocket message. Frames go out as binary messages: flags u8 (bit0 key) | timestamp µs u64 |
//! Annex B. The page sends "k" to ask for a key frame (after a decoder error).
//!
//! TCP doesn't drop, it queues: when the writer falls behind, frames are skipped (the next one is a key frame)
//! and the bitrate comes down, so the picture stays current instead of getting later.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tracing::{info, warn};

use crate::capture::{self, Capture, LatestFrame};
use crate::mirror::{Control, VideoEncoder};
use crate::{Codec, MirrorArgs};

pub const PORT: u16 = 7443;
const PAGE: &str = include_str!("web.html");
const MIN_BITRATE: u32 = 1_000_000;
// Pings carry µs since this instant; the reader turns the echoes into round-trip times.
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn now_us() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

type Socket = TlsStream<TcpStream>;

/// A browser that sent the right token: the stream to write frames to, and what it asks for.
struct Viewer {
    peer: SocketAddr,
    writer: tokio::io::WriteHalf<Socket>,
    requests: mpsc::Receiver<u8>,
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
        match stream(&args, &control, viewer, &mut viewers).await {
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
        let body = PAGE.as_bytes();
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
    let (requests_tx, requests) = mpsc::channel(8);
    tokio::spawn(async move {
        let (mut rtts, mut since) = (Vec::<u64>::new(), Instant::now());
        while let Ok((opcode, payload)) = read_frame(&mut reader).await {
            if since.elapsed() >= Duration::from_secs(1) && !rtts.is_empty() {
                let (min, max) = (rtts.iter().min().unwrap(), rtts.iter().max().unwrap());
                let avg = rtts.iter().sum::<u64>() / rtts.len() as u64;
                tracing::info!("browser {peer}: round trip {:.0}/{:.0}/{:.0} ms (min/avg/max)", *min as f64 / 1e3, avg as f64 / 1e3, *max as f64 / 1e3);
                (rtts, since) = (Vec::new(), Instant::now());
            }
            match opcode {
                // Echo of a ping: "e" + the µs it was sent at.
                1 if payload.first() == Some(&b'e') => {
                    if let Some(sent) = std::str::from_utf8(&payload[1..]).ok().and_then(|s| s.parse::<u64>().ok()) {
                        rtts.push(now_us().saturating_sub(sent));
                    }
                }
                8 => break,
                1 if payload.first() == Some(&b'k') => {
                    let _ = requests_tx.send(b'k').await;
                }
                // The page's own numbers, once a second.
                1 if payload.first() == Some(&b's') => {
                    tracing::info!("browser {peer}: {}", String::from_utf8_lossy(&payload[1..]));
                }
                _ => {}
            }
        }
        let _ = requests_tx.send(b'x').await;
    });
    viewers.send(Viewer { peer, writer, requests }).await?;
    Ok(())
}

/// Streams to one viewer until it leaves, the session stops, or another viewer connects (returned).
async fn stream(
    args: &MirrorArgs,
    control: &Control,
    viewer: Viewer,
    viewers: &mut mpsc::Receiver<Viewer>,
) -> Result<Option<Viewer>> {
    let Viewer { mut writer, mut requests, peer } = viewer;
    // Writing runs apart from encoding: a full queue means the network is behind, so skip instead of queueing.
    let (frames_tx, mut frames_rx) = mpsc::channel::<Vec<u8>>(2);
    let writing = tokio::spawn(async move {
        while let Some(frame) = frames_rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    let fps = args.fps;
    let latest = Arc::new(LatestFrame::default());
    let cap = Capture::start(args.display, args.window.as_deref(), args.width, fps, latest.clone(), None)?;
    let mut bitrate = args.bitrate;
    let mut encoder = VideoEncoder::new(Codec::H264, cap.width, cap.height, fps, bitrate)?;
    let origin = Instant::now();
    let (mut need_key, mut last, mut last_pts) = (true, None::<capture::Frame>, Duration::ZERO);
    let (mut sent, mut dropped, mut bytes, mut largest) = (0u32, 0u32, 0usize, 0usize);
    let mut ping = tokio::time::interval(Duration::from_millis(200));
    let mut calm_since = Instant::now();
    let mut report = tokio::time::interval(Duration::from_secs(1));
    let refresh = Duration::from_secs(1) / fps;
    let mut stop = control.stop.clone();

    let next = loop {
        let frame = tokio::select! {
            _ = stop.wait_for(|s| *s) => break None,
            v = viewers.recv() => break v,
            r = requests.recv() => match r {
                Some(b'k') => {
                    need_key = true;
                    continue;
                }
                _ => {
                    info!("browser {peer} left");
                    break None;
                }
            },
            _ = ping.tick() => {
                // Through the same queue as the frames, so it measures what a frame waits too.
                let _ = frames_tx.try_send(frame_bytes(1, format!("p{}", now_us()).as_bytes()));
                continue;
            }
            _ = report.tick() => {
                let line = format!(
                    "{sent} fps · {:.1} Mbit/s · largest {} KB · target {:.1} · skipped {dropped}",
                    bytes as f64 * 8.0 / 1e6,
                    largest / 1024,
                    bitrate as f64 / 1e6
                );
                largest = 0;
                if args.stats {
                    eprintln!("mac: {line}");
                }
                (control.stats)(line);
                // Raise the rate again after a few calm seconds.
                if dropped == 0 && calm_since.elapsed() > Duration::from_secs(3) && bitrate < args.bitrate {
                    bitrate = (bitrate + args.bitrate / 20).min(args.bitrate);
                    encoder.set_bitrate(bitrate)?;
                }
                (sent, dropped, bytes) = (0, 0, 0);
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
        let force = std::mem::take(&mut need_key);
        let Some(encoded) = tokio::task::block_in_place(|| encoder.encode(frame.image(), pts, force))? else {
            need_key |= force;
            continue;
        };
        last = Some(frame);
        let mut message = Vec::with_capacity(encoded.data.len() + 9);
        message.push(u8::from(encoded.key_frame));
        message.extend_from_slice(&(pts.as_micros() as u64).to_be_bytes());
        message.extend_from_slice(&encoded.data);
        let size = message.len();
        match frames_tx.try_send(frame_bytes(2, &message)) {
            Ok(()) => {
                sent += 1;
                bytes += size;
                largest = largest.max(size);
            }
            // The decoder can't use later delta frames without this one: restart from a key frame, slower.
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped += 1;
                need_key = true;
                calm_since = Instant::now();
                let lower = (bitrate as f64 * 0.8) as u32;
                if lower >= MIN_BITRATE && lower < bitrate {
                    bitrate = lower;
                    encoder.set_bitrate(bitrate)?;
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break None,
        }
    };
    writing.abort();
    drop(cap);
    Ok(next)
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
