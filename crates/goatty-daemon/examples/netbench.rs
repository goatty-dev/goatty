//! Headless end-to-end benchmark of the client ⇄ daemon path: keystroke echo
//! latency (alone, and while another pane on the same connection floods),
//! flood throughput, multi-pane window restore (sequential vs. pipelined
//! `attach_many`), workspace live sync (a save's round trip and how soon
//! another window has it, #215) and what a background workspace's
//! subscriptions cost the client, with the client feeding a real mirror VT
//! (same work the GUI does, minus rendering).
//!
//!   cargo run --release -p goatty-daemon --example netbench            # private local daemon
//!   cargo run --release -p goatty-daemon --example netbench -- proxy   # same, through `goattyd proxy` (remote code path)
//!   cargo run --release -p goatty-daemon --example netbench -- delay 50 # proxy with 50 ms simulated RTT
//!   cargo run --release -p goatty-daemon --example netbench -- slow 2m  # proxy throttled to 2 MB/s by `pv -L`
//!   cargo run --release -p goatty-daemon --example netbench -- ssh HOST
//!   cargo run --release -p goatty-daemon --example netbench -- tailnet HOST.TAILNET.ts.net   # a paired tailnet host
//!
//! Env:
//!   FLOOD_MB       (default 200; 0 skips the flood phase)
//!   RESTORE_PANES  (default 8; 0 skips the multi-pane restore phase)
//!   RESTORE_LINES  (default 0; lines of scrollback seeded per restored pane)
//!   CONTEND_PANES  (default 2; panes flooding during the second echo phase; 0 skips it)
//!   SYNC_TABS      (default 20; tabs in the workspace two windows edit; 0 skips the sync phase)
//!   BG_PANES       (default 8; a background workspace's panes printing a line every 100 ms; 0 skips it)

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use goatty_client::{AttachRequest, Connection, Endpoint, PaneEvent, PaneSink};
use goatty_proto::{SessionId, WinSize};
use goatty_term::{Palette, TermSize, Terminal};
use parking_lot::Mutex;

/// Kills the private daemon even if the benchmark panics.
struct Guard(std::process::Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Sink {
    term: Terminal,
    bytes: AtomicU64,
    resyncs: AtomicU64,
    skipped: AtomicU64,
    tail: Mutex<Vec<u8>>,
}

impl Sink {
    fn new(size: TermSize) -> Arc<Self> {
        let (term, _ch) = Terminal::new(size, false, Arc::new(Palette::default()));
        Arc::new(Self { term, bytes: 0.into(), resyncs: 0.into(), skipped: 0.into(), tail: Mutex::new(Vec::new()) })
    }

    fn wait_tail(&self, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.tail.lock().windows(needle.len()).any(|w| w == needle) {
                return true;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        false
    }
}

impl PaneSink for Sink {
    fn feed(&self, b: &[u8]) {
        self.term.feed(b);
        self.bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
        let mut t = self.tail.lock();
        t.extend_from_slice(&b[b.len().saturating_sub(4096)..]);
        let excess = t.len().saturating_sub(4096);
        t.drain(..excess);
    }
    fn event(&self, ev: PaneEvent) {
        if let PaneEvent::Resynced { skipped } = ev {
            self.resyncs.fetch_add(1, Ordering::Relaxed);
            self.skipped.fetch_add(skipped, Ordering::Relaxed);
        }
    }
}

/// Internal stdio relay used by `delay <RTT_MS>`: forwards raw bytes between
/// stdin/stdout and the daemon socket with a timestamped FIFO one-way
/// propagation delay in each direction, so pipelined bursts travel back-to-back
/// just like over a real WAN link.
fn run_delay_proxy(sock: &Path, one_way: Duration) -> anyhow::Result<()> {
    use std::os::fd::FromRawFd;

    struct RawFdFile(std::mem::ManuallyDrop<std::fs::File>);
    impl Read for RawFdFile {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Write for RawFdFile {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    let stream = UnixStream::connect(sock)?;
    let to_daemon = stream.try_clone()?;
    let from_daemon = stream;
    // SAFETY: fds 0/1 are owned by this process for its whole life and nothing
    // else in `__delay-proxy` uses them.
    let stdin = RawFdFile(std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) }));
    // SAFETY: as above.
    let stdout = RawFdFile(std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) }));

    fn delay_pipe(
        mut from: impl Read + Send + 'static,
        mut to: impl Write + Send + 'static,
        delay: Duration,
        shutdown: Option<UnixStream>,
    ) {
        let (tx, rx) = flume::unbounded::<(Instant, Vec<u8>)>();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 << 10];
            loop {
                match from.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send((Instant::now() + delay, buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        while let Ok((deliver_at, chunk)) = rx.recv() {
            if let Some(wait) = deliver_at.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            if to.write_all(&chunk).is_err() {
                break;
            }
        }
        if let Some(s) = shutdown {
            let _ = s.shutdown(std::net::Shutdown::Write);
        }
    }

    let up_shutdown = to_daemon.try_clone()?;
    let up = std::thread::spawn(move || delay_pipe(stdin, to_daemon, one_way, Some(up_shutdown)));
    delay_pipe(from_daemon, stdout, one_way, None);
    drop(up);
    Ok(())
}

struct RestoreMetrics {
    panes: usize,
    lines: usize,
    seq_total: Duration,
    seq_attach: Duration,
    pipe_total: Duration,
    pipe_attach: Duration,
}

fn wait_all_detached(observer: &Connection, ids: &[SessionId]) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = observer.list_sessions()?;
        let attached = ids.iter().any(|id| list.iter().any(|s| s.id == *id && s.attached > 0));
        if !attached {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for sessions to detach");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct RestoreCleanup<'a> {
    setup_endpoint: &'a Endpoint,
    conn: Option<Arc<Connection>>,
    ids: Vec<SessionId>,
    /// The workspace made for the benchmark.
    workspace: Option<goatty_proto::WorkspaceId>,
}

/// Workspace news nobody reads.
struct NoSink;

impl goatty_client::WorkspaceSink for NoSink {
    fn workspace(&self, _conn: usize, _event: goatty_client::WorkspaceEvent) {}
}

impl Drop for RestoreCleanup<'_> {
    fn drop(&mut self) {
        if self.ids.is_empty() && self.workspace.is_none() {
            return;
        }
        let conn = self
            .conn
            .take()
            .or_else(|| Connection::open_once(self.setup_endpoint.clone(), "netbench-cleanup", Some(Duration::from_secs(5))).ok());
        let Some(conn) = conn else { return };
        if let Some(id) = self.workspace {
            let _ = conn.close_workspace(id);
        }
        for &id in &self.ids {
            let _ = conn.kill(id);
        }
    }
}

fn bench_restore(
    endpoint: &Endpoint,
    setup_endpoint: &Endpoint,
    panes: usize,
    lines: usize,
    size: TermSize,
    ws: WinSize,
) -> anyhow::Result<RestoreMetrics> {
    // Declared first so LIFO drop order tears down `setup`/`c_seq`/`c_pipe`
    // before cleanup runs, on both normal return and every early `?` error
    // (important for real ssh/tailnet hosts).
    let mut cleanup = RestoreCleanup { setup_endpoint, conn: None, ids: Vec::with_capacity(panes), workspace: None };

    // Create and seed the panes on a setup connection (local Unix socket for
    // private daemons so setup itself doesn't pay simulated latency), save a
    // workspace, then drop the setup connection so its sessions are detached.
    let setup = Connection::open(setup_endpoint.clone(), "netbench-setup")?;
    let mut sinks = Vec::with_capacity(panes);
    for i in 0..panes {
        let sink = Sink::new(size);
        let id = setup.create(ws, None, sink.clone())?;
        cleanup.ids.push(id);
        if lines > 0 {
            let cmd = format!(
                "awk -v p={i} -v n={lines} 'BEGIN{{for(j=0;j<n;j++) printf \"pane %d line %04d: the quick brown fox jumps over the lazy dog 0123456789\\n\", p, j}}'; echo PANE-{i}-READY-$((1+1))\r"
            );
            setup.input(id, cmd.as_bytes());
        } else {
            setup.input(id, format!("echo PANE-{i}-READY-$((1+1))\r").as_bytes());
        }
        sinks.push(sink);
    }
    for (i, sink) in sinks.iter().enumerate() {
        let marker = format!("PANE-{i}-READY-2");
        anyhow::ensure!(sink.wait_tail(marker.as_bytes(), Duration::from_secs(30)), "pane {i} did not finish setup");
    }
    let (ws_info, _) = setup.create_workspace(1, Some("netbench".into()), Arc::new(NoSink))?;
    cleanup.workspace = Some(ws_info.id);
    let tabs = u32::try_from(panes).unwrap_or(u32::MAX);
    setup.save_workspace(1, ws_info.id, 0, tabs, cleanup.ids.clone(), r#"{"version":2,"tabs":[]}"#.into())?;
    drop(setup);

    let observer = Connection::open(setup_endpoint.clone(), "netbench-obs")?;
    cleanup.conn = Some(observer.clone());
    wait_all_detached(&observer, &cleanup.ids)?;

    // 1) Sequential restore: join the workspace + N sequential `attach` calls.
    let c_seq = Connection::open(endpoint.clone(), "netbench-seq")?;
    let t_seq = Instant::now();
    let (_, layout) = c_seq.join_workspace(2, Some(ws_info.id), Arc::new(NoSink))?;
    anyhow::ensure!(layout.sessions.len() == panes, "expected {panes} sessions in layout, got {}", layout.sessions.len());
    let t_seq_attach = Instant::now();
    for &id in &cleanup.ids {
        c_seq.attach(id, ws, Sink::new(size))?;
    }
    let seq_attach = t_seq_attach.elapsed();
    let seq_total = t_seq.elapsed();
    drop(c_seq);

    wait_all_detached(&observer, &cleanup.ids)?;

    // 2) Pipelined restore: join the workspace + one `attach_many` burst.
    let c_pipe = Connection::open(endpoint.clone(), "netbench-pipe")?;
    let t_pipe = Instant::now();
    let (_, layout) = c_pipe.join_workspace(3, Some(ws_info.id), Arc::new(NoSink))?;
    anyhow::ensure!(layout.sessions.len() == panes, "expected {panes} sessions in layout, got {}", layout.sessions.len());
    let requests: Vec<AttachRequest> =
        cleanup.ids.iter().map(|&session| AttachRequest { session, size: ws, sink: Sink::new(size) }).collect();
    let t_pipe_attach = Instant::now();
    let results = c_pipe.attach_many(requests);
    let pipe_attach = t_pipe_attach.elapsed();
    let pipe_total = t_pipe.elapsed();
    for (i, r) in results.into_iter().enumerate() {
        r.map_err(|e| anyhow::anyhow!("pipelined attach pane {i} failed: {e:#}"))?;
    }

    Ok(RestoreMetrics { panes, lines, seq_total, seq_attach, pipe_total, pipe_attach })
}

/// Kills the flooding panes even if the contended-echo phase fails: on ssh
/// and tailnet hosts they would otherwise keep running.
struct KillOnDrop<'a> {
    conn: &'a Connection,
    ids: Vec<SessionId>,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        for &id in &self.ids {
            let _ = self.conn.kill(id);
        }
    }
}

/// Keystroke echo in pane `id` while `flooders` other panes on the same
/// connection flood: the cross-pane head-of-line delay of the daemon's
/// writer queue.
fn bench_contended(
    conn: &Connection,
    id: SessionId,
    sink: &Sink,
    ws: WinSize,
    size: TermSize,
    flooders: usize,
) -> anyhow::Result<Vec<Duration>> {
    let mut flood = KillOnDrop { conn, ids: Vec::new() };
    for _ in 0..flooders {
        let flood_sink = Sink::new(size);
        let flooder = conn.create(ws, None, flood_sink.clone())?;
        flood.ids.push(flooder);
        conn.input(flooder, b"echo READY-$((2+2))\r");
        assert!(flood_sink.wait_tail(b"READY-4", Duration::from_secs(10)), "flooding shell did not start");
    }
    for &flooder in &flood.ids {
        conn.input(flooder, b"cat /dev/zero | tr '\\0' x\r");
    }
    // Let the flood fill the link, the windows and the queues.
    std::thread::sleep(Duration::from_secs(1));
    conn.input(id, b"stty raw -echo; dd bs=1 count=80 status=none; stty sane; echo\r");
    std::thread::sleep(Duration::from_millis(500));
    sink.tail.lock().clear();
    let mut lat = Vec::new();
    for i in 0..20u8 {
        let marker = format!("<{i:02}>").into_bytes();
        let t = Instant::now();
        conn.input(id, &marker);
        assert!(sink.wait_tail(&marker, Duration::from_secs(10)), "echo timeout during flood");
        lat.push(t.elapsed());
        sink.tail.lock().clear();
    }
    drop(flood);
    lat.sort();
    Ok(lat)
}

/// Workspace news for the sync phase: when each revision arrived.
#[derive(Default)]
struct Heard(Mutex<Vec<(u64, Instant)>>);

impl goatty_client::WorkspaceSink for Heard {
    fn workspace(&self, _conn: usize, event: goatty_client::WorkspaceEvent) {
        if let goatty_client::WorkspaceEvent::Changed(l) = event {
            self.0.lock().push((l.revision, Instant::now()));
        }
    }
}

struct SyncMetrics {
    tabs: usize,
    bytes: usize,
    save: Vec<Duration>,
    seen: Vec<Duration>,
}

/// Two windows (two connections) show one workspace; one saves a layout of
/// `tabs` tabs (a rename each time, as a person would), the other hears it.
fn bench_sync(endpoint: &Endpoint, tabs: usize) -> anyhow::Result<SyncMetrics> {
    let a = Connection::open(endpoint.clone(), "netbench-sync-a")?;
    let b = Connection::open(endpoint.clone(), "netbench-sync-b")?;
    let (heard_a, heard_b) = (Arc::new(Heard::default()), Arc::new(Heard::default()));
    let (ws, _) = a.create_workspace(1, Some("netbench-sync".into()), heard_a)?;
    b.join_workspace(2, Some(ws.id), heard_b.clone())?;
    let session = a.create(WinSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 }, None, Sink::new(TermSize::new(80, 24)))?;
    let layout = |round: usize| {
        let tabs: Vec<String> =
            (0..tabs).map(|t| format!(r#"{{"id":{},"name":"tab {t} round {round}","tree":{{"leaf":{session}}}}}"#, 1000 + t)).collect();
        format!(r#"{{"version":2,"tabs":[{}]}}"#, tabs.join(","))
    };
    let (mut save, mut seen) = (Vec::new(), Vec::new());
    let mut revision = 0;
    let bytes = layout(0).len();
    for round in 0..50 {
        let t = Instant::now();
        match a.save_workspace(1, ws.id, revision, u32::try_from(tabs).unwrap_or(u32::MAX), vec![session], layout(round))? {
            goatty_proto::SaveOutcome::Saved { revision: r } => revision = r,
            other => anyhow::bail!("save {round}: {other:?}"),
        }
        save.push(t.elapsed());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(&(_, at)) = heard_b.0.lock().iter().find(|(r, _)| *r == revision) {
                seen.push(at.saturating_duration_since(t));
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "the other window never heard revision {revision}");
            std::thread::sleep(Duration::from_micros(100));
        }
    }
    let _ = a.kill(session);
    let _ = a.close_workspace(ws.id);
    save.sort();
    seen.sort();
    Ok(SyncMetrics { tabs, bytes, save, seen })
}

/// CPU time this process has used so far.
fn cpu_time() -> Duration {
    // SAFETY: `usage` is plain data that getrusage fills in.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer to a rusage for the call.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, (t.tv_usec as u32) * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// A background workspace's `panes` stay subscribed (the GUI keeps feeding
/// their mirror terminals, without drawing): the client's CPU and the bytes
/// they bring while each prints a line every 100 ms, for 5 s.
fn bench_background(conn: &Connection, panes: usize) -> anyhow::Result<(f64, f64)> {
    let size = TermSize::new(200, 50);
    let ws = WinSize { cols: 200, rows: 50, cell_width: 8, cell_height: 16 };
    let mut busy = KillOnDrop { conn, ids: Vec::new() };
    let mut sinks = Vec::new();
    for _ in 0..panes {
        let sink = Sink::new(size);
        let id = conn.create(ws, None, sink.clone())?;
        busy.ids.push(id);
        conn.input(id, b"echo READY-$((3+3))\r");
        anyhow::ensure!(sink.wait_tail(b"READY-6", Duration::from_secs(10)), "background shell did not start");
        sinks.push(sink);
    }
    for &id in &busy.ids {
        conn.input(id, b"while :; do date '+%T.%N background pane log line with some text'; sleep 0.1; done\r");
    }
    std::thread::sleep(Duration::from_millis(500));
    let bytes0: u64 = sinks.iter().map(|s| s.bytes.load(Ordering::Relaxed)).sum();
    let (cpu0, t0) = (cpu_time(), Instant::now());
    std::thread::sleep(Duration::from_secs(5));
    let (cpu, wall) = (cpu_time() - cpu0, t0.elapsed());
    let bytes: u64 = sinks.iter().map(|s| s.bytes.load(Ordering::Relaxed)).sum::<u64>() - bytes0;
    drop(busy);
    Ok((cpu.as_secs_f64() / wall.as_secs_f64() * 100.0, bytes as f64 / wall.as_secs_f64() / 1e3))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("__delay-proxy") {
        let sock = Path::new(args.get(1).expect("sock path"));
        let one_way_us: u64 = args.get(2).expect("one-way delay us").parse()?;
        return run_delay_proxy(sock, Duration::from_micros(one_way_us));
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("error,rio_vt=error")).init();
    let flood_mb: u64 = std::env::var("FLOOD_MB").ok().and_then(|s| s.parse().ok()).unwrap_or(200);
    let restore_panes: usize = std::env::var("RESTORE_PANES").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    let contend_panes: usize = std::env::var("CONTEND_PANES").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    let restore_lines: usize = std::env::var("RESTORE_LINES").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let sync_tabs: usize = std::env::var("SYNC_TABS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    let bg_panes: usize = std::env::var("BG_PANES").ok().and_then(|s| s.parse().ok()).unwrap_or(8);

    // Removed when dropped at the end of main, after the daemon is killed.
    let dir = tempfile::Builder::new().prefix("goatty-netbench-").tempdir()?;
    let sock = dir.path().join("d.sock");
    let exe = std::env::current_exe()?;
    let goattyd = exe.parent().unwrap().parent().unwrap().join("goattyd");
    let mut daemon = None;
    let mut private_local = false;
    let endpoint = match args.first().map(String::as_str) {
        Some("ssh") => Endpoint::ssh(args.get(1).expect("ssh HOST"), None),
        Some("tailnet") => {
            let dns = args.get(1).expect("tailnet MAGICDNS-NAME (of a host this computer paired with)").clone();
            let dirs = goatty_shell::Dirs::from_env().expect("$HOME");
            Endpoint::Tailnet(goatty_client::TailnetTarget {
                name: dns.split('.').next().unwrap_or(&dns).to_string(),
                dns,
                paths: Some(goatty_net::Paths::new(dirs.tailnet_dir())),
                tailscale: goatty_net::tailscale::Tailscale::system(),
            })
        }
        mode => {
            daemon = Some(Guard(
                Command::new(&goattyd)
                    .arg("serve")
                    .env("GOATTY_SOCKET", &sock)
                    .env("GOATTY_SHELL", "/bin/sh")
                    .env("RUST_LOG", "error")
                    .stdout(Stdio::null())
                    .spawn()?,
            ));
            while UnixStream::connect(&sock).is_err() {
                std::thread::sleep(Duration::from_millis(10));
            }
            // SAFETY: single-threaded at this point.
            unsafe { std::env::set_var("GOATTY_SOCKET", &sock) };
            private_local = true;
            if mode == Some("slow") {
                let rate = args.get(1).map(String::as_str).unwrap_or("2m");
                let script = format!("GOATTY_SOCKET={} {} proxy | pv -q -L {rate}", sock.display(), goattyd.display());
                Endpoint::Command { label: format!("proxy throttled to {rate}B/s"), program: "sh".into(), args: vec!["-c".into(), script] }
            } else if mode == Some("delay") {
                let rtt_ms: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50);
                let one_way_us = (rtt_ms * 1000) / 2;
                Endpoint::Command {
                    label: format!("proxy with {rtt_ms} ms RTT"),
                    program: exe.to_string_lossy().into_owned(),
                    args: vec!["__delay-proxy".into(), sock.to_string_lossy().into_owned(), one_way_us.to_string()],
                }
            } else if mode == Some("proxy") {
                let args = vec![format!("GOATTY_SOCKET={}", sock.display()), goattyd.display().to_string(), "proxy".into()];
                Endpoint::Command { label: "local proxy".into(), program: "env".into(), args }
            } else {
                Endpoint::Local
            }
        }
    };

    let label = endpoint.label().to_string();
    let opts = endpoint.default_opts();
    let size = TermSize::new(200, 50);
    let ws = WinSize { cols: 200, rows: 50, cell_width: 8, cell_height: 16 };

    let setup_endpoint = if private_local { Endpoint::Local } else { endpoint.clone() };
    let restore =
        if restore_panes > 0 { Some(bench_restore(&endpoint, &setup_endpoint, restore_panes, restore_lines, size, ws)?) } else { None };

    let sync = if sync_tabs > 0 { Some(bench_sync(&endpoint, sync_tabs)?) } else { None };

    let t0 = Instant::now();
    let conn = Connection::open(endpoint, "netbench")?;
    let connect = t0.elapsed();
    let sink = Sink::new(size);
    let t1 = Instant::now();
    let id = conn.create(ws, None, sink.clone())?;
    let attach = t1.elapsed();

    // Wait for a prompt, then switch the tty to raw/no-echo `cat` so each
    // keystroke comes straight back: pure transport + PTY round trip.
    conn.input(id, b"echo READY-$((1+1))\r");
    assert!(sink.wait_tail(b"READY-2", Duration::from_secs(10)), "shell did not start");
    // 50 markers x 4 bytes: `dd` exits by itself after exactly 200 bytes
    // (raw mode has no ^C/^D to stop a `cat`; `head` would stdio-buffer).
    conn.input(id, b"stty raw -echo; dd bs=1 count=200 status=none; stty sane; echo\r");
    std::thread::sleep(Duration::from_millis(300));
    let mut lat = Vec::new();
    for i in 0..50u8 {
        let marker = format!("[{i:02}]").into_bytes();
        let t = Instant::now();
        conn.input(id, &marker);
        assert!(sink.wait_tail(&marker, Duration::from_secs(5)), "echo timeout");
        lat.push(t.elapsed());
        sink.tail.lock().clear();
    }
    std::thread::sleep(Duration::from_millis(300));
    lat.sort();

    let flood_result = if flood_mb > 0 {
        let before = sink.bytes.load(Ordering::Relaxed);
        let t2 = Instant::now();
        conn.input(id, format!("head -c {}000000 /dev/zero | tr '\\0' x; echo; echo FLOOD-$((40+2))\r", flood_mb).as_bytes());
        assert!(sink.wait_tail(b"FLOOD-42", Duration::from_secs(120)), "flood marker not seen");
        let flood = t2.elapsed();
        let received = sink.bytes.load(Ordering::Relaxed) - before;
        Some((flood, received))
    } else {
        None
    };

    let contended = if contend_panes > 0 { Some(bench_contended(&conn, id, &sink, ws, size, contend_panes)?) } else { None };

    let background = if bg_panes > 0 { Some(bench_background(&conn, bg_panes)?) } else { None };

    let active_window = conn.flow_window();
    if active_window != opts.window {
        println!("endpoint            {label} (window {} → {} KiB, history {})", opts.window >> 10, active_window >> 10, opts.history);
    } else {
        println!("endpoint            {label} (window {} KiB, history {})", opts.window >> 10, opts.history);
    }
    println!("connect             {:>8.2} ms", connect.as_secs_f64() * 1e3);
    println!("create+snapshot     {:>8.2} ms", attach.as_secs_f64() * 1e3);
    if let Some(r) = restore {
        let lines_tag = if r.lines > 0 { format!(", {} lines/pane", r.lines) } else { String::new() };
        let total_speedup = r.seq_total.as_secs_f64() / r.pipe_total.as_secs_f64().max(1e-9);
        let attach_speedup = r.seq_attach.as_secs_f64() / r.pipe_attach.as_secs_f64().max(1e-9);
        println!(
            "restore {} panes{}   {:>8.2} ms seq → {:>8.2} ms pipe ({:.2}× faster; attach {:.2} → {:.2} ms, {:.2}×)",
            r.panes,
            lines_tag,
            r.seq_total.as_secs_f64() * 1e3,
            r.pipe_total.as_secs_f64() * 1e3,
            total_speedup,
            r.seq_attach.as_secs_f64() * 1e3,
            r.pipe_attach.as_secs_f64() * 1e3,
            attach_speedup,
        );
    }
    println!(
        "keystroke echo      p50 {:.3} ms  p90 {:.3} ms  max {:.3} ms",
        lat[25].as_secs_f64() * 1e3,
        lat[45].as_secs_f64() * 1e3,
        lat[49].as_secs_f64() * 1e3
    );
    if let Some(lat) = contended {
        println!(
            "echo, {contend_panes} flooding  p50 {:.3} ms  p90 {:.3} ms  max {:.3} ms",
            lat[lat.len() / 2].as_secs_f64() * 1e3,
            lat[lat.len() * 9 / 10].as_secs_f64() * 1e3,
            lat[lat.len() - 1].as_secs_f64() * 1e3
        );
    }
    if let Some((flood, received)) = flood_result {
        println!(
            "flood {flood_mb} MB        {:>8.2} s → {:.0} MB/s produced, {:.1} MB delivered, {} resyncs ({:.1} MB skipped)",
            flood.as_secs_f64(),
            flood_mb as f64 / flood.as_secs_f64(),
            received as f64 / 1e6,
            sink.resyncs.load(Ordering::Relaxed),
            sink.skipped.load(Ordering::Relaxed) as f64 / 1e6
        );
    }
    if let Some(m) = sync {
        let p = |v: &[Duration], q: usize| v[(v.len() * q / 100).min(v.len() - 1)].as_secs_f64() * 1e3;
        println!(
            "workspace save      {} tabs ({:.1} KB): round trip p50 {:.3} ms p90 {:.3} ms; other window has it p50 {:.3} ms p90 {:.3} ms",
            m.tabs,
            m.bytes as f64 / 1e3,
            p(&m.save, 50),
            p(&m.save, 90),
            p(&m.seen, 50),
            p(&m.seen, 90)
        );
    }
    if let Some((cpu, kbps)) = background {
        println!("background {bg_panes} panes  {kbps:.1} KB/s fed to mirror terminals, client CPU {cpu:.1}% (of one core)");
    }
    if let Some(rtt) = conn.rtt() {
        println!("heartbeat rtt       {:>8.3} ms", rtt.as_secs_f64() * 1e3);
    }
    let _ = conn.kill(id);
    drop(conn);
    drop(daemon);
    Ok(())
}
