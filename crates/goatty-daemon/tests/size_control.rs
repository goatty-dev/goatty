//! End-to-end tests of the size policy (#12) against a real goattyd: one
//! controlling client per session sets the size, the others watch at it
//! (`SizeControl`) until they take over (`TakeSize`) or the controller
//! leaves; clients older than v14 take control as before.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use goatty_client::Control;
use goatty_proto::{ClientMsg, PROTOCOL_VERSION, SIZE_CONTROL_SINCE, ServerMsg, SessionId, StreamOpts, WinSize, encode, read_msg};

struct Daemon {
    child: Child,
    sock: PathBuf,
    _dir: tempfile::TempDir,
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
    let child = Command::new(env!("CARGO_BIN_EXE_goattyd"))
        .env("GOATTY_SOCKET", &sock)
        .env("GOATTY_SHELL", "/bin/sh")
        .env("RUST_LOG", "warn,rio_vt=error")
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .spawn()
        .unwrap();
    // The guard first, so a daemon that misses the deadline is killed too.
    let d = Daemon { child, sock, _dir: dir };
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&d.sock).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    d
}

fn send(s: &mut UnixStream, msg: &ClientMsg) {
    s.write_all(&encode(msg).unwrap()).unwrap();
}

fn connect(sock: &Path, version: u32) -> UnixStream {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send(&mut s, &ClientMsg::Hello { version, client: "test".into() });
    assert!(matches!(read_msg::<_, ServerMsg>(&mut s).unwrap(), ServerMsg::Hello { .. }));
    s
}

fn ws(cols: u16, rows: u16) -> WinSize {
    WinSize { cols, rows, cell_width: 8, cell_height: 16 }
}

/// The next `SizeControl` for `session` as (cols, rows, controlling),
/// skipping everything else.
fn next_size_control(s: &mut UnixStream, session: SessionId) -> (u16, u16, bool) {
    loop {
        match read_msg::<_, ServerMsg>(s).unwrap() {
            ServerMsg::SizeControl { session: id, size, controlling } if id == session => return (size.cols, size.rows, controlling),
            _ => {}
        }
    }
}

/// Everything this connection sent so far has been handled.
fn round_trip(s: &mut UnixStream) {
    send(s, &ClientMsg::Ping { nonce: 7 });
    loop {
        if let ServerMsg::Pong { nonce: 7 } = read_msg::<_, ServerMsg>(s).unwrap() {
            return;
        }
    }
}

/// Read output of `session` until it contains `needle`.
fn output_until(s: &mut UnixStream, session: SessionId, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains(needle) {
        assert!(Instant::now() < deadline, "no {needle:?} in {:?}", String::from_utf8_lossy(&seen));
        if let ServerMsg::Output { session: id, data } = read_msg::<_, ServerMsg>(s).unwrap()
            && id == session
        {
            seen.extend(data);
        }
    }
}

fn size_of(sock: &Path, id: SessionId) -> (u16, u16) {
    let mut c = Control::connect(sock, "test").unwrap();
    let s = c.list_sessions().unwrap().into_iter().find(|s| s.id == id).expect("session listed");
    (s.cols, s.rows)
}

fn wait_size(sock: &Path, id: SessionId, want: (u16, u16)) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while size_of(sock, id) != want {
        assert!(Instant::now() < deadline, "session is {:?}, want {want:?}", size_of(sock, id));
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn one_client_controls_the_size_and_others_watch_until_they_take_over() {
    let d = start_daemon();

    // A creates the session and controls it.
    let mut a = connect(&d.sock, PROTOCOL_VERSION);
    send(&mut a, &ClientMsg::Create { req: 1, size: ws(60, 12), cwd: None, opts: StreamOpts::LOCAL });
    let (id, first) = loop {
        match read_msg::<_, ServerMsg>(&mut a).unwrap() {
            ServerMsg::SizeControl { session, size, controlling } => break (session, (size.cols, size.rows, controlling)),
            ServerMsg::Attached { .. } => panic!("SizeControl comes before Attached"),
            _ => {}
        }
    };
    assert_eq!(first, (60, 12, true));

    // B attaches with a bigger pane: it watches, before its snapshot, and
    // the session keeps A's size.
    let mut b = connect(&d.sock, PROTOCOL_VERSION);
    send(&mut b, &ClientMsg::Attach { req: 2, session: id, size: ws(100, 30), opts: StreamOpts::LOCAL });
    let first = loop {
        match read_msg::<_, ServerMsg>(&mut b).unwrap() {
            ServerMsg::SizeControl { size, controlling, .. } => break (size.cols, size.rows, controlling),
            ServerMsg::Attached { .. } => panic!("SizeControl comes before Attached"),
            _ => {}
        }
    };
    assert_eq!(first, (60, 12, false));
    assert_eq!(size_of(&d.sock, id), (60, 12));

    // A watcher's resize only records what it wants.
    send(&mut b, &ClientMsg::Resize { session: id, size: ws(90, 20) });
    round_trip(&mut b);
    assert_eq!(size_of(&d.sock, id), (60, 12));

    // The controller's resize applies, and the watcher hears it.
    send(&mut a, &ClientMsg::Resize { session: id, size: ws(70, 14) });
    assert_eq!(next_size_control(&mut b, id), (70, 14, false));
    assert_eq!(next_size_control(&mut a, id), (70, 14, true));
    wait_size(&d.sock, id, (70, 14));

    // B takes over: the session takes B's last size.
    send(&mut b, &ClientMsg::TakeSize { session: id });
    assert_eq!(next_size_control(&mut b, id), (90, 20, true));
    assert_eq!(next_size_control(&mut a, id), (90, 20, false));
    wait_size(&d.sock, id, (90, 20));

    // Now A's resizes don't apply...
    send(&mut a, &ClientMsg::Resize { session: id, size: ws(72, 15) });
    round_trip(&mut a);
    assert_eq!(size_of(&d.sock, id), (90, 20));

    // ...until B leaves: control goes back to A, at A's size.
    send(&mut b, &ClientMsg::Detach { session: id });
    assert_eq!(next_size_control(&mut a, id), (72, 15, true));
    wait_size(&d.sock, id, (72, 15));

    // A client from before v14 takes control as it always did.
    let mut c = connect(&d.sock, SIZE_CONTROL_SINCE - 1);
    send(&mut c, &ClientMsg::Attach { req: 3, session: id, size: ws(50, 10), opts: StreamOpts::LOCAL });
    assert_eq!(next_size_control(&mut a, id), (50, 10, false));
    wait_size(&d.sock, id, (50, 10));
    send(&mut a, &ClientMsg::Resize { session: id, size: ws(80, 24) });
    round_trip(&mut a);
    assert_eq!(size_of(&d.sock, id), (50, 10));
    // Its connection ending hands control back.
    drop(c);
    assert_eq!(next_size_control(&mut a, id), (80, 24, true));
    wait_size(&d.sock, id, (80, 24));

    // The PTY has the size too, not only the daemon's terminal, and had it
    // before A heard it controls again.
    send(&mut a, &ClientMsg::Input { session: id, data: b"stty size\r".to_vec() });
    output_until(&mut a, id, "24 80");
}

#[test]
fn the_first_client_to_attach_to_an_unwatched_session_controls_it() {
    let d = start_daemon();
    let mut a = connect(&d.sock, PROTOCOL_VERSION);
    send(&mut a, &ClientMsg::Create { req: 1, size: ws(60, 12), cwd: None, opts: StreamOpts::LOCAL });
    let id = loop {
        if let ServerMsg::Attached { session, .. } = read_msg::<_, ServerMsg>(&mut a).unwrap() {
            break session;
        }
    };
    send(&mut a, &ClientMsg::Detach { session: id });
    round_trip(&mut a);

    // Nobody's left: the next client to attach sets the size.
    let mut b = connect(&d.sock, PROTOCOL_VERSION);
    send(&mut b, &ClientMsg::Attach { req: 2, session: id, size: ws(100, 30), opts: StreamOpts::LOCAL });
    assert_eq!(next_size_control(&mut b, id), (100, 30, true));
    wait_size(&d.sock, id, (100, 30));

    // A size no screen has is refused, not clamped.
    let mut c = connect(&d.sock, PROTOCOL_VERSION);
    send(&mut c, &ClientMsg::Attach { req: 4, session: id, size: ws(5000, 5000), opts: StreamOpts::LOCAL });
    match read_msg::<_, ServerMsg>(&mut c).unwrap() {
        ServerMsg::Error { req: Some(4), message } => assert!(message.contains("too large"), "{message}"),
        other => panic!("{other:?}"),
    }
    send(&mut b, &ClientMsg::Resize { session: id, size: ws(4000, 2000) });
    send(&mut b, &ClientMsg::Ping { nonce: 8 });
    let mut refused = false;
    loop {
        match read_msg::<_, ServerMsg>(&mut b).unwrap() {
            ServerMsg::Error { req: None, message } if message.contains("too large") => refused = true,
            ServerMsg::Pong { nonce: 8 } => break,
            _ => {}
        }
    }
    assert!(refused, "the oversized resize got no error");
    assert_eq!(size_of(&d.sock, id), (100, 30));
}
