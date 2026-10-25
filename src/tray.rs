//! Menu-bar app: a click on the icon opens a small window (a web view, `ui.html`) to pick what to cast,
//! the receiver and a profile; a right click offers Restart app and Quit.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chromiacast::discover;
use clap::Parser;
use serde::Deserialize;
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tao::window::WindowBuilder;
use tokio::sync::watch;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use wry::{WebView, WebViewBuilder};

use crate::mirror::{self, Control, Source};
use crate::Cli;

struct Profile {
    name: &'static str,
    detail: &'static str,
    delay: u64,
    fps: u32,
    width: u32,
    bitrate: u32,
    auto: bool,
}

// Measured on a Philips TPM191E and a Chromecast on its HDMI input; see README.
const PROFILES: [Profile; 6] = [
    Profile { name: "Lowest latency", detail: "Chromecast, 60 ms buffer", delay: 60, fps: 30, width: 1920, bitrate: 6_000_000, auto: false },
    Profile { name: "Balanced", detail: "100 ms buffer", delay: 100, fps: 30, width: 1920, bitrate: 6_000_000, auto: false },
    Profile { name: "TV built-in Cast", detail: "150 ms, its lowest", delay: 150, fps: 30, width: 1920, bitrate: 6_000_000, auto: false },
    Profile { name: "Smooth video", detail: "200 ms, 60 fps", delay: 200, fps: 60, width: 1920, bitrate: 8_000_000, auto: false },
    // Few frames, many bits each: sharp 4K text; motion looks jerky.
    Profile { name: "Presentation", detail: "4K, 10 fps, sharp text", delay: 200, fps: 10, width: 3840, bitrate: 12_000_000, auto: false },
    // Direct mode only: steps 1080p60 → 540p30 with the Wi-Fi quality and back.
    Profile { name: "Auto", detail: "TV app: adapts to the Wi-Fi", delay: 100, fps: 60, width: 1920, bitrate: 6_000_000, auto: true },
];
const DEFAULT_PROFILE: usize = 1;
const WIDTH: f64 = 320.0;

enum Ui {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Js(String),
    Devices(Vec<Found>),
    Status(String),
    Stats(String),
    Stopped(Option<String>),
    // Picked in the macOS window picker: id and "app: title".
    Window(u32, String),
    // The device shows a pairing code now.
    PairWaiting,
    // Paired with the device of this id.
    Paired(String),
    PairFailed(String),
}

/// Commands from `ui.html`.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
enum Cmd {
    Ready,
    Height { px: f64 },
    Hide,
    CastScreen { index: usize },
    CastWindow,
    Stop,
    Receiver { name: String },
    Profile { index: usize },
    Audio { on: bool },
    Mute { on: bool },
    Encrypt { on: bool },
    PairCode { code: String },
    PairCancel,
    Rescan,
    Relaunch,
    Quit,
}

/// A Cast receiver from discovery; `tv_app`: our receiver app (android/) is installed, reachable over ADB.
struct Found {
    name: String,
    addr: String,
    tv_app: bool,
    // False for a device found only over ADB or by its own announcement: direct mode only.
    cast: bool,
    // ADB reaches it and the app is installed, so the app can be started with a fresh key.
    adb: bool,
    // The app announced itself on the network with this device id (it is open right now).
    id: Option<String>,
}

/// A receiver entry: a Cast receiver, or its TV app (direct mode).
struct Device {
    name: String,
    addr: String,
    direct: bool,
    adb: bool,
    id: Option<String>,
    paired: bool,
}

impl Device {
    /// The TV app with neither ADB nor a stored key: cast only after pairing.
    fn needs_pairing(&self) -> bool {
        self.direct && !self.adb && !self.paired
    }
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

/// Everything the window shows; `view()` hands it to `render()` in ui.html.
struct State {
    status: String,
    // Live measurements while casting.
    stats: String,
    devices: Vec<Device>,
    selected: Option<String>,
    screens: Vec<String>,
    profile: usize,
    target: Option<Target>,
    audio: bool,
    mute: bool,
    // Direct mode only; off sends the TV app stream unencrypted.
    encrypt: bool,
    // Name of the device whose pairing code the window asks for, and the last pairing problem.
    pairing: Option<String>,
    pair_error: String,
}

impl State {
    fn view(&self, casting: bool) -> String {
        let source = match &self.target {
            Some(Target::Display(..)) => "screen",
            Some(Target::Window(..)) => "window",
            None => "",
        };
        serde_json::json!({
            "status": self.status,
            "stats": self.stats,
            "casting": casting,
            "source": source,
            "receivers": self.devices.iter().map(|d| serde_json::json!({
                "name": d.name, "direct": d.direct, "needsPairing": d.needs_pairing(),
            })).collect::<Vec<_>>(),
            "pairing": self.pairing,
            "pairError": self.pair_error,
            "selected": self.selected,
            "screens": self.screens,
            "profiles": PROFILES.iter().map(|p| serde_json::json!({"name": p.name, "detail": p.detail})).collect::<Vec<_>>(),
            "profile": self.profile,
            "audio": self.audio,
            "mute": self.mute,
            "encrypt": self.encrypt,
        })
        .to_string()
    }

    fn device(&self) -> Option<&Device> {
        self.devices.iter().find(|d| Some(&d.name) == self.selected.as_ref())
    }
}

pub fn run() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let mut event_loop = EventLoopBuilder::<Ui>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();
    let p = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(Ui::Menu(e));
    }));
    let p = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(Ui::Tray(e));
    }));

    // Right click: what's needed even when the window misbehaves.
    let relaunch = MenuItem::new("Restart app", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&relaunch, &quit])?;

    let window = WindowBuilder::new()
        .with_title("Cast Mirror")
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_always_on_top(true)
        .with_visible(false)
        .with_inner_size(LogicalSize::new(WIDTH, 480.0))
        .build(&event_loop)?;
    let p = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_transparent(true)
        .with_custom_protocol("app".into(), serve)
        .with_ipc_handler(move |req| {
            let _ = p.send_event(Ui::Js(req.into_body()));
        })
        .with_url("app://localhost/")
        .build(&window)?;

    rt.spawn(scan(proxy.clone()));

    let mut state = State {
        status: "Looking for receivers…".into(),
        stats: String::new(),
        devices: Vec::new(),
        selected: None,
        screens: Vec::new(),
        profile: DEFAULT_PROFILE,
        target: None,
        audio: true,
        mute: false,
        encrypt: true,
        pairing: None,
        pair_error: String::new(),
    };
    // A pairing request the device answered; the code the user types finishes it.
    let pending: Arc<std::sync::Mutex<Option<crate::pairing::Pending>>> = Arc::default();
    let start_pairing = {
        let (pending, proxy) = (pending.clone(), proxy.clone());
        move |rt: &tokio::runtime::Runtime, addr: String| {
            let (pending, proxy) = (pending.clone(), proxy.clone());
            rt.spawn(async move {
                let event = match async { crate::pairing::begin(addr.parse()?, &crate::pairing::Store::load()).await }.await {
                    Ok(p) => {
                        *pending.lock().unwrap() = Some(p);
                        Ui::PairWaiting
                    }
                    Err(e) => Ui::PairFailed(format!("{e:#}")),
                };
                let _ = proxy.send_event(event);
            });
        }
    };
    let mut tray: Option<TrayIcon> = None;
    let mut tray_menu = Some(menu);
    let mut stop: Option<watch::Sender<bool>> = None;
    let (mut restart, mut quitting) = (false, false);
    // Set when we muted the Mac, so stopping restores exactly what the user had.
    let mut muted_by_us = false;

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        let casting = stop.is_some();
        let mut begin = false;
        let mut changed = false;
        // Quit or Restart app: stop the session first so the receiver gets its STOP.
        let leave = |relaunch: bool, flow: &mut ControlFlow, quitting: &mut bool, status: &mut String| {
            if relaunch && let Err(err) = relaunch_after_exit() {
                *status = format!("Restart failed: {err:#}");
                return;
            }
            match &stop {
                Some(tx) => {
                    *quitting = true;
                    let _ = tx.send(true);
                }
                None => *flow = ControlFlow::Exit,
            }
        };
        match event {
            // tray-icon wants the event loop running before the icon exists.
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(tray_menu.take().unwrap()))
                    .with_menu_on_left_click(false)
                    .with_icon_templated(icon(false))
                    .with_tooltip("Cast Mirror")
                    .build()
                    .inspect_err(|e| tracing::error!("tray icon: {e}"))
                    .ok();
                tracing::info!("tray icon created: {}", tray.is_some());
            }
            Event::UserEvent(Ui::Tray(TrayIconEvent::Click {
                rect,
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            })) => {
                if window.is_visible() {
                    window.set_visible(false);
                } else {
                    // Centered under the icon, like a system popover.
                    let width = WIDTH * window.scale_factor();
                    let x = rect.position.x + rect.size.width as f64 / 2.0 - width / 2.0;
                    window.set_outer_position(PhysicalPosition::new(x, rect.position.y + rect.size.height as f64 + 4.0));
                    window.set_visible(true);
                    window.set_focus();
                }
            }
            Event::WindowEvent { event: WindowEvent::Focused(false), .. } => window.set_visible(false),
            Event::UserEvent(Ui::Menu(e)) => {
                if e.id == *quit.id() || e.id == *relaunch.id() {
                    leave(e.id == *relaunch.id(), flow, &mut quitting, &mut state.status);
                }
            }
            Event::UserEvent(Ui::Js(body)) => {
                let cmd = match serde_json::from_str::<Cmd>(&body) {
                    Ok(cmd) => cmd,
                    Err(e) => return tracing::warn!("ui: {e}: {body}"),
                };
                changed = true;
                let mut cast = |t: Target| {
                    tracing::info!("cast {}", t.label());
                    state.target = Some(t);
                    if casting {
                        restart = true;
                    } else {
                        begin = true;
                    }
                };
                match cmd {
                    Cmd::Ready => {}
                    Cmd::Height { px } => {
                        window.set_inner_size(LogicalSize::new(WIDTH, px));
                        changed = false;
                    }
                    Cmd::Hide => window.set_visible(false),
                    Cmd::CastScreen { index } => {
                        let name = state.screens.get(index).cloned().unwrap_or_else(|| "1".into());
                        cast(Target::Display(index, name));
                    }
                    Cmd::CastWindow => {
                        window.set_visible(false);
                        show_window_picker(proxy.clone());
                    }
                    Cmd::Stop => {
                        if let Some(tx) = &stop {
                            tracing::info!("stop");
                            let _ = tx.send(true);
                            // An explicit stop wins over a restart queued by an earlier settings change.
                            restart = false;
                            state.status = "Stopping…".into();
                        }
                    }
                    Cmd::Receiver { name } => {
                        state.selected = Some(name);
                        restart = casting;
                    }
                    Cmd::Profile { index } => {
                        state.profile = index.min(PROFILES.len() - 1);
                        restart = casting;
                    }
                    Cmd::Audio { on } => {
                        state.audio = on;
                        restart = casting;
                    }
                    Cmd::Mute { on } => state.mute = on,
                    Cmd::Encrypt { on } => {
                        state.encrypt = on;
                        restart = casting && state.device().is_some_and(|d| d.direct);
                    }
                    Cmd::PairCode { code } => {
                        let (pending, proxy) = (pending.clone(), proxy.clone());
                        state.pair_error = "Checking…".into();
                        rt.spawn(async move {
                            let request = pending.lock().unwrap().take();
                            let event = match request {
                                Some(p) => match p.finish(&code, &mut crate::pairing::Store::load()).await {
                                    Ok(id) => Ui::Paired(id),
                                    Err(e) => Ui::PairFailed(format!("{e:#}")),
                                },
                                None => Ui::PairFailed("no pairing request open".into()),
                            };
                            let _ = proxy.send_event(event);
                        });
                    }
                    Cmd::PairCancel => {
                        state.pairing = None;
                        pending.lock().unwrap().take();
                    }
                    Cmd::Rescan => {
                        state.status = "Looking for receivers…".into();
                        rt.spawn(scan(proxy.clone()));
                    }
                    Cmd::Relaunch | Cmd::Quit => {
                        leave(matches!(cmd, Cmd::Relaunch), flow, &mut quitting, &mut state.status)
                    }
                }
            }
            Event::UserEvent(Ui::Devices(found)) => {
                state.devices.clear();
                let store = crate::pairing::Store::load();
                for f in found {
                    if f.tv_app {
                        let name = format!("{} · TV app", f.name);
                        let paired = f.id.as_deref().is_some_and(|id| store.is_paired(id));
                        let (addr, adb, id) = (f.addr.clone(), f.adb, f.id.clone());
                        state.devices.push(Device { name, addr, direct: true, adb, id, paired });
                    }
                    if f.cast {
                        let device = Device { name: f.name, addr: f.addr, direct: false, adb: false, id: None, paired: false };
                        state.devices.push(device);
                    }
                }
                if state.device().is_none() {
                    let first = state.devices.iter().find(|d| !d.direct).or(state.devices.first());
                    state.selected = first.map(|d| d.name.clone());
                }
                // Displays come and go like receivers; refresh both on every scan.
                state.screens = crate::capture::display_labels().inspect_err(|e| tracing::warn!("{e:#}")).unwrap_or_default();
                if !casting {
                    state.status = if state.devices.is_empty() { "No receivers found" } else { "Ready" }.into();
                }
                changed = true;
            }
            Event::UserEvent(Ui::PairWaiting) => {
                state.pair_error.clear();
                changed = true;
            }
            Event::UserEvent(Ui::Paired(id)) => {
                tracing::info!("paired with {id}");
                for d in state.devices.iter_mut().filter(|d| d.id.as_deref() == Some(id.as_str())) {
                    d.paired = true;
                }
                state.pairing = None;
                state.pair_error.clear();
                begin = true;
                changed = true;
            }
            Event::UserEvent(Ui::PairFailed(error)) => {
                tracing::warn!("pairing: {error}");
                // A wrong code ends that request on our side: ask again, the device shows a fresh code.
                if error.contains("wrong code") && let Some(device) = state.device() {
                    start_pairing(&rt, device.addr.clone());
                    state.pair_error = "Wrong code. The device shows a new one.".into();
                } else {
                    state.pair_error = error;
                }
                changed = true;
            }
            Event::UserEvent(Ui::Status(text)) => {
                state.status = text;
                changed = true;
            }
            Event::UserEvent(Ui::Stats(text)) => {
                state.stats = text;
                changed = true;
            }
            Event::UserEvent(Ui::Window(id, title)) => {
                tracing::info!("cast window {title}");
                state.target = Some(Target::Window(id, title));
                if casting {
                    restart = true;
                } else {
                    begin = true;
                }
                changed = true;
            }
            Event::UserEvent(Ui::Stopped(error)) => {
                stop = None;
                state.status = error.unwrap_or_else(|| "Stopped".into());
                state.stats.clear();
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
                changed = true;
            }
            _ => {}
        }
        if restart && let Some(tx) = &stop {
            let _ = tx.send(true);
            state.status = "Restarting…".into();
        }

        // An unpaired TV app that ADB can't start: pair first, casting begins once paired.
        let unpaired = state.device().filter(|d| d.needs_pairing() && state.encrypt).map(|d| (d.name.clone(), d.addr.clone()));
        if begin && stop.is_none() && let Some((name, addr)) = unpaired {
            if state.pairing.is_none() {
                state.pairing = Some(name);
                state.pair_error = "Asking the device for a code…".into();
                start_pairing(&rt, addr);
            }
            begin = false;
            changed = true;
        }
        if begin
            && stop.is_none()
            && let (Some(device), Some(what)) = (state.device(), state.target.clone())
        {
            let profile = &PROFILES[state.profile];
            let mut args = Cli::parse_from(["cast-mirror"]).mirror;
            args.device = Some(device.addr.clone());
            args.delay = profile.delay;
            args.fps = profile.fps;
            args.width = profile.width;
            args.bitrate = profile.bitrate;
            args.auto = profile.auto;
            args.no_audio = !state.audio;
            args.no_encryption = !state.encrypt;
            match &what {
                Target::Display(i, _) => args.display = *i,
                Target::Window(id, _) => args.window = Some(id.to_string()),
            }
            let direct = device.direct;
            if direct {
                // HEVC: the TV's MediaTek HEVC decoder buffers less than its H.264 one.
                args.codecs = vec![crate::Codec::Hevc];
                args.device = args.device.map(|a| a.rsplit_once(':').map_or(a.clone(), |(ip, _)| ip.to_string()));
            }
            let (tx, rx) = watch::channel(false);
            let status_proxy = proxy.clone();
            let label = what.label();
            let stats_proxy = proxy.clone();
            let control = Control {
                stop: rx,
                status: Arc::new(move |s| {
                    let _ = status_proxy.send_event(Ui::Status(format!("{s} · {label}")));
                }),
                stats: Arc::new(move |s| {
                    let _ = stats_proxy.send_event(Ui::Stats(s));
                }),
            };
            let done = proxy.clone();
            // Like the CLI: block_on on its own thread, as the session holds non-Send CoreVideo buffers.
            std::thread::spawn(move || {
                let session = || -> Result<()> {
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(async {
                        if direct {
                            crate::direct::run(args, control).await
                        } else {
                            mirror::run(args, Source::Screen, control).await
                        }
                    })
                };
                // A panic must still report Stopped, or the window stays on "Stop" and Quit waits forever.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(session))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("session crashed, see the log")));
                let _ = done.send_event(Ui::Stopped(result.err().map(|e| format!("Error: {e:#}"))));
            });
            stop = Some(tx);
            state.status = format!("Starting {} ({})…", what.label(), profile.name);
            if let Some(t) = &tray {
                let _ = t.set_icon_templated(Some(icon(true)));
            }
            changed = true;
        }
        // Mute only while audio actually goes to the receiver; direct mode sends none.
        let direct = state.device().is_some_and(|d| d.direct);
        set_output_muted(stop.is_some() && state.mute && state.audio && !direct, &mut muted_by_us);
        if changed {
            render(&webview, &state, stop.is_some());
        }
    })
}

fn render(webview: &WebView, state: &State, casting: bool) {
    if let Err(e) = webview.evaluate_script(&format!("render({})", state.view(casting))) {
        tracing::warn!("ui: {e}");
    }
}

/// `app://localhost/` is ui.html; `app://localhost/icon/<SF Symbol>` its icons, rendered from the system
/// (no Apple artwork ships with the app) and tinted by CSS.
fn serve(_: wry::WebViewId, request: wry::http::Request<Vec<u8>>) -> wry::http::Response<Cow<'static, [u8]>> {
    let path = request.uri().path();
    let (mime, body): (&str, Cow<'static, [u8]>) = match path.strip_prefix("/icon/") {
        Some(name) => ("image/png", symbol_png(name).map(Cow::Owned).unwrap_or_default()),
        None => ("text/html", Cow::Borrowed(include_str!("ui.html").as_bytes())),
    };
    wry::http::Response::builder().header("Content-Type", mime).body(body).expect("static response")
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

/// Whether the receiver at `host` is an Android TV with network ADB authorized for us and the TV app installed.
async fn tv_app_installed(host: String) -> bool {
    let serial = format!("{host}:5555");
    let limit = Duration::from_secs(3);
    let stdout = |o: std::process::Output| String::from_utf8_lossy(&o.stdout).into_owned();
    let connected = crate::direct::adb(&["connect", &serial], limit).await.map(stdout).is_ok_and(|s| s.contains("connected to"));
    connected
        && crate::direct::adb(&["-s", &serial, "shell", "pm", "path", crate::direct::TV_APP], limit)
            .await
            .map(stdout)
            .is_ok_and(|s| s.contains("package:"))
}

async fn scan(proxy: EventLoopProxy<Ui>) {
    let (cast, announced) = tokio::join!(discover(Duration::from_secs(3)), crate::pairing::discover(Duration::from_secs(3)));
    let found = match cast {
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
        devices.push(Found {
            name: d.name().to_string(),
            addr: d.preferred_endpoint().address().to_string(),
            tv_app,
            cast: true,
            adb: tv_app,
            id: None,
        });
    }
    // Devices ADB already reaches over the network that run the app but aren't Cast receivers.
    for host in adb_hosts().await {
        let known = devices.iter().any(|d| d.addr.starts_with(&format!("{host}:")));
        if !known && tv_app_installed(host.clone()).await {
            let name = adb_name(&host).await;
            let addr = format!("{host}:{}", crate::direct::PORT);
            devices.push(Found { name, addr, tv_app: true, cast: false, adb: true, id: None });
        }
    }
    // Apps announcing themselves: attach to a receiver at the same address, or list them on their own.
    for app in announced.inspect_err(|e| tracing::warn!("app discovery: {e:#}")).unwrap_or_default() {
        let prefix = format!("{}:", app.addr.ip());
        match devices.iter_mut().find(|d| d.addr.starts_with(&prefix)) {
            Some(d) => {
                d.tv_app = true;
                d.id = Some(app.id);
            }
            None => devices.push(Found {
                name: app.name,
                addr: app.addr.to_string(),
                tv_app: true,
                cast: false,
                adb: false,
                id: Some(app.id),
            }),
        }
    }
    let _ = proxy.send_event(Ui::Devices(devices));
}

/// Hosts `adb devices` lists as connected over the network on port 5555.
async fn adb_hosts() -> Vec<String> {
    let Ok(out) = crate::direct::adb(&["devices"], Duration::from_secs(3)).await else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_suffix("\tdevice")?.strip_suffix(":5555").map(str::to_string))
        .collect()
}

/// A device's marketing name ("HUAWEI P10"), else its model, else the address.
async fn adb_name(host: &str) -> String {
    let serial = format!("{host}:5555");
    for prop in ["ro.config.marketing_name", "ro.product.model"] {
        if let Ok(out) = crate::direct::adb(&["-s", &serial, "shell", "getprop", prop], Duration::from_secs(3)).await {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    host.to_string()
}

// Icons for the window: 64 px is sharp at the largest size ui.html draws (26 pt) on Retina.
const SYMBOL_PX: usize = 64;

fn symbol_png(name: &str) -> Option<Vec<u8>> {
    let rgba = symbol_rgba(name)?;
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, SYMBOL_PX as u32, SYMBOL_PX as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().ok()?.write_image_data(&rgba).ok()?;
    Some(png)
}

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
    let image = image.imageWithSymbolConfiguration(&NSImageSymbolConfiguration::configurationWithPointSize_weight(56.0, 0.0))?;
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
    fn ui_symbols_exist_and_render() {
        for name in ["display", "laptopcomputer", "macwindow", "tv", "bolt.fill", "checkmark", "arrow.clockwise", "arrow.triangle.2.circlepath", "power"] {
            assert!(super::symbol_png(name).is_some());
            let rgba = super::symbol_rgba(name).unwrap_or_else(|| panic!("{name} did not render"));
            let drawn = rgba.chunks(4).filter(|p| p[3] > 0).count();
            println!("{name}: {drawn} pixels");
            assert!(drawn > 50, "{name} rendered empty");
        }
    }
}
