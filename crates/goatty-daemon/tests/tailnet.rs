//! goatty over the tailnet end to end, against a fake `tailscale` (never
//! the user's Tailscale or daemon): the host's goattyd adds its Serve route
//! and opens pairing through the `Net` carrier, a "client computer" (its
//! own device key) pairs through `tailscale nc` (the fake relays into the
//! gateway socket), connects with `Endpoint::Tailnet` and runs a command
//! in a real session. An unpaired key gets nowhere.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use goatty_client::tailnet::{self, Pairing};
use goatty_client::{Connection, Control, Endpoint, PaneEvent, PaneSink, TailnetTarget};
use goatty_net::Paths;
use goatty_net::api::{NetStatus, Reply, Request, RouteStatus};
use goatty_net::tailscale::{State, Tailscale};
use goatty_proto::WinSize;
use goatty_term::{Frame, Palette, TermSize, Terminal};
use parking_lot::Mutex;

const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../goatty-net/tests/fake-tailscale.sh");

fn fake(dir: &Path, status: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("tcp")).unwrap();
    std::fs::copy(FAKE, dir.join("tailscale")).unwrap();
    std::fs::write(dir.join("status.json"), status).unwrap();
    dir.join("tailscale")
}

const HOST_STATUS: &str = r#"{"Version": "1.104.7", "TUN": false, "BackendState": "Running",
  "Self": {"HostName": "desk", "DNSName": "desk.tail1.ts.net.", "OS": "linux", "UserID": 1, "TailscaleIPs": ["100.64.0.1"]},
  "Peer": {}, "User": {}}"#;

const CLIENT_STATUS: &str = r#"{"Version": "1.104.7", "TUN": false, "BackendState": "Running",
  "Self": {"HostName": "laptop", "DNSName": "laptop.tail1.ts.net.", "OS": "linux", "UserID": 1, "TailscaleIPs": ["100.64.0.2"]},
  "Peer": {"nodekey:a": {"HostName": "desk", "DNSName": "desk.tail1.ts.net.", "OS": "linux", "UserID": 1,
                         "TailscaleIPs": ["100.64.0.1"], "Online": true, "CurAddr": "192.168.1.4:41641"}},
  "User": {}}"#;

struct Daemon {
    child: Child,
    sock: PathBuf,
    dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_daemon() -> Daemon {
    let dir = tempfile::Builder::new().prefix("goatty-test-").tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let bin = dir.path().join("fakebin");
    fake(&bin, HOST_STATUS);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let child = Command::new(env!("CARGO_BIN_EXE_goattyd"))
        .env("GOATTY_SOCKET", &sock)
        .env("GOATTY_SHELL", "/bin/sh")
        .env("RUST_LOG", "warn,rio_vt=error")
        .env("PATH", path)
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&sock).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child, sock, dir }
}

fn net(c: &mut Control, req: &Request) -> anyhow::Result<NetStatus> {
    let reply = c.net(serde_json::to_string(req).unwrap())?;
    serde_json::from_str::<Reply>(&reply).unwrap().into_result()
}

struct Sink {
    term: Mutex<Terminal>,
    events: Mutex<Vec<String>>,
}

impl PaneSink for Sink {
    fn feed(&self, bytes: &[u8]) {
        self.term.lock().feed(bytes);
    }
    fn event(&self, ev: PaneEvent) {
        self.events.lock().push(format!("{ev:?}"));
    }
}

impl Sink {
    fn screen(&self) -> String {
        let mut f = Frame::default();
        self.term.lock().snapshot_into(&mut f);
        (0..f.rows).map(|r| f.row(r).iter().map(|c| c.ch).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }
}

fn has_python() -> bool {
    Command::new("python3").arg("-c").arg("").status().is_ok_and(|s| s.success())
}

#[test]
fn pair_and_run_a_session_over_the_tailnet() {
    if !has_python() {
        eprintln!("skipped: the fake `tailscale nc` needs python3");
        return;
    }
    let d = start_daemon();
    let mut c = Control::connect(&d.sock, "test").unwrap();
    let s = net(&mut c, &Request::Status).unwrap();
    assert!(matches!(s.tailscale, State::Running(_)), "{:?}", s.tailscale);
    assert!(!s.available);

    let s = net(&mut c, &Request::Enable { port: None }).unwrap();
    assert_eq!(s.route, RouteStatus::Active);
    let gateway = goatty_net::gateway_socket(&d.sock);
    assert!(gateway.exists());
    let route = std::fs::read_to_string(d.dir.path().join("fakebin/tcp/7422")).unwrap();
    assert!(route.contains(&format!("unix:{}", gateway.display())), "{route}");
    // The device key lives in the sandbox's data dir, private.
    let key = d.dir.path().join(".local/share/goatty/tailnet/device.key");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);

    // The "client computer": its own fake tailscale (nc relays to the gateway) and files.
    let client = tempfile::tempdir().unwrap();
    let ts = Tailscale::with_program(fake(&client.path().join("bin"), CLIENT_STATUS));
    std::fs::write(client.path().join("bin/nc-socket"), gateway.to_str().unwrap()).unwrap();
    let paths = Paths::new(client.path().join("tailnet"));
    let target = TailnetTarget { dns: "desk.tail1.ts.net".into(), name: "desk".into(), paths: paths.clone(), tailscale: ts.clone() };

    // Not paired yet: refused before anything is sent to the host.
    let err = Connection::open(Endpoint::Tailnet(target.clone()), "test").err().unwrap();
    assert!(format!("{err:#}").contains("isn't paired"), "{err:#}");

    // Discovery finds the host by probing it.
    let found = tailnet::discover(&ts, &paths, goatty_net::DEFAULT_PORT, false);
    assert_eq!(found.found.len(), 1, "{found:?}");
    assert_eq!(found.found[0].probe.greeting.key, s.key);
    assert!(!found.found[0].paired && !found.found[0].probe.pairing);

    // Pairing while the window is closed is refused.
    let State::Running(status) = ts.detect() else { panic!() };
    let node = status.peer("desk.tail1.ts.net").unwrap().clone();
    assert!(Pairing::start(&ts, &paths, &status, &node, 7422, "laptop").is_err());

    net(&mut c, &Request::OpenPairing { secs: None }).unwrap();
    let mut pairing = Pairing::start(&ts, &paths, &status, &node, 7422, "laptop").unwrap();
    let pending = net(&mut c, &Request::Status).unwrap().pairing.unwrap().pending;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].code, pairing.code, "both screens show the same code");
    net(&mut c, &Request::Decide { id: pending[0].id, accept: true }).unwrap();
    pairing.wait().unwrap();
    pairing.pin().unwrap();
    drop(pairing);
    assert_eq!(net(&mut c, &Request::Status).unwrap().devices[0].name, "laptop");
    assert!(tailnet::discover(&ts, &paths, 7422, false).found[0].paired);

    // A real session over the tailnet.
    let conn = Connection::open(Endpoint::Tailnet(target.clone()), "test").unwrap();
    assert_eq!(conn.protocol_version(), goatty_proto::PROTOCOL_VERSION);
    let size = WinSize { cols: 60, rows: 12, cell_width: 8, cell_height: 16 };
    let tsize = TermSize { cols: 60, rows: 12, cell_width: 8, cell_height: 16 };
    let sink = Arc::new(Sink { term: Mutex::new(Terminal::new(tsize, false, Arc::new(Palette::default())).0), events: Mutex::default() });
    let id = conn.create(size, None, sink.clone()).unwrap();
    conn.input(id, b"echo over-the-$((40+2))-tailnet\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sink.screen().contains("over-the-42-tailnet") {
        assert!(Instant::now() < deadline, "no output; screen:\n{}\nevents: {:?}", sink.screen(), sink.events.lock());
        std::thread::sleep(Duration::from_millis(20));
    }
    // The host's goattyd can be managed over the same link.
    let reply: Reply = serde_json::from_str(&conn.net(serde_json::to_string(&Request::Status).unwrap()).unwrap()).unwrap();
    assert!(reply.into_result().unwrap().available);
    drop(conn);

    // Revoked on the host: refused again.
    net(&mut c, &Request::Revoke { key: goatty_net::identity::Identity::load_or_create(&paths.key()).unwrap().public()[..8].into() })
        .unwrap();
    let err = Connection::open(Endpoint::Tailnet(target), "test").err().unwrap();
    assert!(format!("{err:#}").contains("isn't paired with"), "{err:#}");

    // Off: the route and the gateway socket are gone.
    net(&mut c, &Request::Disable).unwrap();
    assert!(!d.dir.path().join("fakebin/tcp/7422").exists());
    assert!(!gateway.exists());
}

#[test]
fn availability_comes_back_after_the_daemon_restarts() {
    let d = start_daemon();
    let mut c = Control::connect(&d.sock, "test").unwrap();
    net(&mut c, &Request::Enable { port: Some(7500) }).unwrap();
    let gateway = goatty_net::gateway_socket(&d.sock);
    drop(c);
    // `goattyd restart` re-executes the daemon (the gateway socket closes with the old image).
    let mut c = Control::connect(&d.sock, "test").unwrap();
    c.restart(None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(mut c) = Control::connect(&d.sock, "test")
            && let Ok(s) = net(&mut c, &Request::Status)
            && s.listening
        {
            assert_eq!((s.port, s.route), (7500, RouteStatus::Active));
            break;
        }
        assert!(Instant::now() < deadline, "the gateway didn't come back");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(gateway.exists());
}
