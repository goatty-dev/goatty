//! Pairing: the code both screens show, the devices a host accepts and the
//! hosts a client pinned.
//!
//! Each side derives the code from both public keys and both nonces, so a
//! device in the middle (which would have to swap in its own key) makes
//! the two screens show different codes.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The 6-digit code for a pairing between `host_key` and `client_key`
/// (hex public keys) with their nonces.
pub fn code(host_key: &str, host_nonce: &str, client_key: &str, client_nonce: &str) -> String {
    let mut h = Sha256::new();
    for part in ["goatty-pair-code-v1", host_key, host_nonce, client_key, client_nonce] {
        h.update((part.len() as u32).to_le_bytes());
        h.update(part.as_bytes());
    }
    let d = h.finalize();
    let n = u64::from_le_bytes(d[..8].try_into().expect("8 bytes")) % 1_000_000;
    format!("{n:06}")
}

/// `123456` → `123 456`, as the screens show it.
pub fn spaced(code: &str) -> String {
    if code.len() == 6 { format!("{} {}", &code[..3], &code[3..]) } else { code.to_string() }
}

/// A device allowed to connect to this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Its public key (hex).
    pub key: String,
    /// The name it gave when it paired (its hostname).
    pub name: String,
    pub paired_unix: u64,
}

/// The devices paired with this host (`devices.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Devices {
    pub devices: Vec<Device>,
}

impl Devices {
    pub fn load(path: &Path) -> Result<Self> {
        crate::read_json(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::write_atomic(path, &serde_json::to_vec_pretty(self)?, 0o600)
    }

    pub fn get(&self, key: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.key == key)
    }

    /// Add or rename a device.
    pub fn add(&mut self, device: Device) {
        self.devices.retain(|d| d.key != device.key);
        self.devices.push(device);
    }

    /// Remove the devices whose key starts with `prefix` (at least 8 hex
    /// digits, so a typo can't revoke everything). Returns the removed ones.
    pub fn revoke(&mut self, prefix: &str) -> Vec<Device> {
        if prefix.len() < 8 {
            return Vec::new();
        }
        let (gone, kept) = std::mem::take(&mut self.devices).into_iter().partition(|d| d.key.starts_with(prefix));
        self.devices = kept;
        gone
    }
}

/// A host this computer paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHost {
    /// Its MagicDNS name without the trailing dot (`box.tail1234.ts.net`):
    /// what the host list keys it by.
    pub dns: String,
    /// Its Tailscale host name, as shown.
    pub name: String,
    /// Its tailnet address when it paired (used when the status can't be read).
    pub addr: String,
    pub port: u16,
    /// Its public key (hex): a host that shows another is refused.
    pub key: String,
    pub paired_unix: u64,
}

/// The hosts this computer paired with (`hosts.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHosts {
    pub hosts: Vec<KnownHost>,
}

impl KnownHosts {
    pub fn load(path: &Path) -> Result<Self> {
        crate::read_json(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::write_atomic(path, &serde_json::to_vec_pretty(self)?, 0o600)
    }

    pub fn get(&self, dns: &str) -> Option<&KnownHost> {
        self.hosts.iter().find(|h| h.dns == dns)
    }

    pub fn add(&mut self, host: KnownHost) {
        self.hosts.retain(|h| h.dns != host.dns);
        self.hosts.push(host);
    }

    pub fn remove(&mut self, dns: &str) -> bool {
        let before = self.hosts.len();
        self.hosts.retain(|h| h.dns != dns);
        self.hosts.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_depends_on_every_input() {
        let c = code("aa", "11", "bb", "22");
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(c, code("aa", "11", "bb", "22"));
        for other in
            [code("ab", "11", "bb", "22"), code("aa", "12", "bb", "22"), code("aa", "11", "bc", "22"), code("aa", "11", "bb", "23")]
        {
            assert_ne!(c, other);
        }
        // Field boundaries count: moving a character between fields changes it.
        assert_ne!(code("aab", "11", "b", "22"), code("aa", "11", "bb", "22"));
        assert_eq!(spaced("012345"), "012 345");
    }

    #[test]
    fn devices_and_hosts_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devices.json");
        let mut d = Devices::load(&path).unwrap();
        assert!(d.devices.is_empty());
        d.add(Device { key: "a".repeat(64), name: "laptop".into(), paired_unix: 1 });
        d.add(Device { key: "b".repeat(64), name: "desk".into(), paired_unix: 2 });
        d.add(Device { key: "a".repeat(64), name: "laptop2".into(), paired_unix: 3 });
        d.save(&path).unwrap();
        let mut d = Devices::load(&path).unwrap();
        assert_eq!(d.devices.len(), 2);
        assert_eq!(d.get(&"a".repeat(64)).unwrap().name, "laptop2");
        assert!(d.revoke("aaa").is_empty(), "too short a prefix revokes nothing");
        assert_eq!(d.revoke("aaaaaaaa").len(), 1);
        assert!(d.get(&"a".repeat(64)).is_none());

        let path = dir.path().join("hosts.json");
        let mut h = KnownHosts::load(&path).unwrap();
        h.add(KnownHost {
            dns: "box.ts.net".into(),
            name: "box".into(),
            addr: "100.1.2.3".into(),
            port: 7422,
            key: "c".repeat(64),
            paired_unix: 1,
        });
        h.save(&path).unwrap();
        let mut h = KnownHosts::load(&path).unwrap();
        assert_eq!(h.get("box.ts.net").unwrap().addr, "100.1.2.3");
        assert!(h.remove("box.ts.net"));
        assert!(!h.remove("box.ts.net"));
    }
}
