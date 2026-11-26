//! End-to-end CLI tests for `goatty open <URL>` and bare `goatty goatty://…`
//! using a sandboxed `XDG_RUNTIME_DIR` (never touches the user's live session).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use goatty_client::{LocalListener, LocalStream};

struct MockApp {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl MockApp {
    fn start(runtime_dir: &Path, pid: u32) -> Self {
        std::fs::create_dir_all(runtime_dir).unwrap();
        let path = runtime_dir.join(format!("app-{pid}.sock"));
        let listener = LocalListener::bind(&path).unwrap();
        #[cfg(windows)]
        {
            let _ = std::fs::File::create(&path);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_loop = stop.clone();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_loop.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut s) = stream else { break };
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
                let mut line = String::new();
                if BufReader::new(&mut s).read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                let url = v["open"].as_str().unwrap().to_string();
                let _ = tx.send(url);
                let _ = s.write_all(b"{\"ok\":true}\n");
                let _ = s.flush();
            }
        });
        Self { path, stop, rx, thread: Some(thread) }
    }

    fn recv(&self) -> String {
        self.rx.recv_timeout(Duration::from_secs(5)).expect("mock app should receive URL")
    }
}

impl Drop for MockApp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = LocalStream::connect(&self.path);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn goatty(xdg_runtime: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_goatty"));
    cmd.env("XDG_RUNTIME_DIR", xdg_runtime);
    cmd
}

#[test]
fn open_subcommand_and_bare_url_deliver_to_active_and_clean_stale() {
    let xdg = tempfile::tempdir().unwrap();
    let dir = xdg.path().join("goatty");
    let app1 = MockApp::start(&dir, 1001);
    let app2 = MockApp::start(&dir, 1002);

    // Most recently focused process (1002) receives `goatty open <URL>`.
    std::fs::write(dir.join("app-active"), "1002\n").unwrap();
    let out = goatty(xdg.path()).args(["open", "goatty://settings/terminal?setting=links.open_on"]).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(app2.recv(), "goatty://settings/terminal?setting=links.open_on");
    assert!(app1.rx.try_recv().is_err());

    // Switching active process (1001) routes bare `goatty goatty://…` there.
    std::fs::write(dir.join("app-active"), "1001\n").unwrap();
    let out = goatty(xdg.path()).arg("goatty://plugins/eza?version=1.0.0").output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(app1.recv(), "goatty://plugins/eza?version=1.0.0");
    assert!(app2.rx.try_recv().is_err());

    // Point `app-active` at a stale socket: cleaned up and delivered to a live app.
    let stale = dir.join("app-5555.sock");
    #[cfg(unix)]
    {
        let dead = std::os::unix::net::UnixListener::bind(&stale).unwrap();
        drop(dead);
    }
    #[cfg(windows)]
    {
        std::fs::File::create(&stale).unwrap();
    }
    std::fs::write(dir.join("app-active"), "5555\n").unwrap();

    let out = goatty(xdg.path()).args(["open", "goatty://plugins"]).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!stale.exists(), "stale socket should have been cleaned up");
    let received = app1.rx.try_recv().or_else(|_| app2.rx.try_recv()).expect("a live app should receive the URL");
    assert_eq!(received, "goatty://plugins");
}

#[test]
fn invalid_urls_exit_nonzero_without_launching_gui() {
    let xdg = tempfile::tempdir().unwrap();
    for bad in ["http://example.com", "GOATTY://settings", "goatty://", "goatty://settings/.."] {
        let out = goatty(xdg.path()).args(["open", bad]).output().unwrap();
        assert!(!out.status.success(), "expected failure for `goatty open {bad}`");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.trim().is_empty(), "expected error message on stderr for {bad}");

        let out = goatty(xdg.path()).arg(bad).output().unwrap();
        assert!(!out.status.success(), "expected failure for bare `goatty {bad}`");
    }
}
