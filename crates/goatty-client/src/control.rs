//! Blocking request/response access to a local goattyd, for CLIs and
//! scripts. One request at a time; no threads, no reconnect, and unlike
//! [`crate::Connection`] it never starts a daemon.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use goatty_proto::{ClientMsg, HostInfo, PROTOCOL_VERSION, ReqId, ServerMsg, SessionId, SessionInfo, encode, read_msg};

use crate::handshake::{self, Outcome};

pub struct Control {
    stream: UnixStream,
    host: HostInfo,
    pid: u32,
    version: u32,
    next_req: ReqId,
}

/// The pid of the process listening on the Unix socket at `path`
/// (`SO_PEERCRED`; `LOCAL_PEERPID` on macOS), whatever protocol it speaks.
/// Only processes of this user count (the socket directory is 0700 anyway).
pub fn socket_owner(path: &Path) -> Result<u32> {
    let stream = UnixStream::connect(path).with_context(|| format!("nothing is listening on {}", path.display()))?;
    let (pid, uid) = peer_cred(&stream)?;
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    if uid != me || pid <= 0 {
        bail!("{} belongs to another user (uid {uid})", path.display());
    }
    Ok(pid as u32)
}

/// The pid and uid of the process at the other end of `stream`.
#[cfg(target_os = "linux")]
fn peer_cred(stream: &UnixStream) -> Result<(libc::pid_t, libc::uid_t)> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes of the sizes passed, and
    // the fd stays open for the duration of the call.
    let rc = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &raw mut len) };
    if rc != 0 {
        bail!("SO_PEERCRED: {}", std::io::Error::last_os_error());
    }
    Ok((cred.pid, cred.uid))
}

/// The pid and uid of the process at the other end of `stream`.
#[cfg(target_os = "macos")]
fn peer_cred(stream: &UnixStream) -> Result<(libc::pid_t, libc::uid_t)> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `pid` and `len` are valid for writes of the sizes passed, and
    // the fd stays open for the duration of the call.
    let rc = unsafe { libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &raw mut len) };
    if rc != 0 {
        bail!("LOCAL_PEERPID: {}", std::io::Error::last_os_error());
    }
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: `uid` and `gid` are valid for writes; the fd is open.
    if unsafe { libc::getpeereid(fd, &raw mut uid, &raw mut gid) } != 0 {
        bail!("getpeereid: {}", std::io::Error::last_os_error());
    }
    Ok((pid, uid))
}

impl Control {
    /// Connect to the daemon socket at `path` and perform the handshake.
    pub fn connect(path: &Path, client: &str) -> Result<Self> {
        Self::connect_within(path, client, None)
    }

    /// [`Self::connect`], giving up when the daemon hasn't greeted within
    /// `timeout` (a socket can accept connections before anything serves it).
    pub fn connect_within(path: &Path, client: &str, timeout: Option<std::time::Duration>) -> Result<Self> {
        let mut announce = PROTOCOL_VERSION;
        loop {
            let mut stream =
                UnixStream::connect(path).with_context(|| format!("goattyd is not running (no socket at {})", path.display()))?;
            stream.set_read_timeout(timeout)?;
            stream.set_write_timeout(timeout)?;
            stream.write_all(&handshake::hello(client, announce)?)?;
            match handshake::read_greeting(&mut stream, &path.display().to_string(), announce)? {
                Outcome::Ok(g) => {
                    stream.set_read_timeout(None)?;
                    stream.set_write_timeout(None)?;
                    return Ok(Self { stream, host: g.host, pid: g.pid, version: g.version, next_req: 1 });
                }
                // An old exact-match daemon: speak its version (only ever lower).
                Outcome::RetryWith(v) => announce = v,
            }
        }
    }

    /// Connect to this user's daemon (`goatty_proto::socket_path`), falling back
    /// to a pre-upgrade macOS daemon on `goatty_proto::legacy_socket_path` when
    /// nothing listens on the default path.
    pub fn connect_local(client: &str) -> Result<Self> {
        let primary = goatty_proto::socket_path();
        match Self::connect(&primary, client) {
            Ok(c) => Ok(c),
            Err(err) => {
                if UnixStream::connect(&primary).is_err()
                    && let Some(legacy) = goatty_proto::legacy_socket_path()
                    && UnixStream::connect(&legacy).is_ok()
                {
                    return Self::connect(&legacy, client);
                }
                Err(err)
            }
        }
    }

    pub fn host(&self) -> &HostInfo {
        &self.host
    }

    /// The daemon's pid.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The protocol version agreed with the daemon.
    pub fn protocol_version(&self) -> u32 {
        self.version
    }

    /// Send one request and wait for its reply. A daemon `Error` becomes `Err`.
    pub fn request(&mut self, msg: impl FnOnce(ReqId) -> ClientMsg) -> Result<ServerMsg> {
        let req = self.next_req;
        self.next_req += 1;
        self.stream.write_all(&encode(&msg(req))?)?;
        loop {
            let reply: ServerMsg = read_msg(&mut self.stream).context("goattyd closed the connection")?;
            match reply {
                ServerMsg::Error { req: r, message } if r.is_none() || r == Some(req) => return Err(anyhow!("goattyd: {message}")),
                _ if reply.reply_to() == Some(req) => return Ok(reply),
                _ => continue, // not ours (e.g. a late stream frame)
            }
        }
    }

    pub fn list_sessions(&mut self) -> Result<Vec<SessionInfo>> {
        match self.request(|req| ClientMsg::ListSessions { req })? {
            ServerMsg::Sessions { list, .. } => Ok(list),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    pub fn kill(&mut self, session: SessionId) -> Result<()> {
        self.request(|req| ClientMsg::Kill { req, session }).map(drop)
    }

    /// Type `data` into a session without attaching to it.
    pub fn send_input(&mut self, session: SessionId, data: Vec<u8>) -> Result<()> {
        self.request(|req| ClientMsg::SendInput { req, session, data }).map(drop)
    }

    /// The host settings document (`host.json` as the daemon sees it).
    pub fn host_settings_json(&mut self) -> Result<String> {
        match self.request(|req| ClientMsg::GetHostSettings { req })? {
            ServerMsg::HostSettings { json, .. } => Ok(json),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Validate, activate and save host settings. Returns the issues found.
    pub fn apply_host_settings(&mut self, json: String) -> Result<Vec<String>> {
        match self.request(|req| ClientMsg::SetHostSettings { req, json, apply: true })? {
            ServerMsg::HostSettingsApplied { issues, .. } => Ok(issues),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Ask the running managed shells to reload. Returns how many were asked.
    pub fn reload_shells(&mut self) -> Result<u32> {
        match self.request(|req| ClientMsg::ReloadShells { req })? {
            ServerMsg::ShellsReloaded { count, .. } => Ok(count),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// A plugin operation (`goatty_plugin::api::Request` as JSON); returns
    /// the reply JSON.
    pub fn plugin(&mut self, request: String) -> Result<String> {
        match self.request(|req| ClientMsg::Plugin { req, request })? {
            ServerMsg::Plugin { reply, .. } => Ok(reply),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// A tailnet operation (`goatty_net::api::Request` as JSON); returns
    /// the reply JSON. Needs protocol v12.
    pub fn net(&mut self, request: String) -> Result<String> {
        if self.version < goatty_proto::NET_SINCE {
            bail!("goattyd (protocol v{}) is too old for the tailnet; restart it with `goattyd restart`", self.version);
        }
        match self.request(|req| ClientMsg::Net { req, request })? {
            ServerMsg::Net { reply, .. } => Ok(reply),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// Replace the daemon with `exe` (default: the goattyd it was started
    /// as), keeping every session (protocol v10). Returns once the old
    /// process image is gone: the daemon closes this connection when it
    /// switches. Connect again to talk to the new one.
    pub fn restart(&mut self, exe: Option<&Path>) -> Result<()> {
        if self.version < goatty_proto::RESTART_SINCE {
            bail!("goattyd (protocol v{}) can't restart without ending its sessions", self.version);
        }
        let exe = exe.map(|p| p.to_string_lossy().into_owned());
        self.request(|req| ClientMsg::Restart { req, exe })?;
        // The switch closes the connection (EOF); a failed one leaves it open.
        self.stream.set_read_timeout(Some(std::time::Duration::from_secs(15)))?;
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut self.stream, &mut buf) {
                Ok(0) => return Ok(()),
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Ok(()),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    bail!("goattyd didn't restart (its log says why); it keeps running as before")
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Stop the daemon and every session it owns.
    pub fn shutdown(&mut self) -> Result<()> {
        self.request(|req| ClientMsg::Shutdown { req }).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_owner_is_the_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(socket_owner(&path).unwrap(), std::process::id());
        assert!(socket_owner(&dir.path().join("none.sock")).is_err());
    }
}
