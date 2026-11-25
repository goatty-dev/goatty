//! PTYs: on Unix, `portable-pty` opens them and starts the child, after which
//! goatty works on the master fd itself (enabling fd handoff across `execve`
//! and systemd's fd store via [`Pty::adopt`]). On Windows, uses Win32 ConPTY
//! directly (`CreatePseudoConsole` + `CreateProcessW(CREATE_SUSPENDED)` +
//! `AssignProcessToJobObject` before `ResumeThread`) so the entire process tree
//! is assigned to a Job Object from instruction zero and `ClosePseudoConsole`
//! can run concurrently with output draining when the child exits.

use std::path::PathBuf;

#[cfg(unix)]
use portable_pty::PtySize;

use crate::TermSize;

/// The terminal (`/dev/pts/N`) of a goatty pane, set in its shell's
/// environment. goatty's managed zsh compares it with `$TTY` (and
/// `TERM_PROGRAM`) to tell whether it runs in goatty or elsewhere.
pub const TTY_ENV: &str = "GOATTY_TTY";

#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

fn default_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(p));
        }
        if let (Some(drive), Some(path)) =
            (std::env::var_os("HOMEDRIVE").filter(|v| !v.is_empty()), std::env::var_os("HOMEPATH").filter(|v| !v.is_empty()))
        {
            let mut combined = drive;
            combined.push(path);
            return Some(PathBuf::from(combined));
        }
    }
    std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

impl ShellCommand {
    /// `program args` with extra `env`, started in the user's home directory.
    /// Which shell to run is `goatty-shell`'s decision (`Launcher::launch`).
    pub fn new(program: String, args: Vec<String>, env: Vec<(String, String)>) -> Self {
        Self { program, args, cwd: default_home_dir(), env }
    }
}

#[cfg(unix)]
fn pty_size(s: TermSize) -> PtySize {
    PtySize {
        rows: s.rows.max(1),
        cols: s.cols.max(1),
        pixel_width: s.cols.saturating_mul(s.cell_width),
        pixel_height: s.rows.saturating_mul(s.cell_height),
    }
}

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use portable_pty::{CommandBuilder, native_pty_system};

    use super::{ShellCommand, TTY_ENV, pty_size};
    use crate::TermSize;

    /// Output reader returned by [`Pty::spawn`].
    pub type PtyReader = File;

    /// The process on the other side of the PTY.
    enum Process {
        /// Started by this process image.
        Spawned(Box<dyn portable_pty::Child + Send + Sync>),
        /// Taken over ([`Pty::adopt`]): still our child after a re-exec (exit
        /// codes come from `waitpid`), or somebody else's after systemd handed
        /// us the PTY (no exit code). `pidfd` pins the process for signals.
        Adopted { pid: u32, pidfd: Option<OwnedFd>, exited: Option<u32> },
    }

    /// Turn off XON/XOFF output flow control (`stty -ixon`) on a new PTY.
    /// Otherwise Ctrl+S stops all output until Ctrl+Q, which looks like a hung
    /// terminal (#192). Nothing needs it on a local PTY, and it frees Ctrl+S
    /// for the shell (zsh's forward history search) and other programs. Setting
    /// it on the master changes the pair's shared termios. Best effort: if it
    /// fails, the PTY keeps its defaults.
    fn disable_flow_control(fd: RawFd) {
        let mut t = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `fd` is the PTY master, open for the duration of the call,
        // and `t` is a valid pointer to termios-sized storage.
        if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } != 0 {
            log::warn!("tcgetattr on a new pty: {}", std::io::Error::last_os_error());
            return;
        }
        // SAFETY: tcgetattr succeeded, so it filled in `t`.
        let mut t = unsafe { t.assume_init() };
        t.c_iflag &= !libc::IXON;
        // SAFETY: as above; `t` is an initialized termios.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
            log::warn!("tcsetattr on a new pty: {}", std::io::Error::last_os_error());
        }
    }

    /// A running child process attached to a PTY.
    pub struct Pty {
        master: OwnedFd,
        process: Process,
    }

    /// Input to a PTY: its own handle on the master (see [`Pty::input`]).
    pub struct PtyInput(File);

    impl PtyInput {
        /// Blocks while the PTY's input queue is full (the program doesn't read).
        pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.0.write_all(bytes)?;
            self.0.flush()
        }
    }

    /// Duplicate `fd` with `FD_CLOEXEC` set.
    fn dup(fd: BorrowedFd<'_>) -> std::io::Result<OwnedFd> {
        fd.try_clone_to_owned()
    }

    impl Pty {
        /// Spawn `cmd`. Returns the PTY plus a blocking reader for its output,
        /// which the caller should drive from a dedicated thread.
        pub fn spawn(cmd: &ShellCommand, size: TermSize) -> Result<(Self, PtyReader)> {
            let sys = native_pty_system();
            let pair = sys.openpty(pty_size(size)).context("openpty")?;
            if let Some(fd) = pair.master.as_raw_fd() {
                disable_flow_control(fd);
            }
            let mut b = CommandBuilder::new(&cmd.program);
            b.args(&cmd.args);
            if let Some(cwd) = &cmd.cwd {
                b.cwd(cwd);
            }
            b.env("TERM", "xterm-256color");
            b.env("COLORTERM", "truecolor");
            b.env("TERM_PROGRAM", "goatty");
            b.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
            // The pane's own terminal: goatty's shell sends goatty-only escape
            // codes only while `$TTY` is this one. Programs it starts inherit
            // the variable, but their shells (an IDE's, tmux panes) run on
            // other terminals.
            if let Some(tty) = pair.master.tty_name() {
                b.env(TTY_ENV, tty);
            }
            for (k, v) in &cmd.env {
                b.env(k, v);
            }
            let child = pair.slave.spawn_command(b).with_context(|| format!("spawn {}", cmd.program))?;
            drop(pair.slave);
            let raw = pair.master.as_raw_fd().context("the PTY has no fd")?;
            // SAFETY: `raw` is the master's fd, open for as long as `pair.master`
            // lives; it's only borrowed to duplicate it.
            let master = dup(unsafe { BorrowedFd::borrow_raw(raw) })?;
            // portable-pty's master closes its own fd on drop; ours stays.
            drop(pair.master);
            Self::from_parts(master, Process::Spawned(child))
        }

        /// Take over a PTY whose master is `master` and whose shell (the
        /// terminal's session leader) is `pid`. The fd is set close-on-exec.
        pub fn adopt(master: OwnedFd, pid: u32) -> Result<(Self, PtyReader)> {
            set_cloexec(master.as_raw_fd(), true)?;
            Self::from_parts(master, Process::Adopted { pid, pidfd: pidfd_open(pid), exited: None })
        }

        fn from_parts(master: OwnedFd, process: Process) -> Result<(Self, PtyReader)> {
            let reader = File::from(dup(master.as_fd())?);
            Ok((Self { master, process }, reader))
        }

        /// The master side, e.g. to pass it on to the next goattyd.
        pub fn master_fd(&self) -> BorrowedFd<'_> {
            self.master.as_fd()
        }

        /// A handle for writing input, separate from the `Pty`: a write that
        /// blocks (the program stopped reading) must not hold up [`Pty::kill`]
        /// or the other calls behind whatever lock guards the `Pty` (#233).
        pub fn input(&self) -> std::io::Result<PtyInput> {
            Ok(PtyInput(File::from(dup(self.master.as_fd())?)))
        }

        pub fn resize(&self, size: TermSize) -> Result<()> {
            set_window_size(self.master.as_fd(), size)
        }

        pub fn pid(&self) -> Option<u32> {
            match &self.process {
                Process::Spawned(c) => c.process_id(),
                Process::Adopted { pid, .. } => Some(*pid),
            }
        }

        /// The exit code once the child exited. `None` while it runs, and always
        /// for an adopted process that isn't our child.
        pub fn try_wait(&mut self) -> Option<u32> {
            match &mut self.process {
                Process::Spawned(c) => c.try_wait().ok().flatten().map(|s| s.exit_code()),
                Process::Adopted { exited: Some(code), .. } => Some(*code),
                Process::Adopted { pid, exited, .. } => {
                    let mut status = 0;
                    // SAFETY: plain syscall; for a process that isn't our child it
                    // fails with ECHILD and changes nothing.
                    let r = unsafe { libc::waitpid(*pid as libc::pid_t, &raw mut status, libc::WNOHANG) };
                    *exited = (r > 0).then(|| match libc::WIFEXITED(status) {
                        true => libc::WEXITSTATUS(status) as u32,
                        false => 128 + libc::WTERMSIG(status) as u32,
                    });
                    *exited
                }
            }
        }

        /// End the child: SIGHUP, like a closed terminal, and SIGKILL when it's
        /// still there shortly after.
        pub fn kill(&mut self) {
            match &mut self.process {
                Process::Spawned(c) => {
                    let _ = c.kill();
                }
                Process::Adopted { pid, pidfd, .. } => {
                    let (pid, pidfd) = (*pid, pidfd.as_ref().map(|f| f.as_raw_fd()));
                    if !signal(pid, pidfd, libc::SIGHUP) {
                        return;
                    }
                    let deadline = Instant::now() + Duration::from_millis(250);
                    while Instant::now() < deadline {
                        if self.try_wait().is_some() || !alive(pid, pidfd) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    signal(pid, pidfd, libc::SIGKILL);
                }
            }
        }

        /// The foreground process group's leader (the shell at its prompt, the
        /// running program otherwise).
        pub fn foreground_pid(&self) -> Option<u32> {
            // SAFETY: plain syscall on an fd we own.
            let pgrp = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
            u32::try_from(pgrp).ok().filter(|&p| p > 0)
        }

        /// Name of the foreground program (see [`crate::process`]).
        pub fn foreground_name(&self) -> Option<String> {
            crate::process::process_name(self.foreground_pid()?)
        }

        /// Foreground process group's working directory is a later feature;
        /// for now report the shell's cwd.
        pub fn cwd(&self) -> Option<PathBuf> {
            crate::process::cwd(self.pid()?)
        }
    }

    /// Send `sig` through the pidfd when there is one (no pid reuse), else to
    /// the pid. False when the process is gone.
    #[cfg(target_os = "linux")]
    fn pidfd_open(pid: u32) -> Option<OwnedFd> {
        use std::os::fd::FromRawFd;
        // SAFETY: plain syscall; a failure (old kernel, gone process) is handled.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        // SAFETY: on success the syscall returned a new fd that nothing else owns.
        (raw >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw as RawFd) })
    }

    /// Other systems have no pidfds: signals go to the pid.
    #[cfg(not(target_os = "linux"))]
    fn pidfd_open(_pid: u32) -> Option<OwnedFd> {
        None
    }

    fn signal(pid: u32, pidfd: Option<RawFd>, sig: libc::c_int) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(fd) = pidfd {
            // SAFETY: plain syscall; a null siginfo is allowed for pidfd_send_signal.
            return unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd, sig, std::ptr::null::<libc::siginfo_t>(), 0) } == 0;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = pidfd;
        // SAFETY: plain syscall.
        unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
    }

    fn alive(pid: u32, pidfd: Option<RawFd>) -> bool {
        signal(pid, pidfd, 0)
    }

    /// Set or clear `FD_CLOEXEC` on `fd`.
    pub fn set_cloexec(fd: RawFd, on: bool) -> std::io::Result<()> {
        // SAFETY: plain fcntl calls; an invalid fd fails with EBADF.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
            if libc::fcntl(fd, libc::F_SETFD, flags) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// The session leader of the terminal whose master is `master`: the shell
    /// goatty started in it (`tcgetsid`, the `TIOCGSID` ioctl on Linux).
    pub fn session_leader(master: BorrowedFd<'_>) -> Option<u32> {
        // SAFETY: plain call on an open fd; a failure returns -1.
        let sid = unsafe { libc::tcgetsid(master.as_raw_fd()) };
        (sid > 0).then_some(sid as u32)
    }

    /// Give the terminal behind `master` the size `size` (`TIOCSWINSZ`). Needs
    /// no `Pty`, so no lock that a blocked input write could hold.
    pub fn set_window_size(master: BorrowedFd<'_>, size: TermSize) -> Result<()> {
        let s = pty_size(size);
        let ws = libc::winsize { ws_row: s.rows, ws_col: s.cols, ws_xpixel: s.pixel_width, ws_ypixel: s.pixel_height };
        // SAFETY: `ws` is a valid winsize and the fd is open.
        if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &raw const ws) } != 0 {
            return Err(std::io::Error::last_os_error()).context("TIOCSWINSZ");
        }
        Ok(())
    }

    /// The size of the terminal behind `master` (`TIOCGWINSZ`), cell pixels
    /// included when they were set.
    pub fn window_size(master: BorrowedFd<'_>) -> Option<TermSize> {
        let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: `ws` is valid for writes and the fd is open.
        let r = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGWINSZ, &raw mut ws) };
        (r == 0 && ws.ws_col > 0 && ws.ws_row > 0).then(|| TermSize {
            cols: ws.ws_col,
            rows: ws.ws_row,
            cell_width: ws.ws_xpixel / ws.ws_col,
            cell_height: ws.ws_ypixel / ws.ws_row,
        })
    }
}

#[cfg(unix)]
pub use unix::{Pty, PtyInput, PtyReader, session_leader, set_cloexec, set_window_size, window_size};

#[cfg(windows)]
mod windows {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::{Context, Result, bail};
    use parking_lot::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
        PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };

    use super::ShellCommand;
    use crate::TermSize;

    /// Output reader returned by [`Pty::spawn`].
    pub type PtyReader = Box<dyn Read + Send>;

    fn to_wide_nul(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn conpty_coord(s: TermSize) -> COORD {
        COORD { X: s.cols.clamp(2, i16::MAX as u16) as i16, Y: s.rows.clamp(1, i16::MAX as u16) as i16 }
    }

    /// Quote a single argument according to Windows `CommandLineToArgvW` rules.
    fn quote_windows_arg(arg: &str) -> String {
        if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '"')) {
            return arg.to_string();
        }
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('"');
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    for _ in 0..(backslashes * 2 + 1) {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push('"');
                }
                c => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push(c);
                }
            }
        }
        for _ in 0..(backslashes * 2) {
            out.push('\\');
        }
        out.push('"');
        out
    }

    fn build_cmdline(program: &str, args: &[String]) -> Vec<u16> {
        let mut line = quote_windows_arg(program);
        for arg in args {
            line.push(' ');
            line.push_str(&quote_windows_arg(arg));
        }
        to_wide_nul(OsStr::new(&line))
    }

    /// Build a sorted UTF-16 `CREATE_UNICODE_ENVIRONMENT` block inheriting the
    /// current process environment and overlaying terminal defaults + `extra`.
    fn build_env_block(extra: &[(String, String)]) -> Vec<u16> {
        // Key by ASCII-lowercased name so Windows case-insensitive keys like
        // `Path`/`PATH` don't duplicate, and `BTreeMap` produces sorted order.
        let mut map: BTreeMap<String, (String, String)> = BTreeMap::new();
        for (k, v) in std::env::vars_os() {
            let ks = k.to_string_lossy().into_owned();
            let vs = v.to_string_lossy().into_owned();
            map.insert(ks.to_ascii_lowercase(), (ks, vs));
        }
        for (k, v) in [
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("TERM_PROGRAM", "goatty"),
            ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION")),
        ] {
            map.insert(k.to_ascii_lowercase(), (k.to_string(), v.to_string()));
        }
        for (k, v) in extra {
            map.insert(k.to_ascii_lowercase(), (k.clone(), v.clone()));
        }
        let mut block = Vec::new();
        for (_, (k, v)) in map {
            block.extend(OsStr::new(&k).encode_wide());
            block.push(b'=' as u16);
            block.extend(OsStr::new(&v).encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    struct OwnedHandle(HANDLE);

    // SAFETY: Win32 kernel handles (pipes, processes, threads, jobs) are safe to
    // transfer and wait on across threads.
    unsafe impl Send for OwnedHandle {}
    // SAFETY: see above.
    unsafe impl Sync for OwnedHandle {}

    impl OwnedHandle {
        fn raw(&self) -> HANDLE {
            self.0
        }

        fn into_raw(self) -> HANDLE {
            let h = self.0;
            std::mem::forget(self);
            h
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: `self.0` is a valid handle owned exclusively by `self`.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn create_pipe() -> std::io::Result<(OwnedHandle, OwnedHandle)> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: FALSE,
        };
        let mut read_h: HANDLE = std::ptr::null_mut();
        let mut write_h: HANDLE = std::ptr::null_mut();
        // SAFETY: `sa`, `read_h`, and `write_h` are valid pointers.
        if unsafe { CreatePipe(&raw mut read_h, &raw mut write_h, &raw const sa, 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((OwnedHandle(read_h), OwnedHandle(write_h)))
    }

    struct HpconGuard(HPCON);

    // SAFETY: `HPCON` is a kernel pseudoconsole handle synchronized by `Mutex`.
    unsafe impl Send for HpconGuard {}

    impl Drop for HpconGuard {
        fn drop(&mut self) {
            // SAFETY: `self.0` is an open `HPCON` owned exclusively by `self`.
            unsafe {
                ClosePseudoConsole(self.0);
            }
        }
    }

    #[derive(Clone)]
    struct PseudoConsole(Arc<Mutex<Option<HpconGuard>>>);

    impl PseudoConsole {
        fn new(size: TermSize, input_read: HANDLE, output_write: HANDLE) -> Result<Self> {
            let mut hpcon: HPCON = 0;
            // SAFETY: `input_read` and `output_write` are valid open pipe handles.
            let hr = unsafe { CreatePseudoConsole(conpty_coord(size), input_read, output_write, 0, &raw mut hpcon) };
            if hr < 0 {
                bail!("CreatePseudoConsole failed (HRESULT 0x{:08x})", hr as u32);
            }
            Ok(Self(Arc::new(Mutex::new(Some(HpconGuard(hpcon))))))
        }

        fn raw_handle(&self) -> Option<HPCON> {
            self.0.lock().as_ref().map(|g| g.0)
        }

        fn resize(&self, size: TermSize) -> Result<()> {
            let guard = self.0.lock();
            let Some(hpc) = guard.as_ref() else {
                return Ok(());
            };
            // SAFETY: `hpc.0` stays open while `guard` is held.
            let hr = unsafe { ResizePseudoConsole(hpc.0, conpty_coord(size)) };
            if hr < 0 {
                bail!("ResizePseudoConsole failed (HRESULT 0x{:08x})", hr as u32);
            }
            Ok(())
        }

        /// Close the underlying ConPTY (`ClosePseudoConsole`). Must be called
        /// while the output reader is draining (or after the reader is done) so
        /// Windows never deadlocks flushing buffered output.
        fn close(&self) {
            // Take the handle out before dropping so `ClosePseudoConsole` never
            // executes while holding the mutex (which `PtyResizer` might touch).
            let guard = self.0.lock().take();
            drop(guard);
        }
    }

    struct ProcThreadAttrList {
        _buf: Vec<usize>,
        ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
    }

    impl ProcThreadAttrList {
        fn with_pseudoconsole(hpcon: HPCON) -> std::io::Result<Self> {
            let mut bytes = 0usize;
            // SAFETY: probing required buffer size with null list pointer is documented.
            unsafe {
                InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut bytes);
            }
            if bytes == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let words = bytes.div_ceil(std::mem::size_of::<usize>()).max(1);
            let mut buf = vec![0usize; words];
            let ptr: LPPROC_THREAD_ATTRIBUTE_LIST = buf.as_mut_ptr().cast();
            // SAFETY: `buf` is pointer-aligned and holds at least `bytes` writable bytes.
            if unsafe { InitializeProcThreadAttributeList(ptr, 1, 0, &raw mut bytes) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let list = Self { _buf: buf, ptr };
            // SAFETY: `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` takes the `HPCON` value
            // cast to `*const c_void` (not a pointer to `HPCON`), with `size_of::<HPCON>()`.
            if unsafe {
                UpdateProcThreadAttribute(
                    list.ptr,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    hpcon as *const std::ffi::c_void,
                    std::mem::size_of::<HPCON>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(list)
        }

        fn as_mut_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.ptr
        }
    }

    impl Drop for ProcThreadAttrList {
        fn drop(&mut self) {
            // SAFETY: `self.ptr` was initialized by `InitializeProcThreadAttributeList` and
            // `_buf` stays alive until after `Drop::drop` finishes.
            unsafe {
                DeleteProcThreadAttributeList(self.ptr);
            }
        }
    }

    /// Owned Win32 Job Object handle (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) so
    /// terminating or dropping the PTY tears down the shell's whole process tree.
    struct JobObject(OwnedHandle);

    impl JobObject {
        fn new() -> std::io::Result<Self> {
            // SAFETY: null security attributes and unnamed job object are valid.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Self(OwnedHandle(handle));
            // SAFETY: `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` is plain C data; all zeros is valid.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `job.0` is a valid Job Object handle and `info` is valid for its exact size.
            let ok = unsafe {
                SetInformationJobObject(
                    job.0.raw(),
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }

        fn assign_handle(&self, proc: HANDLE) -> std::io::Result<()> {
            // SAFETY: both `self.0` and `proc` are valid open handles.
            if unsafe { AssignProcessToJobObject(self.0.raw(), proc) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        fn terminate(&self) {
            // SAFETY: `self.0` is a valid Job Object handle we own.
            unsafe {
                TerminateJobObject(self.0.raw(), 1);
            }
        }
    }

    /// Resize handle for a ConPTY, independent of [`PtyInput`] so a blocked
    /// input write never holds up resize (#233).
    #[derive(Clone)]
    pub struct PtyResizer {
        console: PseudoConsole,
    }

    impl PtyResizer {
        pub fn resize(&self, size: TermSize) -> Result<()> {
            self.console.resize(size)
        }
    }

    /// Watches the child process independently of the output pipe and closes
    /// the ConPTY (`ClosePseudoConsole`) once the child exits so the reader
    /// thread drains remaining output and observes EOF without deadlocking.
    pub struct PtyExitWatch {
        process: Arc<OwnedHandle>,
        console: PseudoConsole,
        exit_code: Arc<Mutex<Option<u32>>>,
    }

    impl PtyExitWatch {
        pub fn wait_and_close_console(self) {
            // SAFETY: `self.process` is a valid process handle kept alive by `Arc`.
            unsafe {
                WaitForSingleObject(self.process.raw(), INFINITE);
            }
            let mut code = 0u32;
            // SAFETY: `self.process` is open and `code` is a valid out-pointer.
            if unsafe { GetExitCodeProcess(self.process.raw(), &raw mut code) } != 0 && code != STILL_ACTIVE as u32 {
                *self.exit_code.lock() = Some(code);
            }
            self.console.close();
        }
    }

    /// A running child process attached to a Windows ConPTY.
    pub struct Pty {
        console: PseudoConsole,
        writer: Mutex<Option<File>>,
        process: Arc<OwnedHandle>,
        pid: u32,
        exit_code: Arc<Mutex<Option<u32>>>,
        job: JobObject,
    }

    /// Input to a ConPTY: its own write pipe handle, never sharing the
    /// pseudoconsole resize/control lock (#233).
    pub struct PtyInput(File);

    impl PtyInput {
        /// Blocks while the ConPTY input pipe buffer is full.
        pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.0.write_all(bytes)?;
            self.0.flush()
        }
    }

    impl Pty {
        /// Spawn `cmd` under ConPTY. Returns the PTY plus a blocking output reader.
        pub fn spawn(cmd: &ShellCommand, size: TermSize) -> Result<(Self, PtyReader)> {
            let (in_read, in_write) = create_pipe().context("create ConPTY input pipe")?;
            let (out_read, out_write) = create_pipe().context("create ConPTY output pipe")?;
            let console = PseudoConsole::new(size, in_read.raw(), out_write.raw())?;
            // ConPTY duplicated/took the slave ends; close our copies so EOF
            // propagates cleanly as soon as `ClosePseudoConsole` runs.
            drop(in_read);
            drop(out_write);

            let hpcon = console.raw_handle().context("pseudoconsole closed unexpectedly")?;
            let attrs = ProcThreadAttrList::with_pseudoconsole(hpcon).context("init PROC_THREAD_ATTRIBUTE_LIST")?;

            // SAFETY: `STARTUPINFOEXW` is plain C data; zeroed with `cb` set is required.
            let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            // Prevent the child from inheriting redirected stdio handles (e.g.
            // `goattyd.log`) instead of attaching to the ConPTY.
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            si.lpAttributeList = attrs.as_mut_ptr();

            let mut cmdline = build_cmdline(&cmd.program, &cmd.args);
            let env_block = build_env_block(&cmd.env);
            let cwd_wide = cmd.cwd.as_ref().map(|p| to_wide_nul(p.as_os_str()));
            let cwd_ptr = cwd_wide.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());

            let job = JobObject::new().context("create Win32 job object")?;

            // SAFETY: `PROCESS_INFORMATION` is plain C data; zero-init is valid.
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            // Start suspended so we assign the child to the Job Object BEFORE it
            // executes a single instruction or spawns any grandchildren.
            // SAFETY: `cmdline`, `env_block`, `cwd_ptr`, `si`, and `attrs` all stay alive across the call.
            let ok = unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    cmdline.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    FALSE,
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                    env_block.as_ptr().cast(),
                    cwd_ptr,
                    &raw const si.StartupInfo,
                    &raw mut pi,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| format!("spawn {}", cmd.program));
            }
            let proc = OwnedHandle(pi.hProcess);
            let thread = OwnedHandle(pi.hThread);

            if let Err(e) = job.assign_handle(proc.raw()) {
                // SAFETY: `proc` is an open handle to the suspended child we own.
                unsafe {
                    TerminateProcess(proc.raw(), 1);
                }
                return Err(e).with_context(|| format!("assign {} (pid {}) to job object", cmd.program, pi.dwProcessId));
            }

            // SAFETY: `thread` is the suspended main thread of `proc`.
            if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
                let err = std::io::Error::last_os_error();
                // SAFETY: `proc` is an open handle to the suspended child we own.
                unsafe {
                    TerminateProcess(proc.raw(), 1);
                }
                return Err(err).with_context(|| format!("resume {} (pid {})", cmd.program, pi.dwProcessId));
            }
            drop(thread);

            // SAFETY: we transfer exclusive ownership of `out_read` and `in_write`
            // into `File` handles.
            let reader_file = unsafe { File::from_raw_handle(out_read.into_raw()) };
            // SAFETY: as above.
            let writer_file = unsafe { File::from_raw_handle(in_write.into_raw()) };
            let reader: PtyReader = Box::new(reader_file);

            Ok((
                Self {
                    console,
                    writer: Mutex::new(Some(writer_file)),
                    process: Arc::new(proc),
                    pid: pi.dwProcessId,
                    exit_code: Arc::new(Mutex::new(None)),
                    job,
                },
                reader,
            ))
        }

        /// A handle for resizing the ConPTY without locking `Pty` or `PtyInput`.
        pub fn resizer(&self) -> PtyResizer {
            PtyResizer { console: self.console.clone() }
        }

        /// An exit watcher that waits on the child process handle and closes
        /// the pseudoconsole on exit while the output reader drains to EOF.
        pub fn exit_watcher(&self) -> PtyExitWatch {
            PtyExitWatch { process: self.process.clone(), console: self.console.clone(), exit_code: self.exit_code.clone() }
        }

        /// Close the pseudoconsole explicitly (e.g. in tests after killing).
        #[cfg(test)]
        pub fn close_console(&self) {
            self.console.close();
        }

        /// Take the writer handle for sending input to the ConPTY. May be called
        /// once per spawned `Pty`.
        pub fn input(&self) -> std::io::Result<PtyInput> {
            self.writer.lock().take().map(PtyInput).ok_or_else(|| std::io::Error::other("ConPTY input writer already taken"))
        }

        pub fn resize(&self, size: TermSize) -> Result<()> {
            self.console.resize(size)
        }

        pub fn pid(&self) -> Option<u32> {
            Some(self.pid)
        }

        /// The exit code once the child exited, or `None` while still running.
        pub fn try_wait(&mut self) -> Option<u32> {
            if let Some(code) = *self.exit_code.lock() {
                return Some(code);
            }
            let mut code = 0u32;
            // SAFETY: `self.process` is an open handle owned by `self`.
            if unsafe { GetExitCodeProcess(self.process.raw(), &raw mut code) } != 0 && code != STILL_ACTIVE as u32 {
                *self.exit_code.lock() = Some(code);
                return Some(code);
            }
            None
        }

        /// Terminate the shell's process tree via its Job Object.
        pub fn kill(&mut self) {
            self.job.terminate();
        }

        /// Foreground process PID inferred by walking the session shell's Win32
        /// process tree.
        pub fn foreground_pid(&self) -> Option<u32> {
            crate::process::windows_foreground_pid(self.pid)
        }

        /// Name of the foreground program (see [`crate::process`]).
        pub fn foreground_name(&self) -> Option<String> {
            crate::process::process_name(self.foreground_pid()?)
        }

        /// Windows relies on OSC 7 for CWD tracking.
        pub fn cwd(&self) -> Option<PathBuf> {
            crate::process::cwd(self.pid)
        }
    }
}

#[cfg(windows)]
pub use windows::{Pty, PtyExitWatch, PtyInput, PtyReader, PtyResizer};

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::time::{Duration, Instant};

    use super::*;

    #[cfg(unix)]
    fn sh(script: &str) -> ShellCommand {
        let mut c = ShellCommand::new("/bin/sh".into(), vec!["-c".into(), script.into()], Vec::new());
        c.cwd = None;
        c
    }

    #[cfg(unix)]
    fn read_until(r: &mut impl Read, needle: &str) -> String {
        let (mut out, mut buf) = (Vec::new(), [0u8; 4096]);
        while !String::from_utf8_lossy(&out).contains(needle) {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn an_adopted_pty_keeps_working() {
        let (pty, reader) = Pty::spawn(&sh("read line; echo got:$line; sleep 30"), TermSize::new(80, 24)).unwrap();
        let pid = pty.pid().unwrap();
        assert_eq!(session_leader(pty.master_fd()), Some(pid), "the shell leads the terminal's session");
        assert_eq!(window_size(pty.master_fd()).map(|s| (s.cols, s.rows)), Some((80, 24)));
        // What the next goattyd gets: the master fd, nothing else.
        let master = pty.master_fd().try_clone_to_owned().unwrap();
        drop(reader);
        std::mem::forget(pty); // a re-exec doesn't run destructors either
        let (mut pty, mut reader) = Pty::adopt(master, pid).unwrap();
        pty.resize(TermSize { cols: 100, rows: 30, cell_width: 9, cell_height: 18 }).unwrap();
        let ws = window_size(pty.master_fd()).unwrap();
        assert_eq!((ws.cols, ws.rows, ws.cell_width, ws.cell_height), (100, 30, 9, 18));
        pty.input().unwrap().write(b"hello\r").unwrap();
        assert!(read_until(&mut reader, "got:hello").contains("got:hello"));
        pty.kill();
        // Still our child: the exit status is there (SIGHUP).
        let deadline = Instant::now() + Duration::from_secs(5);
        let code = loop {
            if let Some(c) = pty.try_wait() {
                break Some(c);
            }
            if Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(code, Some(128 + libc::SIGHUP as u32));
    }

    /// Ctrl+S must reach the program instead of freezing output (#192).
    #[cfg(unix)]
    #[test]
    fn flow_control_is_off() {
        let cmd = ShellCommand { program: "stty".into(), args: vec!["-a".into()], cwd: None, env: Vec::new() };
        let (mut pty, mut reader) = Pty::spawn(&cmd, TermSize::new(80, 24)).unwrap();
        let mut out = String::new();
        let mut buf = [0u8; 4096];
        // EIO once the child exits and the slave closes.
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        pty.try_wait();
        assert!(out.split_whitespace().any(|w| w == "-ixon"), "{out}");
    }

    #[cfg(windows)]
    #[test]
    fn conpty_spawn_io_resize_and_kill_work() {
        let cmd = ShellCommand {
            program: "cmd.exe".into(),
            args: vec!["/Q".into(), "/K".into(), "echo ready-conpty".into()],
            cwd: None,
            env: Vec::new(),
        };
        let (mut pty, mut reader) = Pty::spawn(&cmd, TermSize::new(80, 24)).unwrap();
        assert!(pty.pid().is_some());
        let resizer = pty.resizer();
        let exit_watch = pty.exit_watcher();
        let mut input = pty.input().unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let reader_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let wait_marker = |acc: &mut Vec<u8>, needle: &str, timeout: Duration| -> bool {
            let deadline = Instant::now() + timeout;
            while !String::from_utf8_lossy(acc).contains(needle) {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                match rx.recv_timeout(left) {
                    Ok(chunk) => acc.extend_from_slice(&chunk),
                    Err(_) => return false,
                }
            }
            true
        };

        let mut acc = Vec::new();
        if !wait_marker(&mut acc, "ready-conpty", Duration::from_secs(10)) {
            pty.kill();
            pty.close_console();
            let _ = reader_thread.join();
            panic!("timed out waiting for ready-conpty; got:\n{}", String::from_utf8_lossy(&acc));
        }

        resizer.resize(TermSize::new(100, 30)).unwrap();
        input.write(b"echo second-line\r\n").unwrap();
        if !wait_marker(&mut acc, "second-line", Duration::from_secs(10)) {
            pty.kill();
            pty.close_console();
            let _ = reader_thread.join();
            panic!("timed out waiting for second-line; got:\n{}", String::from_utf8_lossy(&acc));
        }

        let exit_thread = std::thread::spawn(move || exit_watch.wait_and_close_console());
        pty.kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pty.try_wait().is_none() {
            assert!(Instant::now() < deadline, "ConPTY child did not exit after kill()");
            std::thread::sleep(Duration::from_millis(20));
        }
        exit_thread.join().unwrap();
        reader_thread.join().unwrap();
    }
}
