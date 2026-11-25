//! One shell session: its PTY, its authoritative terminal, and the
//! connections subscribed to its output (with per-subscriber flow control).

use std::collections::VecDeque;
use std::io::Read;
#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
#[cfg(unix)]
use std::time::{Duration, Instant};

use goatty_proto::{Frame, ReqId, ServerMsg, SessionId, SessionInfo, StreamOpts, TopLineCause, WinSize, encode, encode_server};
use goatty_term::process::ForegroundWatch;
use goatty_term::pty::{Pty, PtyInput, PtyReader};
use goatty_term::{TermSize, Terminal, UiEvent};
#[cfg(unix)]
use parking_lot::Condvar;
use parking_lot::Mutex;

use crate::client::{Outbound, try_acquire_worker};
use crate::flow::FlowControl;
#[cfg(unix)]
use crate::systemd::StoredPty;

/// Max concurrent OSC 7 `canonicalize` worker threads per session (bounds
/// thread leaks if a slow or unavailable mount stalls filesystem resolution).
const MAX_OSC_CWD_WORKERS: usize = 4;

/// Per-subscriber adapter onto the connection's shared [`FlowControl`].
///
/// Tracks this subscriber's own `sent`/`acked` watermarks and unacknowledged
/// zstd-compressed snapshot byte ranges so:
/// * compressed bytes stay attributed to their exact stream positions until
///   acknowledged or released (even across interleaved panes and epochs), and
/// * releasing remaining `in_flight` bytes on session exit or subscriber drop
///   is atomic and idempotent.
struct SubFlow {
    fc: Arc<FlowControl>,
    state: Mutex<SubFlowState>,
}

#[derive(Default)]
struct SubFlowState {
    released: bool,
    sent: u64,
    acked: u64,
    compressed_ranges: VecDeque<(u64, u64)>,
}

impl SubFlow {
    fn new(fc: Arc<FlowControl>) -> Self {
        Self { fc, state: Mutex::new(SubFlowState::default()) }
    }

    fn window(&self) -> u64 {
        self.fc.window()
    }

    fn on_send(&self, stream_len: u64, compressed: bool) {
        let mut st = self.state.lock();
        if st.released {
            return;
        }
        let start = st.sent;
        let end = start.saturating_add(stream_len);
        st.sent = end;
        if compressed {
            st.compressed_ranges.push_back((start, end));
        }
        self.fc.on_send(stream_len);
    }

    fn on_ack(&self, consumed: u64, lagged: bool) -> Option<u32> {
        let mut st = self.state.lock();
        if st.released {
            return None;
        }
        let prev = st.acked;
        let next = consumed.min(st.sent);
        if next <= prev {
            return None;
        }
        let delta = next - prev;
        let mut compressed_delta = 0u64;
        while let Some(&(start, end)) = st.compressed_ranges.front() {
            if end <= prev {
                st.compressed_ranges.pop_front();
                continue;
            }
            if start >= next {
                break;
            }
            let overlap_start = start.max(prev);
            let overlap_end = end.min(next);
            compressed_delta = compressed_delta.saturating_add(overlap_end.saturating_sub(overlap_start));
            if end <= next {
                st.compressed_ranges.pop_front();
            } else {
                break;
            }
        }
        st.acked = next;
        self.fc.on_ack(delta, compressed_delta, lagged)
    }

    fn release(&self) {
        let mut st = self.state.lock();
        if st.released {
            return;
        }
        st.released = true;
        let remaining = st.sent.saturating_sub(st.acked);
        st.compressed_ranges.clear();
        self.fc.on_drop_in_flight(remaining);
    }
}

/// One connection's subscription to one session.
///
/// Flow control: `sent` counts stream bytes queued for this subscriber,
/// `acked` what the client reported as consumed. When `sent - acked` would
/// exceed the active window (`flow`, or `fixed_window` for local sockets), the
/// subscriber goes `lagging`: raw output is skipped (counted in `skipped`)
/// until the client has consumed everything, then it gets one `Resync`
/// snapshot of the current screen and streaming resumes.
pub(crate) struct Subscriber {
    conn: u64,
    session: SessionId,
    tx: flume::Sender<Outbound>,
    fixed_window: u64,
    flow: Option<SubFlow>,
    sent: AtomicU64,
    acked: AtomicU64,
    lagging: AtomicBool,
    skipped: AtomicU64,
    /// The client speaks v11: it gets `TopLine`s.
    lines: bool,
    /// The client speaks v13: it gets `Cwd` updates.
    cwd: bool,
    /// The client speaks v14: it gets `SizeControl` and only sets the size
    /// while it controls the session. Older clients take control with every
    /// `Attach` and `Resize` (they can't watch at another client's size).
    size_ctl: bool,
    /// The size this client last asked for (`Attach`, `Resize`): the
    /// session's size whenever it controls it.
    wanted: Mutex<WinSize>,
    /// Compress snapshots (the connection sent `Compress`).
    pub(crate) zstd: AtomicBool,
}

/// What a connection's subscriptions get besides the stream.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SubFeatures {
    pub(crate) lines: bool,
    pub(crate) zstd: bool,
    pub(crate) cwd: bool,
    pub(crate) size_ctl: bool,
    pub(crate) flow_control: bool,
}

/// Connection-level context passed to [`Session::subscribe`].
#[derive(Clone, Copy)]
pub(crate) struct SubConn<'a> {
    pub(crate) id: u64,
    pub(crate) features: SubFeatures,
    pub(crate) flow: &'a Arc<FlowControl>,
    pub(crate) tx: &'a flume::Sender<Outbound>,
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.release_flow();
    }
}

impl Subscriber {
    fn window(&self) -> u64 {
        self.flow.as_ref().map_or(self.fixed_window, SubFlow::window)
    }

    fn release_flow(&self) {
        if let Some(flow) = &self.flow {
            flow.release();
        }
    }

    fn push(&self, frame: Frame, stream_len: usize) -> bool {
        self.push_stream(frame, stream_len, false)
    }

    fn push_stream(&self, frame: Frame, stream_len: usize, compressed: bool) -> bool {
        if stream_len > 0 {
            // Update `flow` before publishing `sent` so any `Ack` clamped to
            // `sent` always observes the corresponding `on_send`.
            if let Some(flow) = &self.flow {
                flow.on_send(stream_len as u64, compressed);
            }
            self.sent.fetch_add(stream_len as u64, Ordering::AcqRel);
        }
        let ok = self.tx.send(Outbound::session(frame, self.session)).is_ok();
        if !ok {
            self.release_flow();
        }
        ok
    }

    fn in_flight(&self) -> u64 {
        self.sent.load(Ordering::Acquire).saturating_sub(self.acked.load(Ordering::Acquire))
    }

    /// A snapshot message, compressed if the client asked for it.
    fn push_snapshot(&self, msg: &ServerMsg) {
        let zstd = self.zstd.load(Ordering::Relaxed);
        if let Ok(f) = encode_server(msg, zstd) {
            let stream_len = msg.stream_len();
            let compressed = zstd && f.len() < stream_len;
            self.push_stream(f, stream_len, compressed);
        }
    }
}

#[derive(Default)]
pub(crate) struct CwdState {
    /// The session's current working directory (OSC 7 preferred to preserve
    /// symlinks, with `/proc/<pid>/cwd` fallback).
    pub(crate) current: Option<String>,
    /// Last `/proc/<pid>/cwd` reflected in `current` (directly or via the
    /// canonical target of an OSC 7 path), so a subsequent foreground probe
    /// doesn't clobber a symlinked OSC 7 path when `/proc` didn't change.
    pub(crate) last_proc: Option<PathBuf>,
    /// Monotonic sequence assigned to each OSC 7 event on the output pump.
    pub(crate) osc_seq: u64,
    /// Monotonic counter incremented when `last_proc` changes via a `/proc`
    /// probe, so a slow OSC 7 worker cannot overwrite a newer non-OSC `cd`.
    pub(crate) proc_seq: u64,
}

impl CwdState {
    /// Record a `/proc/<pid>/cwd` observation. Returns `Some(new_cwd)` when
    /// `current` changed and subscribers should be notified.
    fn apply_proc(&mut self, proc_cwd: PathBuf) -> Option<Option<String>> {
        if self.last_proc.as_ref() == Some(&proc_cwd) {
            return None;
        }
        self.last_proc = Some(proc_cwd.clone());
        self.proc_seq = self.proc_seq.wrapping_add(1);
        let next = proc_cwd.to_string_lossy().into_owned();
        if self.current.as_deref() == Some(next.as_str()) {
            return None;
        }
        self.current = Some(next.clone());
        Some(Some(next))
    }

    /// Assign a sequence number to an incoming OSC 7 event on the output pump.
    fn begin_osc(&mut self) -> (u64, u64) {
        self.osc_seq = self.osc_seq.wrapping_add(1);
        (self.osc_seq, self.proc_seq)
    }

    /// Record an OSC 7 report together with its canonical physical path if the
    /// `(seq, expected_proc_seq)` snapshot is still current.
    /// Never marks an unrelated `/proc/<pid>/cwd` observed later as already
    /// reflected by this OSC report. Returns `Some(new_cwd)` when `current`
    /// changed and subscribers should be notified.
    fn apply_osc(
        &mut self,
        seq: u64,
        expected_proc_seq: u64,
        osc_cwd: String,
        canonical_target: Option<PathBuf>,
    ) -> Option<Option<String>> {
        if seq != self.osc_seq {
            return None;
        }
        if self.proc_seq != expected_proc_seq && (canonical_target.is_none() || self.last_proc.as_ref() != canonical_target.as_ref()) {
            return None;
        }
        self.last_proc = canonical_target;
        if self.current.as_deref() == Some(osc_cwd.as_str()) {
            return None;
        }
        self.current = Some(osc_cwd.clone());
        Some(Some(osc_cwd))
    }
}

/// Coordinates serialized CWD state (`OSC 7` + `/proc/<pid>/cwd`), subscriber
/// notifications, and off-pump OSC 7 canonicalization workers.
pub(crate) struct CwdCoordinator {
    session_id: SessionId,
    subs: Arc<Mutex<Vec<Arc<Subscriber>>>>,
    state: Mutex<CwdState>,
    workers: Arc<AtomicUsize>,
}

impl CwdCoordinator {
    pub(crate) fn new(
        session_id: SessionId,
        subs: Arc<Mutex<Vec<Arc<Subscriber>>>>,
        current: Option<String>,
        last_proc: Option<PathBuf>,
    ) -> Self {
        Self {
            session_id,
            subs,
            state: Mutex::new(CwdState { current, last_proc, osc_seq: 0, proc_seq: 0 }),
            workers: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn current(&self) -> Option<String> {
        self.state.lock().current.clone()
    }

    fn notify_update(&self, update: impl FnOnce(&mut CwdState) -> Option<Option<String>>) {
        // Lock order matches `subscribe`: `subs` before `state`, so updates
        // and subscriber notifications cannot interleave out of order.
        let subs = self.subs.lock();
        let next = {
            let mut state = self.state.lock();
            update(&mut state)
        };
        let Some(cwd) = next else { return };
        let Ok(frame) = encode(&ServerMsg::Cwd { session: self.session_id, cwd }) else { return };
        for sub in subs.iter() {
            if sub.cwd {
                sub.push(frame.clone(), 0);
            }
        }
    }

    pub(crate) fn apply_proc(&self, proc_cwd: Option<PathBuf>) {
        let Some(path) = proc_cwd else { return };
        self.notify_update(|state| state.apply_proc(path));
    }

    pub(crate) fn apply_osc_async(self: &Arc<Self>, cwd: String) {
        self.apply_osc_with(cwd, |path| std::fs::canonicalize(path).ok());
    }

    pub(crate) fn apply_osc_with(self: &Arc<Self>, cwd: String, canonicalize: impl FnOnce(&str) -> Option<PathBuf> + Send + 'static) {
        let (seq, proc_seq) = {
            let mut state = self.state.lock();
            if self.workers.load(Ordering::Acquire) == 0 && state.last_proc.is_some() && state.current.as_deref() == Some(cwd.as_str()) {
                return;
            }
            state.begin_osc()
        };
        let Some(guard) = try_acquire_worker(&self.workers, MAX_OSC_CWD_WORKERS) else {
            self.notify_update(|state| state.apply_osc(seq, proc_seq, cwd, None));
            return;
        };
        let weak = Arc::downgrade(self);
        let worker_cwd = cwd.clone();
        if let Err(e) = std::thread::Builder::new().name(format!("s{}-cwd", self.session_id)).spawn(move || {
            let _guard = guard;
            let Some(coord) = weak.upgrade() else { return };
            if coord.state.lock().osc_seq != seq {
                return;
            }
            // Release strong ref before blocking filesystem I/O so an exit on
            // a hung mount does not keep session/coordinator state alive.
            drop(coord);
            let canonical = canonicalize(&worker_cwd);
            let Some(coord) = weak.upgrade() else { return };
            coord.notify_update(|state| state.apply_osc(seq, proc_seq, worker_cwd, canonical));
        }) {
            log::warn!("session {}: couldn't spawn cwd worker: {e}", self.session_id);
            self.notify_update(|state| state.apply_osc(seq, proc_seq, cwd, None));
        }
    }
}

pub(crate) struct Session {
    pub(crate) id: SessionId,
    pub(crate) term: Terminal,
    /// Process control (kill, wait, foreground). Never held across I/O.
    pub(crate) pty: Mutex<Pty>,
    /// Input writes, through their own handle on the master: a write blocks
    /// while the program doesn't read, and that mustn't hold up `pty`
    /// (closing the tab of a hung program, #233).
    pub(crate) input: Mutex<PtyInput>,
    /// Held while feeding + broadcasting, so taking a snapshot and
    /// (un)subscribing are atomic with respect to the output stream: no gap,
    /// no overlap. Also acquired before `cwd.state` when notifying CWD changes.
    pub(crate) subs: Arc<Mutex<Vec<Arc<Subscriber>>>>,
    pub(crate) created_unix: u64,
    /// The foreground program, kept current by `fg` (see [`Session::watch_foreground`]).
    pub(crate) process: Mutex<Option<String>>,
    /// Serialized working directory state (`OSC 7` + `/proc/<pid>/cwd`).
    pub(crate) cwd: Arc<CwdCoordinator>,
    pub(crate) fg: OnceLock<ForegroundWatch>,
    /// goatty's managed zsh (it handles reload requests).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) managed: bool,
    /// Lets a restart stop the output pump between two reads (Unix only).
    #[cfg(unix)]
    pub(crate) pause: Pause,
    /// The process and the PTY master/resizer, known without taking a lock.
    pub(crate) pid: Option<u32>,
    #[cfg(unix)]
    pub(crate) master_fd: RawFd,
    #[cfg(windows)]
    pub(crate) resizer: goatty_term::pty::PtyResizer,
    /// Terminal UI events (used to observe OSC 7 `CwdChanged`).
    pub(crate) ui_events: flume::Receiver<UiEvent>,
    /// The terminal's query answers on their way to the PTY: the replies
    /// thread's queue and how many it took and wrote, so a restart can wait
    /// until none is left behind.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) replies: flume::Receiver<Vec<u8>>,
    pub(crate) replies_taken: AtomicU64,
    pub(crate) replies_written: AtomicU64,
    /// The connection that sets the session's size (#12). Guarded by
    /// `subs`' order: only changed while `subs` is held.
    pub(crate) controller: Mutex<Option<u64>>,
    /// The connection that created the session, until it subscribes: it
    /// gets control even when another client attached in between. `None`
    /// for restored sessions. Changed while `subs` is held.
    pub(crate) creator: Mutex<Option<u64>>,
    /// Exit code once the session has finished (`Some(code)`). Read and written
    /// only while `subs` is held so a subscriber racing with `finish` never
    /// misses `ServerMsg::Exited`.
    pub(crate) exited: Mutex<Option<Option<u32>>>,
}

#[cfg(unix)]
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
#[cfg(unix)]
pub(crate) struct Pause {
    requested: AtomicBool,
    /// The pump is parked.
    parked: Mutex<bool>,
    changed: Condvar,
    /// Wakes the pump out of `poll`.
    wake_rx: OwnedFd,
    wake_tx: OwnedFd,
}

#[cfg(unix)]
impl Pause {
    pub(crate) fn new() -> std::io::Result<Self> {
        let (wake_rx, wake_tx) = std::io::pipe()?;
        let (wake_rx, wake_tx) = (OwnedFd::from(wake_rx), OwnedFd::from(wake_tx));
        for fd in [&wake_rx, &wake_tx] {
            // SAFETY: plain fcntl calls on an open fd.
            let ok = unsafe {
                let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                flags >= 0 && libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
            };
            if !ok {
                return Err(std::io::Error::last_os_error());
            }
        }
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
                let name = s.pty.lock().foreground_name();
                let proc_cwd = s.proc_cwd();
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

    fn proc_cwd(&self) -> Option<PathBuf> {
        goatty_term::process::cwd(self.pid?)
    }

    pub(crate) fn current_cwd(&self) -> Option<String> {
        self.cwd.current().or_else(|| self.proc_cwd().map(|p| p.to_string_lossy().into_owned()))
    }

    fn maybe_update_proc_cwd(&self, proc_cwd: Option<PathBuf>) {
        self.cwd.apply_proc(proc_cwd);
    }

    pub(crate) fn set_osc_cwd(&self, cwd: String) {
        self.cwd.apply_osc_async(cwd);
    }

    pub(crate) fn info(&self) -> SessionInfo {
        let (cols, rows) = self.term.size();
        let tracked_cwd = self.cwd.current();
        let proc_cwd = self.proc_cwd();
        SessionInfo {
            id: self.id,
            pid: self.pid,
            title: self.term.title(),
            cwd: tracked_cwd.or_else(|| proc_cwd.map(|p| p.to_string_lossy().into_owned())),
            cols,
            rows,
            attached: self.subs.lock().len() as u32,
            created_unix: self.created_unix,
            process: self.process.lock().clone(),
        }
    }

    /// Connection `from` asks for size `ws`. It's recorded as what `from`
    /// wants; the session takes it when `from` controls the session (a
    /// client older than v14 takes control here). Rows reflow, so
    /// subscribers that number lines hear the new top line: all of them
    /// when the size changed, else just `from`. Lagging ones don't: they lack
    /// output the line counts, and their `Resync` brings the line.
    pub(crate) fn resize(&self, ws: WinSize, from: u64) {
        // The reflow and its top line are one point in the stream, like a
        // snapshot and its top line in `subscribe`/`resync`.
        let subs = self.subs.lock();
        let Some(sub) = subs.iter().find(|s| s.conn == from) else { return };
        *sub.wanted.lock() = ws;
        let mut controller = self.controller.lock();
        let took = *controller != Some(from) && !sub.size_ctl;
        if took {
            *controller = Some(from);
        }
        if *controller != Some(from) {
            return; // watching at someone else's size
        }
        drop(controller);
        let changed = self.apply_size(ws);
        if changed || took {
            self.push_size_control(&subs, None);
        }
        self.push_resized(&subs, changed, Some(from));
    }

    /// Connection `conn` takes control of the session's size (`TakeSize`):
    /// the session takes the size it last asked for.
    pub(crate) fn take_size(&self, conn: u64) {
        let subs = self.subs.lock();
        let Some(sub) = subs.iter().find(|s| s.conn == conn) else { return };
        if self.creator.lock().is_some_and(|c| c != conn) {
            return; // the creator gets it first (`subscribe`)
        }
        let ws = *sub.wanted.lock();
        let mut controller = self.controller.lock();
        if *controller == Some(conn) {
            return;
        }
        *controller = Some(conn);
        drop(controller);
        let changed = self.apply_size(ws);
        self.push_size_control(&subs, None);
        self.push_resized(&subs, changed, Some(conn));
    }

    /// Resize the PTY and the authoritative terminal to `ws`. Caller holds
    /// `subs`, so (when the ioctl succeeds) the PTY has the size before any
    /// client hears of it, and sizes apply in the order they were decided.
    /// True when the size changed.
    fn apply_size(&self, ws: WinSize) -> bool {
        let size = to_size(ws);
        #[cfg(unix)]
        {
            // SAFETY: `master_fd` is open as long as `pty`, which `self` owns.
            let master = unsafe { BorrowedFd::borrow_raw(self.master_fd) };
            if let Err(e) = goatty_term::pty::set_window_size(master, size) {
                log::warn!("session {}: resize failed: {e}", self.id);
            }
        }
        #[cfg(windows)]
        {
            if let Err(e) = self.resizer.resize(size) {
                log::warn!("session {}: resize failed: {e}", self.id);
            }
        }
        let changed = self.term.size() != (size.cols, size.rows);
        if changed {
            self.term.resize(size);
        }
        changed
    }

    /// Tell v14 subscribers (all, or just `only`) the session's size and
    /// whether they control it. Caller holds `subs`.
    fn push_size_control(&self, subs: &[Arc<Subscriber>], only: Option<u64>) {
        let controller = *self.controller.lock();
        let (cols, rows) = self.term.size();
        let size = WinSize { cols, rows, cell_width: 0, cell_height: 0 };
        let frame = |controlling| encode(&ServerMsg::SizeControl { session: self.id, size, controlling }).ok();
        let (mut yes, mut no) = (None, None);
        for sub in subs.iter().filter(|s| s.size_ctl && only.is_none_or(|c| c == s.conn)) {
            let controlling = controller == Some(sub.conn);
            let slot = if controlling { &mut yes } else { &mut no };
            if let Some(f) = slot.get_or_insert_with(|| frame(controlling)) {
                sub.push(f.clone(), 0);
            }
        }
    }

    /// The controller left: hand control to the newest remaining subscriber,
    /// which brings its size. Caller holds `subs`.
    fn hand_over(&self, subs: &[Arc<Subscriber>], gone: u64) {
        let mut controller = self.controller.lock();
        if controller.is_none_or(|c| c != gone && subs.iter().any(|s| s.conn == c)) {
            return;
        }
        let next = subs.last();
        *controller = next.map(|s| s.conn);
        drop(controller);
        let Some(next) = next else { return };
        let ws = *next.wanted.lock();
        let changed = self.apply_size(ws);
        self.push_size_control(subs, None);
        self.push_resized(subs, changed, None);
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

    /// Snapshot + subscribe under one lock, asking for size `size`. The
    /// subscriber takes control of the session's size when nobody has it
    /// (or it had it already, or it predates v14); then the snapshot already
    /// has its geometry (reflowed by the VT core). Returns the subscriber.
    pub(crate) fn subscribe(&self, sub_conn: SubConn<'_>, req: ReqId, size: WinSize, opts: StreamOpts) -> Arc<Subscriber> {
        let SubConn { id: conn, features, flow: conn_flow, tx } = sub_conn;
        let mut subs = self.subs.lock();
        subs.retain(|s| {
            if s.conn == conn {
                s.release_flow();
                false
            } else {
                true
            }
        }); // re-attach on the same connection replaces
        {
            let mut controller = self.controller.lock();
            // The connection that created the session gets it, even if
            // someone else attached before its own subscription.
            let mut creator = self.creator.lock();
            let reserved = creator.is_some_and(|c| c != conn);
            if *creator == Some(conn) {
                *creator = None;
            }
            drop(creator);
            let free = controller.is_none_or(|c| c == conn || !subs.iter().any(|s| s.conn == c));
            if (free && !reserved) || !features.size_ctl {
                let took = *controller != Some(conn);
                *controller = Some(conn);
                drop(controller);
                let changed = self.apply_size(size);
                if took || changed {
                    // The others watch at the new size from here on.
                    self.push_size_control(&subs, None);
                }
                self.push_resized(&subs, changed, None);
            }
        }
        // Local links (`StreamOpts::LOCAL`, 8 MiB) stay fixed lossless with
        // zero per-chunk overhead. Smaller (remote) windows share the
        // connection's adaptive `FlowControl`.
        let (fixed_window, flow) = if opts.window >= StreamOpts::MAX_WINDOW {
            (u64::from(opts.window), None)
        } else {
            let initial = u64::from(opts.window.max(StreamOpts::MIN_WINDOW));
            let min_w = if features.flow_control {
                u64::from(StreamOpts::MIN_WINDOW)
            } else {
                // Pre-v15 peers never receive `FlowWindow` and ack every
                // `REMOTE.window / 4`, so never let their floor drop below 256 KiB.
                initial.max(u64::from(StreamOpts::REMOTE.window))
            };
            conn_flow.init_if_needed(initial, min_w, features.flow_control);
            (initial, Some(SubFlow::new(conn_flow.clone())))
        };
        let sub = Arc::new(Subscriber {
            conn,
            session: self.id,
            tx: tx.clone(),
            fixed_window,
            flow,
            sent: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            lagging: AtomicBool::new(false),
            skipped: AtomicU64::new(0),
            lines: features.lines,
            cwd: features.cwd,
            size_ctl: features.size_ctl,
            wanted: Mutex::new(size),
            zstd: AtomicBool::new(features.zstd),
        });
        // Before the snapshot: a watching client sizes its terminal first, so
        // the snapshot lands in a grid of the session's size.
        self.push_size_control(std::slice::from_ref(&sub), None);
        let snapshot = self.term.reattach_snapshot_with_history(opts.history as usize);
        sub.push_snapshot(&ServerMsg::Attached { req, session: self.id, snapshot });
        self.push_top_line(&sub, TopLineCause::Attach);
        let name = self.process.lock().clone();
        if name.is_some()
            && let Ok(f) = encode(&ServerMsg::Process { session: self.id, name })
        {
            sub.push(f, 0);
        }
        if sub.cwd {
            let cwd = self.cwd.current();
            if cwd.is_some()
                && let Ok(f) = encode(&ServerMsg::Cwd { session: self.id, cwd })
            {
                sub.push(f, 0);
            }
        }
        if let Some(code) = *self.exited.lock() {
            if let Ok(f) = encode(&ServerMsg::Exited { session: self.id, code }) {
                let _ = sub.tx.send(Outbound::session(f, self.id));
            }
            sub.release_flow();
        } else {
            subs.push(sub.clone());
        }
        sub
    }

    pub(crate) fn unsubscribe(&self, conn: u64) {
        let mut subs = self.subs.lock();
        subs.retain(|s| {
            if s.conn == conn {
                s.release_flow();
                false
            } else {
                true
            }
        });
        let mut creator = self.creator.lock();
        if *creator == Some(conn) {
            *creator = None;
        }
        drop(creator);
        self.hand_over(&subs, conn);
    }

    /// Client acknowledged `consumed` bytes. If it was lagging and has now
    /// caught up completely, send the current screen and resume streaming.
    /// Returns a new flow-control window to advertise via `ServerMsg::FlowWindow`
    /// when the connection's adaptive window changed materially.
    pub(crate) fn ack(&self, sub: &Subscriber, consumed: u64) -> Option<u32> {
        let sent = sub.sent.load(Ordering::Acquire);
        let clamped = consumed.min(sent);
        let prev = sub.acked.fetch_max(clamped, Ordering::AcqRel);
        let delta = clamped.saturating_sub(prev);
        let mut lagged = sub.lagging.load(Ordering::Acquire);
        if sub.in_flight() == 0 {
            let subs = self.subs.lock(); // order against the output pump
            // Recheck under `subs`: the pump may have read pre-Ack `in_flight`
            // and set `lagging` right as this Ack emptied the pane.
            let should_resync = sub.lagging.load(Ordering::Acquire) && sub.in_flight() == 0;
            lagged |= should_resync;
            let notify = if delta > 0
                && let Some(flow) = &sub.flow
            {
                flow.on_ack(clamped, lagged)
            } else {
                None
            };
            if should_resync && subs.iter().any(|s| std::ptr::eq(Arc::as_ptr(s), sub)) {
                self.resync(sub);
            }
            return notify;
        }
        if delta > 0
            && let Some(flow) = &sub.flow
        {
            flow.on_ack(clamped, lagged)
        } else {
            None
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
        let mut subs = self.subs.lock();
        *self.exited.lock() = Some(code);
        for sub in subs.drain(..) {
            // Let lagging clients see the final screen before the end.
            if sub.lagging.load(Ordering::Acquire) {
                self.resync(&sub);
            }
            if let Some(f) = &exited {
                let _ = sub.tx.send(Outbound::session(f.clone(), self.id));
            }
            // Debit any unacknowledged bytes immediately; `client.rs` may keep
            // `Arc<Subscriber>` until `Detach` or disconnect, and `Drop` skips
            // already-released subscribers.
            sub.release_flow();
        }
    }
}

pub(crate) fn to_size(ws: WinSize) -> TermSize {
    TermSize { cols: ws.cols.max(2), rows: ws.rows.max(1), cell_width: ws.cell_width, cell_height: ws.cell_height }
}

fn dispatch_output_chunk(s: &Session, chunk: &[u8]) {
    let n = chunk.len();
    s.poke_foreground();
    {
        let mut subs = s.subs.lock();
        s.term.feed(chunk);
        // Encode once, lazily: nobody may be listening, or everyone
        // may be lagging.
        let mut frame: Option<Frame> = None;
        subs.retain(|sub| {
            if sub.lagging.load(Ordering::Acquire) {
                sub.skipped.fetch_add(n as u64, Ordering::Relaxed);
                return true;
            }
            let in_flight = sub.in_flight();
            if in_flight > 0 && in_flight + n as u64 > sub.window() {
                sub.lagging.store(true, Ordering::Release);
                sub.skipped.fetch_add(n as u64, Ordering::Relaxed);
                return true;
            }
            let f =
                frame.get_or_insert_with(|| encode(&ServerMsg::Output { session: s.id, data: chunk.to_vec() }).expect("encode")).clone();
            sub.push(f, n)
        });
    }
    let mut cwd_changed = false;
    for ev in s.ui_events.try_iter() {
        if matches!(ev, UiEvent::CwdChanged) {
            cwd_changed = true;
        }
    }
    if cwd_changed && let Some(dir) = s.term.current_directory() {
        s.set_osc_cwd(dir.to_string_lossy().into_owned());
    }
}

/// Read PTY output until EOF, feeding the authoritative terminal and every
/// subscriber. Runs on the session's reader thread. On Unix, between two reads
/// it can be paused ([`Pause`]).
#[cfg(unix)]
pub(crate) fn pump_output(s: &Session, mut reader: PtyReader) {
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
            Ok(n) => dispatch_output_chunk(s, &buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO when the child exits
        }
    }
    s.pause.finished();
}

/// Read ConPTY output until EOF on Windows (direct blocking `reader.read()`
/// loop; Windows has no in-place restart / `Pause`).
#[cfg(windows)]
pub(crate) fn pump_output(s: &Session, mut reader: PtyReader) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => dispatch_output_chunk(s, &buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn test_coord() -> (Arc<CwdCoordinator>, flume::Receiver<Outbound>) {
        let (tx, rx) = flume::unbounded();
        let sub = Arc::new(Subscriber {
            conn: 1,
            session: 1,
            tx,
            fixed_window: 64 << 10,
            flow: None,
            sent: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            lagging: AtomicBool::new(false),
            skipped: AtomicU64::new(0),
            lines: false,
            cwd: true,
            size_ctl: false,
            wanted: Mutex::new(WinSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 }),
            zstd: AtomicBool::new(false),
        });
        let subs = Arc::new(Mutex::new(vec![sub]));
        let coord = Arc::new(CwdCoordinator::new(1, subs, None, None));
        (coord, rx)
    }

    fn drain_cwds(rx: &flume::Receiver<Outbound>) -> Vec<Option<String>> {
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            let msg: ServerMsg = goatty_proto::read_msg(&mut std::io::Cursor::new(&item.frame)).unwrap();
            if let ServerMsg::Cwd { cwd, .. } = msg {
                out.push(cwd);
            }
        }
        out
    }

    fn wait_workers_idle(coord: &CwdCoordinator) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while coord.workers.load(Ordering::Acquire) > 0 {
            assert!(std::time::Instant::now() < deadline, "cwd workers did not finish");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn cwd_state_preserves_symlinks_without_masking_later_proc_changes() {
        let mut state = CwdState::default();

        // Symlinked OSC 7 directory records its canonical target so a matching
        // `/proc/<pid>/cwd` probe does not clobber the logical symlink path.
        let (seq, proc_seq) = state.begin_osc();
        let update = state.apply_osc(seq, proc_seq, "/home/u/sym".into(), Some(PathBuf::from("/srv/real")));
        assert_eq!(update, Some(Some("/home/u/sym".into())));
        assert_eq!(state.apply_proc(PathBuf::from("/srv/real")), None);
        assert_eq!(state.current.as_deref(), Some("/home/u/sym"));

        // If the shell later changes to `/srv/other` before/after an earlier OSC 7
        // for `/srv/a` is processed, `/srv/other` is not masked as already reflected.
        let (seq, proc_seq) = state.begin_osc();
        let update = state.apply_osc(seq, proc_seq, "/srv/a".into(), Some(PathBuf::from("/srv/a")));
        assert_eq!(update, Some(Some("/srv/a".into())));
        assert_eq!(state.apply_proc(PathBuf::from("/srv/other")), Some(Some("/srv/other".into())));
        assert_eq!(state.current.as_deref(), Some("/srv/other"));

        // When canonicalization fails (`None`), subsequent `/proc` probes still apply.
        let (seq, proc_seq) = state.begin_osc();
        state.apply_osc(seq, proc_seq, "/missing/dir".into(), None);
        assert_eq!(state.apply_proc(PathBuf::from("/srv/other")), Some(Some("/srv/other".into())));
    }

    #[test]
    fn osc_cwd_worker_does_not_block_pump_and_rejects_stale_completions() {
        let (coord, rx) = test_coord();
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        // 1. Slow mount canonicalize blocks on `release`, but `apply_osc_with` returns immediately.
        let (s1, r1) = (started.clone(), release.clone());
        coord.apply_osc_with("/mnt/slow/sym".into(), move |_| {
            s1.wait();
            r1.wait();
            Some(PathBuf::from("/mnt/slow/real"))
        });
        started.wait();
        assert_eq!(coord.current(), None);

        // 2. Newer fast OSC 7 event arrives and completes immediately.
        coord.apply_osc_with("/tmp/fast".into(), |_| Some(PathBuf::from("/tmp/fast")));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while coord.current().as_deref() != Some("/tmp/fast") {
            assert!(std::time::Instant::now() < deadline, "fast cwd update did not apply");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // 3. Release the slow worker: its stale completion must be discarded.
        release.wait();
        wait_workers_idle(&coord);
        assert_eq!(coord.current().as_deref(), Some("/tmp/fast"));
        assert_eq!(drain_cwds(&rx), vec![Some("/tmp/fast".into())]);
    }

    #[test]
    fn osc_cwd_worker_interaction_with_proc_probes() {
        let (coord, _rx) = test_coord();
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        // A blocked OSC 7 worker must not overwrite a subsequent non-OSC `/proc` change.
        let (s1, r1) = (started.clone(), release.clone());
        coord.apply_osc_with("/mnt/slow/sym".into(), move |_| {
            s1.wait();
            r1.wait();
            Some(PathBuf::from("/mnt/slow/real"))
        });
        started.wait();
        coord.apply_proc(Some(PathBuf::from("/srv/other")));
        assert_eq!(coord.current().as_deref(), Some("/srv/other"));
        release.wait();
        wait_workers_idle(&coord);
        assert_eq!(coord.current().as_deref(), Some("/srv/other"));

        // If `/proc` moved to the *canonical target* of the symlink while canonicalize
        // was in flight, the completing OSC 7 worker preserves the symlink path.
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (s2, r2) = (started.clone(), release.clone());
        coord.apply_osc_with("/home/u/sym".into(), move |_| {
            s2.wait();
            r2.wait();
            Some(PathBuf::from("/srv/real"))
        });
        started.wait();
        coord.apply_proc(Some(PathBuf::from("/srv/real")));
        assert_eq!(coord.current().as_deref(), Some("/srv/real"));
        release.wait();
        wait_workers_idle(&coord);
        assert_eq!(coord.current().as_deref(), Some("/home/u/sym"));
    }

    #[test]
    fn osc_cwd_worker_limit_falls_back_without_blocking() {
        let (coord, _rx) = test_coord();
        let release = Arc::new(Barrier::new(MAX_OSC_CWD_WORKERS + 1));

        for i in 0..MAX_OSC_CWD_WORKERS {
            let started = Arc::new(Barrier::new(2));
            let (s, r) = (started.clone(), release.clone());
            coord.apply_osc_with(format!("/mnt/hung/{i}"), move |_| {
                s.wait();
                r.wait();
                Some(PathBuf::from(format!("/mnt/hung/real-{i}")))
            });
            started.wait();
        }
        assert_eq!(coord.workers.load(Ordering::Acquire), MAX_OSC_CWD_WORKERS);

        // Next OSC 7 event must fall back immediately without blocking or spawning a 5th worker,
        // and once hung workers unblock they must all be rejected as stale.
        coord.apply_osc_with("/tmp/fallback".into(), |_| unreachable!("must not spawn above limit"));
        assert_eq!(coord.current().as_deref(), Some("/tmp/fallback"));

        release.wait();
        wait_workers_idle(&coord);
        assert_eq!(coord.current().as_deref(), Some("/tmp/fallback"));
    }

    #[test]
    fn sub_flow_release_is_idempotent_and_tracks_compressed_ranges() {
        let fc = Arc::new(FlowControl::new());
        fc.init_if_needed(u64::from(StreamOpts::REMOTE.window), u64::from(StreamOpts::MIN_WINDOW), true);

        let sub_a = SubFlow::new(fc.clone());
        let sub_b = SubFlow::new(fc.clone());

        // Sub A sends 64 KiB plain + 128 KiB compressed + 64 KiB plain;
        // Sub B sends 32 KiB plain.
        sub_a.on_send(64 << 10, false);
        sub_a.on_send(128 << 10, true);
        sub_a.on_send(64 << 10, false);
        sub_b.on_send(32 << 10, false);
        assert_eq!(fc.in_flight(), 288 << 10);

        // Partial Ack on Sub A spans plain + half of compressed range.
        sub_a.on_ack(128 << 10, false);
        assert_eq!(fc.in_flight(), 160 << 10);

        // Session::finish releases Sub A's remaining 128 KiB immediately,
        // without touching Sub B's 32 KiB.
        sub_a.release();
        assert_eq!(fc.in_flight(), 32 << 10);

        // Subsequent Drop release and late Ack on Sub A are both no-ops.
        sub_a.release();
        assert_eq!(sub_a.on_ack(256 << 10, false), None);
        assert_eq!(fc.in_flight(), 32 << 10);

        // Releasing Sub B drains the rest cleanly.
        sub_b.release();
        assert_eq!(fc.in_flight(), 0);
    }
}
