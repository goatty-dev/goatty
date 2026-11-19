//! Headless end-to-end benchmark of the client ⇄ daemon path: keystroke echo
//! latency, flood throughput, and multi-pane window restore (sequential vs.
//! pipelined `attach_many`), with the client feeding a real mirror VT (same
//! work the GUI does, minus rendering).
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
    layout_saved: bool,
}

impl Drop for RestoreCleanup<'_> {
    fn drop(&mut self) {
        if self.ids.is_empty() && !self.layout_saved {
            return;
        }
        let conn = self.conn.take().or_else(|| Connection::open(self.setup_endpoint.clone(), "netbench-cleanup").ok());
        let Some(conn) = conn else { return };
        if self.layout_saved {
            conn.save_layout(0x127, Vec::new(), String::new());
        }
        for &id in &self.ids {
            let _ = conn.kill(id);
        }
        if self.layout_saved {
            // Ensure the daemon processed `save_layout` before disconnecting.
            let _ = conn.list_sessions();
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
    let mut cleanup = RestoreCleanup { setup_endpoint, conn: None, ids: Vec::with_capacity(panes), layout_saved: false };

    // Create and seed the panes on a setup connection (local Unix socket for
    // private daemons so setup itself doesn't pay simulated latency), save a
    // layout, then drop the setup connection so the layout is claimable.
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
    setup.save_layout(0x127, cleanup.ids.clone(), r#"{"version":1,"tabs":[],"active":0}"#.into());
    cleanup.layout_saved = true;
    // Ensure the daemon processed `save_layout` before we disconnect.
    let _ = setup.list_sessions()?;
    drop(setup);

    let observer = Connection::open(setup_endpoint.clone(), "netbench-obs")?;
    cleanup.conn = Some(observer.clone());
    wait_all_detached(&observer, &cleanup.ids)?;

    // 1) Sequential restore: `claim_layout` + N sequential `attach` calls.
    let c_seq = Connection::open(endpoint.clone(), "netbench-seq")?;
    let t_seq = Instant::now();
    let layout = c_seq.claim_layout()?.ok_or_else(|| anyhow::anyhow!("layout missing for sequential restore"))?;
    anyhow::ensure!(layout.sessions.len() == panes, "expected {panes} sessions in layout, got {}", layout.sessions.len());
    let t_seq_attach = Instant::now();
    for &id in &cleanup.ids {
        c_seq.attach(id, ws, Sink::new(size))?;
    }
    let seq_attach = t_seq_attach.elapsed();
    let seq_total = t_seq.elapsed();
    drop(c_seq);

    wait_all_detached(&observer, &cleanup.ids)?;

    // 2) Pipelined restore: `claim_layout` + one `attach_many` burst.
    let c_pipe = Connection::open(endpoint.clone(), "netbench-pipe")?;
    let t_pipe = Instant::now();
    let layout = c_pipe.claim_layout()?.ok_or_else(|| anyhow::anyhow!("layout missing for pipelined restore"))?;
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
    let restore_lines: usize = std::env::var("RESTORE_LINES").ok().and_then(|s| s.parse().ok()).unwrap_or(0);

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
    if let Some(rtt) = conn.rtt() {
        println!("heartbeat rtt       {:>8.3} ms", rtt.as_secs_f64() * 1e3);
    }
    let _ = conn.kill(id);
    drop(conn);
    drop(daemon);
    Ok(())
}
