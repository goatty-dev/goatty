#![cfg(unix)]
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
use goatty_client::{AttachRequest, Connection, Control, Endpoint, PaneEvent, PaneSink, TailnetTarget};
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
    // The guard first: a panic while waiting still kills the daemon.
    let d = Daemon { child, sock, dir };
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&d.sock).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    d
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
    let target = TailnetTarget { dns: "desk.tail1.ts.net".into(), name: "desk".into(), paths: Some(paths.clone()), tailscale: ts.clone() };

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
    let sink2 = Arc::new(Sink { term: Mutex::new(Terminal::new(tsize, false, Arc::new(Palette::default())).0), events: Mutex::default() });
    let id2 = conn.create(size, None, sink2.clone()).unwrap();
    conn.input(id2, b"echo second-$((20+1))-pane\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sink.screen().contains("over-the-42-tailnet") || !sink2.screen().contains("second-21-pane") {
        assert!(Instant::now() < deadline, "no output; screen:\n{}\n{}", sink.screen(), sink2.screen());
        std::thread::sleep(Duration::from_millis(20));
    }
    // The host's goattyd can be managed over the same link.
    let reply: Reply = serde_json::from_str(&conn.net(serde_json::to_string(&Request::Status).unwrap()).unwrap()).unwrap();
    assert!(reply.into_result().unwrap().available);
    drop(conn);

    // Reopen over the tailnet and reattach both panes in one pipelined burst.
    let conn2 = Connection::open(Endpoint::Tailnet(target.clone()), "test-restore").unwrap();
    let r1 = Arc::new(Sink { term: Mutex::new(Terminal::new(tsize, false, Arc::new(Palette::default())).0), events: Mutex::default() });
    let r2 = Arc::new(Sink { term: Mutex::new(Terminal::new(tsize, false, Arc::new(Palette::default())).0), events: Mutex::default() });
    let results = conn2
        .attach_many(vec![AttachRequest { session: id, size, sink: r1.clone() }, AttachRequest { session: id2, size, sink: r2.clone() }]);
    assert!(results.iter().all(Result::is_ok), "tailnet attach_many failed: {results:?}");
    assert!(r1.screen().contains("over-the-42-tailnet"), "pane 1 missing output:\n{}", r1.screen());
    assert!(r2.screen().contains("second-21-pane"), "pane 2 missing output:\n{}", r2.screen());
    drop(conn2);

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
    net(&mut c, &Request::Enable { port: Some(7500) }).unwrap_or_else(|e| panic!("enable: {e:#}"));
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

#[test]
fn headless_token_pairing_and_direct_remote_pairing() {
    if !has_python() {
        eprintln!("skipped: the fake `tailscale nc` needs python3");
        return;
    }
    let d = start_daemon();
    let gateway = goatty_net::gateway_socket(&d.sock);
    let bin = d.dir.path().join("fakebin");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());

    // 1. `goattyd net pair --token-only --port 7510` auto-enables on a custom
    //    port, opens pairing, and prints a single-use `goatty-pair:…` token.
    let out = Command::new(env!("CARGO_BIN_EXE_goattyd"))
        .args(["net", "pair", "--token-only", "--port", "7510"])
        .env("GOATTY_SOCKET", &d.sock)
        .env("PATH", &path)
        .env("HOME", d.dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(goatty_net::pairing::PairToken::looks_like(&token), "{token}");

    // Client pairs in one step with the token — no confirmation needed on host,
    // and custom port 7510 is carried automatically.
    let client = tempfile::tempdir().unwrap();
    let ts = Tailscale::with_program(fake(&client.path().join("bin"), CLIENT_STATUS));
    std::fs::write(client.path().join("bin/nc-socket"), gateway.to_str().unwrap()).unwrap();
    let paths = Paths::new(client.path().join("tailnet"));
    let host = tailnet::pair_with_token(&ts, &paths, &token, "laptop").unwrap();
    assert_eq!(host.dns, "desk.tail1.ts.net");
    assert_eq!(host.port, 7510);

    // Connecting works immediately over Endpoint::Tailnet on the custom port.
    let target = TailnetTarget { dns: host.dns.clone(), name: host.name.clone(), paths: Some(paths.clone()), tailscale: ts.clone() };
    let conn = Connection::open(Endpoint::Tailnet(target), "test-token").unwrap();
    assert_eq!(conn.protocol_version(), goatty_proto::PROTOCOL_VERSION);
    drop(conn);

    // Reusing the same token fails (single-use).
    let client2 = tempfile::tempdir().unwrap();
    let paths2 = Paths::new(client2.path().join("tailnet"));
    assert!(tailnet::pair_with_token(&ts, &paths2, &token, "other").is_err());

    // 2. Direct remote pairing over an authenticated control link (as over SSH):
    //    disable first, then `pair_via_remote` re-enables, authorizes, pins, and probes.
    let mut c = Control::connect(&d.sock, "test-ssh").unwrap();
    net(&mut c, &Request::Disable).unwrap();
    let outcome = tailnet::pair_via_remote(|req| net(&mut c, req), &ts, &paths2, "workstation").unwrap();
    let tailnet::DirectPairOutcome::Paired { host: h2, reachability } = outcome else {
        panic!("expected Paired, got {outcome:?}");
    };
    assert_eq!(h2.dns, "desk.tail1.ts.net");
    assert_eq!(h2.port, 7510);
    assert!(reachability.is_ok(), "reachability: {reachability:?}");
    let target2 = TailnetTarget { dns: h2.dns, name: h2.name, paths: Some(paths2), tailscale: ts };
    let conn2 = Connection::open(Endpoint::Tailnet(target2), "test-direct").unwrap();
    assert_eq!(conn2.protocol_version(), goatty_proto::PROTOCOL_VERSION);
}
