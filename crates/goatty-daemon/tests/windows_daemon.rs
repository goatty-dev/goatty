#![cfg(windows)]
//! End-to-end tests for `goattyd.exe` on Windows (#277):
//! * Named Pipes local IPC (sandboxed `GOATTY_SOCKET` file path mapped to
//!   `\\.\pipe\goatty-<sha256>`, plus explicit `\\.\pipe\...` names),
//! * `Control::connect` + `host()` reporting Windows user/hostname/OS,
//! * `Connection::open` + ConPTY session creation (`cmd.exe`), input/output,
//!   resize, detach/reattach preserving screen contents, and Job Object `kill`,
//! * `goattyd.exe proxy` binary framing over stdio (`_setmode(O_BINARY)`),
//! * `control::socket_owner` (`GetNamedPipeServerProcessId` + user SID check),
//! * `local::outdated_than`, `goattyd restart` refusing without `--force`, and
//!   `goattyd stop --force` stopping the daemon.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use goatty_client::local;
use goatty_client::{Connection, Control, Endpoint, LocalStream, PaneEvent, PaneSink};
use goatty_proto::WinSize;
use goatty_term::{Frame, Palette, TermSize, Terminal};
use parking_lot::Mutex;

const SIZE: WinSize = WinSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };
const TSIZE: TermSize = TermSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };

struct TestDaemon {
    child: Child,
    sock: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_goattyd(exe: &Path, dir: &Path, sock: &Path) -> Child {
    Command::new(exe)
        .arg("serve")
        .env("GOATTY_SOCKET", sock)
        .env("GOATTY_SHELL", "cmd.exe")
        .env("RUST_LOG", "warn,rio_vt=error")
        .env("USERPROFILE", dir)
        .env("APPDATA", dir.join("AppData\\Roaming"))
        .env("LOCALAPPDATA", dir.join("AppData\\Local"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_RUNTIME_DIR")
        .spawn()
        .unwrap()
}

fn start_daemon() -> TestDaemon {
    let dir = tempfile::Builder::new().prefix("goatty-win-test-").tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let child = spawn_goattyd(Path::new(env!("CARGO_BIN_EXE_goattyd")), dir.path(), &sock);
    let deadline = Instant::now() + Duration::from_secs(10);
    while LocalStream::connect(&sock).is_err() {
        assert!(Instant::now() < deadline, "goattyd.exe did not start listening on {}", sock.display());
        std::thread::sleep(Duration::from_millis(25));
    }
    TestDaemon { child, sock, _dir: dir }
}

struct MirrorSink {
    term: Terminal,
    events: Mutex<Vec<String>>,
}

impl MirrorSink {
    fn new() -> Arc<Self> {
        let (term, _ch) = Terminal::new(TSIZE, false, Arc::new(Palette::default()));
        Arc::new(Self { term, events: Mutex::new(Vec::new()) })
    }

    fn text(&self) -> String {
        let mut f = Frame::default();
        self.term.snapshot_into(&mut f);
        (0..f.rows).map(|r| f.row(r).iter().map(|c| c.ch).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.text().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }
}

impl PaneSink for MirrorSink {
    fn feed(&self, bytes: &[u8]) {
        self.term.feed(bytes);
    }

    fn event(&self, ev: PaneEvent) {
        self.events.lock().push(format!("{ev:?}"));
    }
}

#[test]
fn windows_daemon_named_pipes_conpty_and_proxy_end_to_end() {
    let d = start_daemon();

    // 1. Control client & Named Pipe server PID ownership verification.
    let mut ctl = Control::connect(&d.sock, "win-e2e").unwrap();
    assert_eq!(ctl.pid(), d.child.id());
    assert_eq!(ctl.host().os, "windows");
    assert!(!ctl.host().hostname.is_empty());
    assert_eq!(goatty_client::control::socket_owner(&d.sock).unwrap(), d.child.id());

    // 2. Connect via `goattyd.exe proxy` over stdio (exercises binary mode +
    //    `GetStdHandle` + Named Pipe relay + ConPTY session lifecycle).
    let proxy_ep = Endpoint::Command {
        label: "win-proxy".into(),
        program: "cmd.exe".into(),
        args: vec!["/C".into(), format!("set GOATTY_SOCKET={}&& \"{}\" proxy", d.sock.display(), env!("CARGO_BIN_EXE_goattyd"))],
    };
    let conn = Connection::open(proxy_ep, "win-e2e-conn").unwrap();
    let sink = MirrorSink::new();
    let id = conn.create(SIZE, None, sink.clone()).unwrap();

    conn.input(id, b"echo WIN_CONPTY_OK_12345\r\n");
    assert!(sink.wait_for("WIN_CONPTY_OK_12345", Duration::from_secs(10)), "screen:\n{}", sink.text());

    // Resize without blocking.
    conn.resize(id, WinSize { cols: 100, rows: 30, cell_width: 8, cell_height: 16 });
    assert_eq!(ctl.list_sessions().unwrap().len(), 1);

    // Detach and reattach on a fresh mirror sink: screen contents survive.
    conn.detach(id);
    let sink2 = MirrorSink::new();
    conn.attach(id, SIZE, sink2.clone()).unwrap();
    assert!(sink2.wait_for("WIN_CONPTY_OK_12345", Duration::from_secs(10)), "reattached screen:\n{}", sink2.text());

    // Short-lived `Run` exits cleanly, closes ConPTY, sends Exited, and leaves `list_sessions`.
    let run_sink = MirrorSink::new();
    let _run_id =
        conn.run(SIZE, None, vec!["cmd.exe".into(), "/C".into(), "echo".into(), "WIN_RUN_EXIT_OK".into()], run_sink.clone()).unwrap();
    assert!(run_sink.wait_for("WIN_RUN_EXIT_OK", Duration::from_secs(10)), "run screen:\n{}", run_sink.text());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !run_sink.events.lock().iter().any(|e| e.starts_with("Exited(")) {
        assert!(
            Instant::now() < deadline,
            "short-lived Run session did not deliver PaneEvent::Exited (events: {:?})",
            *run_sink.events.lock()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(run_sink.events.lock().iter().any(|e| e == "Exited(Some(0))"), "expected Exited(Some(0)), got {:?}", *run_sink.events.lock());
    let deadline = Instant::now() + Duration::from_secs(10);
    while ctl.list_sessions().unwrap().len() != 1 {
        assert!(Instant::now() < deadline, "short-lived Run session did not leave list_sessions");
        std::thread::sleep(Duration::from_millis(25));
    }

    // Kill ends the remaining interactive session cleanly via the Job Object.
    conn.kill(id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ctl.list_sessions().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "session did not exit after kill");
        std::thread::sleep(Duration::from_millis(25));
    }

    drop(conn);
    drop(ctl);
}

#[test]
fn windows_proxy_exits_cleanly_after_stdin_eof_and_delivers_replies() {
    use goatty_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg, read_msg, write_msg};
    use std::io::{Read, Write};
    use std::process::Stdio;

    let d = start_daemon();
    let mut proxy = Command::new(env!("CARGO_BIN_EXE_goattyd"))
        .arg("proxy")
        .env("GOATTY_SOCKET", &d.sock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = proxy.stdin.take().unwrap();
    let mut stdout = proxy.stdout.take().unwrap();

    write_msg(&mut stdin, &ClientMsg::Hello { version: PROTOCOL_VERSION, client: "win-proxy-eof".into() }).unwrap();
    write_msg(&mut stdin, &ClientMsg::ListSessions { req: 1 }).unwrap();
    stdin.flush().unwrap();
    // Close stdin immediately: proxy must signal write-EOF to the daemon,
    // still flush both replies to stdout, and exit cleanly without hanging.
    drop(stdin);

    let hello: ServerMsg = read_msg(&mut stdout).unwrap();
    assert!(matches!(hello, ServerMsg::Hello { .. }), "expected Hello, got {hello:?}");
    let sessions: ServerMsg = read_msg(&mut stdout).unwrap();
    assert!(matches!(sessions, ServerMsg::Sessions { req: 1, .. }), "expected Sessions, got {sessions:?}");
    let mut tail = Vec::new();
    stdout.read_to_end(&mut tail).unwrap();
    assert!(tail.is_empty(), "unexpected trailing bytes: {tail:?}");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = proxy.try_wait().unwrap() {
            assert!(status.success(), "proxy exited with {status}");
            break;
        }
        assert!(Instant::now() < deadline, "goattyd.exe proxy hung after stdin EOF");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn windows_outdated_detection_restart_policy_and_force_stop() {
    let dir = tempfile::Builder::new().prefix("goatty-win-restart-").tempdir().unwrap();
    let v1_dir = dir.path().join("v1");
    let v2_dir = dir.path().join("v2");
    std::fs::create_dir_all(&v1_dir).unwrap();
    std::fs::create_dir_all(&v2_dir).unwrap();
    let exe_v1 = v1_dir.join("goattyd.exe");
    let exe_v2 = v2_dir.join("goattyd.exe");
    let sock = dir.path().join("d.sock");
    std::fs::copy(env!("CARGO_BIN_EXE_goattyd"), &exe_v1).unwrap();

    let mut daemon = TestDaemon { child: spawn_goattyd(&exe_v1, dir.path(), &sock), sock: sock.clone(), _dir: dir };
    let deadline = Instant::now() + Duration::from_secs(10);
    while LocalStream::connect(&sock).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(25));
    }
    let pid = daemon.child.id();

    // Running binary vs itself: not outdated.
    assert_eq!(local::outdated_than(pid, exe_v1.clone()), None);

    // On Windows, `std::fs::copy` preserves the source mtime via `CopyFileEx`.
    // Touch `exe_v2` explicitly so its mtime is strictly newer than the daemon start time.
    std::thread::sleep(Duration::from_millis(50));
    std::fs::copy(env!("CARGO_BIN_EXE_goattyd"), &exe_v2).unwrap();
    std::fs::File::options().write(true).open(&exe_v2).unwrap().set_modified(std::time::SystemTime::now()).unwrap();
    let outdated = local::outdated_than(pid, exe_v2.clone()).expect("newer copy should be detected as outdated");
    assert_eq!(outdated.pid, pid);

    // `goattyd restart` without `--force` refuses on Windows (no execve / ConPTY handoff).
    let out = Command::new(&exe_v2).arg("restart").env("GOATTY_SOCKET", &sock).output().unwrap();
    assert!(!out.status.success(), "restart without --force should fail on Windows");

    // `goattyd stop --force` stops the running daemon cleanly.
    let out = Command::new(&exe_v2).args(["stop", "--force"]).env("GOATTY_SOCKET", &sock).output().unwrap();
    assert!(out.status.success(), "stop --force failed: {}", String::from_utf8_lossy(&out.stderr));
    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "daemon did not exit after stop --force");
        std::thread::sleep(Duration::from_millis(25));
    }
}
