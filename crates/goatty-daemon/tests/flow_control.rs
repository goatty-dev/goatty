//! End-to-end tests for adaptive flow control (#9, protocol v15):
//! * fast remote proxy links measure RTT promptly and ramp their window up
//!   well above `StreamOpts::REMOTE.window`,
//! * slow links shrink toward `StreamOpts::MIN_WINDOW` (64 KiB) and skip early
//!   with `Resync`,
//! * local Unix socket connections stay fixed at `StreamOpts::LOCAL` (8 MiB),
//! * pre-v15 clients never receive `ServerMsg::FlowWindow`,
//! * reconnect resets the flow window and re-measures RTT on the new transport.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use goatty_client::{Connection, Endpoint, PaneEvent, PaneSink};
use goatty_proto::{ClientMsg, FLOW_CONTROL_SINCE, PROTOCOL_VERSION, ServerMsg, SessionId, StreamOpts, WinSize, encode, read_msg};
use goatty_term::{Frame, Palette, TermSize, Terminal};
use parking_lot::Mutex;

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
    let dir = tempfile::Builder::new().prefix("goatty-flow-test-").tempdir().unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&sock).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child, sock, _dir: dir }
}

const SIZE: WinSize = WinSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };
const TSIZE: TermSize = TermSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 };

fn term() -> Terminal {
    Terminal::new(TSIZE, false, Arc::new(Palette::default())).0
}

fn screen_text(t: &Terminal) -> String {
    let mut f = Frame::default();
    t.snapshot_into(&mut f);
    (0..f.rows).map(|r| f.row(r).iter().map(|c| c.ch).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

#[derive(Default)]
struct FloodSink {
    term: Mutex<Option<Terminal>>,
    bytes: AtomicU64,
    resyncs: AtomicU64,
    events: Mutex<Vec<String>>,
}

impl PaneSink for FloodSink {
    fn feed(&self, bytes: &[u8]) {
        self.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        self.term.lock().get_or_insert_with(term).feed(bytes);
    }

    fn event(&self, ev: PaneEvent) {
        if matches!(ev, PaneEvent::Resynced { .. }) {
            self.resyncs.fetch_add(1, Ordering::Relaxed);
        }
        self.events.lock().push(format!("{ev:?}"));
    }
}

impl FloodSink {
    fn screen(&self) -> String {
        self.term.lock().as_ref().map(screen_text).unwrap_or_default()
    }

    fn wait_for(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.screen().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; screen:\n{}\nevents: {:?}",
                self.screen(),
                self.events.lock()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn proxy_endpoint(d: &Daemon) -> Endpoint {
    Endpoint::Command {
        label: "test-proxy".into(),
        program: "env".into(),
        args: vec![format!("GOATTY_SOCKET={}", d.sock.display()), env!("CARGO_BIN_EXE_goattyd").into(), "proxy".into()],
    }
}

fn send(s: &mut UnixStream, msg: &ClientMsg) {
    s.write_all(&encode(msg).unwrap()).unwrap();
}

fn connect_version(sock: &Path, version: u32) -> UnixStream {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send(&mut s, &ClientMsg::Hello { version, client: "flow-test".into() });
    match read_msg::<_, ServerMsg>(&mut s).unwrap() {
        ServerMsg::Hello { version: agreed, .. } => assert_eq!(agreed, version.min(PROTOCOL_VERSION)),
        other => panic!("expected Hello, got {other:?}"),
    }
    s
}

fn create_raw(s: &mut UnixStream, opts: StreamOpts) -> (SessionId, u64) {
    send(s, &ClientMsg::Create { req: 1, size: SIZE, cwd: None, opts });
    loop {
        match read_msg::<_, ServerMsg>(s).unwrap() {
            ServerMsg::Attached { req: 1, session, snapshot } => return (session, snapshot.len() as u64),
            ServerMsg::Process { .. }
            | ServerMsg::Cwd { .. }
            | ServerMsg::TopLine { .. }
            | ServerMsg::SizeControl { .. }
            | ServerMsg::FlowWindow { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn fast_proxy_ramps_window_up_and_measures_rtt() {
    let d = start_daemon();
    let conn = Connection::open(proxy_endpoint(&d), "flow-fast").unwrap();

    // Immediate remote Ping should populate RTT well before the 5 s heartbeat.
    let rtt_deadline = Instant::now() + Duration::from_secs(2);
    while conn.rtt().is_none() {
        assert!(Instant::now() < rtt_deadline, "remote connection should measure initial RTT promptly");
        std::thread::sleep(Duration::from_millis(10));
    }

    let sink = Arc::new(FloodSink::default());
    let id = conn.create(SIZE, None, sink.clone()).unwrap();
    // Produce ~40 MB continuously (> 200 ms even on fast CPUs) so the adaptive
    // controller crosses multiple 50 ms epochs and ramps the window well past 256 KiB.
    conn.input(id, b"dd if=/dev/zero bs=65536 count=640 2>/dev/null | tr '\\0' A; echo; echo FAST-$((40+2))\r");
    sink.wait_for("FAST-42", Duration::from_secs(15));

    let window = conn.flow_window();
    assert!(
        window > StreamOpts::REMOTE.window,
        "fast proxy flood should grow window above {} KiB, got {} KiB",
        StreamOpts::REMOTE.window >> 10,
        window >> 10,
    );
    assert!(
        sink.bytes.load(Ordering::Relaxed) > 30_000_000,
        "fast proxy should deliver most/all bytes (got {})",
        sink.bytes.load(Ordering::Relaxed),
    );
}

#[test]
fn slow_link_shrinks_window_to_floor_and_resyncs() {
    let d = start_daemon();
    let mut c = connect_version(&d.sock, PROTOCOL_VERSION);
    send(&mut c, &ClientMsg::Rtt { rtt_us: 1_000 });
    let (id, mut consumed) = create_raw(&mut c, StreamOpts::REMOTE);
    send(&mut c, &ClientMsg::Ack { session: id, consumed });

    // Flood ~12 MB so the daemon stays backpressured while we pace acks at ~400 KB/s.
    send(
        &mut c,
        &ClientMsg::Input {
            session: id,
            data: b"dd if=/dev/zero bs=65536 count=200 2>/dev/null | tr '\\0' B; echo; echo SLOW-$((40+2))\r".to_vec(),
        },
    );

    let t = term();
    let mut min_window_seen = StreamOpts::REMOTE.window;
    let mut saw_resync = false;
    let mut delivered = 0u64;
    let mut unacked = 0u64;
    let deadline = Instant::now() + Duration::from_secs(15);
    c.set_read_timeout(Some(Duration::from_millis(100))).unwrap();

    while !screen_text(&t).contains("SLOW-42") {
        assert!(Instant::now() < deadline, "timed out waiting for SLOW-42; screen:\n{}", screen_text(&t));
        let msg = match read_msg::<_, ServerMsg>(&mut c) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                if unacked > 0 {
                    // Drain-ack partial window (matches real client's buffer-empty rule).
                    let pace_ms = (unacked * 1000 / 400_000).max(50);
                    std::thread::sleep(Duration::from_millis(pace_ms));
                    send(&mut c, &ClientMsg::Ack { session: id, consumed });
                    unacked = 0;
                }
                continue;
            }
            Err(e) => panic!("read_msg failed: {e}"),
        };
        let slen = msg.stream_len() as u64;
        consumed += slen;
        unacked += slen;
        match msg {
            ServerMsg::Output { data, .. } => {
                delivered += data.len() as u64;
                t.feed(&data);
            }
            ServerMsg::Resync { snapshot, .. } => {
                saw_resync = true;
                t.feed(&snapshot);
            }
            ServerMsg::FlowWindow { window } => {
                min_window_seen = min_window_seen.min(window);
            }
            ServerMsg::Process { .. }
            | ServerMsg::Cwd { .. }
            | ServerMsg::TopLine { .. }
            | ServerMsg::SizeControl { .. }
            | ServerMsg::Pong { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
        // Simulate a ~400 KB/s link: wait ~160 ms per 64 KiB before acking.
        if unacked >= 64 << 10 {
            std::thread::sleep(Duration::from_millis(160));
            send(&mut c, &ClientMsg::Ack { session: id, consumed });
            unacked = 0;
        }
    }

    assert!(saw_resync, "slow link should trigger skip-and-resync");
    assert_eq!(
        min_window_seen,
        StreamOpts::MIN_WINDOW,
        "slow ~400 KB/s link should shrink window to {} KiB floor, lowest seen was {} KiB",
        StreamOpts::MIN_WINDOW >> 10,
        min_window_seen >> 10,
    );
    assert!(delivered < 4_000_000, "slow link should skip most of the 12 MB flood (delivered {delivered})");
}

#[test]
fn local_connection_keeps_fixed_local_window() {
    let d = start_daemon();
    let mut c = connect_version(&d.sock, PROTOCOL_VERSION);
    let (id, mut consumed) = create_raw(&mut c, StreamOpts::LOCAL);
    send(&mut c, &ClientMsg::Ack { session: id, consumed });

    // Even if a client sends Rtt and paces acks slowly, a LOCAL (8 MiB)
    // subscription never attaches adaptive flow control, never emits
    // FlowWindow, and delivers a 2 MiB burst losslessly without resync.
    send(&mut c, &ClientMsg::Rtt { rtt_us: 1_000 });
    send(
        &mut c,
        &ClientMsg::Input {
            session: id,
            data: b"dd if=/dev/zero bs=65536 count=32 2>/dev/null | tr '\\0' L; echo; echo LOCAL-$((40+2))\r".to_vec(),
        },
    );

    let t = term();
    let mut delivered = 0u64;
    let mut unacked = 0u64;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_text(&t).contains("LOCAL-42") {
        assert!(Instant::now() < deadline, "timed out waiting for LOCAL-42");
        let msg = read_msg::<_, ServerMsg>(&mut c).unwrap();
        let slen = msg.stream_len() as u64;
        consumed += slen;
        unacked += slen;
        match msg {
            ServerMsg::FlowWindow { window } => panic!("LOCAL subscription must not emit FlowWindow({window})"),
            ServerMsg::Resync { .. } => panic!("2 MiB burst on 8 MiB LOCAL window must stay lossless"),
            ServerMsg::Output { data, .. } => {
                delivered += data.len() as u64;
                t.feed(&data);
            }
            ServerMsg::Process { .. }
            | ServerMsg::Cwd { .. }
            | ServerMsg::TopLine { .. }
            | ServerMsg::SizeControl { .. }
            | ServerMsg::Pong { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
        if unacked >= 256 << 10 {
            std::thread::sleep(Duration::from_millis(60));
            send(&mut c, &ClientMsg::Ack { session: id, consumed });
            unacked = 0;
        }
    }
    assert!(delivered >= 2_000_000, "expected full 2 MiB delivered, got {delivered}");
}

#[test]
fn pre_v15_client_never_receives_flow_window() {
    let d = start_daemon();
    let mut c = connect_version(&d.sock, FLOW_CONTROL_SINCE - 1);
    let (id, mut consumed) = create_raw(&mut c, StreamOpts::REMOTE);
    send(&mut c, &ClientMsg::Ack { session: id, consumed });

    send(
        &mut c,
        &ClientMsg::Input {
            session: id,
            data: b"dd if=/dev/zero bs=65536 count=160 2>/dev/null | tr '\\0' O; echo; echo OLD-$((40+2))\r".to_vec(),
        },
    );

    let t = term();
    let mut unacked = 0u64;
    let deadline = Instant::now() + Duration::from_secs(15);
    c.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    while !screen_text(&t).contains("OLD-42") {
        assert!(Instant::now() < deadline, "timed out waiting for OLD-42");
        let msg = match read_msg::<_, ServerMsg>(&mut c) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                if unacked > 0 {
                    std::thread::sleep(Duration::from_millis(80));
                    send(&mut c, &ClientMsg::Ack { session: id, consumed });
                    unacked = 0;
                }
                continue;
            }
            Err(e) => panic!("read_msg failed: {e}"),
        };
        let slen = msg.stream_len() as u64;
        consumed += slen;
        unacked += slen;
        match msg {
            ServerMsg::FlowWindow { .. } => panic!("daemon must not send FlowWindow to pre-v15 client"),
            ServerMsg::Output { data, .. } => {
                t.feed(&data);
            }
            ServerMsg::Resync { snapshot, .. } => {
                t.feed(&snapshot);
                send(&mut c, &ClientMsg::Ack { session: id, consumed });
                unacked = 0;
            }
            ServerMsg::Process { .. }
            | ServerMsg::Cwd { .. }
            | ServerMsg::TopLine { .. }
            | ServerMsg::SizeControl { .. }
            | ServerMsg::Pong { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
        if unacked >= 64 << 10 {
            std::thread::sleep(Duration::from_millis(120));
            send(&mut c, &ClientMsg::Ack { session: id, consumed });
            unacked = 0;
        }
    }
}

#[test]
fn reconnect_resets_flow_window_and_remeasures_rtt() {
    let d = start_daemon();
    let conn = Connection::open(proxy_endpoint(&d), "flow-reconnect").unwrap();
    let sink = Arc::new(FloodSink::default());
    let id = conn.create(SIZE, None, sink.clone()).unwrap();

    conn.input(id, b"dd if=/dev/zero bs=65536 count=640 2>/dev/null | tr '\\0' R; echo; echo BEFORE-$((40+2))\r");
    sink.wait_for("BEFORE-42", Duration::from_secs(15));
    assert!(
        conn.flow_window() > StreamOpts::REMOTE.window,
        "expected ramped window before disconnect, got {} KiB",
        conn.flow_window() >> 10,
    );

    conn.force_disconnect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sink.events.lock().iter().any(|e| e == "Reconnected") {
        assert!(Instant::now() < deadline, "did not reconnect; events: {:?}", sink.events.lock());
        std::thread::sleep(Duration::from_millis(20));
    }

    // Fresh connection starts back at REMOTE default and measures RTT promptly.
    assert_eq!(conn.flow_window(), StreamOpts::REMOTE.window);
    let rtt_deadline = Instant::now() + Duration::from_secs(2);
    while conn.rtt().is_none() {
        assert!(Instant::now() < rtt_deadline, "reconnected link should measure RTT promptly");
        std::thread::sleep(Duration::from_millis(10));
    }
}
