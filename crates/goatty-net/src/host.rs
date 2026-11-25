//! Availability on this host: the state the user chose, goatty's Serve
//! route, the gateway socket and its accept loop.
//!
//! goattyd owns one [`Host`]. It listens on the gateway socket (0600, next
//! to the daemon socket in its 0700 directory) only while the host is
//! available, and hands every connection that authenticated as a paired
//! device to the daemon's normal connection handler. On Unix it never binds
//! a network socket: tailscaled forwards the tailnet port into the gateway
//! socket.
//!
//! On Windows Serve can forward only to TCP (no Unix sockets, no named
//! pipes), so the gateway listens on a loopback port instead
//! (`127.0.0.1:<port>`, kept in `state.json` so the route stays valid across
//! restarts) and the route is `tcp://127.0.0.1:<port>`. Nothing about the
//! security changes: every connection still has to pass the gateway
//! handshake, with a paired device key, before it reaches the daemon.

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::api::{NetStatus, Reply, Request, RouteStatus};
use crate::gateway::{Accepted, Gateway, HostInfo, PAIRING_WINDOW};
use crate::pairing::Device;
use crate::tailscale::{Route, ServeError, Tailscale, Target};
use crate::{DEFAULT_PORT, Paths};

/// How long a connection may take to finish the gateway handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest pairing window a request can open.
const MAX_PAIRING: Duration = Duration::from_secs(15 * 60);

/// What the user chose for this host (`state.json`). State, not a setting:
/// it mirrors the Serve route goatty added to the user's Tailscale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostState {
    pub available: bool,
    pub port: u16,
    /// Windows: the loopback port the gateway listens on (Serve's target).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_port: Option<u16>,
    /// Windows: the loopback ports the gateway had to leave (something else
    /// took them) while goatty's route may still forward there. A route to
    /// one counts as goatty's until no route forwards there any more
    /// ([`Host::prune_moved`]), across restarts and further moves too.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub moved_from: Vec<u16>,
}

impl Default for HostState {
    fn default() -> Self {
        Self { available: false, port: DEFAULT_PORT, local_port: None, moved_from: Vec::new() }
    }
}

/// A connection to the gateway: its Unix socket, or on Windows its loopback
/// port.
#[cfg(unix)]
pub type GatewayStream = UnixStream;
#[cfg(not(unix))]
pub type GatewayStream = std::net::TcpStream;

/// Called with each connection that authenticated as a paired device.
pub type Handler = Box<dyn Fn(GatewayStream, Device) + Send + Sync>;

struct Listening {
    stop: Arc<AtomicBool>,
    /// The socket file's inode, to notice it was removed.
    #[cfg(unix)]
    ino: u64,
    /// The loopback port.
    #[cfg(not(unix))]
    port: u16,
    /// The accept loop, which holds the listener until it ends.
    #[cfg(not(unix))]
    thread: std::thread::JoinHandle<()>,
}

pub struct Host {
    ts: Tailscale,
    paths: Paths,
    socket: PathBuf,
    pub gateway: Gateway,
    state: Mutex<HostState>,
    route: Mutex<RouteStatus>,
    listening: Mutex<Option<Listening>>,
    handler: OnceLock<Handler>,
    /// Serializes changes (enable, disable, repairs) to the route.
    changing: Mutex<()>,
}

impl Host {
    /// `socket`: the gateway socket's path ([`crate::gateway_socket`]).
    pub fn new(ts: Tailscale, paths: Paths, socket: PathBuf, info: HostInfo) -> Result<Self> {
        let state: HostState = crate::read_json(&paths.state())?;
        let gateway = Gateway::new(paths.clone(), info)?;
        Ok(Self {
            ts,
            paths,
            socket,
            gateway,
            state: Mutex::new(state),
            route: Mutex::new(RouteStatus::Off),
            listening: Mutex::new(None),
            handler: OnceLock::new(),
            changing: Mutex::new(()),
        })
    }

    pub fn state(&self) -> HostState {
        self.state.lock().clone()
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Where the gateway listens now, as a Serve target (`None` on Windows
    /// before it has a port).
    #[cfg(unix)]
    fn target(&self) -> Option<Target> {
        Some(Target::Unix(self.socket.clone()))
    }

    #[cfg(not(unix))]
    fn target(&self) -> Option<Target> {
        let listening = self.listening.lock().as_ref().map(|l| l.port);
        listening.or(self.state().local_port).map(Target::Loopback)
    }

    /// Every target a route of goatty's may have: [`Self::target`], and on
    /// Windows the port the gateway moved away from while a route still goes
    /// there ([`HostState::moved_from`]).
    fn targets(&self) -> Vec<Target> {
        let targets = self.target().into_iter();
        #[cfg(not(unix))]
        let targets = targets.chain(self.state().moved_from.into_iter().map(Target::Loopback));
        targets.collect()
    }

    /// Forget the ports the gateway moved away from once no route forwards
    /// there (goatty's was repointed or removed): from then on a route to one
    /// is someone else's. Without a Serve config to check, nothing is
    /// forgotten.
    fn prune_moved(&self) {
        let state = self.state();
        if state.moved_from.is_empty() {
            return;
        }
        let Ok(cfg) = self.ts.serve_config() else { return };
        let used = |p: &u16| cfg.tcp.values().any(|h| Target::Loopback(*p).matches(&h.tcp_forward));
        let moved_from: Vec<u16> = state.moved_from.iter().copied().filter(used).collect();
        if moved_from != state.moved_from
            && let Err(e) = self.save_state(HostState { moved_from, ..state })
        {
            log::warn!("tailnet: {e:#}");
        }
    }

    fn save_state(&self, state: HostState) -> Result<()> {
        let json = serde_json::to_vec_pretty(&state)?;
        *self.state.lock() = state;
        crate::write_atomic(&self.paths.state(), &json, 0o600)
    }

    /// Start serving: with the host available, listen and put the route in
    /// place (repairing it if it went stale); otherwise remove a leftover
    /// route of ours. Talks to Tailscale, so call it off any hot path.
    pub fn start(self: &Arc<Self>, handler: Handler) {
        let _ = self.handler.set(handler);
        // Held from reading the state to the end: an `Enable` meanwhile must
        // not have its new route taken for a leftover and removed.
        let _guard = self.changing.lock();
        let state = self.state();
        if state.available {
            if let Err(e) = self.listen() {
                log::error!("tailnet: the gateway couldn't listen: {e:#}");
                *self.route.lock() = RouteStatus::Failed { message: format!("{e:#}") };
                return;
            }
            self.ensure_route(state.port);
        } else if self.ts.serve_config().is_ok_and(|c| !c.our_ports(&self.targets()).is_empty()) {
            self.remove_routes(None);
        }
        self.prune_moved();
    }

    /// Re-listen when the gateway socket vanished (logind removing
    /// `/run/user/$UID`; the daemon's socket watchdog calls this).
    #[cfg(unix)]
    pub fn check(self: &Arc<Self>) {
        if !self.state().available {
            return;
        }
        let ino = self.listening.lock().as_ref().map(|l| l.ino);
        let current = std::fs::metadata(&self.socket).ok().map(|m| m.ino());
        if ino.is_some() && current == ino {
            return;
        }
        log::warn!("tailnet: gateway socket {} disappeared; listening again", self.socket.display());
        self.stop_listening();
        if let Err(e) = self.listen() {
            log::error!("tailnet: couldn't listen again: {e:#}");
        }
    }

    #[cfg(not(unix))]
    pub fn check(self: &Arc<Self>) {}

    pub fn request(self: &Arc<Self>, req: Request) -> Reply {
        let done = match req {
            Request::Status => Ok(()),
            Request::Enable { port } => self.enable(port),
            Request::Disable => self.disable(),
            Request::OpenPairing { secs } => {
                if self.state().available {
                    let d = secs.map(Duration::from_secs).unwrap_or(PAIRING_WINDOW).min(MAX_PAIRING);
                    self.gateway.open_pairing(d);
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("make this computer available on the tailnet first"))
                }
            }
            Request::ClosePairing => {
                self.gateway.close_pairing();
                Ok(())
            }
            Request::Decide { id, accept } => self.gateway.decide(id, accept),
            Request::Revoke { key } => self
                .gateway
                .revoke(&key)
                .and_then(|gone| if gone.is_empty() { bail!("no paired device has a key starting with {key}") } else { Ok(()) }),
            Request::Leave => {
                // Forget the devices even if the route couldn't be removed.
                let disabled = self.disable();
                self.gateway.forget_all().and(disabled)
            }
        };
        match done {
            Ok(()) => Reply::Status(Box::new(self.status())),
            Err(e) => Reply::Error { message: format!("{e:#}") },
        }
    }

    /// The current status. Reads Tailscale's status (no changes).
    pub fn status(&self) -> NetStatus {
        let state = self.state();
        let tailscale = self.ts.detect();
        let mut route = self.route.lock().clone();
        // Re-check the route against the live Serve config: someone may have
        // removed it (`serve reset`), or Tailscale came back.
        if state.available && tailscale.running().is_some() {
            let target = self.target();
            match self.ts.serve_config().map(|c| (c.route(state.port, &self.targets()), c)) {
                Ok((Route::Ours, c)) if target.is_some_and(|t| c.forwards_to(state.port, &t)) => route = RouteStatus::Active,
                Ok((Route::Ours, _)) => {
                    route = RouteStatus::Failed {
                        message: "goatty's Serve route goes to the gateway's old address: turn availability off and on".into(),
                    }
                }
                Ok((Route::Other { what }, _)) => route = RouteStatus::PortTaken { what },
                Ok((Route::Free, _)) if route == RouteStatus::Active => {
                    route = RouteStatus::Failed {
                        message: "goatty's Serve route is gone (was Serve reset?): turn availability off and on".into(),
                    }
                }
                _ => {}
            }
        } else if state.available {
            route = RouteStatus::NoTailscale;
        }
        NetStatus {
            tailscale,
            available: state.available,
            port: state.port,
            route,
            listening: self.listening.lock().is_some(),
            pairing: self.gateway.pairing(),
            devices: self.gateway.devices().unwrap_or_default(),
            key: self.gateway.public_key(),
        }
    }

    fn enable(self: &Arc<Self>, port: Option<u16>) -> Result<()> {
        let _guard = self.changing.lock();
        let old = self.state();
        let port = port.unwrap_or(old.port);
        if port == 0 {
            bail!("port 0 isn't a port");
        }
        let tailscale = self.ts.detect();
        if tailscale.running().is_none() {
            bail!("{}", tailscale.describe());
        }
        self.listen()?;
        match self.ensure_route(port) {
            RouteStatus::Active => {
                // A port change: our route on the old port goes.
                if old.available && old.port != port {
                    self.remove_routes(Some(port));
                }
                self.save_state(HostState { available: true, port, ..self.state() })?;
                self.prune_moved();
                Ok(())
            }
            problem => {
                if old.available {
                    // Keep serving where it did.
                    self.ensure_route(old.port);
                } else {
                    // Nothing to serve without the route.
                    self.stop_listening();
                    *self.route.lock() = problem.clone();
                }
                bail!("{}", problem.describe(port))
            }
        }
    }

    fn disable(&self) -> Result<()> {
        let _guard = self.changing.lock();
        self.gateway.close_pairing();
        // Tear down first: a failed save must not leave the gateway serving.
        self.stop_listening();
        let problem = self.remove_routes(None);
        self.prune_moved();
        *self.route.lock() = RouteStatus::Off;
        let state = self.state();
        self.save_state(HostState { available: false, ..state })?;
        if let Some(e) = problem {
            bail!("availability is off, but goatty's Serve route is still there: {e}");
        }
        Ok(())
    }

    /// Put goatty's route on `port` in place, unless the port serves
    /// something else. Records and returns the outcome.
    fn ensure_route(&self, port: u16) -> RouteStatus {
        let serve = |target: &Target| match self.ts.serve_tcp(port, target) {
            Ok(()) => {
                log::info!("tailnet: serving port {port} into {target}");
                RouteStatus::Active
            }
            Err(ServeError::Permission(p)) => RouteStatus::Permission(p),
            Err(ServeError::Failed(message)) => RouteStatus::Failed { message },
        };
        let status = match (self.ts.serve_config(), self.target()) {
            (Err(e), _) if self.ts.detect().running().is_none() => {
                log::info!("tailnet: Tailscale isn't usable: {e:#}");
                RouteStatus::NoTailscale
            }
            (Err(e), _) => RouteStatus::Failed { message: format!("{e:#}") },
            (Ok(_), None) => RouteStatus::Failed { message: "the gateway isn't listening".into() },
            (Ok(cfg), Some(target)) => match cfg.route(port, &self.targets()) {
                Route::Ours if cfg.forwards_to(port, &target) => RouteStatus::Active,
                // Ours, to the port the gateway moved away from: point it here.
                Route::Ours => match self.ts.serve_tcp_off(port) {
                    Ok(()) => serve(&target),
                    Err(ServeError::Permission(p)) => RouteStatus::Permission(p),
                    Err(ServeError::Failed(message)) => RouteStatus::Failed { message },
                },
                Route::Other { what } => RouteStatus::PortTaken { what },
                Route::Free => serve(&target),
            },
        };
        *self.route.lock() = status.clone();
        status
    }

    /// Remove goatty's routes (except on `keep`). Returns what went wrong.
    fn remove_routes(&self, keep: Option<u16>) -> Option<String> {
        let ports = match self.ts.serve_config() {
            Ok(cfg) => cfg.our_ports(&self.targets()),
            Err(e) => return self.ts.detect().running().map(|_| format!("{e:#}")),
        };
        let mut problem = None;
        for port in ports.into_iter().filter(|p| Some(*p) != keep) {
            match self.ts.serve_tcp_off(port) {
                Ok(()) => log::info!("tailnet: removed goatty's route on port {port}"),
                Err(e) => {
                    log::warn!("tailnet: couldn't remove goatty's route on port {port}: {e}");
                    problem = Some(e.to_string());
                }
            }
        }
        problem
    }

    #[cfg(unix)]
    fn listen(self: &Arc<Self>) -> Result<()> {
        let mut listening = self.listening.lock();
        if listening.is_some() {
            return Ok(());
        }
        let dir = self.socket.parent().context("gateway socket path has no parent")?;
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if self.socket.exists() {
            if UnixStream::connect(&self.socket).is_ok() {
                bail!("another goattyd serves {}", self.socket.display());
            }
            std::fs::remove_file(&self.socket)?;
        }
        let listener = UnixListener::bind(&self.socket).with_context(|| format!("bind {}", self.socket.display()))?;
        std::fs::set_permissions(&self.socket, std::fs::Permissions::from_mode(0o600))?;
        let ino = std::fs::metadata(&self.socket)?.ino();
        let (stop, _) = self.accept_loop(move || listener.accept().map(|(s, _)| s))?;
        log::info!("tailnet: gateway listening on {}", self.socket.display());
        *listening = Some(Listening { stop, ino });
        Ok(())
    }

    /// Windows: listen on the saved loopback port, or a new one when there
    /// is none or it's taken (then the route moves along, see
    /// [`Self::ensure_route`]).
    #[cfg(not(unix))]
    fn listen(self: &Arc<Self>) -> Result<()> {
        use std::net::{Ipv4Addr, TcpListener};
        let mut listening = self.listening.lock();
        if listening.is_some() {
            return Ok(());
        }
        let saved = self.state().local_port;
        let listener = match saved.map(|p| (p, TcpListener::bind((Ipv4Addr::LOCALHOST, p)))) {
            Some((_, Ok(l))) => l,
            Some((p, Err(_))) if self.gateway_answers(p) => bail!("another goattyd serves this computer's tailnet gateway (127.0.0.1:{p})"),
            _ => TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("bind a loopback port for the tailnet gateway")?,
        };
        let port = listener.local_addr()?.port();
        if saved != Some(port) {
            let state = self.state();
            // Kept until the route is repaired: goatty's route may still
            // forward to the old port, or to one it left before that.
            let mut moved_from = state.moved_from.clone();
            moved_from.extend(saved.filter(|p| !moved_from.contains(p)));
            moved_from.retain(|&p| p != port);
            if let Some(old) = saved {
                log::warn!("tailnet: gateway port {old} is taken; moving to {port}");
            }
            self.save_state(HostState { local_port: Some(port), moved_from, ..state })?;
        }
        let (stop, thread) = self.accept_loop(move || {
            let (s, _) = listener.accept()?;
            let _ = s.set_nodelay(true);
            Ok(s)
        })?;
        log::info!("tailnet: gateway listening on 127.0.0.1:{port}");
        *listening = Some(Listening { stop, port, thread });
        Ok(())
    }

    /// Windows: whether a gateway with this install's key (another goattyd
    /// of this user) listens on loopback `port`.
    #[cfg(not(unix))]
    fn gateway_answers(&self, port: u16) -> bool {
        let addr = (std::net::Ipv4Addr::LOCALHOST, port).into();
        let Ok(s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else { return false };
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        crate::gateway::read_greeting(&mut std::io::BufReader::new(s)).is_ok_and(|g| g.key == self.gateway.public_key())
    }

    /// Accept connections with `accept` on a thread of their own, each served
    /// on its own thread, until the returned flag is set (and one more
    /// connection wakes the loop).
    fn accept_loop(
        self: &Arc<Self>,
        accept: impl Fn() -> std::io::Result<GatewayStream> + Send + 'static,
    ) -> Result<(Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
        let stop = Arc::new(AtomicBool::new(false));
        let (host, flag) = (Arc::downgrade(self), stop.clone());
        let thread = std::thread::Builder::new().name("gateway-accept".into()).spawn(move || {
            loop {
                let stream = accept();
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                let Some(host) = host.upgrade() else { break };
                match stream {
                    Ok(s) => {
                        let _ = std::thread::Builder::new().name("gateway-conn".into()).spawn(move || host.serve(s));
                    }
                    Err(e) => {
                        log::warn!("gateway accept: {e}");
                        // Don't spin on a persistent error.
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        })?;
        Ok((stop, thread))
    }

    #[cfg(unix)]
    fn stop_listening(&self) {
        let Some(l) = self.listening.lock().take() else { return };
        l.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = UnixStream::connect(&self.socket);
        if std::fs::metadata(&self.socket).is_ok_and(|m| m.ino() == l.ino) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    #[cfg(not(unix))]
    fn stop_listening(&self) {
        let Some(l) = self.listening.lock().take() else { return };
        l.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag, and wait until it dropped
        // the listener: listening again right away gets the same port.
        let woke = std::net::TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST, l.port).into(), Duration::from_secs(1));
        if woke.is_ok() && l.thread.thread().id() != std::thread::current().id() {
            let _ = l.thread.join();
        }
    }

    fn serve(&self, mut stream: GatewayStream) {
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));
        match self.gateway.accept(&mut stream) {
            Ok(Accepted::Device(device)) => {
                let _ = stream.set_read_timeout(None);
                let _ = stream.set_write_timeout(None);
                log::info!("tailnet: {} connected", device.name);
                if let Some(handler) = self.handler.get() {
                    handler(stream, device);
                }
            }
            Ok(Accepted::Done) => {}
            Err(e) => log::info!("tailnet: gateway handshake failed: {e:#}"),
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop_listening();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::io::BufReader;
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    use super::*;

    fn host(root: &Path) -> Arc<Host> {
        let info = HostInfo { name: "desk".into(), goatty: "0.1.0".into(), protocol: (7, 12) };
        let ts = Tailscale::with_program(root.join("no-tailscale.exe"));
        Arc::new(Host::new(ts, Paths::new(root.join("tailnet")), root.join("goattyd.gateway.sock"), info).unwrap())
    }

    fn probe(port: u16) -> crate::gateway::Probe {
        let s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        crate::gateway::probe(&mut BufReader::new(s.try_clone().unwrap()), &mut &s).unwrap()
    }

    #[test]
    fn the_gateway_listens_on_a_loopback_port_it_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let h = host(dir.path());
        h.listen().unwrap();
        let port = h.state().local_port.expect("the port is saved");
        assert_eq!(h.target(), Some(Target::Loopback(port)));
        assert_eq!(probe(port).greeting.key, h.gateway.public_key());
        // A second goattyd of the same install doesn't take over.
        let other = host(dir.path());
        assert!(other.listen().is_err());
        h.stop_listening();
        drop(h);

        // After a restart it listens on the same port, so the route stays valid.
        let h = host(dir.path());
        h.listen().unwrap();
        assert_eq!(h.state().local_port, Some(port));
        assert_eq!(probe(port).greeting.key, h.gateway.public_key());
        h.stop_listening();
        drop(h);

        // Taken by something else: it moves, and a route to the old port
        // still counts as goatty's.
        let squatter = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let h = host(dir.path());
        h.listen().unwrap();
        let moved = h.state().local_port.unwrap();
        assert_ne!(moved, port);
        assert_eq!(h.state().moved_from, [port]);
        assert_eq!(h.targets(), [Target::Loopback(moved), Target::Loopback(port)]);
        h.stop_listening();
        drop(h);
        // The old port stays goatty's across a restart until the route is repaired.
        let h = host(dir.path());
        h.listen().unwrap();
        assert_eq!(h.targets(), [Target::Loopback(moved), Target::Loopback(port)]);
        // No Serve config to read (no Tailscale): nothing is forgotten on a guess.
        h.prune_moved();
        assert_eq!(h.state().moved_from, [port]);
        h.stop_listening();
        drop(h);

        // Moving again before the route was repaired keeps both old ports.
        let squatter2 = TcpListener::bind((Ipv4Addr::LOCALHOST, moved)).unwrap();
        let h = host(dir.path());
        h.listen().unwrap();
        let third = h.state().local_port.unwrap();
        assert!(third != moved && third != port);
        assert_eq!(h.targets(), [Target::Loopback(third), Target::Loopback(port), Target::Loopback(moved)]);
        drop((squatter, squatter2));
    }
}
