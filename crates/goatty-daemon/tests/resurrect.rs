#![cfg(target_os = "linux")]
//! Tabs come back when their shells ended with goattyd (#344): a goattyd
//! killed outright (as by a reboot or a crash) takes its shells with it; the
//! next one opens a shell for each session, with the same id, in the same
//! folder, starts a listed program again there, and keeps the workspaces.
//! `goattyd stop` keeps them too, and a coding agent's session hook
//! (`goattyd agent-hook`) names the conversation the tab resumes.
//!
//! Sandboxed: the daemon has its own `GOATTY_SOCKET` and `HOME`. Linux
//! only: the program in the foreground is read from `/proc`.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use goatty_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg, SessionInfo, StreamOpts, WinSize, encode, read_msg};

const SIZE: WinSize = WinSize { cols: 70, rows: 14, cell_width: 8, cell_height: 16 };

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn goattyd(dir: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_goattyd"))
        .arg("serve")
        .env("GOATTY_SOCKET", dir.join("d.sock"))
        .env("GOATTY_SHELL", "/bin/sh")
        .env("RUST_LOG", "warn,rio_vt=error")
        .env("HOME", dir)
        // The fake agents in `bin/`, and the goattyd their hooks run.
        .env("PATH", format!("{}:/usr/bin:/bin", dir.join("bin").display()))
        .env("FAKE_GOATTYD", env!("CARGO_BIN_EXE_goattyd"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("LISTEN_PID")
        .env_remove("LISTEN_FDS")
        .env_remove("LISTEN_FDNAMES")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(dir.join("d.sock")).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon(child)
}

fn send(s: &mut UnixStream, msg: &ClientMsg) {
    s.write_all(&encode(msg).unwrap()).unwrap();
}

fn connect(dir: &Path) -> UnixStream {
    let mut s = UnixStream::connect(dir.join("d.sock")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut s, &ClientMsg::Hello { version: PROTOCOL_VERSION, client: "test".into() });
    assert!(matches!(read_msg::<_, ServerMsg>(&mut s).unwrap(), ServerMsg::Hello { .. }));
    s
}

fn create(s: &mut UnixStream, req: u32, cwd: &Path) -> u64 {
    send(s, &ClientMsg::Create { req, size: SIZE, cwd: Some(cwd.to_string_lossy().into()), opts: StreamOpts::LOCAL });
    loop {
        match read_msg::<_, ServerMsg>(s).unwrap() {
            ServerMsg::Attached { session, .. } => return session,
            ServerMsg::Error { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
}

fn list(dir: &Path) -> Vec<SessionInfo> {
    let mut s = connect(dir);
    send(&mut s, &ClientMsg::ListSessions { req: 1 });
    loop {
        if let ServerMsg::Sessions { list, .. } = read_msg::<_, ServerMsg>(&mut s).unwrap() {
            return list;
        }
    }
}

/// Poll until `ok` holds, for up to `secs`.
fn wait_for(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn record(dir: &Path) -> Option<String> {
    let records = dir.join(".local/state/goatty/sessions");
    let file = std::fs::read_dir(records).ok()?.flatten().find(|e| e.path().extension().is_some_and(|x| x == "json"))?;
    std::fs::read_to_string(file.path()).ok()
}

fn canonical(p: &Path) -> String {
    p.canonicalize().unwrap().to_string_lossy().into_owned()
}

#[test]
fn tabs_come_back_after_goattyd_died_with_their_shells() {
    let dir = tempfile::Builder::new().prefix("goatty-test-").tempdir().unwrap();
    let home = dir.path();
    // `sleep` stands for lazygit: a program the user listed.
    std::fs::create_dir_all(home.join(".config/goatty")).unwrap();
    std::fs::write(home.join(".config/goatty/host.json"), r#"{"restore": {"programs": ["sleep"]}}"#).unwrap();
    let (work, other) = (home.join("work"), home.join("other dir"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&other).unwrap();

    let mut daemon = goattyd(home);
    let mut s = connect(home);
    let busy = create(&mut s, 1, &work);
    let idle = create(&mut s, 2, &other);
    send(&mut s, &ClientMsg::Input { session: busy, data: b"sleep 600\r".to_vec() });
    send(&mut s, &ClientMsg::JoinWorkspace { req: 3, view: 1, id: None });
    let main = loop {
        if let ServerMsg::Joined { info, .. } = read_msg::<_, ServerMsg>(&mut s).unwrap() {
            break info.id;
        }
    };
    let layout = format!(r#"{{"tabs":[{busy},{idle}]}}"#);
    send(
        &mut s,
        &ClientMsg::SaveWorkspace { req: 4, view: 1, id: main, base: 0, tabs: 2, sessions: vec![busy, idle], json: layout.clone() },
    );
    // Saved before the daemon goes.
    loop {
        if let ServerMsg::WorkspaceSaved { .. } = read_msg::<_, ServerMsg>(&mut s).unwrap() {
            break;
        }
    }
    wait_for(20, "the record to name sleep", || {
        record(home).is_some_and(|r| r.contains(r#""argv":["sleep","600"]"#) && r.contains("tabs"))
    });

    // A crash or a reboot: goattyd and its shells are gone at once.
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    drop(s);

    let _daemon = goattyd(home);
    let sessions = list(home);
    let ids: Vec<u64> = sessions.iter().map(|s| s.id).collect();
    assert_eq!(ids, [busy, idle], "the same sessions, by id");
    let cwd = |id| list(home).into_iter().find(|s| s.id == id).and_then(|s| s.cwd);
    assert_eq!(cwd(busy).as_deref(), Some(canonical(&work).as_str()));
    assert_eq!(cwd(idle).as_deref(), Some(canonical(&other).as_str()));
    wait_for(15, "sleep to run again", || list(home).iter().any(|s| s.id == busy && s.process.as_deref() == Some("sleep")));
    assert_ne!(list(home).iter().find(|s| s.id == idle).and_then(|s| s.process.clone()).as_deref(), Some("sleep"));

    // The workspace still shows them.
    let mut s = connect(home);
    send(&mut s, &ClientMsg::JoinWorkspace { req: 1, view: 1, id: Some(main) });
    loop {
        match read_msg::<_, ServerMsg>(&mut s).unwrap() {
            ServerMsg::Joined { info, layout: saved, .. } => {
                assert_eq!(info.id, main);
                assert_eq!(saved.json, layout);
                break;
            }
            ServerMsg::Error { message, .. } => panic!("{message}"),
            _ => {}
        }
    }

    // A new tab doesn't take an old one's id.
    let fresh = create(&mut s, 2, &work);
    assert!(fresh > idle, "{fresh} after {idle}");
}

#[test]
fn tabs_come_back_after_goattyd_stop() {
    let dir = tempfile::Builder::new().prefix("goatty-test-").tempdir().unwrap();
    let home = dir.path();
    let mut daemon = goattyd(home);
    let mut s = connect(home);
    let id = create(&mut s, 1, home);
    wait_for(20, "the record", || record(home).is_some());
    send(&mut s, &ClientMsg::Shutdown { req: 2 });
    let _ = daemon.0.wait();
    assert!(record(home).is_some(), "stop keeps the record");
    let _daemon = goattyd(home);
    assert_eq!(list(home).iter().map(|s| s.id).collect::<Vec<_>>(), [id]);
}

/// A stand-in for Claude Code: notes how it was started, runs its
/// `SessionStart` hook as Claude Code would (the session as JSON on
/// stdin), then waits. Its process is called `claude`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo "args: $*" >> "$HOME/claude-started"
printf '{"session_id":"sess-42","hook_event_name":"SessionStart","source":"startup"}' | "$FAKE_GOATTYD" agent-hook claude
sleep 600
"#;

#[test]
fn an_agents_hook_names_the_conversation_to_resume() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::Builder::new().prefix("goatty-test-").tempdir().unwrap();
    let home = dir.path();
    std::fs::create_dir_all(home.join("bin")).unwrap();
    let claude = home.join("bin/claude");
    std::fs::write(&claude, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let work = home.join("project");
    std::fs::create_dir_all(&work).unwrap();

    let mut daemon = goattyd(home);
    let mut s = connect(home);
    let id = create(&mut s, 1, &work);
    send(&mut s, &ClientMsg::Input { session: id, data: b"claude\r".to_vec() });
    wait_for(20, "the record to name the hook's session", || {
        record(home).is_some_and(|r| r.contains(r#""argv":["claude","--resume","sess-42"]"#))
    });
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    drop(s);

    let _daemon = goattyd(home);
    wait_for(15, "claude to resume the conversation", || {
        std::fs::read_to_string(home.join("claude-started")).is_ok_and(|t| t.contains("args: --resume sess-42"))
    });
    let started = std::fs::read_to_string(home.join("claude-started")).unwrap();
    assert_eq!(started.lines().collect::<Vec<_>>(), ["args: ", "args: --resume sess-42"], "{started}");
}

#[test]
fn nothing_comes_back_when_it_is_switched_off() {
    let dir = tempfile::Builder::new().prefix("goatty-test-").tempdir().unwrap();
    let home = dir.path();
    std::fs::create_dir_all(home.join(".config/goatty")).unwrap();
    std::fs::write(home.join(".config/goatty/host.json"), r#"{"restore": {"sessions": false}}"#).unwrap();
    let mut daemon = goattyd(home);
    let mut s = connect(home);
    create(&mut s, 1, home);
    wait_for(20, "the record", || record(home).is_some());
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    let _daemon = goattyd(home);
    assert!(list(home).is_empty());
}
