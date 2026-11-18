//! What systemd hands goattyd and keeps for it (see `goatty_client::service`).
//!
//! * `LISTEN_FDS`/`LISTEN_FDNAMES` (`sd_listen_fds(3)`): the listening socket
//!   of `goattyd.socket`, plus, after a crash or `systemctl --user restart`,
//!   the PTYs goattyd put into the fd store.
//! * `NOTIFY_SOCKET` (`sd_notify(3)`): the fd store itself. Every session's
//!   PTY master goes in when the session starts (`FDSTORE=1`, named by
//!   [`fd_name`]) and comes out when it ends (`FDSTOREREMOVE=1`). The unit
//!   needs `NotifyAccess=main` and `FileDescriptorStoreMax=`; without them
//!   there's no `NOTIFY_SOCKET` and nothing is stored.
//!
//! The variables are read once, first thing ([`capture_env`]), and removed
//! so the shells don't inherit them. A re-exec passes `NOTIFY_SOCKET` on.

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::OnceLock;

use goatty_proto::SessionId;

const SD_LISTEN_FDS_START: RawFd = 3;
/// Prefix of the names of goattyd's PTYs in the fd store.
const PTY_PREFIX: &str = "goatty-s";

struct Env {
    /// `LISTEN_FDS` were for us: (fd, name).
    listen: Vec<(RawFd, String)>,
    notify: Option<String>,
}

static ENV: OnceLock<Env> = OnceLock::new();

/// Read and remove systemd's variables. Call it before the process starts
/// any thread; later calls return what the first one read.
pub fn capture_env() {
    ENV.get_or_init(|| {
        let pid = std::env::var("LISTEN_PID").ok();
        let fds = std::env::var("LISTEN_FDS").ok();
        let names = std::env::var("LISTEN_FDNAMES").ok();
        let notify = std::env::var("NOTIFY_SOCKET").ok().filter(|s| !s.is_empty());
        // SAFETY: called first thing in `main` (and again, a no-op, from
        // `serve`), before the daemon starts any thread.
        unsafe {
            for v in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES", "NOTIFY_SOCKET"] {
                std::env::remove_var(v);
            }
        }
        let ours = pid.and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id());
        let count = fds.and_then(|n| n.parse::<RawFd>().ok()).filter(|_| ours).unwrap_or(0);
        let names: Vec<String> = names.map(|n| n.split(':').map(str::to_string).collect()).unwrap_or_default();
        let listen = (0..count).map(|i| (SD_LISTEN_FDS_START + i, names.get(i as usize).cloned().unwrap_or_default())).collect();
        Env { listen, notify }
    });
}

fn env() -> &'static Env {
    capture_env();
    ENV.get().expect("captured")
}

/// The fds systemd passed, close-on-exec from now on.
pub(crate) struct Passed {
    /// The listening socket (the fd not named like a PTY).
    pub listener: Option<OwnedFd>,
    /// PTYs from the fd store: the parsed name and the master.
    pub ptys: Vec<(StoredPty, OwnedFd)>,
}

/// Take the fds systemd passed. Only the first call gets them.
pub(crate) fn take_passed() -> Passed {
    static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let mut passed = Passed { listener: None, ptys: Vec::new() };
    if TAKEN.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return passed;
    }
    for (fd, name) in &env().listen {
        if goatty_term::pty::set_cloexec(*fd, true).is_err() {
            continue; // not open after all
        }
        // SAFETY: systemd passed this fd to us, and nothing in this process
        // has adopted it before (`TAKEN`).
        let owned = unsafe { OwnedFd::from_raw_fd(*fd) };
        match StoredPty::parse(name) {
            Some(p) => passed.ptys.push((p, owned)),
            None if passed.listener.is_none() => passed.listener = Some(owned),
            None => log::warn!("ignoring an fd from systemd named {name:?}"),
        }
    }
    passed
}

/// The `NOTIFY_SOCKET` systemd gave us, for a re-exec to pass on.
pub(crate) fn notify_socket() -> Option<&'static str> {
    env().notify.as_deref()
}

/// The fd store is there (the unit was written with it).
pub(crate) fn has_fd_store() -> bool {
    env().notify.is_some()
}

/// What a PTY's name in the fd store says about its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredPty {
    pub id: SessionId,
    pub created_unix: u64,
    /// goatty's managed zsh (reloads go to it).
    pub managed: bool,
}

impl StoredPty {
    /// `goatty-s<id>-<created>-<m|p>` (names are at most 255 printable
    /// characters without `:`).
    pub fn fd_name(&self) -> String {
        format!("{PTY_PREFIX}{}-{}-{}", self.id, self.created_unix, if self.managed { 'm' } else { 'p' })
    }

    fn parse(name: &str) -> Option<Self> {
        let mut parts = name.strip_prefix(PTY_PREFIX)?.split('-');
        let id = parts.next()?.parse().ok()?;
        let created_unix = parts.next()?.parse().ok()?;
        let managed = match parts.next()? {
            "m" => true,
            "p" => false,
            _ => return None,
        };
        parts.next().is_none().then_some(Self { id, created_unix, managed })
    }
}

/// Put a session's PTY master into the fd store.
pub(crate) fn store(pty: &StoredPty, fd: RawFd) {
    if let Err(e) = notify(&format!("FDSTORE=1\nFDNAME={}", pty.fd_name()), &[fd]) {
        log::warn!("session {}: couldn't keep its PTY in systemd's fd store: {e}", pty.id);
    }
}

/// Take a session's PTY out of the fd store (it ended).
pub(crate) fn unstore(pty: &StoredPty) {
    if let Err(e) = notify(&format!("FDSTOREREMOVE=1\nFDNAME={}", pty.fd_name()), &[]) {
        log::warn!("session {}: couldn't remove its PTY from systemd's fd store: {e}", pty.id);
    }
}

/// systemd is Linux only.
#[cfg(not(target_os = "linux"))]
fn notify(_state: &str, _fds: &[RawFd]) -> std::io::Result<()> {
    Ok(())
}

/// `sd_notify` with `fds` attached (`SCM_RIGHTS`). Nothing without `NOTIFY_SOCKET`.
#[cfg(target_os = "linux")]
fn notify(state: &str, fds: &[RawFd]) -> std::io::Result<()> {
    let Some(path) = notify_socket() else { return Ok(()) };
    // SAFETY: plain syscall; the result is checked and owned right away.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a new socket nothing else owns.
    let sock = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: sockaddr_un is plain data; all zeroes is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "NOTIFY_SOCKET is too long"));
    }
    for (dst, &b) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = b as libc::c_char;
    }
    // An abstract socket ("@name") starts with a NUL byte.
    if bytes.first() == Some(&b'@') {
        addr.sun_path[0] = 0;
    }
    let addr_len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len()) as libc::socklen_t;

    let mut iov = libc::iovec { iov_base: state.as_ptr() as *mut libc::c_void, iov_len: state.len() };
    let fds_len = std::mem::size_of_val(fds) as u32;
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(fds_len) } as usize;
    // u64s keep the buffer aligned for cmsghdr.
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: msghdr is plain data; all zeroes is a valid value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = (&raw mut addr).cast();
    msg.msg_namelen = addr_len;
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: `msg_control` points at `space` bytes, enough for one
        // cmsghdr with `fds_len` bytes of data, so CMSG_FIRSTHDR is non-null
        // and CMSG_DATA has room for the fds.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fds_len) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast::<RawFd>(), fds.len());
        }
    }
    // SAFETY: `msg` and everything it points to live until the call returns.
    let sent = unsafe { libc::sendmsg(sock.as_raw_fd(), &raw const msg, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_names_round_trip() {
        let p = StoredPty { id: 42, created_unix: 1_700_000_000, managed: true };
        assert_eq!(p.fd_name(), "goatty-s42-1700000000-m");
        assert_eq!(StoredPty::parse(&p.fd_name()), Some(p));
        let q = StoredPty { id: 1, created_unix: 0, managed: false };
        assert_eq!(StoredPty::parse(&q.fd_name()), Some(q));
        for bad in ["goattyd.socket", "goatty-s1-2", "goatty-sx-2-m", "goatty-s1-2-m-3", "goatty-s1-2-q", ""] {
            assert_eq!(StoredPty::parse(bad), None, "{bad}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn notify_sends_the_fds() {
        use std::io::IoSliceMut;
        use std::os::unix::net::UnixDatagram;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let rx = UnixDatagram::bind(&path).unwrap();
        // `notify` reads the captured env; exercise the wire format through
        // a local copy of it instead.
        let file = std::fs::File::open("/dev/null").unwrap();
        send_to(&path.to_string_lossy(), "FDSTORE=1\nFDNAME=x", &[file.as_raw_fd()]);
        let mut buf = [0u8; 64];
        let mut cmsg = [0u64; 8];
        let mut iov = [IoSliceMut::new(&mut buf)];
        // SAFETY: test-only recvmsg into buffers that outlive the call.
        let (n, fds) = unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = iov.as_mut_ptr().cast();
            msg.msg_iovlen = 1;
            msg.msg_control = cmsg.as_mut_ptr().cast();
            msg.msg_controllen = std::mem::size_of_val(&cmsg) as _;
            let n = libc::recvmsg(rx.as_raw_fd(), &raw mut msg, 0);
            let c = libc::CMSG_FIRSTHDR(&raw const msg);
            let fds = if c.is_null() { 0 } else { ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4 };
            if !c.is_null() {
                libc::close(*libc::CMSG_DATA(c).cast::<RawFd>());
            }
            (n, fds)
        };
        assert_eq!(&buf[..n as usize], b"FDSTORE=1\nFDNAME=x");
        assert_eq!(fds, 1);
    }

    /// [`notify`] against an explicit socket path.
    #[cfg(target_os = "linux")]
    fn send_to(path: &str, state: &str, fds: &[RawFd]) {
        let _ = ENV.set(Env { listen: Vec::new(), notify: Some(path.to_string()) });
        assert_eq!(notify_socket(), Some(path), "no other test sets the env");
        notify(state, fds).unwrap();
    }
}
