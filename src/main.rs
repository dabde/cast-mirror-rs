mod audio;
mod capture;
mod direct;
mod mirror;
mod pairing;
mod tray;
mod video;
#[cfg(feature = "vp8")]
mod vp8;

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chromiacast::{discover, AppAvailability, CastConnection, APP_MIRRORING, CAST_PORT};
use clap::{Parser, Subcommand, ValueEnum};

/// Low-latency macOS → Cast receiver mirroring (Cast Streaming, VideoToolbox H.264).
#[derive(Parser, Debug)]
#[command(version, args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    pub mirror: MirrorArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List Cast receivers on the local network.
    Scan {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Launch the mirroring receiver app and exit (leaves it running).
    Launch { device: String },
    /// Menu-bar app with receiver and profile selection.
    Tray,
    /// Pair with the cast-mirror app on a device (its IP), so --direct works without ADB.
    Pair { device: String },
    /// Stop a running mirroring app (e.g. after the sender was killed).
    Stop { device: String },
    /// Stream a solid colour that changes every second (no screen capture).
    Pattern {
        device: String,
        #[command(flatten)]
        args: MirrorArgs,
    },
}

#[derive(clap::Args, Debug, Clone)]
pub struct MirrorArgs {
    /// Name (substring), id or IP address; default: first receiver found.
    pub device: Option<String>,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    pub fps: u32,
    /// Start/max video bitrate, e.g. 6M or 4500k.
    #[arg(long, default_value = "6M", value_parser = parse_bitrate)]
    pub bitrate: u32,
    /// Max output width; the picture is fitted into width x (width*9/16) at the display's aspect.
    #[arg(long, default_value_t = 1920)]
    pub width: u32,
    /// Target playout delay in ms (Chrome offers 200; Chromecast honours less, Philips' built-in app floors ~150).
    #[arg(long, default_value_t = 100)]
    pub delay: u64,
    #[arg(long)]
    pub no_audio: bool,
    /// Display index as listed by ScreenCaptureKit.
    #[arg(long, default_value_t = 0)]
    pub display: usize,
    /// Capture only this window (id, or text in "app: title"; the largest match wins) instead of the display.
    #[arg(long)]
    pub window: Option<String>,
    /// Print live fps, bitrate, RTT, retransmits and dropped frames.
    #[arg(long)]
    pub stats: bool,
    /// Video codecs to offer, most preferred first; the receiver picks one.
    #[arg(long, value_delimiter = ',', default_value = "h264")]
    pub codecs: Vec<Codec>,
    /// Use spec payload types (H.264 101, Opus 96) instead of Chrome's 96/127.
    #[arg(long)]
    pub spec_payload_types: bool,
    /// Stream to the cast-mirror Android TV app (android/) at DEVICE's IP instead of Cast.
    #[arg(long)]
    pub direct: bool,
    /// Direct mode: send unencrypted. Anyone on the network can then watch the stream and inject frames.
    #[arg(long)]
    pub no_encryption: bool,
    /// Direct mode: step resolution and frame rate down (and back up) with the Wi-Fi quality.
    #[arg(long)]
    pub auto: bool,
    /// Cap the encoder quantizer (0-51, lower = sharper; ~24 keeps text crisp). Raises bitrate as needed.
    #[arg(long)]
    pub max_qp: Option<u32>,
    /// Skip Cast device authentication (openscreen's dev cast_receiver only).
    #[cfg(feature = "test-receiver")]
    #[arg(long, hide = true)]
    pub unverified: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// H.264 Main (VideoToolbox)
    H264,
    /// H.264 High (VideoToolbox)
    H264High,
    /// HEVC Main (VideoToolbox)
    Hevc,
    /// VP8 (libvpx, software; build with --features vp8)
    Vp8,
}

fn parse_bitrate(s: &str) -> Result<u32, String> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = match s.strip_suffix('m') {
        Some(n) => (n, 1_000_000.0),
        None => s.strip_suffix('k').map_or((s.as_str(), 1.0), |n| (n, 1_000.0)),
    };
    num.parse::<f64>().map(|n| (n * mult) as u32).map_err(|e| format!("bad bitrate {s:?}: {e}"))
}

/// Resolves a device argument to the receiver's control endpoint.
pub async fn resolve(device: Option<&str>) -> Result<SocketAddr> {
    if let Some(addr) = device.and_then(|d| d.parse::<SocketAddr>().ok()) {
        return Ok(addr);
    }
    if let Some(ip) = device.and_then(|d| d.parse::<IpAddr>().ok()) {
        return Ok(SocketAddr::new(ip, CAST_PORT));
    }
    let found = discover(Duration::from_secs(3)).await.context("mDNS discovery")?;
    let pick = match device {
        None => found.first(),
        Some(want) => {
            let want = want.to_lowercase();
            found
                .iter()
                .find(|d| d.id().eq_ignore_ascii_case(&want) || d.name().to_lowercase().contains(&want))
        }
    };
    match pick {
        Some(d) => Ok(d.preferred_endpoint().address()),
        None => bail!("no Cast receiver matching {device:?} (found {})", found.len()),
    }
}

pub async fn connect_and_launch(addr: SocketAddr, unverified: bool) -> Result<(CastConnection, chromiacast::CastApp)> {
    let connection = match unverified {
        #[cfg(feature = "test-receiver")]
        true => CastConnection::connect_address_unverified_for_testing(addr).await,
        _ => CastConnection::connect_address(addr).await,
    }
    .with_context(|| format!("connecting to {addr}"))?;
    if connection.get_app_availability(APP_MIRRORING).await? != AppAvailability::Available {
        bail!("receiver does not offer the mirroring app {APP_MIRRORING}");
    }
    let app = connection.launch(APP_MIRRORING).await.context("launching mirroring app")?;
    Ok((connection, app))
}

fn main() -> Result<()> {
    // Launched from CastMirror.app without arguments: menu-bar mode, logging to a file.
    let in_app = std::env::args().len() == 1
        && std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if in_app {
        let log = std::env::home_dir().unwrap_or_default().join("Library/Logs/CastMirror.log");
        let file = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
        tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(file).init();
        // The default hook writes to stderr, which nobody sees in the menu bar app.
        std::panic::set_hook(Box::new(|info| tracing::error!("{info}")));
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    }
    let cli = if in_app { Cli::parse_from(["cast-mirror", "tray"]) } else { Cli::parse() };
    // The tray's event loop must own the macOS main thread, so it builds its own runtime.
    if matches!(cli.command, Some(Command::Tray)) {
        return tray::run();
    }
    tokio::runtime::Runtime::new()?.block_on(run_cli(cli))
}

async fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Tray) => unreachable!("handled in main"),
        Some(Command::Scan { seconds }) => {
            for d in discover(Duration::from_secs(seconds)).await? {
                println!("{}\t{}\t{}\t{}", d.name(), d.model(), d.id(), d.preferred_endpoint().address());
            }
            Ok(())
        }
        Some(Command::Launch { device }) => {
            let (connection, app) = connect_and_launch(resolve(Some(&device)).await?, false).await?;
            println!(
                "launched {} ({}) session {} transport {}",
                app.display_name().unwrap_or("?"),
                app.app_id(),
                app.session_id(),
                app.transport_id()
            );
            // close() or a drop would STOP the app again; the process exits right after, closing the socket.
            std::mem::forget(connection);
            Ok(())
        }
        Some(Command::Pair { device }) => {
            let addr = device.parse().or_else(|_| format!("{device}:{}", direct::PORT).parse())?;
            let mut store = pairing::Store::load();
            let pending = pairing::begin(addr, &store).await?;
            eprint!("Code shown on {}: ", pending.device_name);
            let mut code = String::new();
            std::io::stdin().read_line(&mut code)?;
            pending.finish(&code, &mut store).await?;
            println!("paired");
            Ok(())
        }
        Some(Command::Stop { device }) => {
            let connection = CastConnection::connect_address(resolve(Some(&device)).await?).await?;
            for app in connection.status().await?.applications().iter().filter(|a| a.app_id() == APP_MIRRORING) {
                connection.stop(app).await?;
                println!("stopped session {}", app.session_id());
            }
            connection.close().await?;
            Ok(())
        }
        Some(Command::Pattern { device, mut args }) => {
            args.device = Some(device);
            args.no_audio = true;
            mirror::run(args, mirror::Source::Pattern, mirror::Control::ctrl_c()).await
        }
        None if cli.mirror.direct => direct::run(cli.mirror, mirror::Control::ctrl_c()).await,
        None => mirror::run(cli.mirror, mirror::Source::Screen, mirror::Control::ctrl_c()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bitrate;

    #[test]
    fn bitrate_suffixes() {
        assert_eq!(parse_bitrate("6M"), Ok(6_000_000));
        assert_eq!(parse_bitrate("4500k"), Ok(4_500_000));
        assert_eq!(parse_bitrate("1.5m"), Ok(1_500_000));
        assert_eq!(parse_bitrate("800000"), Ok(800_000));
        assert!(parse_bitrate("fast").is_err());
    }
}
