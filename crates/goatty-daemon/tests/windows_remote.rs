#![cfg(windows)]
//! A Windows host reached over SSH (#280), without a real sshd: a fake
//! `ssh.cmd` runs the remote command with cmd.exe in a sandboxed profile
//! directory, as Win32-OpenSSH does with its default shell. Covers what
//! goatty runs on such a host exactly:
//!
//! * the POSIX connect script fails there, goatty finds out it's Windows
//!   (the PowerShell probe) and reports `NotInstalled` with its platform,
//! * `remote::install` uploads goattyd.exe through PowerShell into
//!   `AppData\Local\goatty\bin`, and the connect command runs it,
//! * a fresh target finds the installed goattyd on its own,
//! * `remote::stop_daemon` stops the daemon again.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use goatty_client::remote::{self, RemoteProblem};
use goatty_client::{Connection, Endpoint, LocalStream, PaneEvent, PaneSink, SshTarget};
use goatty_proto::WinSize;
use parking_lot::Mutex;

const SIZE: WinSize = WinSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };

struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("goatty-winhost-").tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let sock = dir.path().join("d.sock");
        // Skips ssh's options up to `--`, then the host; runs the remote
        // command from the profile directory with only Windows on PATH (no
        // Git Bash `sh`), like sshd with cmd.exe as the default shell.
        let script = format!(
            "@echo off\r\n\
             setlocal\r\n\
             :skip\r\n\
             if \"%~1\"==\"\" exit /b 255\r\n\
             if \"%~1\"==\"--\" goto host\r\n\
             shift\r\n\
             goto skip\r\n\
             :host\r\n\
             shift\r\n\
             shift\r\n\
             set \"GOATTY_SOCKET={sock}\"\r\n\
             set \"GOATTY_SHELL=cmd.exe\"\r\n\
             set \"RUST_LOG=warn,rio_vt=error\"\r\n\
             set \"USERPROFILE={home}\"\r\n\
             set \"LOCALAPPDATA={home}\\AppData\\Local\"\r\n\
             set \"APPDATA={home}\\AppData\\Roaming\"\r\n\
             set \"PATH=%SystemRoot%\\System32;%SystemRoot%;%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\"\r\n\
             cd /d \"{home}\"\r\n\
             %~1\r\n\
             exit /b %errorlevel%\r\n",
            sock = sock.display(),
            home = home.display(),
        );
        std::fs::write(dir.path().join("ssh.cmd"), script).unwrap();
        Self { dir }
    }

    fn target(&self) -> SshTarget {
        SshTarget { program: self.dir.path().join("ssh.cmd").display().to_string(), ..SshTarget::new("winhost") }
    }

    fn installed(&self) -> PathBuf {
        self.dir.path().join("home").join(remote::WINDOWS_BIN_DIR).join(remote::windows_binary_name())
    }

    fn sock(&self) -> PathBuf {
        self.dir.path().join("d.sock")
    }
}

#[derive(Default)]
struct Sink(Mutex<Vec<u8>>);

impl Sink {
    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if String::from_utf8_lossy(&self.0.lock()).contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }
}

impl PaneSink for Sink {
    fn feed(&self, bytes: &[u8]) {
        self.0.lock().extend_from_slice(bytes);
    }
    fn event(&self, _ev: PaneEvent) {}
}

fn run_a_command(conn: &Connection, marker: &str) {
    let sink = Arc::new(Sink::default());
    let id = conn.create(SIZE, None, sink.clone()).unwrap();
    conn.input(id, format!("echo {marker}\r\n").as_bytes());
    assert!(sink.wait_for(marker, Duration::from_secs(15)), "output: {}", String::from_utf8_lossy(&sink.0.lock()));
}

fn gone(sock: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if LocalStream::connect(sock).is_err() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn windows_host_over_ssh_detect_install_connect_stop() {
    let host = Host::new();
    let target = host.target();

    // 1. Nothing installed: found out to be Windows, with its platform.
    let err = Connection::open(Endpoint::Ssh(target.clone()), "win-remote").err().expect("no goattyd there yet");
    let Some(RemoteProblem::NotInstalled { platform, .. }) = err.downcast_ref::<RemoteProblem>() else { panic!("{err:#}") };
    assert!(platform.is_windows(), "{platform}");
    assert!(platform.target().is_some_and(|t| t.ends_with("-windows")), "{platform}");
    assert!(target.is_windows());

    // 2. Install through PowerShell, then connect the Windows way.
    remote::install(&target, Path::new(env!("CARGO_BIN_EXE_goattyd"))).unwrap();
    assert!(host.installed().is_file(), "{} missing", host.installed().display());
    let conn = Connection::open(Endpoint::Ssh(target.clone()), "win-remote").unwrap();
    assert_eq!(conn.host().os, "windows");
    run_a_command(&conn, "WIN_REMOTE_OK_1");
    drop(conn);

    // 3. A target that doesn't know yet finds it by itself.
    let fresh = host.target();
    let conn = Connection::open(Endpoint::Ssh(fresh.clone()), "win-remote").unwrap();
    assert!(fresh.is_windows());
    run_a_command(&conn, "WIN_REMOTE_OK_2");
    drop(conn);

    // 4. Stop the daemon.
    remote::stop_daemon(&target).unwrap();
    assert!(gone(&host.sock()), "goattyd still serves {}", host.sock().display());
}
