//! The client side of goatty over the user's tailnet: connect to a paired
//! host ([`connect`], behind [`crate::Endpoint::Tailnet`]), find the peers
//! that run goatty ([`discover`]) and pair with one ([`Pairing`]).
//!
//! Outbound connections use plain TCP to the peer's tailnet address when
//! Tailscale has a TUN device, else `tailscale nc` (`goatty_net::link`).

use std::io::{BufReader, Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use goatty_net::gateway::{self, GatewayError, Refusal};
use goatty_net::identity::Identity;
use goatty_net::link::{self, Deadline, Mode};
use goatty_net::pairing::{KnownHost, KnownHosts};
use goatty_net::tailscale::{Node, State, Status, Tailscale};
use goatty_net::{DEFAULT_PORT, Paths};
use parking_lot::Mutex;

use crate::transport::{CloseHandle, Closer, StderrTail, TailnetTarget, Transport};

/// How long connecting plus the gateway handshake may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where `node` can be reached, and how: from the live status when
/// Tailscale runs, else the address it had when it paired.
fn route(ts: &Tailscale, dns: &str, fallback: Option<&KnownHost>, name: &str) -> Result<(Mode, String)> {
    match ts.detect() {
        State::Running(status) => {
            let peer = status.peer(dns);
            if let Some(p) = peer.filter(|p| !p.online) {
                bail!("{} is offline on your tailnet", p.name);
            }
            let addr = peer.and_then(Node::addr).map(String::from).or_else(|| fallback.map(|h| h.addr.clone()));
            let addr = addr.with_context(|| format!("{name} isn't on your tailnet any more"))?;
            Ok((Mode::for_tun(status.tun), addr))
        }
        other => bail!("{}", other.describe()),
    }
}

/// Open a transport to a paired host: the gateway handshake (with this
/// computer's device key; the host must show the key pinned when pairing),
/// then the goatty protocol.
pub(crate) fn connect(t: &TailnetTarget) -> Result<Transport> {
    let paths = t.paths.as_ref().context("no $HOME: goatty keeps this computer's tailnet key there")?;
    let hosts = KnownHosts::load(&paths.hosts())?;
    let Some(host) = hosts.get(&t.dns).cloned() else {
        bail!(GatewayError::Refused {
            code: Refusal::NotPaired,
            reason: format!("this computer isn't paired with {}: pair it first", t.name),
        });
    };
    let identity = Identity::load_or_create(&paths.key())?;
    let (mode, addr) = route(&t.tailscale, &t.dns, Some(&host), &t.name)?;
    let l = link::open(&t.tailscale, mode, &addr, host.port, CONNECT_TIMEOUT)?;
    let stderr = l.stderr.clone();
    let mut reader = BufReader::with_capacity(256 << 10, l.reader);
    let mut writer = l.writer;
    {
        let _deadline = Deadline::start(&l.closer, CONNECT_TIMEOUT);
        if let Err(e) = gateway::authenticate(&mut reader, &mut writer, &identity, &host.key) {
            l.closer.close();
            let said = stderr.lock().last().cloned();
            return Err(match said {
                Some(line) if e.downcast_ref::<GatewayError>().is_none() => e.context(format!("tailscale nc: {line}")),
                _ => e,
            });
        }
    }
    Ok(Transport {
        reader,
        writer,
        closer: Closer(std::sync::Arc::new(Mutex::new(Some(CloseHandle::Net(l.closer))))),
        stderr: StderrTail::default(),
    })
}

/// The pinned hosts (hosts this computer paired with).
pub fn known_hosts(paths: &Paths) -> Vec<KnownHost> {
    KnownHosts::load(&paths.hosts()).map(|h| h.hosts).unwrap_or_default()
}

/// Forget a paired host on this computer (the host keeps this device until
/// it's revoked there).
pub fn forget(paths: &Paths, dns: &str) -> Result<bool> {
    let mut hosts = KnownHosts::load(&paths.hosts())?;
    let removed = hosts.remove(dns);
    if removed {
        hosts.save(&paths.hosts())?;
    }
    Ok(removed)
}

/// A peer found on the tailnet that runs goatty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub node: Node,
    pub probe: gateway::Probe,
    /// This computer paired with it (and the key still matches).
    pub paired: bool,
    /// It shows another key than the one pinned: reinstalled, or not the
    /// same host.
    pub key_changed: bool,
}

/// What discovery saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub tailscale: State,
    pub found: Vec<Found>,
    /// Discovery could tell the user's own devices from others' (it can't
    /// from a tagged node, and then lists everyone's).
    pub owner_known: bool,
}

/// The online peers that answer as a goatty gateway on `port` (the user's
/// own unless `all_users`). Probes in parallel; takes up to a few seconds.
pub fn discover(ts: &Tailscale, paths: &Paths, port: u16, all_users: bool) -> Discovery {
    let state = ts.detect();
    let Some(status) = state.running() else { return Discovery { tailscale: state, found: Vec::new(), owner_known: true } };
    let hosts = known_hosts(paths);
    let found = goatty_net::discovery::discover(ts, status, port, all_users)
        .into_iter()
        .map(|p| {
            let pinned = hosts.iter().find(|h| h.dns == p.node.dns);
            Found {
                paired: pinned.is_some_and(|h| h.key == p.probe.greeting.key),
                key_changed: pinned.is_some_and(|h| h.key != p.probe.greeting.key),
                node: p.node,
                probe: p.probe,
            }
        })
        .collect();
    Discovery { owner_known: goatty_net::discovery::owner_known(status), tailscale: state.clone(), found }
}

/// A pairing in progress: show [`Self::code`], let the host's user confirm
/// it there ([`Self::wait`]), and once this computer's user confirmed the
/// same code here, [`Self::pin`] the host.
pub struct Pairing {
    pub code: String,
    /// The host's name as it introduced itself.
    pub host_name: String,
    node: Node,
    port: u16,
    key: String,
    paths: Paths,
    reader: BufReader<Box<dyn Read + Send>>,
    _writer: Box<dyn Write + Send>,
    closer: link::Closer,
    inner: gateway::Pairing,
}

impl Pairing {
    /// Ask `node` to pair (its pairing window must be open), introducing
    /// this computer as `my_name`. Returns once both sides know the code.
    pub fn start(ts: &Tailscale, paths: &Paths, status: &Status, node: &Node, port: u16, my_name: &str) -> Result<Self> {
        let identity = Identity::load_or_create(&paths.key())?;
        let addr = node.addr().with_context(|| format!("{} has no tailnet address", node.name))?;
        let l = link::open(ts, Mode::for_tun(status.tun), addr, port, CONNECT_TIMEOUT)?;
        let mut reader = BufReader::new(l.reader);
        let mut writer = l.writer;
        let inner = {
            let _deadline = Deadline::start(&l.closer, CONNECT_TIMEOUT);
            gateway::start_pairing(&mut reader, &mut writer, &identity, my_name)
        };
        let inner = match inner {
            Ok(p) => p,
            Err(e) => {
                // No `Pairing` to drop: end `tailscale nc` here.
                l.closer.close();
                return Err(e);
            }
        };
        Ok(Self {
            code: inner.code.clone(),
            host_name: inner.greeting.name.clone(),
            key: inner.greeting.key.clone(),
            node: node.clone(),
            port,
            paths: paths.clone(),
            reader,
            _writer: writer,
            closer: l.closer,
            inner,
        })
    }

    /// Until the host's user confirms the code (`Ok`) or rejects it, or the
    /// host's pairing window closes.
    pub fn wait(&mut self) -> Result<()> {
        self.inner.wait(&mut self.reader)
    }

    /// A handle that ends the pairing from another thread (the user cancelled).
    pub fn canceller(&self) -> impl Fn() + Send + Sync + 'static {
        let closer = self.closer.clone();
        move || closer.close()
    }

    /// Remember the host and its key (after both users confirmed).
    pub fn pin(&self) -> Result<KnownHost> {
        let host = KnownHost {
            dns: self.node.dns.clone(),
            name: self.node.name.clone(),
            addr: self.node.addr().unwrap_or_default().to_string(),
            port: self.port,
            key: self.key.clone(),
            paired_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        };
        let mut hosts = KnownHosts::load(&self.paths.hosts())?;
        hosts.add(host.clone());
        hosts.save(&self.paths.hosts())?;
        Ok(host)
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.closer.close();
    }
}

/// The port to use for `dns`: the one it paired on, else the default.
pub fn port_of(paths: &Paths, dns: &str) -> u16 {
    known_hosts(paths).into_iter().find(|h| h.dns == dns).map_or(DEFAULT_PORT, |h| h.port)
}

/// This computer's name to introduce itself with when pairing.
pub fn my_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "goatty".into())
}
