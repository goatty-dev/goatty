//! goatty over the user's tailnet (#15), without a network socket in goattyd.
//!
//! goatty uses the Tailscale the user already installed and logged in to,
//! only through its public CLI ([`tailscale`]): it never installs, logs in,
//! updates or runs a node of its own. A host that is made available gets one
//! Serve route, `tailscale serve --tcp <port> unix:<gateway socket>`, so
//! tailscaled (root) carries tailnet connections into a Unix socket of
//! goattyd's and goattyd itself binds nothing.
//!
//! On that socket every connection starts with the gateway handshake
//! ([`gateway`]): a probe (answered to anyone who gets through the tailnet),
//! pairing (only while the user has the pairing window open, confirmed with
//! a 6-digit code on both screens), or authentication with a paired device
//! key ([`identity`], [`pairing`]). After a successful authentication the
//! connection carries the normal goatty protocol.
//!
//! * [`tailscale`]: detect, status, Serve routes (add/remove only our own).
//! * [`identity`]: this install's ed25519 key.
//! * [`pairing`]: the code, paired devices (host side), pinned hosts (client side).
//! * [`gateway`]: the handshake, the host side ([`gateway::Gateway`]) and the client side.
//! * [`link`]: a byte stream to a peer: plain TCP, or `tailscale nc`.
//! * [`discovery`]: the peers worth offering, probed on goatty's port.
//! * [`api`]: the requests the GUI sends its local goattyd (`Net`, protocol v12).
//! * [`host`]: availability on this host (state, route, pairing window).
//!
//! std only, no async: like the rest of goatty it uses threads.

pub mod api;
pub mod discovery;
pub mod gateway;
pub mod host;
pub mod identity;
pub mod link;
pub mod pairing;
pub mod tailscale;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The tailnet port goatty asks Serve for unless the user picks another.
pub const DEFAULT_PORT: u16 = 7422;

/// Where goatty keeps its tailnet files (`$XDG_DATA_HOME/goatty/tailnet`):
/// the device key, paired devices, pinned hosts and the host's state.
/// Never in `settings.json`, which can be exported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    /// This install's secret key (0600).
    pub fn key(&self) -> PathBuf {
        self.dir.join("device.key")
    }
    /// Devices allowed to connect to this host.
    pub fn devices(&self) -> PathBuf {
        self.dir.join("devices.json")
    }
    /// Hosts this computer paired with, and their keys.
    pub fn hosts(&self) -> PathBuf {
        self.dir.join("hosts.json")
    }
    /// Whether this host is available, and on which port.
    pub fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }
}

/// The gateway socket next to the daemon socket `socket`
/// (`…/goattyd.sock` → `…/goattyd.gateway.sock`): in the same 0700 directory,
/// separate from the main socket.
pub fn gateway_socket(socket: &Path) -> PathBuf {
    socket.with_extension("gateway.sock")
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex<const N: usize>(s: &str) -> Result<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.is_ascii() {
        bail!("expected {} hex digits", N * 2);
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).context("not hex")?;
    }
    Ok(out)
}

/// `N` bytes from the OS's random source.
pub(crate) fn random<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut out)).context("read /dev/urandom")?;
    Ok(out)
}

/// Write `data` to `path` through a temporary file in the same directory
/// (created 0700 if needed), with `mode`, so readers never see half a file.
pub(crate) fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().context("path has no parent")?;
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = dir.join(format!(".{}.{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp"), std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// A JSON file, or the default when it doesn't exist.
pub(crate) fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("read {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&b), "0001abff");
        assert_eq!(unhex::<4>("0001abff").unwrap(), b);
        assert!(unhex::<4>("0001ab").is_err());
        assert!(unhex::<2>("zzzz").is_err());
        assert!(unhex::<2>("éé").is_err());
    }

    #[test]
    fn gateway_socket_sits_next_to_the_daemon_socket() {
        assert_eq!(gateway_socket(Path::new("/run/user/1/goatty/goattyd.sock")), Path::new("/run/user/1/goatty/goattyd.gateway.sock"));
    }
}
