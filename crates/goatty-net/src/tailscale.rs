//! The user's Tailscale, through its public CLI only (`tailscale status
//! --json`, `serve`, `nc`, `version`). goatty doesn't pin a Tailscale
//! version, so it doesn't use the internal LocalAPI.
//!
//! Rules for touching the user's Serve config (it is per node and shared
//! with everything else the user publishes):
//!
//! * goatty adds one TCP route on one port and removes only that route. It
//!   never runs `serve reset`.
//! * A route is goatty's when its target is goatty's gateway socket. A port
//!   that serves anything else is left alone ([`Route::Other`]).
//! * Serving a Unix socket needs root, or the Tailscale operator who may
//!   also run `sudo tailscale` (tailscaled checks that itself). goatty never
//!   runs sudo: it reports [`ServeError::Permission`] with the commands to
//!   run.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// The oldest Tailscale whose `serve --tcp` forwards to a Unix socket
/// (`unix:` targets for TCP forwarding arrived in 1.102).
pub const MIN_VERSION: (u32, u32) = (1, 102);

/// How long a `tailscale` command may take before goatty gives up on it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The `tailscale` program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tailscale {
    pub program: PathBuf,
}

impl Default for Tailscale {
    fn default() -> Self {
        Self::system()
    }
}

impl Tailscale {
    /// `tailscale` from `PATH`, or the macOS app's CLI.
    pub fn system() -> Self {
        let mac = Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        if cfg!(target_os = "macos") && find_in_path("tailscale").is_none() && mac.exists() {
            return Self { program: mac.into() };
        }
        Self { program: "tailscale".into() }
    }

    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into() }
    }

    /// Run `tailscale args…`: (success, stdout, stderr). Errors when it
    /// can't be run at all ([`is_not_installed`]) or takes too long.
    fn run(&self, args: &[&str]) -> Result<(bool, String, String)> {
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("couldn't run {}", self.program.display()))?;
        let mut out = child.stdout.take().context("stdout")?;
        let mut err = child.stderr.take().context("stderr")?;
        let out_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("`tailscale {}` didn't finish within {}s", args.join(" "), TIMEOUT.as_secs());
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        Ok((status.success(), stdout, stderr))
    }

    /// What state the user's Tailscale is in.
    pub fn detect(&self) -> State {
        match self.run(&["status", "--json"]) {
            Err(e) if is_not_installed(&e) => State::NotInstalled,
            Err(e) => State::NotRunning { message: format!("{e:#}") },
            Ok((_, stdout, stderr)) => match serde_json::from_str::<RawStatus>(&stdout) {
                Ok(raw) => State::from_raw(raw),
                Err(_) => State::NotRunning { message: first_line(&stderr).unwrap_or_else(|| "tailscale status failed".into()) },
            },
        }
    }

    /// The node's Serve config (`serve status --json`, read-only).
    pub fn serve_config(&self) -> Result<ServeConfig> {
        let (ok, stdout, stderr) = self.run(&["serve", "status", "--json"])?;
        if !ok {
            bail!("tailscale serve status: {}", first_line(&stderr).unwrap_or_default());
        }
        ServeConfig::parse(&stdout)
    }

    /// `tailscale serve --bg --tcp=<port> unix:<socket>`.
    pub fn serve_tcp(&self, port: u16, socket: &Path) -> Result<(), ServeError> {
        let target = format!("unix:{}", socket.display());
        let port = format!("--tcp={port}");
        self.change_serve(&["serve", "--bg", &port, &target])
    }

    /// `tailscale serve --tcp=<port> off`: removes that one route.
    pub fn serve_tcp_off(&self, port: u16) -> Result<(), ServeError> {
        let port = format!("--tcp={port}");
        self.change_serve(&["serve", &port, "off"])
    }

    fn change_serve(&self, args: &[&str]) -> Result<(), ServeError> {
        let (ok, stdout, stderr) = self.run(args).map_err(|e| ServeError::Failed(format!("{e:#}")))?;
        if ok {
            return Ok(());
        }
        let text = format!("{stderr}\n{stdout}");
        if is_permission_error(&text) {
            return Err(ServeError::Permission(Permission::commands(args)));
        }
        Err(ServeError::Failed(
            text.lines().map(str::trim).filter(|l| !l.is_empty()).take(3).collect::<Vec<_>>().join(" ").replace("error: ", ""),
        ))
    }

    /// `tailscale nc <addr> <port>` with its stdio piped: a byte stream to
    /// a peer when Tailscale runs in userspace-networking mode.
    pub fn nc(&self, addr: &str, port: u16) -> Command {
        let mut c = Command::new(&self.program);
        c.arg("nc").arg(addr).arg(port.to_string());
        c
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

fn first_line(s: &str) -> Option<String> {
    s.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from)
}

/// The error from running a program that isn't there.
fn is_not_installed(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound))
}

/// tailscaled refused to change the Serve config for this user.
fn is_permission_error(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("serve config denied") || t.contains("must be root") || t.contains("access denied") || t.contains("403 forbidden")
}

/// What it takes for goatty to change Serve, shown when it can't.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    /// Makes this user Tailscale's operator, so goatty can manage its route
    /// itself from then on (the user also needs sudo rights: tailscaled
    /// checks `sudo -l tailscale` before it serves a Unix socket).
    pub operator: String,
    /// Or: the change itself, run once as root.
    pub direct: String,
}

impl Permission {
    fn commands(args: &[&str]) -> Self {
        let user = std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "$USER".into());
        Self { operator: format!("sudo tailscale set --operator={user}"), direct: format!("sudo tailscale {}", args.join(" ")) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServeError {
    /// This user may not change Serve.
    Permission(Permission),
    Failed(String),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Permission(p) => write!(f, "Tailscale doesn't let goatty change Serve: run `{}` once", p.operator),
            ServeError::Failed(m) => write!(f, "tailscale serve: {m}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// The state of the user's Tailscale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    /// No `tailscale` program.
    NotInstalled,
    /// Installed, but tailscaled doesn't answer.
    NotRunning {
        message: String,
    },
    /// Running, but not logged in or turned off (`BackendState`).
    NotConnected {
        backend: String,
    },
    /// Older than [`MIN_VERSION`].
    TooOld {
        version: String,
    },
    Running(Status),
}

impl State {
    fn from_raw(raw: RawStatus) -> Self {
        if raw.backend_state != "Running" {
            return State::NotConnected { backend: raw.backend_state };
        }
        if !version_at_least(&raw.version, MIN_VERSION) {
            return State::TooOld { version: raw.version };
        }
        State::Running(Status::from_raw(raw))
    }

    pub fn running(&self) -> Option<&Status> {
        match self {
            State::Running(s) => Some(s),
            _ => None,
        }
    }

    /// One line for the settings page and the CLI.
    pub fn describe(&self) -> String {
        match self {
            State::NotInstalled => "Tailscale isn't installed".into(),
            State::NotRunning { message } => format!("Tailscale isn't running ({message})"),
            State::NotConnected { backend } => match backend.as_str() {
                "NeedsLogin" | "NoState" => "Tailscale isn't logged in".into(),
                "Stopped" => "Tailscale is turned off".into(),
                "NeedsMachineAuth" => "This device waits for approval by your tailnet's admin".into(),
                other => format!("Tailscale isn't connected ({other})"),
            },
            State::TooOld { version } => {
                format!("Tailscale {version} is too old: goatty needs {}.{} or newer", MIN_VERSION.0, MIN_VERSION.1)
            }
            State::Running(s) => format!("Tailscale {} connected as {}", s.version, s.me.name),
        }
    }
}

/// Whether `version` (`1.104.7`, `1.104.7-tabc`) is at least `min`.
pub fn version_at_least(version: &str, min: (u32, u32)) -> bool {
    let mut parts = version.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    (major, minor) >= min
}

/// What goatty needs from `tailscale status --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    /// Tailscale has a TUN device, so tailnet addresses are reachable with
    /// plain sockets. Without it (userspace networking) goatty uses
    /// `tailscale nc`.
    pub tun: bool,
    pub me: Node,
    pub peers: Vec<Node>,
}

/// A node of the tailnet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Its Tailscale host name (`HostName`), as shown.
    pub name: String,
    /// Its MagicDNS name without the trailing dot.
    pub dns: String,
    /// Tailnet addresses, IPv4 first.
    pub addrs: Vec<String>,
    pub os: String,
    pub user_id: u64,
    /// The owner's login name (`user@github`), when the status says.
    pub user: Option<String>,
    pub online: bool,
    /// Tagged nodes belong to the tailnet, not to a person.
    pub tagged: bool,
    /// Connected directly (else through a relay, `relay`).
    pub direct: bool,
    /// The DERP region it's reached through when not direct.
    pub relay: String,
}

impl Node {
    /// The address to connect to: IPv4 if it has one.
    pub fn addr(&self) -> Option<&str> {
        self.addrs.first().map(String::as_str)
    }
}

impl Status {
    fn from_raw(raw: RawStatus) -> Self {
        let users: BTreeMap<u64, String> = raw.user.into_values().map(|u| (u.id, u.login_name)).collect();
        let node = |n: RawNode| {
            let mut addrs = n.tailscale_ips;
            addrs.sort_by_key(|a| a.contains(':'));
            Node {
                user: users.get(&n.user_id).cloned(),
                name: n.host_name,
                dns: n.dns_name.trim_end_matches('.').to_string(),
                addrs,
                os: n.os,
                user_id: n.user_id,
                online: n.online,
                tagged: n.tags.as_ref().is_some_and(|t| !t.is_empty()),
                direct: !n.cur_addr.is_empty(),
                relay: n.relay,
            }
        };
        let mut peers: Vec<Node> = raw.peer.into_values().map(node).collect();
        peers.sort_by_key(|p| p.name.to_lowercase());
        // `Self` is always online from its own point of view.
        let me = Node { online: true, ..node(raw.self_node.unwrap_or_default()) };
        Status { version: raw.version, tun: raw.tun, me, peers }
    }

    /// Parse `tailscale status --json` output.
    pub fn parse(json: &str) -> Result<State> {
        Ok(State::from_raw(serde_json::from_str(json).context("tailscale status --json")?))
    }

    pub fn peer(&self, dns: &str) -> Option<&Node> {
        self.peers.iter().find(|p| p.dns == dns)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct RawStatus {
    version: String,
    #[serde(rename = "TUN")]
    tun: bool,
    backend_state: String,
    #[serde(rename = "Self")]
    self_node: Option<RawNode>,
    peer: BTreeMap<String, RawNode>,
    user: BTreeMap<String, RawUser>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct RawNode {
    host_name: String,
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(rename = "OS")]
    os: String,
    #[serde(rename = "UserID")]
    user_id: u64,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Vec<String>,
    online: bool,
    tags: Option<Vec<String>>,
    cur_addr: String,
    relay: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct RawUser {
    #[serde(rename = "ID")]
    id: u64,
    login_name: String,
}

/// The parts of the node's Serve config goatty looks at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeConfig {
    /// TCP port → what it serves.
    pub tcp: BTreeMap<u16, TcpHandler>,
    /// Ports with web handlers (`host:port` keys of `Web`).
    pub web_ports: Vec<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct TcpHandler {
    #[serde(rename = "HTTPS")]
    pub https: bool,
    #[serde(rename = "HTTP")]
    pub http: bool,
    #[serde(rename = "TCPForward")]
    pub tcp_forward: String,
    #[serde(rename = "TerminateTLS")]
    pub terminate_tls: String,
}

/// What a port serves, from goatty's point of view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum Route {
    Free,
    /// Forwards to goatty's gateway socket.
    Ours,
    /// Serves something else (described), which goatty won't overwrite.
    Other {
        what: String,
    },
}

impl ServeConfig {
    pub fn parse(json: &str) -> Result<Self> {
        #[derive(Default, Deserialize)]
        #[serde(default, rename_all = "PascalCase")]
        struct Raw {
            #[serde(rename = "TCP")]
            tcp: BTreeMap<String, TcpHandler>,
            web: BTreeMap<String, serde_json::Value>,
        }
        let json = json.trim();
        let raw: Raw = if json.is_empty() || json == "null" {
            Raw::default()
        } else {
            serde_json::from_str(json).map_err(|e| anyhow!("tailscale serve status --json: {e}"))?
        };
        let tcp = raw.tcp.into_iter().filter_map(|(port, h)| Some((port.parse().ok()?, h))).collect();
        let mut web_ports: Vec<u16> = raw.web.keys().filter_map(|k| k.rsplit(':').next()?.parse().ok()).collect();
        web_ports.sort_unstable();
        web_ports.dedup();
        Ok(Self { tcp, web_ports })
    }

    /// What `port` serves.
    pub fn route(&self, port: u16, socket: &Path) -> Route {
        let ours = format!("unix:{}", socket.display());
        match self.tcp.get(&port) {
            None if self.web_ports.contains(&port) => Route::Other { what: "a web server".into() },
            None => Route::Free,
            Some(h) if h.tcp_forward == ours => Route::Ours,
            Some(h) if !h.tcp_forward.is_empty() => Route::Other { what: h.tcp_forward.clone() },
            Some(h) if h.https || h.http || self.web_ports.contains(&port) => Route::Other { what: "a web server".into() },
            Some(_) => Route::Other { what: "something else".into() },
        }
    }

    /// Ports forwarding to `socket` (goatty's routes).
    pub fn our_ports(&self, socket: &Path) -> Vec<u16> {
        self.tcp.keys().copied().filter(|p| self.route(*p, socket) == Route::Ours).collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const STATUS: &str = r#"{
      "Version": "1.104.7", "TUN": true, "BackendState": "Running",
      "Self": {"HostName": "desk", "DNSName": "desk.tail1.ts.net.", "OS": "linux", "UserID": 1,
               "TailscaleIPs": ["fd7a::1", "100.64.0.1"], "Online": false, "Tags": null},
      "Peer": {
        "nodekey:b": {"HostName": "Laptop", "DNSName": "laptop.tail1.ts.net.", "OS": "linux", "UserID": 1,
                      "TailscaleIPs": ["100.64.0.2", "fd7a::2"], "Online": true, "CurAddr": "192.168.1.5:41641", "Relay": "fra"},
        "nodekey:c": {"HostName": "phone", "DNSName": "phone.tail1.ts.net.", "OS": "android", "UserID": 1,
                      "TailscaleIPs": ["100.64.0.3"], "Online": true, "CurAddr": "", "Relay": "fra"},
        "nodekey:d": {"HostName": "server", "DNSName": "server.tail1.ts.net.", "OS": "linux", "UserID": 9,
                      "TailscaleIPs": ["100.64.0.4"], "Online": false, "Tags": ["tag:server"], "Relay": "nue"}
      },
      "User": {"1": {"ID": 1, "LoginName": "me@github"}}
    }"#;

    #[test]
    fn status_is_parsed() {
        let State::Running(s) = Status::parse(STATUS).unwrap() else { panic!() };
        assert!(s.tun);
        assert_eq!(s.me.name, "desk");
        assert_eq!(s.me.dns, "desk.tail1.ts.net");
        assert_eq!(s.me.addr(), Some("100.64.0.1"), "IPv4 first");
        assert!(s.me.online);
        let names: Vec<_> = s.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Laptop", "phone", "server"]);
        let laptop = s.peer("laptop.tail1.ts.net").unwrap();
        assert!(laptop.direct && laptop.online && !laptop.tagged);
        assert_eq!(laptop.user.as_deref(), Some("me@github"));
        let server = s.peer("server.tail1.ts.net").unwrap();
        assert!(server.tagged && !server.direct && !server.online);
        assert_eq!(server.relay, "nue");
    }

    #[test]
    fn states_other_than_running() {
        assert_eq!(
            Status::parse(r#"{"Version":"1.104.0","BackendState":"NeedsLogin"}"#).unwrap(),
            State::NotConnected { backend: "NeedsLogin".into() }
        );
        assert!(Status::parse(r#"{"Version":"1.104.0","BackendState":"NeedsLogin"}"#).unwrap().describe().contains("logged in"));
        assert_eq!(
            Status::parse(r#"{"Version":"1.100.3","BackendState":"Running"}"#).unwrap(),
            State::TooOld { version: "1.100.3".into() }
        );
        assert!(version_at_least("1.102.0", MIN_VERSION));
        assert!(version_at_least("1.104.7-t0123", MIN_VERSION));
        assert!(version_at_least("2.0.0", MIN_VERSION));
        assert!(!version_at_least("1.98.1", MIN_VERSION));
        assert!(!version_at_least("", MIN_VERSION));
    }

    #[test]
    fn routes_tell_ours_from_others() {
        let sock = Path::new("/run/user/1/goatty/goattyd.gateway.sock");
        let cfg = ServeConfig::parse(
            r#"{
              "TCP": {"443": {"HTTPS": true}, "7422": {"TCPForward": "unix:/run/user/1/goatty/goattyd.gateway.sock"},
                      "22": {"TCPForward": "127.0.0.1:22"}, "9000": {"TCPForward": "unix:/run/user/1/goatty/goattyd.gateway.sock"}},
              "Web": {"desk.tail1.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}},
                      "desk.tail1.ts.net:8080": {}}
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.route(7422, sock), Route::Ours);
        assert_eq!(cfg.route(22, sock), Route::Other { what: "127.0.0.1:22".into() });
        assert_eq!(cfg.route(443, sock), Route::Other { what: "a web server".into() });
        assert_eq!(cfg.route(8080, sock), Route::Other { what: "a web server".into() });
        assert_eq!(cfg.route(1234, sock), Route::Free);
        assert_eq!(cfg.route(7422, Path::new("/elsewhere.sock")), Route::Other { what: format!("unix:{}", sock.display()) });
        assert_eq!(cfg.our_ports(sock), [7422, 9000]);
        assert_eq!(ServeConfig::parse("{}").unwrap(), ServeConfig::default());
        assert_eq!(ServeConfig::parse("").unwrap(), ServeConfig::default());
    }

    #[test]
    fn permission_errors_are_recognized() {
        assert!(is_permission_error("error: 403 Forbidden: serve config denied"));
        assert!(is_permission_error("must be root, or be an operator and able to run 'sudo tailscale' to serve a path or Unix socket"));
        assert!(!is_permission_error("cannot serve TCP; already serving web on 443"));
    }

    #[test]
    fn missing_program_is_not_installed() {
        let ts = Tailscale::with_program("/nonexistent/tailscale-for-goatty-tests");
        assert_eq!(ts.detect(), State::NotInstalled);
    }
}
