//! One-shot Android TV Remote Service interoperability diagnostic.
//!
//! This example deliberately keeps its generated identity only in memory. It
//! is suitable for proving pairing and one control action, not for ordinary
//! use: the television will require a new pairing code on every invocation.

use std::io::{self, Write};
use std::net::IpAddr;

use chromiacast::android_tv::{
    AndroidTvKeyCode, AndroidTvPairingSession, AndroidTvRemote, AndroidTvRemoteClientInfo,
    AndroidTvRemoteIdentity,
};

#[derive(Debug, Clone, Copy)]
enum Action {
    Back,
    Down,
    Home,
    Left,
    Mute,
    Right,
    Select,
    Settings,
    Up,
    VolumeUp,
    VolumeDown,
    Wake,
    Standby,
    TogglePower,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }

    let mut arguments = std::env::args().skip(1);
    let ip: IpAddr = arguments
        .next()
        .ok_or("usage: android_tv_control IP [ACTION]")?
        .parse()?;
    let action = arguments
        .next()
        .map(|value| parse_action(&value))
        .transpose()?;
    if arguments.next().is_some() {
        return Err("usage: android_tv_control IP [ACTION]".into());
    }

    eprintln!("Generating an ephemeral client identity; this diagnostic must pair on every run.");
    let identity = AndroidTvRemoteIdentity::generate("chromiacast diagnostic")?;
    let pairing = AndroidTvPairingSession::begin(ip, &identity, "chromiacast diagnostic").await?;
    if let Some(name) = pairing.server_name() {
        eprintln!("Pairing with {name}");
    }
    let code = read_line("Enter the six hexadecimal digits shown by the TV: ").await?;
    pairing.finish(code.trim()).await?;

    let remote =
        AndroidTvRemote::connect(ip, &identity, AndroidTvRemoteClientInfo::default()).await?;
    eprintln!(
        "Connected to {} {} (Remote Service {})",
        remote.device_info().vendor(),
        remote.device_info().model(),
        remote.device_info().service_version(),
    );
    eprintln!(
        "Negotiated Remote Service features: {:#x}",
        remote.features().bits()
    );
    if let Some(action) = action {
        execute(&remote, action).await?;
        remote.close().await?;
        eprintln!("Control frame flushed successfully.");
        return Ok(());
    }

    eprintln!(
        "Actions: up, down, left, right, select, home, back, settings, mute, \
         volume-up, volume-down, wake, standby, power, or quit"
    );
    loop {
        let value = read_line("action> ").await?;
        let value = value.trim();
        if matches!(value, "quit" | "exit") {
            break;
        }
        match parse_action(value) {
            Ok(action) => match execute(&remote, action).await {
                Ok(()) => eprintln!("{action:?} frame flushed."),
                Err(error) => eprintln!("{action:?} failed: {error}"),
            },
            Err(error) => eprintln!("{error}"),
        }
    }
    remote.close().await?;
    Ok(())
}

async fn execute(
    remote: &AndroidTvRemote,
    action: Action,
) -> Result<(), chromiacast::android_tv::AndroidTvError> {
    match action {
        Action::Back => remote.press_key(AndroidTvKeyCode::BACK).await?,
        Action::Down => remote.press_key(AndroidTvKeyCode::DPAD_DOWN).await?,
        Action::Home => remote.press_key(AndroidTvKeyCode::HOME).await?,
        Action::Left => remote.press_key(AndroidTvKeyCode::DPAD_LEFT).await?,
        Action::Mute => remote.press_key(AndroidTvKeyCode::VOLUME_MUTE).await?,
        Action::Right => remote.press_key(AndroidTvKeyCode::DPAD_RIGHT).await?,
        Action::Select => remote.press_key(AndroidTvKeyCode::DPAD_CENTER).await?,
        Action::Settings => remote.press_key(AndroidTvKeyCode::SETTINGS).await?,
        Action::Up => remote.press_key(AndroidTvKeyCode::DPAD_UP).await?,
        Action::VolumeUp => remote.press_key(AndroidTvKeyCode::VOLUME_UP).await?,
        Action::VolumeDown => remote.press_key(AndroidTvKeyCode::VOLUME_DOWN).await?,
        Action::Wake => remote.press_wake_key().await?,
        Action::Standby => remote.press_sleep_key().await?,
        Action::TogglePower => remote.press_power_key().await?,
    }
    Ok(())
}

async fn read_line(prompt: &'static str) -> Result<String, Box<dyn std::error::Error>> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let line = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        io::stdin().read_line(&mut line).map(|_| line)
    })
    .await??;
    Ok(line)
}

fn parse_action(value: &str) -> Result<Action, Box<dyn std::error::Error>> {
    let action = match value {
        "back" => Action::Back,
        "down" => Action::Down,
        "home" => Action::Home,
        "left" => Action::Left,
        "mute" => Action::Mute,
        "right" => Action::Right,
        "select" | "ok" => Action::Select,
        "settings" => Action::Settings,
        "up" => Action::Up,
        "volume-up" => Action::VolumeUp,
        "volume-down" => Action::VolumeDown,
        "wake" => Action::Wake,
        "standby" => Action::Standby,
        "power" => Action::TogglePower,
        _ => {
            return Err(format!(
                "unknown action {value:?}; expected up, down, left, right, select, home, \
                 back, settings, mute, volume-up, volume-down, wake, standby, or power"
            )
            .into())
        }
    };
    Ok(action)
}
