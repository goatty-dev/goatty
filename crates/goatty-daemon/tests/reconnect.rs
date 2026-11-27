#![cfg(unix)]
//! End-to-end tests against a real goattyd:
//! * detach/reattach keeps the shell and restores the screen,
//! * full-screen apps are restored,
//! * several sessions multiplex over one connection,
//! * a client that doesn't keep up is skipped forward with a Resync instead
//!   of buffering without bound,
//! * the remote path (`goattyd proxy` over stdio, exactly what ssh runs)
//!   works, including transparent reconnect + reattach,
//! * windows share a workspace and hear each other's saves (#215),
//! * a client fills its scrollback from the daemon on demand (compressed
//!   over the remote path).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use goatty_client::{AttachRequest, Connection, Control, Endpoint, History, PaneEvent, PaneSink, TopLineCause};
use goatty_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg, SessionId, StreamOpts, WinSize, encode, read_msg};
use goatty_term::{Frame, Palette, TermSize, Terminal};
use parking_lot::Mutex;

/// A private daemon in its own temp dir. Dropping it kills the daemon, then
/// removes the dir (fields drop after `Drop::drop`).
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
        // Host settings and generations stay in the sandbox.
        .env("HOME", dir.path())
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
    Daemon { child, sock, _dir: dir }
}

fn send(s: &mut UnixStream, msg: &ClientMsg) {
    s.write_all(&encode(msg).unwrap()).unwrap();
}

fn connect(sock: &Path) -> UnixStream {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send(&mut s, &ClientMsg::Hello { version: PROTOCOL_VERSION, client: "test".into() });
    assert!(matches!(read_msg::<_, ServerMsg>(&mut s).unwrap(), ServerMsg::Hello { .. }));
    s
}

const SIZE: WinSize = WinSize { cols: 60, rows: 12, cell_width: 8, cell_height: 16 };
const TSIZE: TermSize = TermSize { cols: 60, rows: 12, cell_width: 8, cell_height: 16 };

fn term() -> Terminal {
    Terminal::new(TSIZE, false, Arc::new(Palette::default())).0
}

fn screen_text(t: &Terminal) -> String {
    let mut f = Frame::default();
    t.snapshot_into(&mut f);
    (0..f.rows).map(|r| f.row(r).iter().map(|c| c.ch).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

/// Read frames for `session` into `t` until `needle` appears on screen.
/// Acks everything it consumes.
fn pump_until(s: &mut UnixStream, session: SessionId, t: &Terminal, consumed: &mut u64, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_text(t).contains(needle) {
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?}; screen:\n{}", screen_text(t));
        let msg = read_msg::<_, ServerMsg>(s).unwrap();
        *consumed += msg.stream_len() as u64;
        match msg {
            ServerMsg::Output { session: id, data } if id == session => {
                t.feed(&data);
            }
            ServerMsg::Attached { session: id, snapshot, .. } | ServerMsg::Resync { session: id, snapshot, .. } if id == session => {
                t.feed(&snapshot);
            }
            ServerMsg::Output { .. }
            | ServerMsg::Pong { .. }
            | ServerMsg::Process { .. }
            | ServerMsg::Cwd { .. }
            | ServerMsg::TopLine { .. }
            | ServerMsg::SizeControl { .. }
            | ServerMsg::FlowWindow { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
        send(s, &ClientMsg::Ack { session, consumed: *consumed });
    }
}

fn create(s: &mut UnixStream, opts: StreamOpts) -> (SessionId, u64) {
    send(s, &ClientMsg::Create { req: 1, size: SIZE, cwd: None, opts });
    loop {
        match read_msg::<_, ServerMsg>(s).unwrap() {
            ServerMsg::Attached { req: 1, session, snapshot } => return (session, snapshot.len() as u64),
            // Another session's foreground program, cwd, or line numbers (multiplexed connection).
            ServerMsg::Process { .. }
            | ServerMsg::Cwd { .. }
            | ServerMsg::TopLine { .. }
            | ServerMsg::SizeControl { .. }
            | ServerMsg::FlowWindow { .. } => {}
            other => panic!("{other:?}"),
        }
    }
}

fn session_info(sock: &Path, id: u64) -> goatty_proto::SessionInfo {
    let mut c = Control::connect(sock, "test").unwrap();
    c.list_sessions().unwrap().into_iter().find(|s| s.id == id).expect("session listed")
}

#[test]
fn detach_and_reattach_keeps_shell_and_screen() {
    let d = start_daemon();

    // 1. Create + run a command.
    let mut a = connect(&d.sock);
    let (id, mut consumed) = create(&mut a, StreamOpts::LOCAL);
    let ta = term();
    send(&mut a, &ClientMsg::Input { session: id, data: b"export MARK=goat$((40+2)); echo \"$MARK\"\r".to_vec() });
    pump_until(&mut a, id, &ta, &mut consumed, "goat42");
    let before = session_info(&d.sock, id);
    assert_eq!(before.attached, 1);

    // 2. Detach (drop connection), let the shell keep producing output.
    drop(a);
    std::thread::sleep(Duration::from_millis(100));
    let mut c = connect(&d.sock);
    send(&mut c, &ClientMsg::SendInput { req: 5, session: id, data: b"echo while-detached\r".to_vec() });
    assert!(matches!(read_msg::<_, ServerMsg>(&mut c).unwrap(), ServerMsg::Ok { req: 5 }));
    std::thread::sleep(Duration::from_millis(200));
    let detached = session_info(&d.sock, id);
    assert_eq!(detached.attached, 0);
    assert_eq!(before.pid, detached.pid, "shell must survive detach");

    // 3. Reattach: snapshot must show both the old and the detached output,
    //    and the shell must still have its in-memory state.
    let mut b = connect(&d.sock);
    send(&mut b, &ClientMsg::Attach { req: 2, session: id, size: SIZE, opts: StreamOpts::LOCAL });
    let tb = term();
    let mut consumed = 0;
    pump_until(&mut b, id, &tb, &mut consumed, "while-detached");
    assert!(screen_text(&tb).contains("goat42"), "pre-detach output restored:\n{}", screen_text(&tb));
    send(&mut b, &ClientMsg::Input { session: id, data: b"echo again-$MARK\r".to_vec() });
    pump_until(&mut b, id, &tb, &mut consumed, "again-goat42");
}

#[test]
fn reattach_restores_alt_screen_app() {
    let d = start_daemon();
    let mut a = connect(&d.sock);
    let (id, mut consumed) = create(&mut a, StreamOpts::LOCAL);
    let ta = term();
    // A fake full-screen app: enter alt screen, enable app cursor keys, draw.
    send(
        &mut a,
        &ClientMsg::Input {
            session: id,
            data: b"printf '\\033[?1049h\\033[?1h\\033[2J\\033[5;10H%s-%s' FULLSCREEN APP; sleep 30\r".to_vec(),
        },
    );
    pump_until(&mut a, id, &ta, &mut consumed, "FULLSCREEN-APP");
    drop(a);

    let mut b = connect(&d.sock);
    send(&mut b, &ClientMsg::Attach { req: 2, session: id, size: SIZE, opts: StreamOpts::LOCAL });
    let tb = term();
    let mut consumed = 0;
    pump_until(&mut b, id, &tb, &mut consumed, "FULLSCREEN-APP");
    let m = tb.mode();
    assert!(m.contains(goatty_term::TermMode::ALT_SCREEN), "alt screen restored");
    assert!(m.contains(goatty_term::TermMode::APP_CURSOR), "DECCKM restored");
}

#[test]
fn reattach_in_alt_screen_restores_primary_screen_on_exit() {
    let d = start_daemon();
    let mut a = connect(&d.sock);
    let (id, mut consumed) = create(&mut a, StreamOpts::LOCAL);
    let ta = term();
    send(
        &mut a,
        &ClientMsg::Input {
            session: id,
            data: b"echo START; i=1; while [ $i -le 20 ]; do echo \"hist-$i\"; i=$((i+1)); done; echo BEFORE-$((40+2)); printf '\\033[?1049h\\033[2J\\033[2;2H%s-%s' ALT MODE; read _; printf '\\033[?1049l'; echo AFTER-$((1+1))\r".to_vec(),
        },
    );
    pump_until(&mut a, id, &ta, &mut consumed, "ALT-MODE");
    drop(a);

    let mut b = connect(&d.sock);
    send(&mut b, &ClientMsg::Attach { req: 2, session: id, size: SIZE, opts: StreamOpts::LOCAL });
    let tb = term();
    let mut consumed = 0;
    pump_until(&mut b, id, &tb, &mut consumed, "ALT-MODE");
    assert!(tb.mode().contains(goatty_term::TermMode::ALT_SCREEN));

    // Exit the alt-screen app: the pre-alt primary screen and scrollback must reappear.
    send(&mut b, &ClientMsg::Input { session: id, data: b"\r".to_vec() });
    pump_until(&mut b, id, &tb, &mut consumed, "AFTER-2");
    assert!(!tb.mode().contains(goatty_term::TermMode::ALT_SCREEN));
    assert!(screen_text(&tb).contains("BEFORE-42"), "primary screen lost:\n{}", screen_text(&tb));
    assert!(tb.history_size() > 0, "primary scrollback should be restored");
    let transcript = tb.transcript();
    assert!(
        transcript.lines.iter().any(|l| l.text.trim() == "hist-1"),
        "oldest scrollback missing:\n{}",
        transcript.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"),
    );
}

#[test]
fn sessions_multiplex_over_one_connection() {
    let d = start_daemon();
    let mut c = connect(&d.sock);
    let (s1, mut c1) = create(&mut c, StreamOpts::LOCAL);
    let (s2, mut c2) = create(&mut c, StreamOpts::LOCAL);
    assert_ne!(s1, s2);
    let (t1, t2) = (term(), term());
    send(&mut c, &ClientMsg::Input { session: s1, data: b"echo one-$((1))\r".to_vec() });
    send(&mut c, &ClientMsg::Input { session: s2, data: b"echo two-$((2))\r".to_vec() });
    // Route frames by session id.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(screen_text(&t1).contains("one-1") && screen_text(&t2).contains("two-2")) {
        assert!(Instant::now() < deadline, "s1:\n{}\ns2:\n{}", screen_text(&t1), screen_text(&t2));
        match read_msg::<_, ServerMsg>(&mut c).unwrap() {
            ServerMsg::Output { session, data } if session == s1 => {
                c1 += data.len() as u64;
                t1.feed(&data);
            }
            ServerMsg::Output { session, data } if session == s2 => {
                c2 += data.len() as u64;
                t2.feed(&data);
            }
            ServerMsg::Process { .. } | ServerMsg::TopLine { .. } | ServerMsg::SizeControl { .. } | ServerMsg::Cwd { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(!screen_text(&t1).contains("two-2") && !screen_text(&t2).contains("one-1"), "streams must not mix");
    let _ = (c1, c2);
}

#[test]
fn slow_client_is_skipped_forward_with_resync() {
    let d = start_daemon();
    let mut c = connect(&d.sock);
    // Tiny window (the daemon clamps to 64 KiB) and we don't ack at first.
    let opts = StreamOpts { window: 64 << 10, history: 0 };
    let (id, snap_len) = create(&mut c, opts);
    // ~8 MB of output, then a marker.
    send(&mut c, &ClientMsg::Input { session: id, data: b"head -c 8000000 /dev/zero | tr '\\0' x; echo; echo FLOOD-DONE\r".to_vec() });
    // Let the flood finish on the daemon side while we're "slow".
    std::thread::sleep(Duration::from_millis(1500));

    // Drain what was sent before the daemon stopped streaming to us.
    let t = term();
    let mut consumed = snap_len;
    let mut output_bytes = 0u64;
    c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    while let Ok(msg) = read_msg::<_, ServerMsg>(&mut c) {
        consumed += msg.stream_len() as u64;
        if let ServerMsg::Output { data, .. } = msg {
            output_bytes += data.len() as u64;
            t.feed(&data);
        }
    }
    assert!(output_bytes < 1_000_000, "daemon must stop streaming to a lagging client (got {output_bytes} bytes)");

    // Catch up: the daemon answers with a Resync showing the final screen.
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send(&mut c, &ClientMsg::Ack { session: id, consumed });
    // Foreground-program changes aren't stream data and may come first.
    let resync = loop {
        match read_msg::<_, ServerMsg>(&mut c).unwrap() {
            ServerMsg::Process { .. } | ServerMsg::FlowWindow { .. } => continue,
            other => break other,
        }
    };
    match resync {
        ServerMsg::Resync { session, skipped, snapshot } => {
            assert_eq!(session, id);
            assert!(skipped > 7_000_000, "skipped {skipped}");
            t.feed(&snapshot);
            consumed += snapshot.len() as u64;
        }
        other => panic!("expected Resync, got {other:?}"),
    }
    assert!(screen_text(&t).contains("FLOOD-DONE"), "{}", screen_text(&t));

    // And streaming resumes normally afterwards.
    send(&mut c, &ClientMsg::Ack { session: id, consumed });
    send(&mut c, &ClientMsg::Input { session: id, data: b"echo after-$((6*7))\r".to_vec() });
    pump_until(&mut c, id, &t, &mut consumed, "after-42");
}

#[test]
fn foreground_program_changes_are_reported() {
    let d = start_daemon();
    let mut c = connect(&d.sock);
    let (id, _) = create(&mut c, StreamOpts::LOCAL);
    // Read until a Process message matches `want`.
    let wait_for = |c: &mut UnixStream, want: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "no matching Process message; saw {seen:?}");
            if let ServerMsg::Process { session, name } = read_msg::<_, ServerMsg>(c).unwrap() {
                assert_eq!(session, id);
                let name = name.unwrap_or_default();
                seen.push(name.clone());
                if want(&name) {
                    return;
                }
            }
        }
    };
    send(&mut c, &ClientMsg::Input { session: id, data: b"sleep 2\r".to_vec() });
    wait_for(&mut c, &|n| n == "sleep");
    assert_eq!(session_info(&d.sock, id).process.as_deref(), Some("sleep"));
    // Back at the prompt: the shell again.
    wait_for(&mut c, &|n| n != "sleep" && !n.is_empty());
}

// ---- remote path: goatty-client over `goattyd proxy` ------------------------

#[derive(Default)]
struct TestSink {
    term: Mutex<Option<Terminal>>,
    events: Mutex<Vec<String>>,
    /// History answers and top lines received.
    histories: Mutex<u32>,
    tops: Mutex<u32>,
}

impl PaneSink for TestSink {
    fn feed(&self, bytes: &[u8]) {
        self.term.lock().get_or_insert_with(term).feed(bytes);
    }
    fn event(&self, ev: PaneEvent) {
        self.events.lock().push(format!("{ev:?}"));
    }
    fn top_line(&self, line: u64, cause: TopLineCause) {
        let cause = match cause {
            TopLineCause::Attach => goatty_term::TopLineCause::Attach,
            TopLineCause::Resync => goatty_term::TopLineCause::Resync,
            TopLineCause::Resize => goatty_term::TopLineCause::Resize,
        };
        self.term.lock().get_or_insert_with(term).set_top_line(line, cause);
        *self.tops.lock() += 1;
    }
    fn history(&self, history: Option<History>) {
        let h = history.expect("history answer");
        let rows = goatty_term::HistoryRows { first: h.first, oldest: h.oldest, data: h.data };
        assert!(self.term.lock().as_ref().unwrap().insert_history(h.before, &rows));
        *self.histories.lock() += 1;
    }
}

impl TestSink {
    fn screen(&self) -> String {
        self.term.lock().as_ref().map(screen_text).unwrap_or_default()
    }
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
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
    // Exactly what `ssh host …` ends up running, minus ssh.
    Endpoint::Command {
        label: "test-proxy".into(),
        program: "env".into(),
        args: vec![format!("GOATTY_SOCKET={}", d.sock.display()), env!("CARGO_BIN_EXE_goattyd").into(), "proxy".into()],
    }
}

#[test]
fn client_over_proxy_reconnects_and_reattaches() {
    let d = start_daemon();
    let conn = Connection::open(proxy_endpoint(&d), "test").unwrap();
    assert!(!conn.host().hostname.is_empty());
    let sink = Arc::new(TestSink::default());
    let id = conn.create(SIZE, None, sink.clone()).unwrap();
    conn.input(id, b"export MARK=proxied$((1+1)); echo \"$MARK\"\r");
    sink.wait_for("proxied2");
    let pid = session_info(&d.sock, id).pid;

    // Network "fails": the proxy process dies. The client must reconnect
    // through a fresh proxy and reattach the same session.
    conn.force_disconnect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sink.events.lock().iter().any(|e| e == "Reconnected") {
        assert!(Instant::now() < deadline, "no reconnect; events: {:?}", sink.events.lock());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(conn.is_connected());
    assert!(sink.events.lock().iter().any(|e| e.starts_with("Disconnected")));
    assert!(sink.screen().contains("proxied2"), "screen restored after reconnect:\n{}", sink.screen());
    conn.input(id, b"echo still-$MARK\r");
    sink.wait_for("still-proxied2");
    assert_eq!(session_info(&d.sock, id).pid, pid, "same shell process");
}

#[test]
fn scrollback_is_fetched_from_the_daemon_on_demand() {
    let d = start_daemon();
    let conn = Connection::open(proxy_endpoint(&d), "test").unwrap();
    let sink = Arc::new(TestSink::default());
    // The first client sees the whole output live.
    let id = conn.create(SIZE, None, sink.clone()).unwrap();
    conn.input(id, b"i=0; while [ $i -lt 4000 ]; do i=$((i+1)); echo \"row $i\"; done; echo done-$((1+1))\r");
    sink.wait_for("done-2");
    conn.detach(id);

    // A second client attaches over the remote path: the snapshot replays
    // the newest 2000 lines only (compressed).
    let conn = Connection::open(proxy_endpoint(&d), "test").unwrap();
    let sink = Arc::new(TestSink::default());
    conn.attach(id, SIZE, sink.clone()).unwrap();
    let rows = |sink: &TestSink| -> Vec<String> {
        let t = sink.term.lock();
        let t = t.as_ref().unwrap();
        // dash prints its prompt after the echoed command: `$ row 1`.
        let row =
            |l: &str| l.rfind("row ").map(|i| l[i..].to_string()).filter(|r| r[4..].bytes().all(|b| b.is_ascii_digit()) && r.len() > 4);
        t.transcript().lines.iter().filter_map(|l| row(&l.text)).collect()
    };
    eventually("the attach snapshot and its top line", || rows(&sink).len() > 1000 && *sink.tops.lock() > 0);
    assert!(!rows(&sink).contains(&"row 1".to_string()));
    let mut fetches = 0;
    loop {
        let wanted = {
            let t = sink.term.lock();
            let t = t.as_ref().unwrap();
            t.scroll(i32::MAX / 2);
            t.wanted_history(24)
        };
        let Some(w) = wanted else { break };
        let before = *sink.histories.lock();
        assert!(conn.fetch_history(id, w.before, w.count));
        eventually("a history answer", || *sink.histories.lock() > before);
        fetches += 1;
        assert!(fetches < 20, "keeps fetching");
    }
    let got = rows(&sink);
    let want: Vec<String> = (1..=4000).map(|i| format!("row {i}")).collect();
    assert_eq!(got, want);
    assert!(fetches >= 2, "{fetches} fetches");
}

#[test]
fn snapshots_and_history_are_compressed_when_asked() {
    let d = start_daemon();
    let mut c = connect(&d.sock);
    let (id, _) = create(&mut c, StreamOpts::LOCAL);
    send(&mut c, &ClientMsg::Input { session: id, data: b"i=0; while [ $i -lt 3000 ]; do i=$((i+1)); echo \"row $i\"; done\r".to_vec() });
    let mut c2 = connect(&d.sock);
    send(&mut c2, &ClientMsg::Compress { zstd: true });
    // Wait for the output to finish, then attach with a short history.
    let deadline = Instant::now() + Duration::from_secs(10);
    let top = loop {
        assert!(Instant::now() < deadline, "no output");
        send(&mut c2, &ClientMsg::Attach { req: 7, session: id, size: SIZE, opts: StreamOpts { window: 8 << 20, history: 100 } });
        // (Output still queued from the last attach comes first.)
        let attached = std::iter::repeat_with(|| read_msg::<_, ServerMsg>(&mut c2).unwrap())
            .find(|m| matches!(m, ServerMsg::Zstd { .. } | ServerMsg::Attached { .. }))
            .unwrap();
        // (Snapshots under 1 KiB go as they are.)
        let compressed = matches!(attached, ServerMsg::Zstd { .. });
        let ServerMsg::Attached { snapshot, .. } = attached.unpack().unwrap() else { panic!() };
        let line = loop {
            match read_msg::<_, ServerMsg>(&mut c2).unwrap() {
                ServerMsg::TopLine { line, cause: goatty_proto::TopLineCause::Attach, .. } => break line,
                ServerMsg::Process { .. } | ServerMsg::Cwd { .. } | ServerMsg::Output { .. } => {}
                other => panic!("{other:?}"),
            }
        };
        if String::from_utf8_lossy(&snapshot).contains("row 3000") {
            assert!(compressed, "a 100-line snapshot is compressed");
            break line;
        }
        send(&mut c2, &ClientMsg::Detach { session: id });
        std::thread::sleep(Duration::from_millis(100));
    };
    send(&mut c2, &ClientMsg::FetchHistory { req: 8, session: id, before: top - 100, count: 1000 });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline);
        match read_msg::<_, ServerMsg>(&mut c2).unwrap() {
            m @ ServerMsg::Zstd { .. } => {
                let ServerMsg::History { req: 8, first, before, data, .. } = m.unpack().unwrap() else { panic!() };
                assert_eq!(before - first, 1000);
                let text = String::from_utf8_lossy(&data);
                assert_eq!(text.lines().count(), 1000);
                assert!(text.ends_with("\r\n"));
                break;
            }
            ServerMsg::Process { .. } | ServerMsg::Cwd { .. } | ServerMsg::Output { .. } => {}
            other => panic!("{other:?}"),
        }
    }
}

/// Wait until `f` holds.
fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// v17 raised the oldest protocol (#215): an older client hears the
/// daemon's newest version, so it can say which side to update, and the
/// connection closes.
#[test]
fn clients_older_than_workspaces_are_refused() {
    let d = start_daemon();
    let mut s = UnixStream::connect(&d.sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send(&mut s, &ClientMsg::Hello { version: goatty_proto::MIN_PROTOCOL_VERSION - 1, client: "old".into() });
    assert!(matches!(read_msg::<_, ServerMsg>(&mut s).unwrap(), ServerMsg::Hello { version: PROTOCOL_VERSION, .. }));
    // Closed: end of file, not a timeout.
    assert_eq!(s.read(&mut [0u8; 1]).unwrap(), 0, "closed");
}

/// What a window hears about its workspace.
#[derive(Default)]
struct Heard(Mutex<Vec<goatty_client::WorkspaceEvent>>);

impl goatty_client::WorkspaceSink for Heard {
    fn workspace(&self, _conn: usize, event: goatty_client::WorkspaceEvent) {
        self.0.lock().push(event);
    }
}

impl Heard {
    fn changed(&self) -> Vec<(u64, String)> {
        self.0
            .lock()
            .iter()
            .filter_map(|e| match e {
                goatty_client::WorkspaceEvent::Changed(l) => Some((l.revision, l.json.clone())),
                _ => None,
            })
            .collect()
    }
}

/// #215: two windows (two connections, as two computers would) join the
/// same workspace, see each other's saves live, and a stale save gets the
/// newest layout to rebase on.
#[test]
fn windows_share_a_workspace_and_hear_each_others_saves() {
    use goatty_proto::SaveOutcome;
    let d = start_daemon();
    let a = Connection::open(proxy_endpoint(&d), "window a").unwrap();
    let b = Connection::open(proxy_endpoint(&d), "window b").unwrap();
    let (ha, hb) = (Arc::new(Heard::default()), Arc::new(Heard::default()));
    let (main, layout) = a.join_workspace(1, None, ha.clone()).unwrap();
    assert_eq!((main.name.as_str(), layout.revision, layout.json.as_str()), ("Main", 0, ""));
    let (also, _) = b.join_workspace(2, None, hb.clone()).unwrap();
    assert_eq!((also.id, also.views), (main.id, 2), "a second window joins the same workspace");

    let pane = Arc::new(TestSink::default());
    let s1 = a.create(SIZE, None, pane.clone()).unwrap();
    assert_eq!(a.save_workspace(1, main.id, 0, 1, vec![s1], "a1".into()).unwrap(), SaveOutcome::Saved { revision: 1 });
    eventually("window b hears window a's save", || hb.changed() == [(1, "a1".to_string())]);
    assert!(ha.changed().is_empty(), "the saving window doesn't hear its own save");

    // Window b saves on the old revision: a conflict with the newest layout.
    let s2 = b.create(SIZE, None, Arc::new(TestSink::default())).unwrap();
    match b.save_workspace(2, main.id, 0, 1, vec![s2], "b1".into()).unwrap() {
        SaveOutcome::Conflict(l) => assert_eq!((l.revision, l.json.as_str(), l.sessions), (1, "a1", vec![s1])),
        other => panic!("{other:?}"),
    }
    assert_eq!(b.save_workspace(2, main.id, 1, 2, vec![s1, s2], "a1+b1".into()).unwrap(), SaveOutcome::Saved { revision: 2 });
    eventually("window a hears it", || ha.changed() == [(2, "a1+b1".to_string())]);

    // A reconnect joins the same workspace again.
    a.force_disconnect();
    eventually("window a rejoins", || {
        ha.0.lock()
            .iter()
            .any(|e| matches!(e, goatty_client::WorkspaceEvent::Rejoined { info, layout } if info.id == main.id && layout.revision == 2))
    });

    // A second workspace: listed, renamed, the others hear it.
    let (ops, empty) = b.create_workspace(2, None, hb.clone()).unwrap();
    assert_eq!((ops.name.as_str(), empty.revision), ("Workspace 2", 0));
    b.rename_workspace(ops.id, "ops".into()).unwrap();
    eventually("window a hears the new name", || {
        ha.0.lock().iter().any(|e| matches!(e, goatty_client::WorkspaceEvent::List { list, .. } if list.iter().any(|w| w.name == "ops")))
    });
    let list = a.list_workspaces().unwrap();
    assert_eq!(list.iter().map(|w| (w.name.as_str(), w.views)).collect::<Vec<_>>(), [("Main", 1), ("ops", 1)]);

    // Ended sessions drop out of the listed layout.
    a.kill(s1).unwrap();
    eventually("its session is pruned", || a.get_workspace(main.id).unwrap().unwrap().sessions == [s2]);

    // A workspace ends with its last tab (not the host's only one) ...
    let s3 = b.create(SIZE, None, Arc::new(TestSink::default())).unwrap();
    assert_eq!(b.save_workspace(2, ops.id, 0, 1, vec![s3], "ops".into()).unwrap(), SaveOutcome::Saved { revision: 1 });
    assert_eq!(b.save_workspace(2, ops.id, 1, 0, Vec::new(), String::new()).unwrap(), SaveOutcome::Saved { revision: 2 });
    assert_eq!(a.list_workspaces().unwrap().len(), 1);
    assert_eq!(b.save_workspace(2, ops.id, 2, 0, Vec::new(), String::new()).unwrap(), SaveOutcome::Gone);
    // ... and the only one can't be closed.
    assert!(a.close_workspace(main.id).is_err());
    assert_eq!(b.list_workspaces().unwrap().iter().map(|w| w.id).collect::<Vec<_>>(), [main.id]);
}

/// #215: a workspace nobody shows ends when its last session does, and a
/// closed one keeps its sessions running.
#[test]
fn workspaces_end_with_their_sessions_and_closing_keeps_them() {
    let d = start_daemon();
    let a = Connection::open(proxy_endpoint(&d), "window a").unwrap();
    let heard = Arc::new(Heard::default());
    let (main, _) = a.join_workspace(1, None, heard.clone()).unwrap();
    let s0 = a.create(SIZE, None, Arc::new(TestSink::default())).unwrap();
    a.save_workspace(1, main.id, 0, 1, vec![s0], "main".into()).unwrap();
    let (ops, _) = a.create_workspace(1, Some("ops".into()), heard.clone()).unwrap();
    let s1 = a.create(SIZE, None, Arc::new(TestSink::default())).unwrap();
    a.save_workspace(1, ops.id, 0, 1, vec![s1], "ops".into()).unwrap();
    let (logs, _) = a.create_workspace(1, Some("logs".into()), heard.clone()).unwrap();
    let s2 = a.create(SIZE, None, Arc::new(TestSink::default())).unwrap();
    a.save_workspace(1, logs.id, 0, 1, vec![s2], "logs".into()).unwrap();
    // The window shows "logs" now; "ops" ends with its shell.
    a.kill(s1).unwrap();
    eventually("ops ends", || {
        heard.0.lock().iter().any(|e| matches!(e, goatty_client::WorkspaceEvent::List { list, .. } if !list.iter().any(|w| w.id == ops.id)))
    });
    // Closing "logs" forgets it, but its shell keeps running.
    a.leave_workspace(1);
    a.close_workspace(logs.id).unwrap();
    assert!(a.list_sessions().unwrap().iter().any(|s| s.id == s2));
    assert_eq!(a.list_workspaces().unwrap().iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["Main"]);
    let mut c = Control::connect(&d.sock, "t").unwrap();
    assert_eq!(c.list_workspaces().unwrap().len(), 1);
}

#[test]
fn unhandled_osc_is_silenced_by_default() {
    let dir = tempfile::Builder::new().prefix("goatty-osc-test-").tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_goattyd"))
        .env("GOATTY_SOCKET", &sock)
        .env("GOATTY_SHELL", "/bin/sh")
        .env_remove("RUST_LOG")
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&sock).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut c = connect(&sock);
    let (id, mut consumed) = create(&mut c, StreamOpts::LOCAL);
    let t = term();
    send(&mut c, &ClientMsg::Input { session: id, data: b"printf '\\033]3008;start=1\\007OSC-DONE\\n'\r".to_vec() });
    pump_until(&mut c, id, &t, &mut consumed, "OSC-DONE");
    drop(c);

    let _ = child.kill();
    let _ = child.wait();
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(stderr.contains("goattyd"), "expected info logs with RUST_LOG unset; stderr:\n{stderr}");
    assert!(!stderr.contains("unhandled osc_dispatch"), "rio_vt warning should be silenced by default; stderr:\n{stderr}");
}

#[test]
fn attach_many_restores_multiple_panes_over_proxy() {
    let d = start_daemon();
    let a = Connection::open(proxy_endpoint(&d), "window a").unwrap();
    let mut ids = Vec::new();
    let mut setup_sinks = Vec::new();
    for i in 0..8usize {
        let sink = Arc::new(TestSink::default());
        let id = a.create(SIZE, None, sink.clone()).unwrap();
        a.input(id, format!("echo PANE-{i}-READY-$((1+1))\r").as_bytes());
        ids.push(id);
        setup_sinks.push(sink);
    }
    for (i, sink) in setup_sinks.iter().enumerate() {
        let marker = format!("PANE-{i}-READY-2");
        eventually(&format!("pane {i} ready"), || sink.screen().contains(&marker));
    }
    let (main, _) = a.join_workspace(1, None, Arc::new(Heard::default())).unwrap();
    a.save_workspace(1, main.id, 0, 8, ids.clone(), r#"{"version":2,"tabs":[]}"#.into()).unwrap();
    drop(a);

    // A fresh window over the remote proxy path joins the workspace and
    // attaches all 8 live sessions (plus a missing id) in one pipelined batch.
    let b = Connection::open(proxy_endpoint(&d), "window b").unwrap();
    let (_, joined) = b.join_workspace(2, None, Arc::new(Heard::default())).unwrap();
    assert_eq!(joined.sessions, ids);

    let mut restored_sinks = Vec::new();
    let mut requests = Vec::new();
    for &id in &ids {
        let sink = Arc::new(TestSink::default());
        requests.push(AttachRequest { session: id, size: SIZE, sink: sink.clone() });
        restored_sinks.push(sink);
    }
    let missing_sink = Arc::new(TestSink::default());
    requests.push(AttachRequest { session: 999_999, size: SIZE, sink: missing_sink });

    let results = b.attach_many(requests);
    assert_eq!(results.len(), 9);
    for (i, res) in results[..8].iter().enumerate() {
        res.as_ref().unwrap_or_else(|e| panic!("pane {i} failed to attach: {e:#}"));
        let marker = format!("PANE-{i}-READY-2");
        assert!(restored_sinks[i].screen().contains(&marker), "pane {i} snapshot missing {marker}:\n{}", restored_sinks[i].screen());
    }
    assert!(results[8].as_ref().unwrap_err().to_string().contains("no session 999999"));

    // Live input still works on every attached pane after the batch attach.
    for (i, &id) in ids.iter().enumerate() {
        b.input(id, format!("echo LIVE-{i}-$((2+2))\r").as_bytes());
    }
    for (i, sink) in restored_sinks.iter().enumerate() {
        let marker = format!("LIVE-{i}-4");
        eventually(&format!("pane {i} live echo"), || sink.screen().contains(&marker));
    }
}
