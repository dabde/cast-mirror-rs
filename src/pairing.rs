//! Direct mode without ADB: find the app on the network, pair once with a code it shows, then start
//! each session over the network.
//!
//! The app announces `_castmirror._udp` (TXT `id`, `name`) while it waits. Messages go to its UDP port as
//! "CA" + type; it answers on the same socket with the type byte first:
//! - pair: 'p' mac id(16) | mac public(65) | mac nonce(16) | mac name  →  'p' device id(16) | app public(65) |
//!   app nonce(16) | HMAC(lt, "app")(32) | device name; then 'q' HMAC(lt, "mac")  →  'q' 1
//! - session: 's' mac id | nonce m(16)  →  's' device id | nonce a(16) | HMAC(lt, "s2" m a), or 'u' device id
//!   (not paired); then 't' HMAC(lt, "s3" m a)  →  't' 1. Session key: HMAC(lt, "key" m a)[..16].
//!
//! lt, the long-term key both sides store: HMAC(PBKDF2(code, m ‖ a, 100 000), "cast-mirror pairing" ‖ ECDH ‖
//! mac public ‖ app public). The code is in it, so a man in the middle has to guess the code, and each
//! guess costs the PBKDF2 rounds.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ring::agreement::{agree_ephemeral, EphemeralPrivateKey, UnparsedPublicKey, ECDH_P256};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

pub const SERVICE: &str = "_castmirror._udp.local.";
const ROUNDS: u32 = 100_000;
const TRIES: u32 = 4;
const WAIT: Duration = Duration::from_millis(700);

/// This Mac's id and the devices it paired with, in Application Support.
// ponytail: a 0600 JSON file, not the Keychain; move the keys there if the Mac is shared.
#[derive(Serialize, Deserialize, Default)]
pub struct Store {
    mac_id: String,
    devices: BTreeMap<String, Paired>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Paired {
    pub name: String,
    key: String,
}

impl Store {
    fn path() -> PathBuf {
        std::env::home_dir().unwrap_or_default().join("Library/Application Support/CastMirror/paired.json")
    }

    pub fn load() -> Self {
        let mut store: Self =
            std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if store.mac_id.len() != 32 {
            store.mac_id = hex(&random::<16>());
        }
        store
    }

    pub fn save(&self) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&path)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn is_paired(&self, device_id: &str) -> bool {
        self.devices.contains_key(device_id)
    }

    fn mac_id(&self) -> [u8; 16] {
        unhex(&self.mac_id).unwrap_or_default()
    }
}

/// An app announcing itself on the network.
pub struct Announced {
    pub id: String,
    pub name: String,
    pub addr: SocketAddr,
}

/// Apps that answer the mDNS browse within `wait`.
pub async fn discover(wait: Duration) -> Result<Vec<Announced>> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let events = daemon.browse(SERVICE)?;
    let mut found: Vec<Announced> = Vec::new();
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv_async()).await {
        if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
            let (Some(id), Some(ip)) = (info.get_property_val_str("id"), info.get_addresses_v4().into_iter().next())
            else {
                continue;
            };
            if found.iter().all(|a| a.id != id) {
                let name = info.get_property_val_str("name").unwrap_or(info.get_fullname()).to_string();
                found.push(Announced { id: id.to_string(), name, addr: SocketAddr::new(ip.into(), info.get_port()) });
            }
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}

/// A pairing request the app answered; `finish` checks the code the user read off the device.
pub struct Pending {
    socket: UdpSocket,
    mac_public: Vec<u8>,
    mac_nonce: [u8; 16],
    secret: Vec<u8>,
    device_id: [u8; 16],
    app_public: Vec<u8>,
    app_nonce: [u8; 16],
    app_confirm: [u8; 32],
    pub device_name: String,
}

/// Asks the app at `addr` to pair; it then shows a code.
pub async fn begin(addr: SocketAddr, store: &Store) -> Result<Pending> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(addr).await?;
    let rng = SystemRandom::new();
    let private = EphemeralPrivateKey::generate(&ECDH_P256, &rng).map_err(|_| anyhow!("key generation"))?;
    let mac_public = private.compute_public_key().map_err(|_| anyhow!("public key"))?.as_ref().to_vec();
    let mac_nonce = random::<16>();
    let host = hostname();
    let request = [b"CAp".as_slice(), &store.mac_id(), &mac_public, &mac_nonce, host.as_bytes()].concat();
    let reply = exchange(&socket, &request, b'p').await.context("the app did not answer the pairing request")?;
    if reply.len() < 1 + 16 + 65 + 16 + 32 {
        bail!("short pairing reply");
    }
    let app_public = reply[17..82].to_vec();
    let secret = agree_ephemeral(private, &UnparsedPublicKey::new(&ECDH_P256, &app_public), |s| s.to_vec())
        .map_err(|_| anyhow!("the app sent an invalid public key"))?;
    Ok(Pending {
        socket,
        mac_public,
        mac_nonce,
        secret,
        device_id: reply[1..17].try_into().unwrap(),
        app_public,
        app_nonce: reply[82..98].try_into().unwrap(),
        app_confirm: reply[98..130].try_into().unwrap(),
        device_name: String::from_utf8_lossy(&reply[130..]).into_owned(),
    })
}

impl Pending {
    /// Checks `code`; on success both sides store the key. A wrong code leaves the app waiting for a new request.
    pub async fn finish(self, code: &str, store: &mut Store) -> Result<String> {
        let lt = long_term(code, &self.secret, &self.mac_public, &self.app_public, &self.mac_nonce, &self.app_nonce);
        if !constant_eq(&mac(&lt, &[b"app"]), &self.app_confirm) {
            bail!("wrong code");
        }
        let confirm = [b"CAq".as_slice(), &mac(&lt, &[b"mac"])].concat();
        exchange(&self.socket, &confirm, b'q').await.context("the app did not confirm the pairing")?;
        let id = hex(&self.device_id);
        store.devices.insert(id.clone(), Paired { name: self.device_name.clone(), key: hex(&lt) });
        store.save()?;
        Ok(id)
    }
}

/// Why a session could not start over the network.
#[derive(Debug)]
pub enum NoSession {
    /// Nothing answered: the app isn't open.
    NoAnswer,
    /// The app answered but doesn't know this Mac (or we lost its key): pair first.
    NotPaired(String),
}

impl std::fmt::Display for NoSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAnswer => f.write_str("the app is not open on the device"),
            Self::NotPaired(name) => write!(f, "{name} is not paired with this Mac"),
        }
    }
}

/// Agrees on a session key with the paired app on `socket` (connected to it).
pub async fn session(socket: &UdpSocket, store: &Store) -> std::result::Result<[u8; 16], NoSession> {
    let nonce_m = random::<16>();
    let hello = [b"CAs".as_slice(), &store.mac_id(), &nonce_m].concat();
    let reply = exchange(socket, &hello, b's').await.map_err(|_| NoSession::NoAnswer)?;
    let device = reply.get(1..17).map(hex).unwrap_or_default();
    let not_paired = || NoSession::NotPaired(store.devices.get(&device).map_or(device.clone(), |p| p.name.clone()));
    let (Some(paired), true) = (store.devices.get(&device), reply[0] == b's' && reply.len() >= 1 + 16 + 16 + 32) else {
        return Err(not_paired());
    };
    let lt = unhex::<32>(&paired.key).ok_or_else(not_paired)?;
    let nonce_a = &reply[17..33];
    if !constant_eq(&mac(&lt, &[b"s2", &nonce_m, nonce_a]), &reply[33..65]) {
        return Err(not_paired());
    }
    let proof = [b"CAt".as_slice(), &mac(&lt, &[b"s3", &nonce_m, nonce_a])].concat();
    exchange(socket, &proof, b't').await.map_err(|_| NoSession::NoAnswer)?;
    Ok(mac(&lt, &[b"key", &nonce_m, nonce_a])[..16].try_into().unwrap())
}

/// Sends `request` until a reply starting with `kind` (or 'u' for a session) arrives; UDP may drop either.
async fn exchange(socket: &UdpSocket, request: &[u8], kind: u8) -> Result<Vec<u8>> {
    let mut buf = [0u8; 2048];
    for _ in 0..TRIES {
        socket.send(request).await?;
        // Video feedback from an earlier session may still arrive; skip anything else.
        let deadline = tokio::time::Instant::now() + WAIT;
        while let Ok(Ok(n)) = tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
            if n > 0 && (buf[0] == kind || (kind == b's' && buf[0] == b'u')) {
                return Ok(buf[..n].to_vec());
            }
        }
    }
    bail!("no answer")
}

fn long_term(code: &str, secret: &[u8], mac_public: &[u8], app_public: &[u8], m: &[u8], a: &[u8]) -> [u8; 32] {
    let mut code_key = [0u8; 32];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(ROUNDS).unwrap(),
        &[m, a].concat(),
        normalize(code).as_bytes(),
        &mut code_key,
    );
    mac(&code_key, &[b"cast-mirror pairing", secret, mac_public, app_public])
}

/// The app shows the code as XXXXX-XXXXX in Crockford base32; accept any case, dashes, spaces, O for 0, I/L for 1.
pub fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

fn mac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut ctx = hmac::Context::with_key(&key);
    for p in parts {
        ctx.update(p);
    }
    ctx.sign().as_ref().try_into().unwrap()
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    SystemRandom::new().fill(&mut out).expect("system randomness");
    out
}

fn hostname() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let bytes: Option<Vec<u8>> =
        (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect();
    bytes?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_normalize_like_crockford() {
        assert_eq!(normalize("ab1o0-iLz9 "), "AB10011Z9");
        assert_eq!(normalize("7k3qd-m0xpa"), "7K3QDM0XPA");
        assert_eq!(normalize("O I L"), "011");
    }

    #[test]
    fn long_term_key_depends_on_code_and_both_publics() {
        let base = long_term("7K3QD-M0XPA", b"secret", b"mac", b"app", b"m", b"a");
        assert_eq!(base, long_term("7k3qdm0xpa", b"secret", b"mac", b"app", b"m", b"a"), "spelling doesn't matter");
        assert_ne!(base, long_term("7K3QD-M0XPB", b"secret", b"mac", b"app", b"m", b"a"));
        assert_ne!(base, long_term("7K3QD-M0XPA", b"secret", b"app", b"mac", b"m", b"a"));
        assert_ne!(base, long_term("7K3QD-M0XPA", b"secret", b"mac", b"app", b"a", b"m"));
    }

    #[test]
    fn hex_round_trips() {
        let id = random::<16>();
        assert_eq!(unhex::<16>(&hex(&id)), Some(id));
        assert_eq!(unhex::<16>("zz"), None);
    }
}
