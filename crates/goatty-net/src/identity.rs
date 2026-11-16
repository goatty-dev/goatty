//! This install's device key: an ed25519 key made on first use, stored as
//! the hex seed in `device.key` (0600) under goatty's data dir. The same key
//! identifies this computer as a client (to hosts it paired with) and as a
//! host (to the devices paired with it).

use std::path::Path;

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::{hex, unhex};

pub struct Identity {
    key: SigningKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", short(&self.public()))
    }
}

impl Identity {
    /// The key in `path`, made (0600) if there is none yet.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let line = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).context("empty key file")?;
                let seed = unhex::<32>(line).with_context(|| format!("read {}", path.display()))?;
                Ok(Self { key: SigningKey::from_bytes(&seed) })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Self::generate()?;
                let text = format!("# goatty device key (SECRET). Public key: {}\n{}\n", id.public(), hex(&id.key.to_bytes()));
                crate::write_atomic(path, text.as_bytes(), 0o600)?;
                log::info!("made a new goatty device key ({})", short(&id.public()));
                Ok(id)
            }
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// A new key, not stored anywhere.
    pub fn generate() -> Result<Self> {
        Ok(Self { key: SigningKey::from_bytes(&crate::random::<32>()?) })
    }

    /// The public key, hex.
    pub fn public(&self) -> String {
        hex(self.key.verifying_key().as_bytes())
    }

    /// Signature over `msg`, hex.
    pub fn sign(&self, msg: &[u8]) -> String {
        hex(&self.key.sign(msg).to_bytes())
    }
}

/// Whether `sig` (hex) is `public`'s (hex) signature over `msg`.
pub fn verify(public: &str, msg: &[u8], sig: &str) -> Result<()> {
    let key = VerifyingKey::from_bytes(&unhex::<32>(public).context("bad public key")?).context("bad public key")?;
    let sig = Signature::from_bytes(&unhex::<64>(sig).context("bad signature")?);
    if key.verify(msg, &sig).is_err() {
        bail!("bad signature");
    }
    Ok(())
}

/// Whether `s` looks like a public key (64 hex digits).
pub fn is_public_key(s: &str) -> bool {
    unhex::<32>(s).is_ok()
}

/// A public key as people compare it: `3f2a 9c01`.
pub fn short(public: &str) -> String {
    let p: String = public.chars().take(8).collect();
    if p.len() == 8 { format!("{} {}", &p[..4], &p[4..]) } else { p }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_made_once_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tailnet/device.key");
        let a = Identity::load_or_create(&path).unwrap();
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(is_public_key(&a.public()));
    }

    #[test]
    fn signatures_verify_only_for_their_key_and_message() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let sig = a.sign(b"hello");
        verify(&a.public(), b"hello", &sig).unwrap();
        assert!(verify(&a.public(), b"hellO", &sig).is_err());
        assert!(verify(&b.public(), b"hello", &sig).is_err());
        assert!(verify("nope", b"hello", &sig).is_err());
        assert_eq!(short("3f2a9c01deadbeef"), "3f2a 9c01");
    }
}
