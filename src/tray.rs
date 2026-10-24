//! Menu-bar app: pick a receiver and a profile, start and stop casting.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chromiacast::discover;
use clap::Parser;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tokio::sync::watch;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::mirror::{self, Control, Source};
use crate::Cli;

struct Profile {
    label: &'static str,
    delay: u64,
    fps: u32,
    width: u32,
    bitrate: u32,
    auto: bool,
}

// Measured on a Philips TPM191E and a Chromecast on its HDMI input; see README.
const PROFILES: [Profile; 6] = [
    Profile { label: "Lowest latency (Chromecast, 60 ms)", delay: 60, fps: 30, width: 1920, bitrate: 6_000_000, auto: false },
    Profile { label: "Balanced (100 ms)", delay: 100, fps: 30, width: 1920, bitrate: 6_000_000, auto: false },
    Profile { label: "TV built-in Cast (150 ms)", delay: 150, fps: 30, width: 1920, bitrate: 6_000_000, auto: false },
    Profile { label: "Smooth video (200 ms, 60 fps)", delay: 200, fps: 60, width: 1920, bitrate: 8_000_000, auto: false },
    // Few frames, many bits each: sharp 4K text; motion looks jerky.
    Profile { label: "Presentation (4K, 10 fps, sharp)", delay: 200, fps: 10, width: 3840, bitrate: 12_000_000, auto: false },
    // Direct mode only: steps 1080p60 → 540p30 with the Wi-Fi quality and back.
    Profile { label: "Auto (adapts to Wi-Fi, direct mode)", delay: 100, fps: 60, width: 1920, bitrate: 6_000_000, auto: true },
];
const DEFAULT_PROFILE: usize = 1;

enum Ui {
    Menu(MenuEvent),
    Devices(Vec<(String, String)>),
    Status(String),
    Stopped(Option<String>),
}

struct Device {
    item: CheckMenuItem,
    name: String,
    addr: String,
}

pub fn run() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let mut event_loop = EventLoopBuilder::<Ui>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(Ui::Menu(e));
    }));

    let status = MenuItem::new("Looking for receivers…", false, None);
    let start = MenuItem::new("Start casting", false, None);
    let receivers = Submenu::new("Receiver", true);
    let profiles: Vec<CheckMenuItem> = PROFILES
        .iter()
        .enumerate()
        .map(|(i, p)| CheckMenuItem::new(p.label, true, i == DEFAULT_PROFILE, None))
        .collect();
    let audio = CheckMenuItem::new("Audio", true, true, None);
    let direct = CheckMenuItem::new("Direct to TV app (lowest latency)", true, false, None);
    let rescan = MenuItem::new("Rescan receivers", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&status, &PredefinedMenuItem::separator(), &start, &receivers])?;
    menu.append(&PredefinedMenuItem::separator())?;
    for p in &profiles {
        menu.append(p)?;
    }
    menu.append_items(&[&audio, &direct, &PredefinedMenuItem::separator(), &rescan, &quit])?;

    rt.spawn(scan(proxy.clone()));

    let mut tray: Option<TrayIcon> = None;
    let mut devices: Vec<Device> = Vec::new();
    let mut selected: Option<String> = None;
    let mut profile = DEFAULT_PROFILE;
    let mut stop: Option<watch::Sender<bool>> = None;
    let (mut restart, mut quitting) = (false, false);
    let mut menu = Some(menu);

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let casting = stop.is_some();
        let mut begin = false;
        match event {
            // tray-icon wants the event loop running before the icon exists.
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu.take().unwrap()))
                    .with_icon_templated(icon(false))
                    .with_tooltip("Cast Mirror")
                    .build()
                    .inspect_err(|e| tracing::error!("tray icon: {e}"))
                    .ok();
                tracing::info!("tray icon created: {}", tray.is_some());
            }
            Event::UserEvent(Ui::Devices(found)) => {
                for d in devices.drain(..) {
                    let _ = receivers.remove(&d.item);
                }
                if selected.as_ref().is_none_or(|s| !found.iter().any(|(n, _)| n == s)) {
                    selected = found.first().map(|(n, _)| n.clone());
                }
                for (name, addr) in found {
                    let item = CheckMenuItem::new(&name, true, Some(&name) == selected.as_ref(), None);
                    let _ = receivers.append(&item);
                    devices.push(Device { item, name, addr });
                }
                if !casting {
                    status.set_text(if devices.is_empty() { "No receivers found" } else { "Ready" });
                }
                start.set_enabled(casting || selected.is_some());
            }
            Event::UserEvent(Ui::Status(text)) => status.set_text(text),
            Event::UserEvent(Ui::Stopped(error)) => {
                stop = None;
                start.set_text("Start casting");
                start.set_enabled(selected.is_some());
                status.set_text(error.as_deref().unwrap_or("Stopped"));
                if let Some(t) = &tray {
                    let _ = t.set_icon_templated(Some(icon(false)));
                }
                if quitting {
                    *flow = ControlFlow::Exit;
                    return;
                }
                begin = std::mem::take(&mut restart);
                if begin {
                    tracing::info!("restarting with changed settings");
                }
            }
            Event::UserEvent(Ui::Menu(e)) => {
                if e.id == *start.id() {
                    match &stop {
                        Some(tx) => {
                            tracing::info!("menu: stop");
                            let _ = tx.send(true);
                            // An explicit stop wins over a restart queued by an earlier settings change.
                            restart = false;
                            status.set_text("Stopping…");
                        }
                        None => {
                            tracing::info!("menu: start");
                            begin = true;
                        }
                    }
                } else if e.id == *quit.id() {
                    match &stop {
                        Some(tx) => {
                            // Let the session send STOP to the receiver before exiting.
                            quitting = true;
                            let _ = tx.send(true);
                        }
                        None => *flow = ControlFlow::Exit,
                    }
                } else if e.id == *rescan.id() {
                    status.set_text("Looking for receivers…");
                    rt.spawn(scan(proxy.clone()));
                } else if let Some(i) = profiles.iter().position(|p| e.id == *p.id()) {
                    profile = i;
                    restart = casting;
                } else if let Some(d) = devices.iter().find(|d| e.id == *d.item.id()) {
                    selected = Some(d.name.clone());
                    restart = casting;
                } else if e.id == *audio.id() || e.id == *direct.id() {
                    restart = casting;
                }
                // Check items toggle themselves; keep exactly one profile and one receiver ticked.
                for (i, p) in profiles.iter().enumerate() {
                    p.set_checked(i == profile);
                }
                for d in &devices {
                    d.item.set_checked(Some(&d.name) == selected.as_ref());
                }
                if restart && let Some(tx) = &stop {
                    let _ = tx.send(true);
                    status.set_text("Restarting…");
                }
            }
            _ => {}
        }

        if begin && stop.is_none() {
            let Some(addr) = devices.iter().find(|d| Some(&d.name) == selected.as_ref()).map(|d| d.addr.clone())
            else {
                return;
            };
            let mut args = Cli::parse_from(["cast-mirror"]).mirror;
            args.device = Some(addr);
            args.delay = PROFILES[profile].delay;
            args.fps = PROFILES[profile].fps;
            args.width = PROFILES[profile].width;
            args.bitrate = PROFILES[profile].bitrate;
            args.auto = PROFILES[profile].auto;
            args.no_audio = !audio.is_checked();
            let direct = direct.is_checked();
            if direct {
                // HEVC: the TV's MediaTek HEVC decoder buffers less than its H.264 one.
                args.codecs = vec![crate::Codec::Hevc];
                args.device = args.device.map(|a| a.rsplit_once(':').map_or(a.clone(), |(ip, _)| ip.to_string()));
            }
            let (tx, rx) = watch::channel(false);
            let status_proxy = proxy.clone();
            let control = Control {
                stop: rx,
                status: Arc::new(move |s| {
                    let _ = status_proxy.send_event(Ui::Status(s));
                }),
            };
            let done = proxy.clone();
            // Like the CLI: block_on on its own thread, as the session holds non-Send CoreVideo buffers.
            std::thread::spawn(move || {
                let session = || -> Result<()> {
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(async {
                        if direct {
                            (control.status)("Starting the TV app over ADB…".into());
                            // The TV's graphics plane is 1080p: above that, use the (slower) 4K video plane.
                            start_tv_app(args.device.as_deref().unwrap_or_default(), args.width <= 1920).await?;
                            crate::direct::run(args, control).await
                        } else {
                            mirror::run(args, Source::Screen, control).await
                        }
                    })
                };
                // A panic must still report Stopped, or the menu stays on "Stop casting" and Quit waits forever.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(session))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("session crashed, see the log")));
                let _ = done.send_event(Ui::Stopped(result.err().map(|e| format!("Error: {e:#}"))));
            });
            stop = Some(tx);
            start.set_text("Stop casting");
            status.set_text(format!("Starting ({})…", PROFILES[profile].label));
            if let Some(t) = &tray {
                let _ = t.set_icon_templated(Some(icon(true)));
            }
        }
    })
}

/// (Re)starts the receiver app (android/) over ADB; network debugging must be enabled on the TV.
/// `gpu`: draw on the graphics plane (fast, 1080p max) instead of the video plane (4K, ~85 ms slower).
async fn start_tv_app(host: &str, gpu: bool) -> Result<()> {
    // Finder-launched apps get a minimal PATH, so look where Homebrew puts adb too.
    let adb = ["/opt/homebrew/bin/adb", "/usr/local/bin/adb"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("adb");
    let serial = format!("{host}:5555");
    // adb can hang on an unreachable TV; never wait longer than 10 s per call.
    let run = async |args: &[&str]| -> Result<std::process::Output> {
        let out = tokio::process::Command::new(adb).args(args).kill_on_drop(true).output();
        Ok(tokio::time::timeout(Duration::from_secs(10), out).await.context("adb timed out")??)
    };
    run(&["connect", &serial]).await.context("running adb (brew install --cask android-platform-tools)")?;
    // -S restarts the activity, so a changed plane choice takes effect.
    let texture = gpu.to_string();
    let activity = "de.tisth.castmirror/.MainActivity";
    let out = run(&["-s", &serial, "shell", "am", "start", "-S", "-n", activity, "--ez", "texture", &texture]).await?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || text.contains("Error") {
        anyhow::bail!("TV app not started on {host}: {}", text.trim());
    }
    // Give the app a moment to open its socket.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    Ok(())
}

async fn scan(proxy: EventLoopProxy<Ui>) {
    let event = match discover(Duration::from_secs(3)).await {
        Ok(found) => Ui::Devices(
            found.iter().map(|d| (d.name().to_string(), d.preferred_endpoint().address().to_string())).collect(),
        ),
        Err(e) => Ui::Status(format!("Discovery failed: {e}")),
    };
    let _ = proxy.send_event(event);
}

/// 22x22 template icon: a screen outline, filled while casting.
fn icon(casting: bool) -> Icon {
    const S: usize = 22;
    let mut rgba = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            let screen = (2..20).contains(&x) && (3..16).contains(&y);
            let edge = x == 2 || x == 19 || y == 3 || y == 15;
            let stand = (y == 18 && (7..15).contains(&x)) || ((16..18).contains(&y) && (10..12).contains(&x));
            if (screen && (edge || casting)) || stand {
                rgba[(y * S + x) * 4 + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, S as u32, S as u32).expect("valid icon")
}
