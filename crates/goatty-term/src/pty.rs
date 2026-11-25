//! PTYs: on Unix, `portable-pty` opens them and starts the child, after which
//! goatty works on the master fd itself (enabling fd handoff across `execve`
//! and systemd's fd store via [`Pty::adopt`]). On Windows, wraps
//! `portable-pty`'s ConPTY `MasterPty` + `Child`, with a Win32 Job Object so
//! `kill()` terminates the shell's entire process tree.

use std::path::PathBuf;

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
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use parking_lot::Mutex;
    use portable_pty::{CommandBuilder, MasterPty, native_pty_system};
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    use super::{ShellCommand, pty_size};
    use crate::TermSize;

    /// Output reader returned by [`Pty::spawn`].
    pub type PtyReader = Box<dyn Read + Send>;

    /// Owned Win32 Job Object handle (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) so
    /// terminating or dropping the PTY tears down the shell's whole process tree.
    struct JobObject(HANDLE);

    // SAFETY: Win32 Job Object handles are kernel objects safe to send and share
    // across threads; all operations on them are thread-safe Win32 API calls.
    unsafe impl Send for JobObject {}
    // SAFETY: see above.
    unsafe impl Sync for JobObject {}

    impl JobObject {
        fn new() -> std::io::Result<Self> {
            // SAFETY: null security attributes and unnamed job object are valid.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Self(handle);
            // SAFETY: `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` is plain C data; all zeros is valid.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `job.0` is a valid Job Object handle and `info` is valid for its exact size.
            let ok = unsafe {
                SetInformationJobObject(
                    job.0,
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

        fn assign_pid(&self, pid: u32) -> std::io::Result<()> {
            // SAFETY: plain Win32 call; return handle is checked and closed.
            let proc = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, FALSE, pid) };
            if proc.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: both `self.0` and `proc` are valid handles.
            let ok = unsafe { AssignProcessToJobObject(self.0, proc) };
            let err = if ok == 0 { Some(std::io::Error::last_os_error()) } else { None };
            // SAFETY: `proc` is owned exclusively by this scope.
            unsafe { CloseHandle(proc) };
            match err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }

        fn terminate(&self) {
            // SAFETY: `self.0` is a valid Job Object handle we own.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid handle owned exclusively by `self`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Resize handle for a ConPTY, independent of [`PtyInput`] so a blocked
    /// input write never holds up resize (#233).
    #[derive(Clone)]
    pub struct PtyResizer {
        master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    }

    impl PtyResizer {
        pub fn resize(&self, size: TermSize) -> Result<()> {
            self.master.lock().resize(pty_size(size))
        }
    }

    /// A running child process attached to a Windows ConPTY.
    pub struct Pty {
        master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
        writer: Mutex<Option<Box<dyn Write + Send>>>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
        job: Option<JobObject>,
    }

    /// Input to a ConPTY: its own writer taken from `MasterPty`, never sharing
    /// the `MasterPty` resize/control lock (#233).
    pub struct PtyInput(Box<dyn Write + Send>);

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
            let sys = native_pty_system();
            let pair = sys.openpty(pty_size(size)).context("openpty (ConPTY)")?;
            let mut b = CommandBuilder::new(&cmd.program);
            b.args(&cmd.args);
            if let Some(cwd) = &cmd.cwd {
                b.cwd(cwd);
            }
            b.env("TERM", "xterm-256color");
            b.env("COLORTERM", "truecolor");
            b.env("TERM_PROGRAM", "goatty");
            b.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
            for (k, v) in &cmd.env {
                b.env(k, v);
            }
            let child = pair.slave.spawn_command(b).with_context(|| format!("spawn {}", cmd.program))?;
            drop(pair.slave);

            let job = match JobObject::new() {
                Ok(j) => {
                    if let Some(pid) = child.process_id()
                        && let Err(e) = j.assign_pid(pid)
                    {
                        log::warn!("couldn't assign ConPTY child pid {pid} to job object: {e}");
                    }
                    Some(j)
                }
                Err(e) => {
                    log::warn!("couldn't create Win32 job object for ConPTY child: {e}");
                    None
                }
            };

            let reader = pair.master.try_clone_reader().context("clone ConPTY reader")?;
            let writer = pair.master.take_writer().context("take ConPTY writer")?;
            let master = Arc::new(Mutex::new(pair.master));
            Ok((Self { master, writer: Mutex::new(Some(writer)), child, job }, reader))
        }

        /// A handle for resizing the ConPTY without locking `Pty` or `PtyInput`.
        pub fn resizer(&self) -> PtyResizer {
            PtyResizer { master: self.master.clone() }
        }

        /// Take the writer handle for sending input to the ConPTY. May be called
        /// once per spawned `Pty`.
        pub fn input(&self) -> std::io::Result<PtyInput> {
            self.writer.lock().take().map(PtyInput).ok_or_else(|| std::io::Error::other("ConPTY input writer already taken"))
        }

        pub fn resize(&self, size: TermSize) -> Result<()> {
            self.master.lock().resize(pty_size(size))
        }

        pub fn pid(&self) -> Option<u32> {
            self.child.process_id()
        }

        /// The exit code once the child exited, or `None` while still running.
        pub fn try_wait(&mut self) -> Option<u32> {
            self.child.try_wait().ok().flatten().map(|s| s.exit_code())
        }

        /// Terminate the shell's process tree via its Job Object and `Child::kill`.
        pub fn kill(&mut self) {
            if let Some(job) = &self.job {
                job.terminate();
            }
            let _ = self.child.kill();
        }

        /// Foreground process PID inferred by walking the session shell's Win32
        /// process tree.
        pub fn foreground_pid(&self) -> Option<u32> {
            crate::process::windows_foreground_pid(self.pid()?)
        }

        /// Name of the foreground program (see [`crate::process`]).
        pub fn foreground_name(&self) -> Option<String> {
            crate::process::process_name(self.foreground_pid()?)
        }

        /// Windows relies on OSC 7 for CWD tracking.
        pub fn cwd(&self) -> Option<PathBuf> {
            crate::process::cwd(self.pid()?)
        }
    }
}

#[cfg(windows)]
pub use windows::{Pty, PtyInput, PtyReader, PtyResizer};

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
        let mut input = pty.input().unwrap();
        let out = read_until(&mut reader, "ready-conpty");
        assert!(out.contains("ready-conpty"), "{out}");
        resizer.resize(TermSize::new(100, 30)).unwrap();
        input.write(b"echo second-line\r\n").unwrap();
        let out2 = read_until(&mut reader, "second-line");
        assert!(out2.contains("second-line"), "{out2}");
        pty.kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pty.try_wait().is_none() {
            assert!(Instant::now() < deadline, "ConPTY child did not exit after kill()");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
