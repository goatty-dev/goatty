//! One shell session: its PTY, its authoritative terminal, and the
//! connections subscribed to its output (with per-subscriber flow control).

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use goatty_proto::{Frame, ReqId, ServerMsg, SessionId, SessionInfo, StreamOpts, TopLineCause, WinSize, encode, encode_server};
use goatty_term::process::ForegroundWatch;
use goatty_term::pty::Pty;
use goatty_term::{TermSize, Terminal, UiEvent};
use parking_lot::{Condvar, Mutex};

use crate::systemd::StoredPty;

/// One connection's subscription to one session.
///
/// Flow control: `sent` counts stream bytes queued for this subscriber,
/// `acked` what the client reported as consumed. When `sent - acked` would
/// exceed `window`, the subscriber goes `lagging`: raw output is skipped
/// (counted in `skipped`) until the client has consumed everything, then it
/// gets one `Resync` snapshot of the current screen and streaming resumes.
pub(crate) struct Subscriber {
    conn: u64,
    tx: flume::Sender<Frame>,
    window: u64,
    sent: AtomicU64,
    acked: AtomicU64,
    lagging: AtomicBool,
    skipped: AtomicU64,
    /// The client speaks v11: it gets `TopLine`s.
    lines: bool,
    /// The client speaks v13: it gets `Cwd` updates.
    cwd: bool,
    /// Compress snapshots (the connection sent `Compress`).
    pub(crate) zstd: AtomicBool,
}

/// What a connection's subscriptions get besides the stream.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SubFeatures {
    pub(crate) lines: bool,
    pub(crate) zstd: bool,
    pub(crate) cwd: bool,
}

impl Subscriber {
    fn push(&self, frame: Frame, stream_len: usize) -> bool {
        self.sent.fetch_add(stream_len as u64, Ordering::AcqRel);
        self.tx.send(frame).is_ok()
    }

    fn in_flight(&self) -> u64 {
        self.sent.load(Ordering::Acquire).saturating_sub(self.acked.load(Ordering::Acquire))
    }

    /// A snapshot message, compressed if the client asked for it.
    fn push_snapshot(&self, msg: &ServerMsg) {
        if let Ok(f) = encode_server(msg, self.zstd.load(Ordering::Relaxed)) {
            self.push(f, msg.stream_len());
        }
    }
}

pub(crate) struct Session {
    pub(crate) id: SessionId,
    pub(crate) term: Terminal,
    pub(crate) pty: Mutex<Pty>,
    /// Held while feeding + broadcasting, so taking a snapshot and
    /// (un)subscribing are atomic with respect to the output stream: no gap,
    /// no overlap.
    pub(crate) subs: Mutex<Vec<Arc<Subscriber>>>,
    pub(crate) created_unix: u64,
    /// The foreground program, kept current by `fg` (see [`Session::watch_foreground`]).
    pub(crate) process: Mutex<Option<String>>,
    /// The session's current working directory (OSC 7 preferred to preserve
    /// symlinks, with `/proc/<pid>/cwd` fallback).
    pub(crate) cwd: Mutex<Option<String>>,
    /// Last `/proc/<pid>/cwd` observed, so a subsequent foreground probe doesn't
    /// clobber a symlinked OSC 7 path when `/proc` didn't change.
    pub(crate) last_proc_cwd: Mutex<Option<PathBuf>>,
    pub(crate) fg: OnceLock<ForegroundWatch>,
    /// goatty's managed zsh (it handles reload requests).
    pub(crate) managed: bool,
    /// Lets a restart stop the output pump between two reads.
    pub(crate) pause: Pause,
    /// The process and the PTY master, known without `pty`'s lock (an
    /// input write can hold it for as long as the program doesn't read).
    /// The fd is open as long as `pty` is.
    pub(crate) pid: Option<u32>,
    pub(crate) master_fd: RawFd,
    /// Terminal UI events (used to observe OSC 7 `CwdChanged`).
    pub(crate) ui_events: flume::Receiver<UiEvent>,
    /// The terminal's query answers on their way to the PTY: the replies
    /// thread's queue and how many it took and wrote, so a restart can wait
    /// until none is left behind.
    pub(crate) replies: flume::Receiver<Vec<u8>>,
    pub(crate) replies_taken: AtomicU64,
    pub(crate) replies_written: AtomicU64,
}

impl Session {
    /// No query answer waits to be written.
    pub(crate) fn replies_idle(&self) -> bool {
        self.replies.is_empty() && self.replies_taken.load(Ordering::SeqCst) == self.replies_written.load(Ordering::SeqCst)
    }

    /// How the session's PTY is named in systemd's fd store.
    pub(crate) fn stored(&self) -> StoredPty {
        StoredPty { id: self.id, created_unix: self.created_unix, managed: self.managed }
    }
}

/// Stopping the output pump without losing output: once it's paused, every
/// byte it read is in the terminal, and the rest waits in the PTY for
/// whoever reads it next (the next goattyd, after a restart).
pub(crate) struct Pause {
    requested: AtomicBool,
    /// The pump is parked.
    parked: Mutex<bool>,
    changed: Condvar,
    /// Wakes the pump out of `poll`.
    wake_rx: OwnedFd,
    wake_tx: OwnedFd,
}

impl Pause {
    pub(crate) fn new() -> std::io::Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two fds pipe2 writes.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: pipe2 returned two new fds that nothing else owns.
        let (wake_rx, wake_tx) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Ok(Self { requested: AtomicBool::new(false), parked: Mutex::new(false), changed: Condvar::new(), wake_rx, wake_tx })
    }

    /// Ask the pump to stop and wait until it has (or `timeout` passed).
    /// True when it's parked, or already gone.
    pub(crate) fn pause(&self, timeout: Duration) -> bool {
        self.requested.store(true, Ordering::SeqCst);
        // SAFETY: writing one byte from a valid buffer to our own pipe.
        unsafe { libc::write(self.wake_tx.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        let deadline = Instant::now() + timeout;
        let mut parked = self.parked.lock();
        while !*parked {
            if self.changed.wait_until(&mut parked, deadline).timed_out() {
                return *parked;
            }
        }
        true
    }

    /// Let the pump go on (the restart didn't happen).
    pub(crate) fn resume(&self) {
        // Under the lock, so the pump can't miss the wake-up.
        let _parked = self.parked.lock();
        self.requested.store(false, Ordering::SeqCst);
        self.changed.notify_all();
    }

    /// Pump side: park while a pause is requested.
    fn park_if_requested(&self) {
        let mut buf = [0u8; 16];
        // SAFETY: draining our own non-blocking pipe into a valid buffer.
        while unsafe { libc::read(self.wake_rx.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        if !self.requested.load(Ordering::SeqCst) {
            return;
        }
        let mut parked = self.parked.lock();
        *parked = true;
        self.changed.notify_all();
        while self.requested.load(Ordering::SeqCst) {
            self.changed.wait(&mut parked);
        }
        *parked = false;
    }

    /// Pump side: it's gone for good (EOF), which counts as parked.
    fn finished(&self) {
        *self.parked.lock() = true;
        self.changed.notify_all();
    }
}

impl Session {
    /// Start tracking the foreground program and working directory: subscribers
    /// get `ServerMsg::Process` and `ServerMsg::Cwd` whenever they change.
    pub(crate) fn watch_foreground(self: &Arc<Self>) -> std::io::Result<()> {
        let probe = {
            let s: Weak<Self> = Arc::downgrade(self);
            move || {
                let s = s.upgrade()?;
                let (name, proc_cwd) = {
                    let pty = s.pty.lock();
                    (pty.foreground_name(), pty.cwd())
                };
                s.maybe_update_proc_cwd(proc_cwd);
                name
            }
        };
        let on_change = {
            let s: Weak<Self> = Arc::downgrade(self);
            move |name: Option<String>| {
                if let Some(s) = s.upgrade() {
                    s.set_process(name);
                }
            }
        };
        let watch = ForegroundWatch::spawn(format!("s{}-fg", self.id), probe, on_change)?;
        let _ = self.fg.set(watch);
        Ok(())
    }

    /// Something happened on the PTY: look at the foreground again soon.
    pub(crate) fn poke_foreground(&self) {
        if let Some(w) = self.fg.get() {
            w.poke();
        }
    }

    fn set_process(&self, name: Option<String>) {
        *self.process.lock() = name.clone();
        let Ok(frame) = encode(&ServerMsg::Process { session: self.id, name }) else { return };
        // Not stream data: doesn't count against the flow-control window.
        for sub in self.subs.lock().iter() {
            sub.push(frame.clone(), 0);
        }
    }

    pub(crate) fn current_cwd(&self) -> Option<String> {
        self.cwd.lock().clone().or_else(|| self.pty.lock().cwd().map(|p| p.to_string_lossy().into_owned()))
    }

    fn maybe_update_proc_cwd(&self, proc_cwd: Option<PathBuf>) {
        let Some(path) = proc_cwd else { return };
        {
            let mut last = self.last_proc_cwd.lock();
            if last.as_ref() == Some(&path) {
                return;
            }
            *last = Some(path.clone());
        }
        self.set_cwd(Some(path.to_string_lossy().into_owned()));
    }

    pub(crate) fn set_osc_cwd(&self, cwd: String, proc_cwd: Option<PathBuf>) {
        if proc_cwd.is_some() {
            *self.last_proc_cwd.lock() = proc_cwd;
        }
        self.set_cwd(Some(cwd));
    }

    fn set_cwd(&self, cwd: Option<String>) {
        {
            let mut cur = self.cwd.lock();
            if *cur == cwd {
                return;
            }
            *cur = cwd.clone();
        }
        let Ok(frame) = encode(&ServerMsg::Cwd { session: self.id, cwd }) else { return };
        for sub in self.subs.lock().iter() {
            if sub.cwd {
                sub.push(frame.clone(), 0);
            }
        }
    }

    pub(crate) fn info(&self) -> SessionInfo {
        let (cols, rows) = self.term.size();
        let tracked_cwd = self.cwd.lock().clone();
        let pty = self.pty.lock();
        SessionInfo {
            id: self.id,
            pid: pty.pid(),
            title: self.term.title(),
            cwd: tracked_cwd.or_else(|| pty.cwd().map(|p| p.to_string_lossy().into_owned())),
            cols,
            rows,
            attached: self.subs.lock().len() as u32,
            created_unix: self.created_unix,
            process: self.process.lock().clone(),
        }
    }

    /// Resize for connection `from` (`None`: before it subscribes). Rows
    /// reflow, so subscribers that number lines hear the new top line: all
    /// of them when the size changed, else just `from`. Lagging ones don't:
    /// they lack output the line counts, and their `Resync` brings the line.
    pub(crate) fn resize(&self, ws: WinSize, from: Option<u64>) {
        let size = to_size(ws);
        {
            // The reflow and its top line are one point in the stream, like a
            // snapshot and its top line in `subscribe`/`resync`.
            let subs = self.subs.lock();
            let changed = self.term.size() != (size.cols, size.rows);
            if changed {
                self.term.resize(size);
            }
            self.push_resized(&subs, changed, from);
        }
        if let Err(e) = self.pty.lock().resize(size) {
            log::warn!("session {}: resize failed: {e}", self.id);
        }
    }

    /// Caller holds `subs`.
    fn push_resized(&self, subs: &[Arc<Subscriber>], changed: bool, from: Option<u64>) {
        if let Some(line) = self.term.top_line() {
            let frame = encode(&ServerMsg::TopLine { session: self.id, line, cause: TopLineCause::Resize }).ok();
            let wants = |s: &Subscriber| s.lines && !s.lagging.load(Ordering::Acquire) && (changed || Some(s.conn) == from);
            for sub in subs.iter().filter(|s| wants(s)) {
                if let Some(f) = &frame {
                    sub.push(f.clone(), 0);
                }
            }
        }
    }

    /// The top line message after a snapshot, for clients that number lines.
    fn push_top_line(&self, sub: &Subscriber, cause: TopLineCause) {
        if !sub.lines {
            return;
        }
        if let Some(line) = self.term.top_line()
            && let Ok(f) = encode(&ServerMsg::TopLine { session: self.id, line, cause })
        {
            sub.push(f, 0);
        }
    }

    /// Snapshot + subscribe under one lock. Returns the subscriber.
    pub(crate) fn subscribe(
        &self,
        conn: u64,
        req: ReqId,
        opts: StreamOpts,
        features: SubFeatures,
        tx: &flume::Sender<Frame>,
    ) -> Arc<Subscriber> {
        let mut subs = self.subs.lock();
        subs.retain(|s| s.conn != conn); // re-attach on the same connection replaces
        let snapshot = self.term.reattach_snapshot_with_history(opts.history as usize);
        let sub = Arc::new(Subscriber {
            conn,
            tx: tx.clone(),
            window: opts.window.max(64 << 10) as u64,
            sent: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            lagging: AtomicBool::new(false),
            skipped: AtomicU64::new(0),
            lines: features.lines,
            cwd: features.cwd,
            zstd: AtomicBool::new(features.zstd),
        });
        sub.push_snapshot(&ServerMsg::Attached { req, session: self.id, snapshot });
        self.push_top_line(&sub, TopLineCause::Attach);
        let name = self.process.lock().clone();
        if name.is_some()
            && let Ok(f) = encode(&ServerMsg::Process { session: self.id, name })
        {
            sub.push(f, 0);
        }
        if sub.cwd {
            let cwd = self.cwd.lock().clone();
            if cwd.is_some()
                && let Ok(f) = encode(&ServerMsg::Cwd { session: self.id, cwd })
            {
                sub.push(f, 0);
            }
        }
        subs.push(sub.clone());
        sub
    }

    pub(crate) fn unsubscribe(&self, conn: u64) {
        self.subs.lock().retain(|s| s.conn != conn);
    }

    /// Client acknowledged `consumed` bytes. If it was lagging and has now
    /// caught up completely, send the current screen and resume streaming.
    pub(crate) fn ack(&self, sub: &Subscriber, consumed: u64) {
        sub.acked.fetch_max(consumed, Ordering::AcqRel);
        if sub.lagging.load(Ordering::Acquire) && sub.in_flight() == 0 {
            let _subs = self.subs.lock(); // order against the output pump
            if sub.lagging.load(Ordering::Acquire) && sub.in_flight() == 0 {
                self.resync(sub);
            }
        }
    }

    /// Caller holds `subs`.
    pub(crate) fn resync(&self, sub: &Subscriber) {
        let snapshot = self.term.resync_snapshot();
        let skipped = sub.skipped.swap(0, Ordering::AcqRel);
        sub.push_snapshot(&ServerMsg::Resync { session: self.id, skipped, snapshot });
        self.push_top_line(sub, TopLineCause::Resync);
        sub.lagging.store(false, Ordering::Release);
    }
    /// The shell exited: let every subscriber see the final screen, then
    /// `Exited`. Called once by the reader thread.
    pub(crate) fn finish(&self, code: Option<u32>) {
        let exited = encode(&ServerMsg::Exited { session: self.id, code }).ok();
        for sub in self.subs.lock().drain(..) {
            // Let lagging clients see the final screen before the end.
            if sub.lagging.load(Ordering::Acquire) {
                self.resync(&sub);
            }
            if let Some(f) = &exited {
                let _ = sub.tx.send(f.clone());
            }
        }
    }
}

pub(crate) fn to_size(ws: WinSize) -> TermSize {
    TermSize { cols: ws.cols.max(2), rows: ws.rows.max(1), cell_width: ws.cell_width, cell_height: ws.cell_height }
}

/// Read PTY output until EOF, feeding the authoritative terminal and every
/// subscriber. Runs on the session's reader thread. Between two reads it
/// can be paused ([`Pause`]).
pub(crate) fn pump_output(s: &Session, mut reader: File) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut fds = [
            libc::pollfd { fd: reader.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: s.pause.wake_rx.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: `fds` is a valid array of two pollfds.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 || s.pause.requested.load(Ordering::SeqCst) {
            s.pause.park_if_requested();
            continue;
        }
        if fds[0].revents == 0 {
            continue;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                s.poke_foreground();
                {
                    let mut subs = s.subs.lock();
                    s.term.feed(&buf[..n]);
                    // Encode once, lazily: nobody may be listening, or everyone
                    // may be lagging.
                    let mut frame: Option<Frame> = None;
                    subs.retain(|sub| {
                        if sub.lagging.load(Ordering::Acquire) {
                            sub.skipped.fetch_add(n as u64, Ordering::Relaxed);
                            return true;
                        }
                        let in_flight = sub.in_flight();
                        if in_flight > 0 && in_flight + n as u64 > sub.window {
                            sub.lagging.store(true, Ordering::Release);
                            sub.skipped.fetch_add(n as u64, Ordering::Relaxed);
                            return true;
                        }
                        let f = frame
                            .get_or_insert_with(|| encode(&ServerMsg::Output { session: s.id, data: buf[..n].to_vec() }).expect("encode"))
                            .clone();
                        sub.push(f, n)
                    });
                }
                for ev in s.ui_events.try_iter() {
                    if matches!(ev, UiEvent::CwdChanged)
                        && let Some(dir) = s.term.current_directory()
                    {
                        let proc_cwd = s.pty.lock().cwd();
                        s.set_osc_cwd(dir.to_string_lossy().into_owned(), proc_cwd);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO when the child exits
        }
    }
    s.pause.finished();
}
