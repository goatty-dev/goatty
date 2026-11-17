//! The gateway handshake: what every connection to a host's tailnet port
//! goes through before the goatty protocol.
//!
//! Newline-delimited JSON, one message each way per step:
//!
//! ```text
//! host   → { gateway: 1, goatty, name, key, nonce, commit, protocol }   greeting
//! client → { op: "probe" }                         → { answer: "probe", pairing }, then closed
//! client → { op: "auth", key, nonce, sig }         → { answer: "welcome", sig } → goatty protocol
//! client → { op: "pair", key, nonce, name, sig }   → { answer: "confirm", reveal, sig } … { answer: "paired" }
//!                                                    (or { answer: "refused", reason, code })
//! ```
//!
//! The pairing code comes from both keys, the host's pairing nonce and the
//! client's nonce. The host only *commits* to its pairing nonce in the
//! greeting (`commit` = a hash of its key and the nonce) and reveals it
//! after the client sent its own, so neither side can pick its nonce after
//! seeing the other's. Without that, a device in the middle could try
//! about a million nonces until both screens showed the same code.
//!
//! Every signature covers both public keys and both nonces (fresh per
//! connection), so neither can be replayed: the client's proves it holds
//! a paired key, the host's that it is the host the client pinned. Pairing
//! is only possible while the host's user has the pairing window open; the
//! host adds the device only after its user confirmed the code, and the
//! client pins the host only after its user did.
//!
//! Limit: the gate is authentication, not encryption (WireGuard encrypts
//! the link). Someone able to sit in the middle of a WireGuard connection
//! (a compromised control server handing out its own node key for the
//! host's address) could relay the handshake and then read the stream.

use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};

use crate::identity::{self, Identity};
use crate::pairing::{self, Device, Devices};
use crate::{Paths, hex};

/// The gateway handshake's version (in the greeting).
pub const VERSION: u32 = 1;
/// Longest line either side accepts.
const MAX_LINE: usize = 4096;
/// At most this many pairing requests wait for the host's user at once.
const MAX_PENDING: usize = 3;
/// At most this many pairing attempts per opened window.
const MAX_PAIR_ATTEMPTS: u32 = 10;
/// After this many failed authentications within a minute, refuse every
/// authentication for the rest of that minute.
const MAX_FAILURES_PER_MINUTE: usize = 20;
/// How long a pairing window stays open unless closed earlier.
pub const PAIRING_WINDOW: Duration = Duration::from_secs(5 * 60);

/// The host's first message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Greeting {
    pub gateway: u32,
    /// The host's goatty version.
    pub goatty: String,
    /// The host's name (its hostname).
    pub name: String,
    /// The host's public key (hex).
    pub key: String,
    /// Fresh per connection: what the client signs to authenticate.
    pub nonce: String,
    /// The host's commitment to its pairing nonce ([`commitment`]),
    /// revealed in `Confirm` once the client sent its nonce.
    pub commit: String,
    /// The goatty protocol versions the host's goattyd speaks.
    pub protocol: (u32, u32),
}

/// The commitment to a pairing nonce: SHA-256 over the host's key and the
/// nonce (hex).
pub fn commitment(host_key: &str, pair_nonce: &str) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(transcript(PAIR_COMMIT, host_key, pair_nonce, "", "")))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Hello {
    Probe,
    Auth { key: String, nonce: String, sig: String },
    Pair { key: String, nonce: String, name: String, sig: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
enum Answer {
    Probe {
        pairing: bool,
    },
    Welcome {
        sig: String,
    },
    /// `reveal`: the pairing nonce the greeting committed to.
    Confirm {
        reveal: String,
        sig: String,
    },
    Paired,
    Refused {
        reason: String,
        code: Refusal,
    },
}

/// Why a host refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// This device's key isn't paired with the host (any more).
    NotPaired,
    /// Pairing isn't open on the host.
    PairingClosed,
    /// The host's user rejected the pairing, or the window closed first.
    Rejected,
    /// Too many attempts; try later.
    TooMany,
    BadRequest,
}

/// What went wrong talking to a gateway, for the GUI to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    Refused {
        code: Refusal,
        reason: String,
    },
    /// The host shows another key than the one pinned when pairing.
    HostKeyChanged {
        name: String,
    },
    /// Whatever answered isn't a goatty gateway.
    NotGoatty,
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::Refused { reason, .. } => f.write_str(reason),
            GatewayError::HostKeyChanged { name } => write!(
                f,
                "{name} shows a different goatty key than when you paired: refusing to connect (if it was reinstalled, forget it and pair again)"
            ),
            GatewayError::NotGoatty => f.write_str("no goatty answers on that port"),
        }
    }
}

impl std::error::Error for GatewayError {}

impl GatewayError {
    /// The refusal behind `e`, if a gateway refused.
    pub fn refusal(e: &anyhow::Error) -> Option<Refusal> {
        e.chain().find_map(|c| match c.downcast_ref::<GatewayError>() {
            Some(GatewayError::Refused { code, .. }) => Some(*code),
            _ => None,
        })
    }
}

fn transcript(domain: &str, host_key: &str, host_nonce: &str, client_key: &str, client_nonce: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for part in [domain, host_key, host_nonce, client_key, client_nonce] {
        out.extend_from_slice(&(part.len() as u32).to_le_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out
}

const AUTH: &str = "goatty-gateway-auth-v1";
const AUTH_HOST: &str = "goatty-gateway-auth-host-v1";
const PAIR: &str = "goatty-gateway-pair-v1";
const PAIR_HOST: &str = "goatty-gateway-pair-host-v1";
const PAIR_COMMIT: &str = "goatty-gateway-pair-commit-v1";

fn send<T: Serialize>(w: &mut impl Write, msg: &T) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

/// One line, read a byte at a time so nothing after it is consumed: the
/// goatty protocol that follows a welcome is read by someone else.
fn read_line_unbuffered(r: &mut impl Read) -> Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8];
    loop {
        if r.read(&mut byte)? == 0 {
            bail!("connection closed during the gateway handshake");
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > MAX_LINE {
            bail!("gateway handshake line too long");
        }
    }
    Ok(String::from_utf8(line)?)
}

fn read_line(r: &mut impl BufRead) -> Result<String> {
    let mut line = Vec::new();
    let n = r.take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
    if n == 0 {
        bail!("connection closed during the gateway handshake");
    }
    if line.last() != Some(&b'\n') {
        bail!("gateway handshake line too long");
    }
    line.pop();
    Ok(String::from_utf8(line)?)
}

/// Facts about the host for the greeting.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub name: String,
    pub goatty: String,
    pub protocol: (u32, u32),
}

/// A pairing request waiting for the host's user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPair {
    pub id: u32,
    /// The name the device gave.
    pub name: String,
    pub key: String,
    /// The code its screen shows.
    pub code: String,
}

/// The pairing window, when open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingWindow {
    pub secs_left: u64,
    pub pending: Vec<PendingPair>,
}

#[derive(Default)]
struct PairingState {
    open_until: Option<Instant>,
    attempts: u32,
    pending: Vec<(PendingPair, Option<bool>)>,
    next_id: u32,
}

impl PairingState {
    fn is_open(&self) -> bool {
        self.open_until.is_some_and(|t| Instant::now() < t)
    }
}

/// The host side: answers handshakes, keeps the pairing window and the
/// paired devices.
pub struct Gateway {
    identity: Identity,
    paths: Paths,
    info: HostInfo,
    pairing: Mutex<PairingState>,
    changed: Condvar,
    failures: Mutex<VecDeque<Instant>>,
    /// Held across every read-modify-write of `devices.json`. The number
    /// counts `forget_all`s: an approved pairing that started before one
    /// isn't saved.
    devices: Mutex<u64>,
}

/// The outcome of a handshake that went through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// A paired device: the goatty protocol follows on the stream.
    Device(Device),
    /// A probe or a pairing: the connection is done.
    Done,
}

impl Gateway {
    pub fn new(paths: Paths, info: HostInfo) -> Result<Self> {
        let identity = Identity::load_or_create(&paths.key())?;
        Ok(Self {
            identity,
            paths,
            info,
            pairing: Mutex::default(),
            changed: Condvar::new(),
            failures: Mutex::default(),
            devices: Mutex::new(0),
        })
    }

    pub fn public_key(&self) -> String {
        self.identity.public()
    }

    /// Accept pairing requests for `duration`.
    pub fn open_pairing(&self, duration: Duration) {
        let mut p = self.pairing.lock();
        p.open_until = Some(Instant::now() + duration);
        p.attempts = 0;
        self.changed.notify_all();
    }

    /// Stop accepting pairing requests; waiting ones are rejected.
    pub fn close_pairing(&self) {
        let mut p = self.pairing.lock();
        p.open_until = None;
        for (_, decision) in &mut p.pending {
            decision.get_or_insert(false);
        }
        self.changed.notify_all();
    }

    /// The pairing window, if it's open.
    pub fn pairing(&self) -> Option<PairingWindow> {
        let p = self.pairing.lock();
        let until = p.open_until.filter(|_| p.is_open())?;
        Some(PairingWindow {
            secs_left: until.saturating_duration_since(Instant::now()).as_secs().max(1),
            pending: p.pending.iter().filter(|(_, d)| d.is_none()).map(|(r, _)| r.clone()).collect(),
        })
    }

    /// The host's user confirmed (or rejected) pairing request `id`.
    pub fn decide(&self, id: u32, accept: bool) -> Result<()> {
        let mut p = self.pairing.lock();
        let Some((_, decision)) = p.pending.iter_mut().find(|(r, d)| r.id == id && d.is_none()) else {
            bail!("no pairing request {id} is waiting (it was cancelled, or the window closed)");
        };
        *decision = Some(accept);
        self.changed.notify_all();
        Ok(())
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        Ok(Devices::load(&self.paths.devices())?.devices)
    }

    /// Forget the devices whose key starts with `prefix` (8+ hex digits).
    pub fn revoke(&self, prefix: &str) -> Result<Vec<Device>> {
        let _lock = self.devices.lock();
        let mut d = Devices::load(&self.paths.devices())?;
        let gone = d.revoke(prefix);
        if !gone.is_empty() {
            d.save(&self.paths.devices())?;
        }
        Ok(gone)
    }

    /// Forget every paired device.
    pub fn forget_all(&self) -> Result<()> {
        let mut epoch = self.devices.lock();
        *epoch += 1;
        // Waiting requests are rejected too.
        self.close_pairing();
        match std::fs::remove_file(self.paths.devices()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn failed(&self) {
        self.failures.lock().push_back(Instant::now());
    }

    fn too_many_failures(&self) -> bool {
        let mut f = self.failures.lock();
        while f.front().is_some_and(|t| t.elapsed() > Duration::from_secs(60)) {
            f.pop_front();
        }
        f.len() >= MAX_FAILURES_PER_MINUTE
    }

    /// Run the handshake on a new connection (the caller sets a read
    /// timeout). Reads unbuffered: after [`Accepted::Device`] the stream is
    /// positioned at the goatty protocol.
    pub fn accept(&self, stream: &mut (impl Read + Write)) -> Result<Accepted> {
        let nonce = hex(&crate::random::<32>()?);
        let pair_nonce = hex(&crate::random::<32>()?);
        let host_key = self.identity.public();
        send(
            stream,
            &Greeting {
                gateway: VERSION,
                goatty: self.info.goatty.clone(),
                name: self.info.name.clone(),
                key: host_key.clone(),
                nonce: nonce.clone(),
                commit: commitment(&host_key, &pair_nonce),
                protocol: self.info.protocol,
            },
        )?;
        let hello: Hello = match serde_json::from_str(&read_line_unbuffered(stream)?) {
            Ok(h) => h,
            Err(e) => {
                refuse(stream, Refusal::BadRequest, "not a goatty gateway request")?;
                bail!("bad gateway request: {e}");
            }
        };
        match hello {
            Hello::Probe => {
                send(stream, &Answer::Probe { pairing: self.pairing.lock().is_open() })?;
                Ok(Accepted::Done)
            }
            Hello::Auth { key, nonce: client_nonce, sig } => {
                // A valid signature from a paired key always gets in: failed
                // attempts by others must not lock paired devices out.
                let msg = transcript(AUTH, &host_key, &nonce, &key, &client_nonce);
                let device = identity::verify(&key, &msg, &sig).ok().and_then(|()| {
                    let _lock = self.devices.lock();
                    Devices::load(&self.paths.devices()).ok()?.get(&key).cloned()
                });
                let Some(device) = device else {
                    if self.too_many_failures() {
                        // Slow down whoever is trying (one thread per connection).
                        std::thread::sleep(Duration::from_secs(1));
                        refuse(stream, Refusal::TooMany, "too many failed attempts; try again in a minute")?;
                        bail!("refused an authentication: too many failures");
                    }
                    self.failed();
                    refuse(stream, Refusal::NotPaired, &format!("this device isn't paired with {}", self.info.name))?;
                    bail!("refused key {}: not paired or bad signature", identity::short(&key));
                };
                let sig = self.identity.sign(&transcript(AUTH_HOST, &host_key, &nonce, &key, &client_nonce));
                send(stream, &Answer::Welcome { sig })?;
                Ok(Accepted::Device(device))
            }
            Hello::Pair { key, nonce: client_nonce, name, sig } => {
                let nonces = Nonces { auth: &nonce, pair: &pair_nonce, client: &client_nonce };
                self.pair(stream, &host_key, nonces, &key, &name, &sig)?;
                Ok(Accepted::Done)
            }
        }
    }

    fn pair(&self, stream: &mut (impl Read + Write), host_key: &str, n: Nonces<'_>, key: &str, name: &str, sig: &str) -> Result<()> {
        if identity::verify(key, &transcript(PAIR, host_key, n.auth, key, n.client), sig).is_err() {
            refuse(stream, Refusal::BadRequest, "bad signature")?;
            bail!("pairing request with a bad signature");
        }
        let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
        let code = pairing::code(host_key, n.pair, key, n.client);
        let epoch = *self.devices.lock();
        let id = {
            let mut p = self.pairing.lock();
            if !p.is_open() {
                drop(p);
                let reason = format!("pairing isn't open on {}: choose \"Allow a new device\" there first", self.info.name);
                refuse(stream, Refusal::PairingClosed, &reason)?;
                return Ok(());
            }
            // Decided requests stay until their own connection takes the decision.
            p.attempts += 1;
            let waiting = p.pending.iter().filter(|(_, d)| d.is_none()).count();
            if p.attempts > MAX_PAIR_ATTEMPTS || waiting >= MAX_PENDING {
                drop(p);
                refuse(stream, Refusal::TooMany, "too many pairing requests; open the pairing window again")?;
                return Ok(());
            }
            p.next_id += 1;
            let id = p.next_id;
            p.pending.push((PendingPair { id, name: name.clone(), key: key.into(), code: code.clone() }, None));
            self.changed.notify_all();
            id
        };
        log::info!("pairing request from {name} ({}), code {code}", identity::short(key));
        let sig = self.identity.sign(&transcript(PAIR_HOST, host_key, n.pair, key, n.client));
        let wait = send(stream, &Answer::Confirm { reveal: n.pair.into(), sig }).map(|()| self.wait_for_decision(id));
        let mut p = self.pairing.lock();
        let decision = p.pending.iter().find(|(r, _)| r.id == id).and_then(|(_, d)| *d);
        p.pending.retain(|(r, _)| r.id != id);
        drop(p);
        wait?;
        let saved = (decision == Some(true))
            .then(|| {
                let current = self.devices.lock();
                // Everything was forgotten meanwhile (`Leave`): this approval goes too.
                if *current != epoch {
                    return Ok(false);
                }
                let mut d = Devices::load(&self.paths.devices())?;
                d.add(Device { key: key.into(), name: name.clone(), paired_unix: crate::now_unix() });
                d.save(&self.paths.devices())?;
                anyhow::Ok(true)
            })
            .transpose()?;
        if saved == Some(true) {
            log::info!("paired with {name} ({})", identity::short(key));
            send(stream, &Answer::Paired)?;
        } else {
            refuse(stream, Refusal::Rejected, "the pairing wasn't confirmed on the host")?;
        }
        Ok(())
    }

    /// Until the host's user decides on request `id` or the window closes.
    fn wait_for_decision(&self, id: u32) {
        let mut p = self.pairing.lock();
        loop {
            let decided = p.pending.iter().find(|(r, _)| r.id == id).is_none_or(|(_, d)| d.is_some());
            if decided {
                return;
            }
            let Some(until) = p.open_until.filter(|_| p.is_open()) else { return };
            let _ = self.changed.wait_until(&mut p, until);
        }
    }
}

/// The nonces of one gateway connection.
#[derive(Clone, Copy)]
struct Nonces<'a> {
    /// The greeting's (the client signs it).
    auth: &'a str,
    /// The pairing nonce the greeting committed to.
    pair: &'a str,
    /// The client's.
    client: &'a str,
}

fn refuse(w: &mut impl Write, code: Refusal, reason: &str) -> Result<()> {
    send(w, &Answer::Refused { reason: reason.into(), code })
}

fn read_answer(r: &mut impl BufRead) -> Result<Answer> {
    let line = read_line(r)?;
    serde_json::from_str(&line).map_err(|_| GatewayError::NotGoatty.into())
}

/// Read the host's greeting.
pub fn read_greeting(r: &mut impl BufRead) -> Result<Greeting> {
    let line = read_line(r).map_err(|e| e.context(GatewayError::NotGoatty))?;
    let g: Greeting = serde_json::from_str(&line).map_err(|_| GatewayError::NotGoatty)?;
    if g.gateway != VERSION || !identity::is_public_key(&g.key) {
        bail!(GatewayError::NotGoatty);
    }
    Ok(g)
}

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub greeting: Greeting,
    /// The host's pairing window is open.
    pub pairing: bool,
}

/// Ask a gateway who it is (the connection is done afterwards).
pub fn probe(r: &mut impl BufRead, w: &mut impl Write) -> Result<Probe> {
    let greeting = read_greeting(r)?;
    send(w, &Hello::Probe)?;
    match read_answer(r)? {
        Answer::Probe { pairing } => Ok(Probe { greeting, pairing }),
        other => bail!("unexpected answer to a probe: {other:?}"),
    }
}

/// Authenticate with `identity` to a host whose key must be `pinned`.
/// Afterwards the stream carries the goatty protocol.
pub fn authenticate(r: &mut impl BufRead, w: &mut impl Write, identity: &Identity, pinned: &str) -> Result<Greeting> {
    let g = read_greeting(r)?;
    if g.key != pinned {
        bail!(GatewayError::HostKeyChanged { name: g.name });
    }
    let key = identity.public();
    let nonce = hex(&crate::random::<32>()?);
    let sig = identity.sign(&transcript(AUTH, &g.key, &g.nonce, &key, &nonce));
    send(w, &Hello::Auth { key: key.clone(), nonce: nonce.clone(), sig })?;
    match read_answer(r)? {
        Answer::Welcome { sig } => {
            identity::verify(&g.key, &transcript(AUTH_HOST, &g.key, &g.nonce, &key, &nonce), &sig)
                .context("the host's answer isn't signed with its key")?;
            Ok(g)
        }
        Answer::Refused { reason, code } => bail!(GatewayError::Refused { code, reason }),
        _ => bail!(GatewayError::NotGoatty),
    }
}

/// A pairing in progress on the client side: show [`Self::code`], then
/// [`Self::wait`] for the host's user.
pub struct Pairing {
    pub greeting: Greeting,
    /// The code to compare with the host's screen.
    pub code: String,
}

/// Ask the host to pair with `identity`, introducing this computer as
/// `name`. Returns once the host shows the code.
pub fn start_pairing(r: &mut impl BufRead, w: &mut impl Write, identity: &Identity, name: &str) -> Result<Pairing> {
    let g = read_greeting(r)?;
    let key = identity.public();
    let nonce = hex(&crate::random::<32>()?);
    let sig = identity.sign(&transcript(PAIR, &g.key, &g.nonce, &key, &nonce));
    send(w, &Hello::Pair { key: key.clone(), nonce: nonce.clone(), name: name.into(), sig })?;
    match read_answer(r)? {
        Answer::Confirm { reveal, sig } => {
            // The nonce must be the one the host committed to before it saw ours.
            if commitment(&g.key, &reveal) != g.commit {
                bail!("the host's pairing nonce doesn't match its commitment: refusing to pair");
            }
            identity::verify(&g.key, &transcript(PAIR_HOST, &g.key, &reveal, &key, &nonce), &sig)
                .context("the host's answer isn't signed with its key")?;
            let code = pairing::code(&g.key, &reveal, &key, &nonce);
            Ok(Pairing { greeting: g, code })
        }
        Answer::Refused { reason, code } => bail!(GatewayError::Refused { code, reason }),
        _ => bail!(GatewayError::NotGoatty),
    }
}

impl Pairing {
    /// Until the host's user confirms (`Ok`) or rejects.
    pub fn wait(&self, r: &mut impl BufRead) -> Result<()> {
        match read_answer(r)? {
            Answer::Paired => Ok(()),
            Answer::Refused { reason, code } => bail!(GatewayError::Refused { code, reason }),
            _ => bail!(GatewayError::NotGoatty),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    use super::*;

    fn gateway(dir: &std::path::Path) -> Arc<Gateway> {
        let info = HostInfo { name: "desk".into(), goatty: "0.1.0".into(), protocol: (7, 12) };
        Arc::new(Gateway::new(Paths::new(dir.join("host")), info).unwrap())
    }

    /// Run the host side of one connection in a thread; returns the client
    /// end and the host's outcome.
    fn connect(g: &Arc<Gateway>) -> (BufReader<UnixStream>, UnixStream, std::thread::JoinHandle<Result<Accepted>>) {
        let (client, mut host) = UnixStream::pair().unwrap();
        let g = g.clone();
        let handle = std::thread::spawn(move || {
            let r = g.accept(&mut host);
            if matches!(r, Ok(Accepted::Device(_))) {
                // The protocol follows: echo one line to prove the stream is intact.
                let mut buf = [0u8; 5];
                host.read_exact(&mut buf).unwrap();
                host.write_all(&buf).unwrap();
            }
            r
        });
        let w = client.try_clone().unwrap();
        (BufReader::new(client), w, handle)
    }

    fn pair(g: &Arc<Gateway>, me: &Identity, accept: bool) -> Result<Greeting> {
        let (mut r, mut w, host) = connect(g);
        let p = start_pairing(&mut r, &mut w, me, "laptop")?;
        // The host's user sees the same code.
        let window = g.pairing().unwrap();
        assert_eq!(window.pending.len(), 1);
        assert_eq!(window.pending[0].code, p.code);
        assert_eq!(window.pending[0].name, "laptop");
        g.decide(window.pending[0].id, accept).unwrap();
        let r = p.wait(&mut r).map(|()| p.greeting.clone());
        assert_eq!(host.join().unwrap().unwrap(), Accepted::Done);
        r
    }

    #[test]
    fn probe_answers_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let g = gateway(dir.path());
        let (mut r, mut w, host) = connect(&g);
        let p = probe(&mut r, &mut w).unwrap();
        assert_eq!(p.greeting.name, "desk");
        assert_eq!(p.greeting.key, g.public_key());
        assert_eq!(p.greeting.protocol, (7, 12));
        assert!(!p.pairing);
        assert_eq!(host.join().unwrap().unwrap(), Accepted::Done);
        g.open_pairing(PAIRING_WINDOW);
        let (mut r, mut w, _) = connect(&g);
        assert!(probe(&mut r, &mut w).unwrap().pairing);
    }

    #[test]
    fn pairing_then_authentication() {
        let dir = tempfile::tempdir().unwrap();
        let g = gateway(dir.path());
        let me = Identity::generate().unwrap();

        // Not paired: refused.
        let (mut r, mut w, host) = connect(&g);
        let err = authenticate(&mut r, &mut w, &me, &g.public_key()).unwrap_err();
        assert_eq!(GatewayError::refusal(&err), Some(Refusal::NotPaired));
        assert!(host.join().unwrap().is_err());

        // Pairing while the window is closed: refused.
        let (mut r, mut w, _) = connect(&g);
        let err = start_pairing(&mut r, &mut w, &me, "laptop").err().unwrap();
        assert_eq!(GatewayError::refusal(&err), Some(Refusal::PairingClosed));

        g.open_pairing(PAIRING_WINDOW);
        let err = pair(&g, &me, false).unwrap_err();
        assert_eq!(GatewayError::refusal(&err), Some(Refusal::Rejected));
        assert!(g.devices().unwrap().is_empty());

        let greeting = pair(&g, &me, true).unwrap();
        assert_eq!(greeting.key, g.public_key());
        assert_eq!(g.devices().unwrap()[0].key, me.public());

        // Paired: welcome, and the goatty protocol follows on the same stream.
        let (mut r, mut w, host) = connect(&g);
        authenticate(&mut r, &mut w, &me, &g.public_key()).unwrap();
        w.write_all(b"hello").unwrap();
        let mut back = [0u8; 5];
        r.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"hello");
        let Accepted::Device(d) = host.join().unwrap().unwrap() else { panic!() };
        assert_eq!(d.name, "laptop");

        // Another key pretending to be the host: the client refuses.
        let (mut r, mut w, _) = connect(&g);
        let other = Identity::generate().unwrap();
        let err = authenticate(&mut r, &mut w, &me, &other.public()).unwrap_err();
        assert!(matches!(err.downcast_ref::<GatewayError>(), Some(GatewayError::HostKeyChanged { .. })), "{err:#}");

        // Revoked: refused again.
        assert_eq!(g.revoke(&me.public()[..8]).unwrap().len(), 1);
        let (mut r, mut w, _) = connect(&g);
        let err = authenticate(&mut r, &mut w, &me, &g.public_key()).unwrap_err();
        assert_eq!(GatewayError::refusal(&err), Some(Refusal::NotPaired));
    }

    #[test]
    fn closing_the_window_rejects_waiting_requests() {
        let dir = tempfile::tempdir().unwrap();
        let g = gateway(dir.path());
        g.open_pairing(PAIRING_WINDOW);
        let me = Identity::generate().unwrap();
        let (mut r, mut w, host) = connect(&g);
        let p = start_pairing(&mut r, &mut w, &me, "laptop").unwrap();
        g.close_pairing();
        let err = p.wait(&mut r).unwrap_err();
        assert_eq!(GatewayError::refusal(&err), Some(Refusal::Rejected));
        host.join().unwrap().unwrap();
        assert!(g.pairing().is_none());
        assert!(g.decide(1, true).is_err());
    }

    #[test]
    fn a_window_that_runs_out_rejects_too() {
        let dir = tempfile::tempdir().unwrap();
        let g = gateway(dir.path());
        g.open_pairing(Duration::from_millis(300));
        let me = Identity::generate().unwrap();
        let (mut r, mut w, _) = connect(&g);
        let p = start_pairing(&mut r, &mut w, &me, "laptop").unwrap();
        let err = p.wait(&mut r).unwrap_err();
        assert_eq!(GatewayError::refusal(&err), Some(Refusal::Rejected));
    }

    #[test]
    fn failed_authentications_are_limited() {
        let dir = tempfile::tempdir().unwrap();
        let g = gateway(dir.path());
        let me = Identity::generate().unwrap();
        for _ in 0..MAX_FAILURES_PER_MINUTE {
            let (mut r, mut w, _) = connect(&g);
            let err = authenticate(&mut r, &mut w, &me, &g.public_key()).unwrap_err();
            assert_eq!(GatewayError::refusal(&err), Some(Refusal::NotPaired));
        }
        let (mut r, mut w, _) = connect(&g);
        let err = authenticate(&mut r, &mut w, &me, &g.public_key()).unwrap_err();
        assert_eq!(GatewayError::refusal(&err), Some(Refusal::TooMany));
        // Probes still work.
        let (mut r, mut w, _) = connect(&g);
        probe(&mut r, &mut w).unwrap();
        // And a paired device isn't locked out by someone else's failures.
        let paired = Identity::generate().unwrap();
        g.open_pairing(PAIRING_WINDOW);
        pair(&g, &paired, true).unwrap();
        let (mut r, mut w, host) = connect(&g);
        authenticate(&mut r, &mut w, &paired, &g.public_key()).unwrap();
        w.write_all(b"hello").unwrap();
        assert!(matches!(host.join().unwrap().unwrap(), Accepted::Device(_)));
    }

    #[test]
    fn a_reveal_that_doesnt_match_the_commitment_is_refused() {
        // A "host" that picks its pairing nonce after seeing the client's.
        let (client, mut host) = UnixStream::pair().unwrap();
        let fake = Identity::generate().unwrap();
        let fake_key = fake.public();
        std::thread::spawn(move || {
            let committed = "11".repeat(32);
            let greeting = Greeting {
                gateway: VERSION,
                goatty: "0.1.0".into(),
                name: "desk".into(),
                key: fake_key.clone(),
                nonce: "22".repeat(32),
                commit: commitment(&fake_key, &committed),
                protocol: (7, 12),
            };
            send(&mut host, &greeting).unwrap();
            let Ok(Hello::Pair { key, nonce, .. }) = serde_json::from_str(&read_line_unbuffered(&mut host).unwrap()) else { panic!() };
            let chosen = "33".repeat(32);
            let sig = fake.sign(&transcript(PAIR_HOST, &fake_key, &chosen, &key, &nonce));
            send(&mut host, &Answer::Confirm { reveal: chosen, sig }).unwrap();
        });
        let mut w = client.try_clone().unwrap();
        let mut r = BufReader::new(client);
        let me = Identity::generate().unwrap();
        let err = start_pairing(&mut r, &mut w, &me, "laptop").err().unwrap();
        assert!(err.to_string().contains("commitment"), "{err:#}");
    }

    #[test]
    fn a_decision_waits_for_its_own_connection() {
        let dir = tempfile::tempdir().unwrap();
        let g = gateway(dir.path());
        g.open_pairing(PAIRING_WINDOW);
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (mut ra, mut wa, _) = connect(&g);
        let pa = start_pairing(&mut ra, &mut wa, &a, "a").unwrap();
        let id = g.pairing().unwrap().pending[0].id;
        // Approve a, and have b's request arrive before a's connection reads the decision.
        let mut p = g.pairing.lock();
        p.pending.iter_mut().find(|(r, _)| r.id == id).unwrap().1 = Some(true);
        drop(p);
        let (mut rb, mut wb, _) = connect(&g);
        let _pb = start_pairing(&mut rb, &mut wb, &b, "b").unwrap();
        g.changed.notify_all();
        pa.wait(&mut ra).unwrap();
        assert!(g.devices().unwrap().iter().any(|d| d.key == a.public()));
    }

    #[test]
    fn forgetting_everyone_voids_approvals_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let g = gateway(dir.path());
        g.open_pairing(PAIRING_WINDOW);
        let me = Identity::generate().unwrap();
        let (mut r, mut w, host) = connect(&g);
        let p = start_pairing(&mut r, &mut w, &me, "laptop").unwrap();
        // `Leave` lands between the request and its commit.
        *g.devices.lock() += 1;
        let id = g.pairing().unwrap().pending[0].id;
        g.decide(id, true).unwrap();
        let err = p.wait(&mut r).unwrap_err();
        assert_eq!(GatewayError::refusal(&err), Some(Refusal::Rejected));
        host.join().unwrap().unwrap();
        assert!(g.devices().unwrap().is_empty());
    }

    #[test]
    fn garbage_is_not_a_gateway() {
        let (a, mut b) = UnixStream::pair().unwrap();
        std::thread::spawn(move || b.write_all(b"SSH-2.0-OpenSSH_9.9\r\n"));
        let mut r = BufReader::new(a);
        let err = read_greeting(&mut r).unwrap_err();
        assert!(matches!(err.downcast_ref::<GatewayError>(), Some(GatewayError::NotGoatty)), "{err:#}");
    }
}
