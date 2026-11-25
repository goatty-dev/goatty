//! Downloads through `curl` (or `wget`). A checksum or signature, not the
//! transport, is what makes a download trusted, so goatty needs no TLS
//! stack of its own. HTTPS is still required, and downloads are size-capped.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Something that fetches URLs (swapped out in tests).
pub trait Fetch: Send + Sync + std::fmt::Debug {
    fn fetch(&self, url: &str) -> Result<Vec<u8>>;

    /// Like [`fetch`](Self::fetch), calling `progress` with the bytes so far
    /// now and then; when it returns `false` the download stops with
    /// [`Cancelled`].
    fn fetch_progress(&self, url: &str, progress: &mut dyn FnMut(u64) -> bool) -> Result<Vec<u8>> {
        if !progress(0) {
            return Err(Cancelled.into());
        }
        let bytes = self.fetch(url)?;
        if !progress(bytes.len() as u64) {
            return Err(Cancelled.into());
        }
        Ok(bytes)
    }
}

/// The user cancelled the operation (find it with `downcast_ref`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

impl Cancelled {
    /// `e` (or what it wraps) is a cancel.
    pub fn is(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.downcast_ref::<Cancelled>().is_some())
    }
}

/// How often a running download reports its size.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// The largest download goatty accepts (a release archive or a registry snapshot).
pub const MAX_DOWNLOAD: u64 = 256 << 20;

/// curl, falling back to wget, through a temp file in `cache/downloads`.
#[derive(Debug, Clone)]
pub struct Curl {
    pub cache: PathBuf,
}

impl Fetch for Curl {
    fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        download(url, &self.cache)
    }

    fn fetch_progress(&self, url: &str, progress: &mut dyn FnMut(u64) -> bool) -> Result<Vec<u8>> {
        download_with(url, &self.cache, progress)
    }
}

/// Download `url` (https only) via a temp file under `cache/downloads`.
pub fn download(url: &str, cache: &Path) -> Result<Vec<u8>> {
    download_with(url, cache, &mut |_| true)
}

/// [`download`], reporting the bytes so far to `progress` (which stops it
/// by returning `false`).
pub fn download_with(url: &str, cache: &Path, progress: &mut dyn FnMut(u64) -> bool) -> Result<Vec<u8>> {
    if !url.starts_with("https://") {
        bail!("refusing to download {url}: only https:// URLs");
    }
    let dir = cache.join("downloads");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let part = dir.join(format!("{}-{n}.part", std::process::id()));
    let result = fetch_to(url, &part, progress).and_then(|()| {
        let len = std::fs::metadata(&part)?.len();
        if len > MAX_DOWNLOAD {
            bail!("{url} is larger than {} MiB", MAX_DOWNLOAD >> 20);
        }
        std::fs::read(&part).context("read download")
    });
    let _ = std::fs::remove_file(&part);
    result
}

fn fetch_to(url: &str, out: &Path, progress: &mut dyn FnMut(u64) -> bool) -> Result<()> {
    let max = MAX_DOWNLOAD.to_string();
    let curl_args =
        vec!["--proto", "=https", "--proto-redir", "=https", "--tlsv1.2", "-fsSL", "--retry", "2", "--max-filesize", &max, "-o"];
    let wget_args = vec!["-q", "--https-only", "-O"];
    #[cfg(windows)]
    let attempts: [(&str, &[&str]); 3] = [("curl.exe", &curl_args), ("curl", &curl_args), ("wget", &wget_args)];
    #[cfg(not(windows))]
    let attempts: [(&str, &[&str]); 2] = [("curl", &curl_args), ("wget", &wget_args)];
    for (prog, args) in attempts {
        let mut cmd = Command::new(prog);
        cmd.args(args).arg(out).arg(url).stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let child = cmd.spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context(format!("run {prog}")),
        };
        let status = loop {
            if let Some(s) = child.try_wait().with_context(|| format!("wait for {prog}"))? {
                break s;
            }
            let so_far = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
            if !progress(so_far) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Cancelled.into());
            }
            std::thread::sleep(PROGRESS_EVERY);
        };
        if !status.success() {
            bail!("{prog} failed ({status}) for {url}");
        }
        if !progress(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0)) {
            return Err(Cancelled.into());
        }
        return Ok(());
    }
    bail!("neither curl nor wget is installed; install one of them to download")
}

/// Fixed responses, for tests.
#[derive(Debug, Default, Clone)]
pub struct Fixed(pub std::collections::BTreeMap<String, Vec<u8>>);

impl Fetch for Fixed {
    fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        self.0.get(url).cloned().with_context(|| format!("no such URL in the test fixture: {url}"))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Verify Windows's built-in Schannel `curl.exe` accepts our security flags.
    #[test]
    fn windows_curl_supports_required_flags() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = Command::new("curl.exe")
            .args(["--help", "all"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .output()
            .expect("Windows includes curl.exe");
        assert!(out.status.success(), "curl.exe --help all failed");
        let help = String::from_utf8_lossy(&out.stdout);
        for flag in ["--proto", "--proto-redir", "--tlsv1.2", "--max-filesize"] {
            assert!(help.contains(flag), "curl.exe help missing {flag}");
        }
    }
}
