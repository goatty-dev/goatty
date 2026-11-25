//! The client side of goatty over the user's tailnet: connect to a paired
//! host ([`connect`], behind [`crate::Endpoint::Tailnet`]), find the peers
//! that run goatty ([`discover`]) and pair with one ([`Pairing`]).
//!
//! Outbound connections use plain TCP to the peer's tailnet address when
//! Tailscale has a TUN device, else `tailscale nc` (`goatty_net::link`).

use std::io::{BufReader, Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use goatty_net::api::{NetStatus, Request, RouteStatus};
use goatty_net::gateway::{self, GatewayError, Refusal};
use goatty_net::identity::Identity;
use goatty_net::link::{self, Deadline, Mode};
use goatty_net::pairing::{KnownHost, KnownHosts, PairToken};
use goatty_net::tailscale::{Node, Permission, State, Status, Tailscale, parse_host_port};
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
            let peer = status.peer(dns).or_else(|| status.find_peer(dns));
            if let Some(p) = peer.filter(|p| !p.online) {
                bail!("{} is offline on your tailnet", p.name);
            }
            let addr = peer.and_then(Node::addr).map(String::from).or_else(|| fallback.map(|h| h.addr.clone()));
            let addr = addr.filter(|a| !a.is_empty()).with_context(|| format!("{name} isn't on your tailnet any more"))?;
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
    let Some(host) = hosts.get(&t.dns).or_else(|| hosts.find(&t.dns)).cloned() else {
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

/// Look up a paired host by MagicDNS name, friendly host name, or Tailscale IP.
pub fn find_known_host(paths: &Paths, query: &str) -> Option<KnownHost> {
    KnownHosts::load(&paths.hosts()).ok()?.find(query).cloned()
}

/// Forget a paired host on this computer (the host keeps this device until
/// it's revoked there). Accepts MagicDNS name, friendly name, or Tailscale IP.
pub fn forget(paths: &Paths, target: &str) -> Result<bool> {
    let mut hosts = KnownHosts::load(&paths.hosts())?;
    let key = hosts.find(target).map(|h| h.dns.clone()).unwrap_or_else(|| target.to_string());
    let removed = hosts.remove(&key);
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

    /// Start interactive 6-digit code pairing with `target` — a Tailscale host
    /// name, MagicDNS name, or Tailscale IP, with optional `:port`. Works even
    /// when discovery didn't find the host (tagged VPS, custom port, discovery off).
    pub fn start_target(ts: &Tailscale, paths: &Paths, target: &str, my_name: &str) -> Result<Self> {
        let (host, port_override) = parse_host_port(target);
        if host.is_empty() {
            bail!("enter a Tailscale host name, IP, or pairing code");
        }
        let state = ts.detect();
        let status = state.running().ok_or_else(|| anyhow::anyhow!("{}", state.describe()))?;
        let node = match status.find_peer(&host) {
            Some(p) if !p.online => bail!("{} is offline on your tailnet", p.name),
            Some(p) => p.clone(),
            None => Node {
                name: host.split('.').next().unwrap_or(&host).to_string(),
                dns: host.clone(),
                addrs: vec![host.clone()],
                os: String::new(),
                user_id: 0,
                user: None,
                online: true,
                tagged: false,
                direct: false,
                relay: String::new(),
            },
        };
        let port = port_override.unwrap_or_else(|| port_of(paths, node.host_id()));
        Self::start(ts, paths, status, &node, port, my_name)
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
        let dns = self.node.host_id().to_string();
        let name =
            if self.node.name.is_empty() || (self.node.addrs.first().is_some_and(|a| *a == self.node.name) && !self.host_name.is_empty()) {
                self.host_name.clone()
            } else {
                self.node.name.clone()
            };
        pin_host(
            &self.paths,
            KnownHost {
                dns,
                name,
                addr: self.node.addr().unwrap_or_default().to_string(),
                port: self.port,
                key: self.key.clone(),
                paired_unix: now_unix(),
            },
        )
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.closer.close();
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn pin_host(paths: &Paths, host: KnownHost) -> Result<KnownHost> {
    let mut hosts = KnownHosts::load(&paths.hosts())?;
    hosts.add(host.clone());
    hosts.save(&paths.hosts())?;
    Ok(host)
}

/// Pair with a host in one step using a one-time pairing code (`goatty-pair:…`)
/// printed by `goattyd net pair` or Settings → Network on that host.
pub fn pair_with_token(ts: &Tailscale, paths: &Paths, token_str: &str, my_name: &str) -> Result<KnownHost> {
    let token = PairToken::decode(token_str)?;
    let state = ts.detect();
    let status = state.running().ok_or_else(|| anyhow::anyhow!("{}", state.describe()))?;
    let peer = if !token.dns.is_empty() { status.find_peer(&token.dns) } else { None }
        .or_else(|| (!token.addr.is_empty()).then(|| status.find_peer(&token.addr)).flatten());
    if let Some(p) = peer.filter(|p| !p.online) {
        bail!("{} is offline on your tailnet", p.name);
    }
    let addr = peer
        .and_then(Node::addr)
        .map(String::from)
        .or_else(|| (!token.addr.is_empty()).then(|| token.addr.clone()))
        .or_else(|| (!token.dns.is_empty()).then(|| token.dns.clone()))
        .context("pairing code has no reachable tailnet address")?;
    let identity = Identity::load_or_create(&paths.key())?;
    let l = link::open(ts, Mode::for_tun(status.tun), &addr, token.port, CONNECT_TIMEOUT)?;
    let stderr = l.stderr.clone();
    let mut reader = BufReader::new(l.reader);
    let mut writer = l.writer;
    let greeting = {
        let _deadline = Deadline::start(&l.closer, CONNECT_TIMEOUT);
        gateway::pair_with_token(&mut reader, &mut writer, &identity, my_name, &token)
    };
    l.closer.close();
    let greeting = greeting.map_err(|e| {
        let said = stderr.lock().last().cloned();
        match said {
            Some(line) if e.downcast_ref::<GatewayError>().is_none() => e.context(format!("tailscale nc: {line}")),
            _ => e,
        }
    })?;
    let dns = peer.map(|p| p.host_id().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| token.host_id().to_string());
    let name = peer
        .map(|p| p.name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if !greeting.name.is_empty() { greeting.name.clone() } else { token.display_name().to_string() });
    pin_host(paths, KnownHost { dns, name, addr, port: token.port, key: greeting.key, paired_unix: now_unix() })
}

/// Outcome of pairing a remote host through an already-authenticated control
/// channel (such as an SSH connection to its `goattyd`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectPairOutcome {
    /// Tailscale Serve is active on the remote host, this computer's device
    /// key is authorized there, and the host is pinned locally.
    Paired {
        host: KnownHost,
        /// `Ok(summary)` when this computer reached the host over Tailscale
        /// right now (e.g. `"direct"` or `"relay fra"`), or `Err(note)` when
        /// pairing succeeded but the tailnet link couldn't be verified yet.
        reachability: std::result::Result<String, String>,
    },
    /// Tailscale on the remote host denied adding the Serve route until the
    /// user grants operator permission (or adds the route manually).
    NeedsPermission { host_name: String, permission: Permission },
}

/// Pair a remote host over an already-authenticated `Net` RPC channel (for
/// example, an existing SSH connection to its `goattyd`):
///
/// 1. Check Tailscale on the remote host and enable its Serve route if off.
/// 2. Authorize this computer's device key on the remote host (falling back
///    to an SSH-verified pairing window for older remote `goattyd` builds).
/// 3. Pin the host locally in `hosts.json` and check tailnet reachability.
pub fn pair_via_remote(
    mut net_rpc: impl FnMut(&Request) -> Result<NetStatus>,
    ts: &Tailscale,
    paths: &Paths,
    my_name: &str,
) -> Result<DirectPairOutcome> {
    let mut status = net_rpc(&Request::Status)?;
    let Some(remote_ts) = status.tailscale.running().cloned() else {
        let extra = if matches!(status.tailscale, State::NotInstalled) {
            " (install Tailscale and log in on that host first: https://tailscale.com/download)"
        } else {
            ""
        };
        bail!("{} on the remote host{extra}", status.tailscale.describe());
    };
    let remote_name = if remote_ts.me.name.is_empty() { remote_ts.me.host_id().to_string() } else { remote_ts.me.name.clone() };
    if !status.available || status.route != RouteStatus::Active {
        match net_rpc(&Request::Enable { port: None }) {
            Ok(s) => status = s,
            Err(e) => {
                if let Ok(after) = net_rpc(&Request::Status)
                    && let RouteStatus::Permission(permission) = after.route
                {
                    return Ok(DirectPairOutcome::NeedsPermission { host_name: remote_name, permission });
                }
                return Err(e);
            }
        }
    }
    let remote_ts = status.tailscale.running().context("Tailscale stopped on the remote host")?;
    let remote_dns = remote_ts.me.host_id().to_string();
    let remote_addr = remote_ts.me.addr().unwrap_or_default().to_string();
    let remote_name = if remote_ts.me.name.is_empty() { remote_dns.clone() } else { remote_ts.me.name.clone() };
    let remote_port = status.port;
    let remote_key = status.key.clone();
    if remote_dns.is_empty() && remote_addr.is_empty() {
        bail!("{remote_name} has no Tailscale address yet");
    }

    let identity = Identity::load_or_create(&paths.key())?;
    let my_key = identity.public();
    if let Err(e) = net_rpc(&Request::AuthorizeDevice { key: my_key.clone(), name: my_name.to_string() }) {
        if format!("{e:#}").contains("bad tailnet request") {
            // Older remote goattyd without `AuthorizeDevice`: verify the key
            // and 6-digit code across the authenticated SSH channel instead.
            pair_via_window_fallback(&mut net_rpc, ts, paths, my_name, &remote_dns, &remote_addr, &remote_name, remote_port, &remote_key)?;
        } else {
            return Err(e);
        }
    }

    let host = pin_host(
        paths,
        KnownHost {
            dns: remote_dns.clone(),
            name: remote_name.clone(),
            addr: remote_addr.clone(),
            port: remote_port,
            key: remote_key.clone(),
            paired_unix: now_unix(),
        },
    )?;

    let reachability = check_reachability(ts, &remote_dns, &remote_addr, remote_port, &remote_key);
    Ok(DirectPairOutcome::Paired { host, reachability })
}

#[allow(clippy::too_many_arguments)]
fn pair_via_window_fallback(
    net_rpc: &mut impl FnMut(&Request) -> Result<NetStatus>,
    ts: &Tailscale,
    paths: &Paths,
    my_name: &str,
    remote_dns: &str,
    remote_addr: &str,
    remote_name: &str,
    remote_port: u16,
    remote_key: &str,
) -> Result<()> {
    let local_state = ts.detect();
    let local_ts = local_state.running().ok_or_else(|| {
        anyhow::anyhow!("the remote goattyd is older and needs Tailscale running here to complete pairing ({})", local_state.describe())
    })?;
    let node = local_ts.find_peer(remote_dns).or_else(|| local_ts.find_peer(remote_addr)).cloned().unwrap_or_else(|| Node {
        name: remote_name.to_string(),
        dns: remote_dns.to_string(),
        addrs: if remote_addr.is_empty() { Vec::new() } else { vec![remote_addr.to_string()] },
        os: String::new(),
        user_id: 0,
        user: None,
        online: true,
        tagged: false,
        direct: false,
        relay: String::new(),
    });
    net_rpc(&Request::OpenPairing { secs: Some(60) })?;
    let result = (|| -> Result<()> {
        let mut pairing = Pairing::start(ts, paths, local_ts, &node, remote_port, my_name)?;
        if pairing.key != remote_key {
            bail!(GatewayError::HostKeyChanged { name: remote_name.to_string() });
        }
        let my_key = Identity::load_or_create(&paths.key())?.public();
        let pending = net_rpc(&Request::Status)?
            .pairing
            .and_then(|w| w.pending.into_iter().find(|p| p.key == my_key && p.code == pairing.code))
            .context("pairing request didn't appear on the remote host")?;
        net_rpc(&Request::Decide { id: pending.id, accept: true })?;
        pairing.wait()
    })();
    let _ = net_rpc(&Request::ClosePairing);
    result
}

fn check_reachability(ts: &Tailscale, dns: &str, addr: &str, port: u16, expected_key: &str) -> std::result::Result<String, String> {
    let state = ts.detect();
    let Some(status) = state.running() else {
        return Err(format!("paired, ready once Tailscale runs here ({})", state.describe()));
    };
    let peer = status.find_peer(dns).or_else(|| status.find_peer(addr));
    let Some(connect_addr) = peer.and_then(Node::addr).or((!addr.is_empty()).then_some(addr)) else {
        return Err("paired, but no tailnet address was found for it here".into());
    };
    match goatty_net::discovery::probe(ts, Mode::for_tun(status.tun), connect_addr, port) {
        Ok(p) if p.greeting.key == expected_key => {
            let via = match peer {
                Some(n) if n.direct => "direct".to_string(),
                Some(n) if !n.relay.is_empty() => format!("relay {}", n.relay),
                _ => "tailnet".to_string(),
            };
            Ok(via)
        }
        Ok(_) => Err("a different goatty key answered on that tailnet address".into()),
        Err(e) => Err(format!("paired, but couldn't reach it over Tailscale right now ({e:#})")),
    }
}

/// The port to use for `dns`: the one it paired on, else the default.
pub fn port_of(paths: &Paths, dns: &str) -> u16 {
    known_hosts(paths).into_iter().find(|h| h.dns == dns).map_or(DEFAULT_PORT, |h| h.port)
}

/// This computer's name to introduce itself with when pairing.
pub fn my_name() -> String {
    Some(crate::hostname()).filter(|s| !s.is_empty()).or_else(|| std::env::var("HOSTNAME").ok()).unwrap_or_else(|| "goatty".into())
}
