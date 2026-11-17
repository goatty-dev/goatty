//! Availability on this host: the state the user chose, goatty's Serve
//! route, the gateway socket and its accept loop.
//!
//! goattyd owns one [`Host`]. It listens on the gateway socket (0600, next
//! to the daemon socket in its 0700 directory) only while the host is
//! available, and hands every connection that authenticated as a paired
//! device to the daemon's normal connection handler. It never binds a
//! network socket: tailscaled forwards the tailnet port into the gateway
//! socket.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
use crate::tailscale::{Route, ServeError, Tailscale};
use crate::{DEFAULT_PORT, Paths};

/// How long a connection may take to finish the gateway handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest pairing window a request can open.
const MAX_PAIRING: Duration = Duration::from_secs(15 * 60);

/// What the user chose for this host (`state.json`). State, not a setting:
/// it mirrors the Serve route goatty added to the user's Tailscale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostState {
    pub available: bool,
    pub port: u16,
}

impl Default for HostState {
    fn default() -> Self {
        Self { available: false, port: DEFAULT_PORT }
    }
}

/// Called with each connection that authenticated as a paired device.
pub type Handler = Box<dyn Fn(UnixStream, Device) + Send + Sync>;

struct Listening {
    stop: Arc<AtomicBool>,
    ino: u64,
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
        *self.state.lock()
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn save_state(&self, state: HostState) -> Result<()> {
        *self.state.lock() = state;
        crate::write_atomic(&self.paths.state(), &serde_json::to_vec_pretty(&state)?, 0o600)
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
                log::error!("tailnet: couldn't listen on {}: {e:#}", self.socket.display());
                *self.route.lock() = RouteStatus::Failed { message: format!("{e:#}") };
                return;
            }
            self.ensure_route(state.port);
        } else if self.ts.serve_config().is_ok_and(|c| !c.our_ports(&self.socket).is_empty()) {
            self.remove_routes(None);
        }
    }

    /// Re-listen when the gateway socket vanished (logind removing
    /// `/run/user/$UID`; the daemon's socket watchdog calls this).
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
            match self.ts.serve_config().map(|c| c.route(state.port, &self.socket)) {
                Ok(Route::Ours) => route = RouteStatus::Active,
                Ok(Route::Other { what }) => route = RouteStatus::PortTaken { what },
                Ok(Route::Free) if route == RouteStatus::Active => {
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
                self.save_state(HostState { available: true, port })?;
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
        let status = match self.ts.serve_config() {
            Err(e) if self.ts.detect().running().is_none() => {
                log::info!("tailnet: Tailscale isn't usable: {e:#}");
                RouteStatus::NoTailscale
            }
            Err(e) => RouteStatus::Failed { message: format!("{e:#}") },
            Ok(cfg) => match cfg.route(port, &self.socket) {
                Route::Ours => RouteStatus::Active,
                Route::Other { what } => RouteStatus::PortTaken { what },
                Route::Free => match self.ts.serve_tcp(port, &self.socket) {
                    Ok(()) => {
                        log::info!("tailnet: serving port {port} into {}", self.socket.display());
                        RouteStatus::Active
                    }
                    Err(ServeError::Permission(p)) => RouteStatus::Permission(p),
                    Err(ServeError::Failed(message)) => RouteStatus::Failed { message },
                },
            },
        };
        *self.route.lock() = status.clone();
        status
    }

    /// Remove goatty's routes (except on `keep`). Returns what went wrong.
    fn remove_routes(&self, keep: Option<u16>) -> Option<String> {
        let ports = match self.ts.serve_config() {
            Ok(cfg) => cfg.our_ports(&self.socket),
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
        let stop = Arc::new(AtomicBool::new(false));
        let (host, flag) = (Arc::downgrade(self), stop.clone());
        std::thread::Builder::new().name("gateway-accept".into()).spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                let Some(host) = host.upgrade() else { break };
                match stream {
                    Ok(s) => {
                        let _ = std::thread::Builder::new().name("gateway-conn".into()).spawn(move || host.serve(s));
                    }
                    Err(e) => log::warn!("gateway accept: {e}"),
                }
            }
        })?;
        log::info!("tailnet: gateway listening on {}", self.socket.display());
        *listening = Some(Listening { stop, ino });
        Ok(())
    }

    fn stop_listening(&self) {
        let Some(l) = self.listening.lock().take() else { return };
        l.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = UnixStream::connect(&self.socket);
        if std::fs::metadata(&self.socket).is_ok_and(|m| m.ino() == l.ino) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    fn serve(&self, mut stream: UnixStream) {
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
