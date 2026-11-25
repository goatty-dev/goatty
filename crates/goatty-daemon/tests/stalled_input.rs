#![cfg(unix)]
//! A program that stops reading its input fills the PTY, and writing more
//! blocks. That must not block the connection: it still answers pings, so
//! a live client isn't taken for a dead one, and still sees its own EOF, so
//! a dead one is detached (#13). Nor may it hold up `Kill` (#233).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use goatty_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg, StreamOpts, WinSize, encode, read_msg};

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

fn connect(d: &Daemon) -> UnixStream {
    let mut s = UnixStream::connect(&d.sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send(&mut s, &ClientMsg::Hello { version: PROTOCOL_VERSION, client: "test".into() });
    assert!(matches!(read_msg::<_, ServerMsg>(&mut s).unwrap(), ServerMsg::Hello { .. }));
    s
}

/// The number of clients attached to the only session.
fn attached(d: &Daemon) -> u32 {
    let mut s = connect(d);
    send(&mut s, &ClientMsg::ListSessions { req: 1 });
    loop {
        if let ServerMsg::Sessions { list, .. } = read_msg::<_, ServerMsg>(&mut s).unwrap() {
            return list[0].attached;
        }
    }
}

/// Start `sleep 60` and send it far more input than the PTY holds (a few
/// KiB), in lines so none is dropped. Returns the session.
fn flood_sleeper(s: &mut UnixStream) -> u64 {
    let size = WinSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };
    let argv = vec!["sleep".into(), "60".into()];
    send(s, &ClientMsg::Run { req: 1, size, cwd: None, argv, opts: StreamOpts::LOCAL });
    let session = loop {
        if let ServerMsg::Attached { session, .. } = read_msg::<_, ServerMsg>(s).unwrap() {
            break session;
        }
    };
    let line = [b"x".repeat(1023), b"\n".to_vec()].concat();
    for _ in 0..256 {
        send(s, &ClientMsg::Input { session, data: line.clone() });
    }
    session
}

#[test]
fn input_to_a_program_that_doesnt_read_doesnt_block_the_connection() {
    let d = start_daemon();
    let mut s = connect(&d);
    flood_sleeper(&mut s);

    send(&mut s, &ClientMsg::Ping { nonce: 42 });
    loop {
        match read_msg::<_, ServerMsg>(&mut s) {
            Ok(ServerMsg::Pong { nonce: 42 }) => break,
            Ok(_) => {}
            Err(e) => panic!("no Pong while the PTY is full: {e}"),
        }
    }

    assert_eq!(attached(&d), 1);
    drop(s);
    let deadline = Instant::now() + Duration::from_secs(5);
    while attached(&d) != 0 {
        assert!(Instant::now() < deadline, "the closed connection stayed attached");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Closing the tab of a program that stopped reading must end it, even
/// while an input write to it is stuck (#233).
#[test]
fn kill_ends_a_session_whose_input_is_stuck() {
    let d = start_daemon();
    let mut s = connect(&d);
    let session = flood_sleeper(&mut s);
    // Let the input thread get stuck in its write.
    std::thread::sleep(Duration::from_millis(200));
    send(&mut s, &ClientMsg::Kill { req: 2, session });
    let (mut ok, mut exited) = (false, false);
    while !(ok && exited) {
        match read_msg::<_, ServerMsg>(&mut s) {
            Ok(ServerMsg::Ok { req: 2 }) => ok = true,
            Ok(ServerMsg::Exited { session: id, .. }) if id == session => exited = true,
            Ok(_) => {}
            Err(e) => panic!("Kill didn't end the session (ok: {ok}, exited: {exited}): {e}"),
        }
    }
}
