//! PTYs: `portable-pty` opens them and starts the child, after that goatty
//! works on the master fd itself. That lets goattyd hand a PTY to its next
//! version (an inherited fd across `execve`, or systemd's fd store) and pick
//! it up again with [`Pty::adopt`], with a shell that may no longer be its
//! child.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::TermSize;

#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl ShellCommand {
    /// `program args` with extra `env`, started in `$HOME`. Which shell to
    /// run is `goatty-shell`'s decision (`Launcher::launch`).
    pub fn new(program: String, args: Vec<String>, env: Vec<(String, String)>) -> Self {
        Self { program, args, cwd: std::env::var_os("HOME").map(PathBuf::from), env }
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
    writer: File,
    process: Process,
}

/// Duplicate `fd` with `FD_CLOEXEC` set.
fn dup(fd: BorrowedFd<'_>) -> std::io::Result<OwnedFd> {
    fd.try_clone_to_owned()
}

impl Pty {
    /// Spawn `cmd`. Returns the PTY plus a blocking reader for its output,
    /// which the caller should drive from a dedicated thread.
    pub fn spawn(cmd: &ShellCommand, size: TermSize) -> Result<(Self, File)> {
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
    pub fn adopt(master: OwnedFd, pid: u32) -> Result<(Self, File)> {
        set_cloexec(master.as_raw_fd(), true)?;
        // SAFETY: plain syscall; a failure (old kernel, gone process) is handled.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        // SAFETY: on success the syscall returned a new fd that nothing else owns.
        let pidfd = (raw >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw as RawFd) });
        Self::from_parts(master, Process::Adopted { pid, pidfd, exited: None })
    }

    fn from_parts(master: OwnedFd, process: Process) -> Result<(Self, File)> {
        let writer = File::from(dup(master.as_fd())?);
        let reader = File::from(dup(master.as_fd())?);
        Ok((Self { master, writer, process }, reader))
    }

    /// The master side, e.g. to pass it on to the next goattyd.
    pub fn master_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub fn resize(&self, size: TermSize) -> Result<()> {
        let s = pty_size(size);
        let ws = libc::winsize { ws_row: s.rows, ws_col: s.cols, ws_xpixel: s.pixel_width, ws_ypixel: s.pixel_height };
        // SAFETY: `ws` is a valid winsize and the fd is open.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &raw const ws) } != 0 {
            return Err(std::io::Error::last_os_error()).context("TIOCSWINSZ");
        }
        Ok(())
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
    /// for now report the shell's cwd via /proc (Linux).
    pub fn cwd(&self) -> Option<PathBuf> {
        let pid = self.pid()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
}

/// Send `sig` through the pidfd when there is one (no pid reuse), else to
/// the pid. False when the process is gone.
fn signal(pid: u32, pidfd: Option<RawFd>, sig: libc::c_int) -> bool {
    // SAFETY: plain syscalls; a null siginfo is allowed for pidfd_send_signal.
    let r = unsafe {
        match pidfd {
            Some(fd) => libc::syscall(libc::SYS_pidfd_send_signal, fd, sig, std::ptr::null::<libc::siginfo_t>(), 0) as libc::c_int,
            None => libc::kill(pid as libc::pid_t, sig),
        }
    };
    r == 0
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
/// goatty started in it (`TIOCGSID`).
pub fn session_leader(master: BorrowedFd<'_>) -> Option<u32> {
    let mut sid: libc::pid_t = 0;
    // SAFETY: `sid` is valid for writes and the fd is open.
    let r = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGSID, &raw mut sid) };
    (r == 0 && sid > 0).then_some(sid as u32)
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

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn sh(script: &str) -> ShellCommand {
        let mut c = ShellCommand::new("/bin/sh".into(), vec!["-c".into(), script.into()], Vec::new());
        c.cwd = None;
        c
    }

    fn read_until(r: &mut File, needle: &str) -> String {
        let (mut out, mut buf) = (Vec::new(), [0u8; 4096]);
        while !String::from_utf8_lossy(&out).contains(needle) {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

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
        pty.write(b"hello\r").unwrap();
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
}
