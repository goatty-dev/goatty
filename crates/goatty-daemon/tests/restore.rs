//! Sessions outlive goattyd (#131):
//! * `Restart` re-executes the daemon (here: an updated build at the same
//!   path) and the session, its screen and the saved layout carry over,
//! * a program that can't take over is refused and nothing changes,
//! * PTYs systemd kept in the fd store come back as sessions (a fake
//!   `LISTEN_FDS`, as after a crash), with the screen saved on SIGTERM.
//!
//! Sandboxed: each daemon has its own `GOATTY_SOCKET` and `HOME`.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use goatty_client::Control;
use goatty_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg, SessionId, StreamOpts, WinSize, encode, read_msg};
use goatty_term::TermSize;
use goatty_term::pty::{Pty, ShellCommand};

const SIZE: WinSize = WinSize { cols: 60, rows: 12, cell_width: 8, cell_height: 16 };

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn goattyd(exe: &Path, dir: &Path) -> Command {
    let mut c = Command::new(exe);
    c.env("GOATTY_SOCKET", dir.join("d.sock"))
        .env("GOATTY_SHELL", "/bin/sh")
        .env("RUST_LOG", "warn,rio_vt=error")
        .env("HOME", dir)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("LISTEN_PID")
        .env_remove("LISTEN_FDS")
        .env_remove("LISTEN_FDNAMES");
    c
}

/// Copy and rename, as cargo and package managers replace binaries.
fn install_goattyd(to: &Path) {
    let tmp = to.with_extension("new");
    std::fs::copy(env!("CARGO_BIN_EXE_goattyd"), &tmp).unwrap();
    std::fs::rename(&tmp, to).unwrap();
}

fn wait_for_socket(sock: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(sock).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn send(s: &mut UnixStream, msg: &ClientMsg) {
    s.write_all(&encode(msg).unwrap()).unwrap();
}

fn connect(sock: &Path) -> UnixStream {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut s, &ClientMsg::Hello { version: PROTOCOL_VERSION, client: "test".into() });
    assert!(matches!(read_msg::<_, ServerMsg>(&mut s).unwrap(), ServerMsg::Hello { .. }));
    s
}

/// Read stream frames until their text contains `needle`. Returns the text.
fn read_until(s: &mut UnixStream, needle: &str) -> String {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&out).contains(needle) {
        assert!(Instant::now() < deadline, "no {needle:?} in {:?}", String::from_utf8_lossy(&out));
        match read_msg::<_, ServerMsg>(s).unwrap() {
            ServerMsg::Attached { snapshot, .. } => out.extend(snapshot),
            ServerMsg::Output { data, .. } => out.extend(data),
            ServerMsg::Exited { .. } => panic!("the session ended before {needle:?}: {:?}", String::from_utf8_lossy(&out)),
            _ => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn wait_exited(s: &mut UnixStream, session: SessionId) -> Option<u32> {
    loop {
        if let ServerMsg::Exited { session: id, code } = read_msg::<_, ServerMsg>(s).unwrap()
            && id == session
        {
            return code;
        }
    }
}

#[test]
fn a_restart_into_an_updated_build_keeps_the_session() {
    let dir = tempfile::Builder::new().prefix("goatty-test-").tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let exe = dir.path().join("goattyd");
    install_goattyd(&exe);
    let daemon = Daemon(goattyd(&exe, dir.path()).arg("serve").spawn().unwrap());
    wait_for_socket(&sock);

    let mut s = connect(&sock);
    send(&mut s, &ClientMsg::Create { req: 1, size: SIZE, cwd: None, opts: StreamOpts::LOCAL });
    let id = match read_msg::<_, ServerMsg>(&mut s).unwrap() {
        ServerMsg::Attached { session, .. } => session,
        other => panic!("{other:?}"),
    };
    send(&mut s, &ClientMsg::Input { session: id, data: b"echo before-$((40+2))\r".to_vec() });
    read_until(&mut s, "before-42");
    // A full-screen program is on (the alternate screen) when it restarts.
    send(&mut s, &ClientMsg::Input { session: id, data: b"printf '\\033[?1049hIN-ALT-%s' $((3*3))\r".to_vec() });
    read_until(&mut s, "IN-ALT-9");
    send(&mut s, &ClientMsg::SaveLayout { id: 77, sessions: vec![id], json: "{\"tabs\":1}".into() });
    let pid_before = Control::connect(&sock, "test").unwrap().list_sessions().unwrap()[0].pid;

    // An update replaces the file the daemon runs.
    install_goattyd(&exe);
    if cfg!(target_os = "linux") {
        let running = std::fs::read_link(format!("/proc/{}/exe", daemon.0.id())).unwrap();
        assert!(running.to_string_lossy().ends_with(" (deleted)"));
    }

    Control::connect(&sock, "test").unwrap().restart(None).unwrap();
    // The old connection went with the old process image.
    while read_msg::<_, ServerMsg>(&mut s).is_ok() {}
    wait_for_socket(&sock);

    let mut c = Control::connect(&sock, "test").unwrap();
    assert_eq!(c.pid(), daemon.0.id(), "same process");
    if cfg!(target_os = "linux") {
        let running = std::fs::read_link(format!("/proc/{}/exe", daemon.0.id())).unwrap();
        assert!(!running.to_string_lossy().ends_with(" (deleted)"), "runs the new build: {}", running.display());
    }
    let list = c.list_sessions().unwrap();
    assert_eq!(list.iter().map(|s| (s.id, s.pid)).collect::<Vec<_>>(), [(id, pid_before)]);

    // The screen and the layout carried over; the shell still answers.
    let mut s = connect(&sock);
    send(&mut s, &ClientMsg::ClaimLayout { req: 2 });
    match read_msg::<_, ServerMsg>(&mut s).unwrap() {
        ServerMsg::Layout { layout: Some(l), .. } => assert_eq!((l.id, l.sessions), (77, vec![id])),
        other => panic!("{other:?}"),
    }
    send(&mut s, &ClientMsg::Attach { req: 3, session: id, size: SIZE, opts: StreamOpts::LOCAL });
    read_until(&mut s, "IN-ALT-9");
    // Leaving it shows the primary screen from before the restart.
    send(&mut s, &ClientMsg::Input { session: id, data: b"printf '\\033[?1049l'; echo after-$((1+1))\r".to_vec() });
    read_until(&mut s, "after-2");
    let mut viewer = connect(&sock);
    send(&mut viewer, &ClientMsg::Attach { req: 1, session: id, size: SIZE, opts: StreamOpts::LOCAL });
    match read_msg::<_, ServerMsg>(&mut viewer).unwrap() {
        ServerMsg::Attached { snapshot, .. } => {
            let text = String::from_utf8_lossy(&snapshot);
            assert!(text.contains("before-42") && text.contains("after-2"), "{text:?}");
        }
        other => panic!("{other:?}"),
    }

    // New sessions don't reuse the id.
    send(&mut s, &ClientMsg::Create { req: 4, size: SIZE, cwd: None, opts: StreamOpts::LOCAL });
    loop {
        if let ServerMsg::Attached { req: 4, session, .. } = read_msg::<_, ServerMsg>(&mut s).unwrap() {
            assert!(session > id);
            break;
        }
    }

    // Still its child: ending it reports an exit code.
    send(&mut s, &ClientMsg::Kill { req: 5, session: id });
    assert!(wait_exited(&mut s, id).is_some());
    assert!(!PathBuf::from(format!("{}.restore", sock.display())).exists(), "the saved state is gone");
}

#[test]
fn a_program_that_cant_take_over_is_refused() {
    let dir = tempfile::Builder::new().prefix("goatty-test-").tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let _daemon = Daemon(goattyd(Path::new(env!("CARGO_BIN_EXE_goattyd")), dir.path()).arg("serve").spawn().unwrap());
    wait_for_socket(&sock);
    let mut s = connect(&sock);
    send(&mut s, &ClientMsg::Create { req: 1, size: SIZE, cwd: None, opts: StreamOpts::LOCAL });
    let id = match read_msg::<_, ServerMsg>(&mut s).unwrap() {
        ServerMsg::Attached { session, .. } => session,
        other => panic!("{other:?}"),
    };
    let err = Control::connect(&sock, "test").unwrap().restart(Some(&true_exe())).unwrap_err();
    assert!(format!("{err:#}").contains("can't take over"), "{err:#}");
    send(&mut s, &ClientMsg::Input { session: id, data: b"echo still-$((2+3))\r".to_vec() });
    read_until(&mut s, "still-5");
}

#[test]
fn ptys_from_the_systemd_fd_store_come_back() {
    let dir = tempfile::Builder::new().prefix("goatty-test-").tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    // A shell from before the crash, on a PTY systemd kept.
    let mut cmd = ShellCommand::new("/bin/sh".into(), vec!["-c".into(), "while read l; do echo got:$l; done".into()], Vec::new());
    cmd.cwd = None;
    let (pty, _reader) = Pty::spawn(&cmd, TermSize::new(70, 20)).unwrap();
    let shell = pty.pid().unwrap();
    let master = pty.master_fd().try_clone_to_owned().unwrap();
    let listener = UnixListener::bind(&sock).unwrap();
    // What goattyd saved on SIGTERM.
    let state = dir.path().join("d.sock.restore");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("s7.vt"), "\x1bcSAVED-SCREEN").unwrap();
    std::fs::write(
        state.join("state.json"),
        format!(
            r#"{{"version":1,"next_session":20,"listener_fd":null,"layouts":[],
                "sessions":[{{"id":7,"fd":null,"pid":{shell},"created_unix":1700000000,"managed":false,"snapshot":"s7.vt"}}]}}"#
        ),
    )
    .unwrap();

    // As systemd starts it: the socket at fd 3, the PTY at fd 4, and
    // LISTEN_PID set to the daemon's pid (the shell `exec`s it).
    let (lfd, mfd) = (listener.as_raw_fd(), master.as_raw_fd());
    let mut c = goattyd(Path::new("/bin/sh"), dir.path());
    c.args(["-c", "LISTEN_PID=$$ exec \"$0\" serve", env!("CARGO_BIN_EXE_goattyd")])
        .env("LISTEN_FDS", "2")
        .env("LISTEN_FDNAMES", "goattyd.socket:goatty-s7-1700000000-p");
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and exec.
    unsafe {
        c.pre_exec(move || {
            let (l, m) = (libc::fcntl(lfd, libc::F_DUPFD, 100), libc::fcntl(mfd, libc::F_DUPFD, 100));
            if l < 0 || m < 0 || libc::dup2(l, 3) < 0 || libc::dup2(m, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _daemon = Daemon(c.spawn().unwrap());
    drop(master);
    drop(listener);

    let mut s = connect(&sock);
    send(&mut s, &ClientMsg::ListSessions { req: 1 });
    match read_msg::<_, ServerMsg>(&mut s).unwrap() {
        ServerMsg::Sessions { list, .. } => {
            assert_eq!(list.iter().map(|s| (s.id, s.pid, s.cols, s.rows)).collect::<Vec<_>>(), [(7, Some(shell), 70, 20)]);
            assert_eq!(list[0].created_unix, 1_700_000_000);
        }
        other => panic!("{other:?}"),
    }
    send(&mut s, &ClientMsg::Attach { req: 2, session: 7, size: SIZE, opts: StreamOpts::LOCAL });
    read_until(&mut s, "SAVED-SCREEN");
    send(&mut s, &ClientMsg::Input { session: 7, data: b"hi\r".to_vec() });
    read_until(&mut s, "got:hi");
    // Ids go on after the saved ones.
    send(&mut s, &ClientMsg::Create { req: 3, size: SIZE, cwd: None, opts: StreamOpts::LOCAL });
    loop {
        if let ServerMsg::Attached { req: 3, session, .. } = read_msg::<_, ServerMsg>(&mut s).unwrap() {
            assert_eq!(session, 21);
            break;
        }
    }
    // Not the daemon's child: it ends without an exit code.
    send(&mut s, &ClientMsg::Kill { req: 4, session: 7 });
    assert_eq!(wait_exited(&mut s, 7), None);
    assert!(!state.exists(), "the saved state is gone");
    drop(pty);
}

/// `true` from PATH (`/bin/true` on Linux, `/usr/bin/true` on macOS).
fn true_exe() -> std::path::PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap()).map(|d| d.join("true")).find(|p| p.is_file()).expect("true on PATH")
}
