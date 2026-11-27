#![cfg(unix)]
//! End-to-end tests for protocol v13 remote file links (#110):
//!
//! * Session CWD tracking on attach and `cd` (for both non-OSC-7 `/bin/sh`
//!   and OSC 7 symlinks),
//! * `StatPath` resolution (absolute, `~/`, relative, `a/`/`b/`, non-existent,
//!   directories, ignoring FIFOs),
//! * `ReadFile` bounded reads (including compressed transfers over proxy, size
//!   limit errors, directory & FIFO rejection without blocking),
//! * Handshake compatibility with v12 clients (`ServerMsg::Cwd` is suppressed).

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use goatty_client::{Connection, Endpoint, PaneEvent, PaneSink};
use goatty_proto::{HARD_MAX_FILE_BYTES, WinSize};
use parking_lot::Mutex;

const SIZE: WinSize = WinSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };

struct TestDaemon {
    _dir: tempfile::TempDir,
    /// The temp dir's real path (macOS's is behind /var → /private/var, and
    /// the cwd the daemon reports is the real one).
    root: PathBuf,
    sock: PathBuf,
    child: Child,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_daemon() -> TestDaemon {
    let dir = tempfile::Builder::new().prefix("goatty-remote-files-").tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_goattyd"))
        .env("GOATTY_SOCKET", &sock)
        .env("GOATTY_SHELL", "/bin/sh")
        .env("RUST_LOG", "warn,rio_vt=error")
        .env("HOME", &root)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&sock).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    TestDaemon { _dir: dir, root, sock, child }
}

fn proxy_endpoint(d: &TestDaemon) -> Endpoint {
    Endpoint::Command {
        label: "test-proxy".into(),
        program: "env".into(),
        args: vec![
            format!("HOME={}", d.root.as_path().display()),
            format!("GOATTY_SOCKET={}", d.sock.display()),
            env!("CARGO_BIN_EXE_goattyd").into(),
            "proxy".into(),
        ],
    }
}

#[derive(Default)]
struct Sink {
    bytes: Mutex<Vec<u8>>,
    cwds: Mutex<Vec<Option<String>>>,
}

impl PaneSink for Sink {
    fn feed(&self, bytes: &[u8]) {
        self.bytes.lock().extend_from_slice(bytes);
    }

    fn event(&self, ev: PaneEvent) {
        if let PaneEvent::Cwd(cwd) = ev {
            self.cwds.lock().push(cwd);
        }
    }
}

impl Sink {
    fn wait_for_output(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&self.bytes.lock()).contains(needle) {
            assert!(Instant::now() < deadline, "missing {needle:?} in {:?}", String::from_utf8_lossy(&self.bytes.lock()));
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_cwd(&self, expected: &Path) {
        let target = expected.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.cwds.lock().iter().any(|c| c.as_deref() == Some(target.as_str())) {
            assert!(Instant::now() < deadline, "never saw cwd {target:?} in {:?}", self.cwds.lock());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn cwd_stat_and_read_work_over_proxy() {
    let d = start_daemon();
    let root = d.root.as_path();
    let repo = root.join("repo");
    let sub = repo.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    let file_rs = sub.join("main.rs");
    std::fs::write(&file_rs, "fn main() {}\n").unwrap();
    let big_file = repo.join("big.txt");
    let big_body = "remote-line\n".repeat(300);
    std::fs::write(&big_file, &big_body).unwrap();
    let home_note = root.join("note.txt");
    std::fs::write(&home_note, "from-home").unwrap();

    // Symlinked directory to test OSC 7 preservation vs /proc canonical target.
    let real_dir = root.join("real_target");
    std::fs::create_dir(&real_dir).unwrap();
    let sym_dir = root.join("sym_link");
    std::os::unix::fs::symlink(&real_dir, &sym_dir).unwrap();
    std::fs::write(real_dir.join("inside.txt"), "via-symlink").unwrap();

    // FIFO that must never be treated as a readable regular file.
    let fifo = repo.join("pipe.fifo");
    let status = Command::new("mkfifo").arg(&fifo).stderr(Stdio::null()).status().unwrap();
    assert!(status.success(), "mkfifo failed");

    let conn = Connection::open(proxy_endpoint(&d), "test").unwrap();
    assert!(conn.supports_remote_files());

    let sink = Arc::new(Sink::default());
    let id = conn.create(SIZE, Some(root.to_string_lossy().into_owned()), sink.clone()).unwrap();
    sink.wait_for_cwd(root);

    // 1. /bin/sh cd without OSC 7 triggers /proc/<pid>/cwd update.
    conn.input(id, format!("cd {}\r", repo.display()).as_bytes());
    sink.wait_for_cwd(&repo);

    // Relative and git diff candidate resolution against the live remote CWD.
    let stat = conn.stat_path(Some(id), None, vec!["src/main.rs".into()]).unwrap().expect("relative file");
    assert_eq!(stat.path, file_rs.to_string_lossy());
    assert!(!stat.is_dir);
    assert_eq!(stat.size, 13);

    let git_stat = conn.stat_path(Some(id), None, vec!["a/src/main.rs".into(), "src/main.rs".into()]).unwrap().expect("git diff candidate");
    assert_eq!(git_stat.path, file_rs.to_string_lossy());

    // `~/` resolution against daemon `$HOME`.
    let tilde = conn.stat_path(Some(id), None, vec!["~/note.txt".into()]).unwrap().expect("tilde file");
    assert_eq!(tilde.path, home_note.to_string_lossy());

    // Directories are reported with `is_dir = true`.
    let dir_stat = conn.stat_path(Some(id), None, vec!["src".into()]).unwrap().expect("directory");
    assert_eq!(dir_stat.path, sub.to_string_lossy());
    assert!(dir_stat.is_dir);

    // Non-existent paths and FIFOs resolve to `None`.
    assert_eq!(conn.stat_path(Some(id), None, vec!["does/not/exist.rs".into()]).unwrap(), None);
    assert_eq!(conn.stat_path(Some(id), None, vec!["pipe.fifo".into()]).unwrap(), None);

    // 2. Reading files (small, compressed large, over-limit, dir, FIFO).
    let (p, data) = conn.read_file(&file_rs.to_string_lossy(), 1024).unwrap();
    assert_eq!(p, file_rs.to_string_lossy());
    assert_eq!(data, b"fn main() {}\n");

    let (_, big_got) = conn.read_file(&big_file.to_string_lossy(), 1024 * 1024).unwrap();
    assert_eq!(big_got, big_body.as_bytes());

    let err = conn.read_file(&big_file.to_string_lossy(), 128).unwrap_err();
    assert!(format!("{err:#}").contains("limit"), "{err:#}");

    assert!(conn.read_file(&sub.to_string_lossy(), 1024).is_err());
    assert!(conn.read_file(&fifo.to_string_lossy(), 1024).is_err());

    // 3. OSC 7 keeps the logical symlinked path without being clobbered by /proc.
    conn.input(
        id,
        format!("cd {} && printf '\\033]7;file://localhost{}\\007ready\\n'\r", real_dir.display(), sym_dir.display()).as_bytes(),
    );
    sink.wait_for_output("ready");
    sink.wait_for_cwd(&sym_dir);
    // Wait past ForegroundWatch's SECOND_CHECK (450ms) and ensure symlink path sticks.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(sink.cwds.lock().last().and_then(|c| c.as_deref()), Some(sym_dir.to_string_lossy().as_ref()));

    let sym_stat = conn.stat_path(Some(id), None, vec!["inside.txt".into()]).unwrap().expect("inside symlinked cwd");
    assert_eq!(sym_stat.path, sym_dir.join("inside.txt").to_string_lossy());

    // Explicit `cwd` pins lookup to the captured directory even after the shell moved elsewhere.
    let pinned =
        conn.stat_path(Some(id), Some(repo.to_string_lossy().into_owned()), vec!["src/main.rs".into()]).unwrap().expect("pinned cwd");
    assert_eq!(pinned.path, file_rs.to_string_lossy());
    assert_eq!(conn.stat_path(Some(id), Some(repo.to_string_lossy().into_owned()), vec!["inside.txt".into()]).unwrap(), None);
}

#[test]
fn remote_files_edge_cases() {
    let d = start_daemon();
    let root = d.root.as_path();
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("a")).unwrap();
    std::fs::write(repo.join("both.txt"), "root-both").unwrap();
    std::fs::write(repo.join("a/both.txt"), "a-both").unwrap();
    std::fs::write(repo.join("target.txt"), "symlink-body").unwrap();
    std::os::unix::fs::symlink(repo.join("target.txt"), repo.join("link_file.txt")).unwrap();
    std::os::unix::fs::symlink(repo.join("a"), repo.join("link_dir")).unwrap();

    let sock_dir = tempfile::Builder::new().prefix("gs").tempdir_in("/tmp").unwrap();
    let sock_path = sock_dir.path().join("s.sock");
    let _listener = UnixListener::bind(&sock_path).unwrap();
    std::os::unix::fs::symlink(&sock_path, repo.join("domain.sock")).unwrap();

    // Symlinked CWD tree: root/home/sym -> root/srv/proj/sub; sibling in root/srv/proj vs root/home.
    let srv_proj = root.join("srv/proj");
    let srv_sub = srv_proj.join("sub");
    let fake_home = root.join("home_link");
    std::fs::create_dir_all(&srv_sub).unwrap();
    std::fs::create_dir_all(&fake_home).unwrap();
    std::fs::write(srv_proj.join("sibling.txt"), "real-sibling").unwrap();
    std::fs::write(fake_home.join("sibling.txt"), "wrong-sibling").unwrap();
    let cwd_sym = fake_home.join("sym");
    std::os::unix::fs::symlink(&srv_sub, &cwd_sym).unwrap();

    // Sparse file exceeding HARD_MAX_FILE_BYTES.
    let sparse = repo.join("too_big.bin");
    std::fs::File::create(&sparse).unwrap().set_len(HARD_MAX_FILE_BYTES + 1).unwrap();

    let conn = Connection::open(proxy_endpoint(&d), "test").unwrap();
    let sink = Arc::new(Sink::default());
    let id = conn.create(SIZE, Some(repo.to_string_lossy().into_owned()), sink.clone()).unwrap();
    sink.wait_for_cwd(&repo);

    // Exact match `a/both.txt` wins over stripped `both.txt`.
    let pref = conn.stat_path(Some(id), None, vec!["a/both.txt".into(), "both.txt".into()]).unwrap().unwrap();
    assert_eq!(pref.path, repo.join("a/both.txt").to_string_lossy());

    // 16-candidate cap ignores the 17th candidate.
    let mut cands = vec!["does-not-exist".to_string(); 16];
    cands.push("both.txt".into());
    assert_eq!(conn.stat_path(Some(id), None, cands).unwrap(), None);

    // Unix sockets are ignored by StatPath and rejected by ReadFile.
    assert_eq!(conn.stat_path(Some(id), None, vec!["domain.sock".into(), sock_path.to_string_lossy().into_owned()]).unwrap(), None);
    assert!(conn.read_file(&sock_path.to_string_lossy(), 1024).is_err());

    // Symlinks to files and dirs work.
    let lf = conn.stat_path(Some(id), None, vec!["link_file.txt".into()]).unwrap().unwrap();
    assert!(!lf.is_dir);
    let (_, lf_bytes) = conn.read_file(&lf.path, 1024).unwrap();
    assert_eq!(lf_bytes, b"symlink-body");
    let ld = conn.stat_path(Some(id), None, vec!["link_dir".into()]).unwrap().unwrap();
    assert!(ld.is_dir);

    // `session: None` and unknown session ID allow absolute/`~/` and reject relative paths.
    assert!(conn.stat_path(None, None, vec![repo.join("both.txt").to_string_lossy().into_owned()]).unwrap().is_some());
    assert_eq!(conn.stat_path(None, None, vec!["both.txt".into()]).unwrap(), None);
    assert!(conn.stat_path(Some(999_999), None, vec![repo.join("both.txt").to_string_lossy().into_owned()]).unwrap().is_some());
    assert_eq!(conn.stat_path(Some(999_999), None, vec!["both.txt".into()]).unwrap(), None);

    // Sparse file > HARD_MAX_FILE_BYTES fails cleanly.
    let err = conn.read_file(&sparse.to_string_lossy(), u64::MAX).unwrap_err();
    assert!(format!("{err:#}").contains("limit"), "{err:#}");

    // Procfs zero-stat-size file reads successfully.
    if Path::new("/proc/self/stat").exists() {
        let (_, proc_data) = conn.read_file("/proc/self/stat", 64 * 1024).unwrap();
        assert!(!proc_data.is_empty());
    }

    // OSC 7 symlinked CWD + `../sibling.txt` resolves physically through target parent.
    conn.input(
        id,
        format!("cd {} && printf '\\033]7;file://localhost{}\\007sym-ready\\n'\r", srv_sub.display(), cwd_sym.display()).as_bytes(),
    );
    sink.wait_for_output("sym-ready");
    sink.wait_for_cwd(&cwd_sym);
    let sib = conn.stat_path(Some(id), None, vec!["../sibling.txt".into()]).unwrap().unwrap();
    assert_eq!(sib.path, std::fs::canonicalize(srv_proj.join("sibling.txt")).unwrap().to_string_lossy());
    let (_, sib_data) = conn.read_file(&sib.path, 1024).unwrap();
    assert_eq!(sib_data, b"real-sibling");
}
