#![cfg(unix)]
//! Availability end to end against a fake `tailscale` (never the user's):
//! the route goes in and out of the Serve config without touching anything
//! else, the gateway listens only while available, and paired devices get
//! through to the handler.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use goatty_net::api::{Reply, Request, RouteStatus};
use goatty_net::gateway::{self, HostInfo};
use goatty_net::host::Host;
use goatty_net::identity::Identity;
use goatty_net::tailscale::{State, Tailscale};
use goatty_net::{DEFAULT_PORT, Paths};

const STATUS: &str = r#"{"Version": "1.104.7", "TUN": true, "BackendState": "Running",
  "Self": {"HostName": "desk", "DNSName": "desk.tail1.ts.net.", "OS": "linux", "UserID": 1, "TailscaleIPs": ["100.64.0.1"]},
  "Peer": {}, "User": {}}"#;

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-tailscale.sh"), dir.path().join("tailscale")).unwrap();
        std::fs::write(dir.path().join("status.json"), STATUS).unwrap();
        std::fs::create_dir(dir.path().join("tcp")).unwrap();
        Self { dir }
    }
    fn ts(&self) -> Tailscale {
        Tailscale::with_program(self.dir.path().join("tailscale"))
    }
    fn route(&self, port: u16) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join(format!("tcp/{port}"))).ok()
    }
    fn set_route(&self, port: u16, json: &str) {
        std::fs::write(self.dir.path().join(format!("tcp/{port}")), json).unwrap();
    }
    fn deny(&self, on: bool) {
        let p = self.dir.path().join("deny");
        if on {
            std::fs::write(p, "").unwrap();
        } else {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn host(fake: &Fake, root: &Path) -> Arc<Host> {
    let info = HostInfo { name: "desk".into(), goatty: "0.1.0".into(), protocol: (7, 12) };
    let socket = goatty_net::gateway_socket(&root.join("run/goattyd.sock"));
    let host = Arc::new(Host::new(fake.ts(), Paths::new(root.join("tailnet")), socket, info).unwrap());
    // The "daemon": echo five bytes, as if it spoke the goatty protocol.
    host.start(Box::new(|mut s, _device| {
        let mut buf = [0u8; 5];
        if s.read_exact(&mut buf).is_ok() {
            let _ = s.write_all(&buf);
        }
    }));
    host
}

fn ok(r: Reply) -> goatty_net::api::NetStatus {
    r.into_result().unwrap()
}

fn sock(host: &Host) -> PathBuf {
    host.socket().to_path_buf()
}

#[test]
fn availability_adds_and_removes_only_goattys_route() {
    let fake = Fake::new();
    let root = tempfile::tempdir().unwrap();
    fake.set_route(443, r#"{"HTTPS":true}"#);
    let h = host(&fake, root.path());

    let s = ok(h.request(Request::Status));
    assert!(!s.available && !s.listening);
    assert_eq!(s.route, RouteStatus::Off);
    assert!(matches!(s.tailscale, State::Running(_)));

    let s = ok(h.request(Request::Enable { port: None }));
    assert!(s.available && s.listening);
    assert_eq!((s.port, &s.route), (DEFAULT_PORT, &RouteStatus::Active));
    let target = format!(r#"{{"TCPForward":"unix:{}"}}"#, sock(&h).display());
    assert_eq!(fake.route(DEFAULT_PORT).as_deref(), Some(target.as_str()));
    assert!(sock(&h).exists());

    // A port that serves something else is left alone; nothing changes.
    let err = h.request(Request::Enable { port: Some(443) }).into_result().unwrap_err();
    assert!(err.to_string().contains("443 already serves a web server"), "{err}");
    assert_eq!(fake.route(443).as_deref(), Some(r#"{"HTTPS":true}"#));
    assert_eq!(fake.route(DEFAULT_PORT).as_deref(), Some(target.as_str()));
    assert_eq!(h.state().port, DEFAULT_PORT);

    // Another port: the route moves.
    let s = ok(h.request(Request::Enable { port: Some(7500) }));
    assert_eq!((s.port, &s.route), (7500, &RouteStatus::Active));
    assert_eq!(fake.route(DEFAULT_PORT), None);
    assert_eq!(fake.route(7500).as_deref(), Some(target.as_str()));

    // Survives a restart of the daemon (the state is on disk).
    drop(h);
    let h = host(&fake, root.path());
    let s = ok(h.request(Request::Status));
    assert!(s.available && s.listening);
    assert_eq!((s.port, &s.route), (7500, &RouteStatus::Active));

    let s = ok(h.request(Request::Disable));
    assert!(!s.available && !s.listening);
    assert_eq!(fake.route(7500), None);
    assert_eq!(fake.route(443).as_deref(), Some(r#"{"HTTPS":true}"#), "other routes stay");
    assert!(!sock(&h).exists());
}

#[test]
fn missing_permission_shows_the_commands() {
    let fake = Fake::new();
    let root = tempfile::tempdir().unwrap();
    let h = host(&fake, root.path());
    fake.deny(true);
    let err = h.request(Request::Enable { port: None }).into_result().unwrap_err();
    assert!(err.to_string().contains("sudo tailscale set --operator="), "{err}");
    let s = ok(h.request(Request::Status));
    assert!(!s.available && !s.listening, "nothing to listen for without the route");
    let RouteStatus::Permission(p) = &s.route else { panic!("{:?}", s.route) };
    assert!(p.direct.starts_with("sudo tailscale serve --bg --tcp=7422 unix:"), "{}", p.direct);
    fake.deny(false);
    assert_eq!(ok(h.request(Request::Enable { port: None })).route, RouteStatus::Active);
}

#[test]
fn tailscale_missing_or_logged_out() {
    let fake = Fake::new();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(fake.dir.path().join("status.json"), r#"{"Version":"1.104.7","BackendState":"NeedsLogin"}"#).unwrap();
    let h = host(&fake, root.path());
    let err = h.request(Request::Enable { port: None }).into_result().unwrap_err();
    assert!(err.to_string().contains("isn't logged in"), "{err}");
    assert_eq!(fake.route(DEFAULT_PORT), None);

    let info = HostInfo { name: "desk".into(), goatty: "0.1.0".into(), protocol: (7, 12) };
    let missing = Tailscale::with_program(root.path().join("no-tailscale-here"));
    let h = Arc::new(Host::new(missing, Paths::new(root.path().join("t2")), root.path().join("g.sock"), info).unwrap());
    let s = h.status();
    assert_eq!(s.tailscale, State::NotInstalled);
}

#[test]
fn paired_devices_reach_the_daemon_through_the_gateway() {
    let fake = Fake::new();
    let root = tempfile::tempdir().unwrap();
    let h = host(&fake, root.path());
    // Pairing needs availability.
    assert!(h.request(Request::OpenPairing { secs: None }).into_result().is_err());
    ok(h.request(Request::Enable { port: None }));
    let s = ok(h.request(Request::OpenPairing { secs: None }));
    assert!(s.pairing.is_some());

    let me = Identity::generate().unwrap();
    let conn = UnixStream::connect(sock(&h)).unwrap();
    let mut w = conn.try_clone().unwrap();
    let mut r = BufReader::new(conn);
    let pairing = gateway::start_pairing(&mut r, &mut w, &me, "laptop").unwrap();
    let pending = ok(h.request(Request::Status)).pairing.unwrap().pending;
    assert_eq!(pending[0].code, pairing.code);
    ok(h.request(Request::Decide { id: pending[0].id, accept: true }));
    pairing.wait(&mut r).unwrap();
    let host_key = pairing.greeting.key.clone();
    assert_eq!(ok(h.request(Request::Status)).devices[0].name, "laptop");

    // Through `tailscale nc` (the fake relays to the gateway socket).
    if std::process::Command::new("python3").arg("-c").arg("").status().is_ok_and(|s| s.success()) {
        std::fs::write(fake.dir.path().join("nc-socket"), sock(&h).to_str().unwrap()).unwrap();
        let link =
            goatty_net::link::open(&fake.ts(), goatty_net::link::Mode::Nc, "100.64.0.1", DEFAULT_PORT, Duration::from_secs(3)).unwrap();
        let mut r = BufReader::new(link.reader);
        let mut w = link.writer;
        gateway::authenticate(&mut r, &mut w, &me, &host_key).unwrap();
        w.write_all(b"howdy").unwrap();
        w.flush().unwrap();
        let mut back = [0u8; 5];
        r.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"howdy");
        link.closer.close();
    }

    // Revoked: refused; leaving forgets everyone and removes the route.
    ok(h.request(Request::Revoke { key: me.public()[..8].into() }));
    let conn = UnixStream::connect(sock(&h)).unwrap();
    let mut w = conn.try_clone().unwrap();
    let mut r = BufReader::new(conn);
    assert!(gateway::authenticate(&mut r, &mut w, &me, &host_key).is_err());
    let s = ok(h.request(Request::Leave));
    assert!(s.devices.is_empty() && !s.available);
    assert_eq!(fake.route(DEFAULT_PORT), None);
}
