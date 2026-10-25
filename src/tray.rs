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
use tray_icon::menu::{CheckMenuItem, IconMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
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
    Devices(Vec<Found>),
    Status(String),
    Stopped(Option<String>),
    // Picked in the macOS window picker: id and "app: title".
    Window(u32, String),
}

/// A Cast receiver from discovery; `tv_app`: our receiver app (android/) is installed, reachable over ADB.
struct Found {
    name: String,
    addr: String,
    tv_app: bool,
}

/// One "Receiver" entry: a Cast receiver, or its TV app (direct mode).
struct Device {
    item: CheckMenuItem,
    name: String,
    addr: String,
    direct: bool,
}

/// What gets cast: a display (index as `Capture::start` counts them) or a window.
#[derive(Clone)]
enum Target {
    Display(usize, String),
    Window(u32, String),
}

impl Target {
    fn label(&self) -> String {
        match self {
            Self::Display(_, name) => format!("screen {name}"),
            Self::Window(_, title) => format!("window {}", shorten(title)),
        }
    }
}

/// "Cast screen": one item for one display, a submenu to pick from when there are more.
struct Screens {
    single: IconMenuItem,
    submenu: Submenu,
    items: Vec<IconMenuItem>,
    labels: Vec<String>,
}

impl Screens {
    const POSITION: usize = 2;

    fn refresh(&mut self, menu: &Menu) {
        self.labels = crate::capture::display_labels()
            .inspect_err(|e| tracing::warn!("{e:#}"))
            .unwrap_or_default();
        for item in self.items.drain(..) {
            let _ = self.submenu.remove(&item);
        }
        let _ = menu.remove(&self.single);
        let _ = menu.remove(&self.submenu);
        if self.labels.len() > 1 {
            for label in &self.labels {
                let item = icon_item(label, true, if label.starts_with("Built-in") { "laptopcomputer" } else { "display" });
                let _ = self.submenu.append(&item);
                self.items.push(item);
            }
            let _ = menu.insert(&self.submenu, Self::POSITION);
        } else {
            let _ = menu.insert(&self.single, Self::POSITION);
        }
    }

    /// The display a menu event picked, if it was one of ours.
    fn picked(&self, id: &tray_icon::menu::MenuId) -> Option<Target> {
        let label = |i: usize| self.labels.get(i).cloned().unwrap_or_else(|| format!("{}", i + 1));
        if id == self.single.id() {
            return Some(Target::Display(0, label(0)));
        }
        self.items.iter().position(|item| id == item.id()).map(|i| Target::Display(i, label(i)))
    }

    fn set_enabled(&self, enabled: bool) {
        self.single.set_enabled(enabled);
        self.submenu.set_enabled(enabled);
    }
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
    let mut screens = Screens {
        single: icon_item("Cast screen", false, "display"),
        submenu: Submenu::new("Cast screen", false),
        items: Vec::new(),
        labels: Vec::new(),
    };
    screens.submenu.set_icon_templated(symbol("display"));
    let cast_window = icon_item("Cast window…", false, "macwindow");
    let stop_item = icon_item("Stop casting", false, "stop.circle");
    let receivers = Submenu::new("Receiver", true);
    receivers.set_icon_templated(symbol("tv"));
    let rescan = icon_item("Rescan", true, "arrow.clockwise");
    receivers.append_items(&[&PredefinedMenuItem::separator(), &rescan])?;
    let profiles: Vec<CheckMenuItem> = PROFILES
        .iter()
        .enumerate()
        .map(|(i, p)| CheckMenuItem::new(p.label, true, i == DEFAULT_PROFILE, None))
        .collect();
    let audio = CheckMenuItem::new("Audio", true, true, None);
    // ScreenCaptureKit records the apps' audio before the output volume, so the TV still gets all of it.
    let mute = CheckMenuItem::new("Mute Mac while casting", true, false, None);
    // macOS applies a newly granted Screen Recording permission only to a fresh process.
    let relaunch = icon_item("Restart app", true, "arrow.triangle.2.circlepath");
    let quit = icon_item("Quit", true, "power");
    let menu = Menu::new();
    menu.append_items(&[&status, &PredefinedMenuItem::separator(), &cast_window, &stop_item, &receivers])?;
    menu.append(&PredefinedMenuItem::separator())?;
    for p in &profiles {
        menu.append(p)?;
    }
    menu.append_items(&[&PredefinedMenuItem::separator(), &audio, &mute])?;
    menu.append_items(&[&PredefinedMenuItem::separator(), &relaunch, &quit])?;
    screens.refresh(&menu);

    rt.spawn(scan(proxy.clone()));

    let mut tray: Option<TrayIcon> = None;
    let mut devices: Vec<Device> = Vec::new();
    let mut selected: Option<String> = None;
    let mut profile = DEFAULT_PROFILE;
    let mut target: Option<Target> = None;
    let mut stop: Option<watch::Sender<bool>> = None;
    let (mut restart, mut quitting) = (false, false);
    // Set when we muted the Mac, so stopping restores exactly what the user had.
    let mut muted_by_us = false;
    let mut tray_menu = Some(menu.clone());

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let casting = stop.is_some();
        let mut begin = false;
        // A new target starts casting, or restarts a running session with it.
        let mut cast = |t: Target, begin: &mut bool, restart: &mut bool| {
            tracing::info!("menu: cast {}", t.label());
            target = Some(t);
            if casting {
                *restart = true;
            } else {
                *begin = true;
            }
        };
        match event {
            // tray-icon wants the event loop running before the icon exists.
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(tray_menu.take().unwrap()))
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
                for f in found {
                    let tv_app = f.tv_app.then(|| format!("{} · TV app (lowest latency)", f.name));
                    devices.push(Device { item: CheckMenuItem::new(&f.name, true, false, None), name: f.name, addr: f.addr.clone(), direct: false });
                    if let Some(name) = tv_app {
                        devices.push(Device { item: CheckMenuItem::new(&name, true, false, None), name, addr: f.addr, direct: true });
                    }
                }
                if selected.as_ref().is_none_or(|s| !devices.iter().any(|d| &d.name == s)) {
                    selected = devices.first().map(|d| d.name.clone());
                }
                for (i, d) in devices.iter().enumerate() {
                    d.item.set_checked(Some(&d.name) == selected.as_ref());
                    let _ = receivers.insert(&d.item, i);
                }
                // Displays come and go like receivers; refresh both on every scan.
                screens.refresh(&menu);
                if !casting {
                    status.set_text(if devices.is_empty() { "No receivers found" } else { "Ready" });
                }
            }
            Event::UserEvent(Ui::Status(text)) => status.set_text(text),
            Event::UserEvent(Ui::Window(id, title)) => cast(Target::Window(id, title), &mut begin, &mut restart),
            Event::UserEvent(Ui::Stopped(error)) => {
                stop = None;
                status.set_text(error.as_deref().unwrap_or("Stopped"));
                if let Some(t) = &tray {
                    let _ = t.set_icon_templated(Some(icon(false)));
                }
                if quitting {
                    set_output_muted(false, &mut muted_by_us);
                    *flow = ControlFlow::Exit;
                    return;
                }
                begin = std::mem::take(&mut restart);
                if begin {
                    tracing::info!("restarting with changed settings");
                }
            }
            Event::UserEvent(Ui::Menu(e)) => {
                if let Some(t) = screens.picked(&e.id) {
                    cast(t, &mut begin, &mut restart);
                } else if e.id == *cast_window.id() {
                    show_window_picker(proxy.clone());
                } else if e.id == *stop_item.id() {
                    if let Some(tx) = &stop {
                        tracing::info!("menu: stop");
                        let _ = tx.send(true);
                        // An explicit stop wins over a restart queued by an earlier settings change.
                        restart = false;
                        status.set_text("Stopping…");
                    }
                } else if e.id == *quit.id() || e.id == *relaunch.id() {
                    if e.id == *relaunch.id() {
                        tracing::info!("menu: restart app");
                        if let Err(err) = relaunch_after_exit() {
                            status.set_text(format!("Restart failed: {err:#}"));
                            return;
                        }
                    }
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
                } else if e.id == *audio.id() {
                    restart = casting;
                }
                // Check items toggle themselves; keep exactly one profile and one receiver ticked.
                for (i, p) in profiles.iter().enumerate() {
                    p.set_checked(i == profile);
                }
                for d in &devices {
                    d.item.set_checked(Some(&d.name) == selected.as_ref());
                }
            }
            _ => {}
        }
        if restart && let Some(tx) = &stop {
            let _ = tx.send(true);
            status.set_text("Restarting…");
        }

        if begin && stop.is_none() {
            let Some((addr, direct)) =
                devices.iter().find(|d| Some(&d.name) == selected.as_ref()).map(|d| (d.addr.clone(), d.direct))
            else {
                return;
            };
            let Some(what) = target.clone() else { return };
            let mut args = Cli::parse_from(["cast-mirror"]).mirror;
            args.device = Some(addr);
            args.delay = PROFILES[profile].delay;
            args.fps = PROFILES[profile].fps;
            args.width = PROFILES[profile].width;
            args.bitrate = PROFILES[profile].bitrate;
            args.auto = PROFILES[profile].auto;
            args.no_audio = !audio.is_checked();
            match &what {
                Target::Display(i, _) => args.display = *i,
                Target::Window(id, _) => args.window = Some(id.to_string()),
            }
            if direct {
                // HEVC: the TV's MediaTek HEVC decoder buffers less than its H.264 one.
                args.codecs = vec![crate::Codec::Hevc];
                args.device = args.device.map(|a| a.rsplit_once(':').map_or(a.clone(), |(ip, _)| ip.to_string()));
            }
            let (tx, rx) = watch::channel(false);
            let status_proxy = proxy.clone();
            let label = what.label();
            let control = Control {
                stop: rx,
                status: Arc::new(move |s| {
                    let _ = status_proxy.send_event(Ui::Status(format!("{s} · {label}")));
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
            status.set_text(format!("Starting {} ({})…", what.label(), PROFILES[profile].label));
            if let Some(t) = &tray {
                let _ = t.set_icon_templated(Some(icon(true)));
            }
        }
        // Mute only while audio actually goes to the receiver; direct mode sends none.
        let direct = devices.iter().any(|d| d.direct && Some(&d.name) == selected.as_ref());
        let want_mute = stop.is_some() && mute.is_checked() && audio.is_checked() && !direct;
        set_output_muted(want_mute, &mut muted_by_us);
        let ready = selected.is_some();
        screens.set_enabled(ready);
        cast_window.set_enabled(ready);
        stop_item.set_enabled(stop.is_some());
    })
}

/// Mutes the Mac's output, or restores it if we muted it; leaves a mute the user set alone.
fn set_output_muted(want: bool, muted_by_us: &mut bool) {
    let osascript = |script: &str| {
        std::process::Command::new("osascript").args(["-e", script]).output().ok().filter(|o| o.status.success())
    };
    if want && !*muted_by_us {
        let user_muted = osascript("output muted of (get volume settings)")
            .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "true");
        if !user_muted && osascript("set volume output muted true").is_some() {
            *muted_by_us = true;
        }
    } else if !want && *muted_by_us {
        osascript("set volume output muted false");
        *muted_by_us = false;
    }
}

/// Opens the macOS window picker; the choice comes back as `Ui::Window`.
fn show_window_picker(proxy: EventLoopProxy<Ui>) {
    use screencapturekit::content_sharing_picker::*;
    let mut config = SCContentSharingPickerConfiguration::new();
    config.set_allowed_picker_modes(&[SCContentSharingPickerMode::SingleWindow]);
    SCContentSharingPicker::show(&config, move |outcome| {
        match outcome {
            SCPickerOutcome::Picked(result) => {
                if let Some(w) = result.windows().first() {
                    let _ = proxy.send_event(Ui::Window(w.window_id(), crate::capture::window_label(w)));
                }
            }
            SCPickerOutcome::Cancelled => {}
            SCPickerOutcome::Error(e) => {
                let _ = proxy.send_event(Ui::Status(format!("Window picker: {e}")));
            }
        }
        // Leaving it active keeps a "ready to share" entry in Control Center.
        SCContentSharingPicker::set_active(false);
    });
}

fn shorten(text: &str) -> String {
    match text.char_indices().nth(40) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

/// Starts this app again once the current process has exited (a second instance would add a second icon).
fn relaunch_after_exit() -> Result<()> {
    let exe = std::env::current_exe()?;
    let target = match exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")) {
        Some(bundle) => format!("open {:?}", bundle),
        None => format!("{:?} tray", exe),
    };
    let script = format!("while kill -0 {} 2>/dev/null; do sleep 0.2; done; {target}", std::process::id());
    std::process::Command::new("/bin/sh").args(["-c", &script]).spawn()?;
    Ok(())
}

/// (Re)starts the receiver app (android/) over ADB; network debugging must be enabled on the TV.
/// `gpu`: draw on the graphics plane (fast, 1080p max) instead of the video plane (4K, ~85 ms slower).
async fn start_tv_app(host: &str, gpu: bool) -> Result<()> {
    let serial = format!("{host}:5555");
    let ten = Duration::from_secs(10);
    adb(&["connect", &serial], ten).await.context("running adb (brew install --cask android-platform-tools)")?;
    // -S restarts the activity, so a changed plane choice takes effect.
    let texture = gpu.to_string();
    let activity = format!("{TV_APP}/.MainActivity");
    let out = adb(&["-s", &serial, "shell", "am", "start", "-S", "-n", &activity, "--ez", "texture", &texture], ten).await?;
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || text.contains("Error") {
        anyhow::bail!("TV app not started on {host}: {}", text.trim());
    }
    // Give the app a moment to open its socket.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    Ok(())
}

const TV_APP: &str = "de.tisth.castmirror";

/// Runs adb, which can hang on an unreachable TV, for at most `limit`.
async fn adb(args: &[&str], limit: Duration) -> Result<std::process::Output> {
    // Finder-launched apps get a minimal PATH, so look where Homebrew puts adb too.
    let path = ["/opt/homebrew/bin/adb", "/usr/local/bin/adb"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("adb");
    let out = tokio::process::Command::new(path).args(args).kill_on_drop(true).output();
    Ok(tokio::time::timeout(limit, out).await.context("adb timed out")??)
}

/// Whether the receiver at `host` is an Android TV with network ADB authorized for us and the TV app installed.
async fn tv_app_installed(host: String) -> bool {
    let serial = format!("{host}:5555");
    let limit = Duration::from_secs(3);
    let stdout = |o: std::process::Output| String::from_utf8_lossy(&o.stdout).into_owned();
    let connected = adb(&["connect", &serial], limit).await.map(stdout).is_ok_and(|s| s.contains("connected to"));
    connected
        && adb(&["-s", &serial, "shell", "pm", "path", TV_APP], limit)
            .await
            .map(stdout)
            .is_ok_and(|s| s.contains("package:"))
}

async fn scan(proxy: EventLoopProxy<Ui>) {
    let found = match discover(Duration::from_secs(3)).await {
        Ok(found) => found,
        Err(e) => {
            let _ = proxy.send_event(Ui::Status(format!("Discovery failed: {e}")));
            return;
        }
    };
    let checks: Vec<_> = found
        .iter()
        .map(|d| tokio::spawn(tv_app_installed(d.preferred_endpoint().address().ip().to_string())))
        .collect();
    let mut devices = Vec::new();
    for (d, check) in found.iter().zip(checks) {
        let tv_app = check.await.unwrap_or(false);
        devices.push(Found { name: d.name().to_string(), addr: d.preferred_endpoint().address().to_string(), tv_app });
    }
    let _ = proxy.send_event(Ui::Devices(devices));
}

fn icon_item(text: &str, enabled: bool, symbol_name: &str) -> IconMenuItem {
    let item = IconMenuItem::new(text, enabled, None, None);
    item.set_icon_templated(symbol(symbol_name));
    item
}

/// An SF Symbol as a template icon, which macOS tints for light and dark menus. Rendered from the
/// system at runtime, so no Apple artwork ships with the app.
fn symbol(name: &str) -> Option<tray_icon::menu::Icon> {
    let rgba = symbol_rgba(name)?;
    tray_icon::menu::Icon::from_rgba(rgba, SYMBOL_PX as u32, SYMBOL_PX as u32).ok()
}

// 18 pt (the tallest a menu icon gets) at 2x.
const SYMBOL_PX: usize = 36;

fn symbol_rgba(name: &str) -> Option<Vec<u8>> {
    use objc2_app_kit::{NSImage, NSImageSymbolConfiguration};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo};
    const PX: usize = SYMBOL_PX;
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&objc2_foundation::NSString::from_str(name), None)
        .or_else(|| {
            tracing::warn!("no SF Symbol {name:?}");
            None
        })?;
    let image = image.imageWithSymbolConfiguration(&NSImageSymbolConfiguration::configurationWithPointSize_weight(32.0, 0.0))?;
    let cg = unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
    let (w, h) = (CGImage::width(Some(&cg)) as f64, CGImage::height(Some(&cg)) as f64);
    let mut rgba = vec![0u8; PX * PX * 4];
    let space = CGColorSpace::new_device_rgb()?;
    let alpha = CGImageAlphaInfo::PremultipliedLast.0;
    let ctx = unsafe { CGBitmapContextCreate(rgba.as_mut_ptr().cast(), PX, PX, 8, PX * 4, Some(&space), alpha) }?;
    let scale = (PX as f64 / w).min(PX as f64 / h);
    let size = CGSize::new(w * scale, h * scale);
    let origin = CGPoint::new((PX as f64 - size.width) / 2.0, (PX as f64 - size.height) / 2.0);
    CGContext::draw_image(Some(&ctx), CGRect::new(origin, size), Some(&cg));
    drop(ctx);
    Some(rgba)
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

#[cfg(test)]
mod tests {
    #[test]
    fn menu_symbols_exist_and_render() {
        for name in ["display", "laptopcomputer", "macwindow", "stop.circle", "tv", "arrow.clockwise", "arrow.triangle.2.circlepath", "power"] {
            let rgba = super::symbol_rgba(name).unwrap_or_else(|| panic!("{name} did not render"));
            let drawn = rgba.chunks(4).filter(|p| p[3] > 0).count();
            println!("{name}: {drawn} pixels");
            assert!(drawn > 50, "{name} rendered empty");
        }
    }
}
