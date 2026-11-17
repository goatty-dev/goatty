//! Remote path resolution, stat, and bounded file reads for remote file links.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use goatty_proto::{HARD_MAX_FILE_BYTES, RemotePathInfo, ReqId, ServerMsg, SessionId};

use crate::server::Daemon;

/// Maximum number of candidate paths checked in a single `StatPath` request.
const MAX_CANDIDATES: usize = 16;

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Lexically normalize `.` and `..` without hitting the filesystem (so symlinks
/// keep their logical paths).
pub(crate) fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push(Component::ParentDir.as_os_str());
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

fn join_path(raw: &str, cwd: Option<&str>) -> Option<PathBuf> {
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    if raw == "~" {
        home_dir()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        Some(home_dir()?.join(rest))
    } else {
        let p = Path::new(raw);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            let cwd = cwd.filter(|c| !c.is_empty() && !c.contains('\0'))?;
            let cwd_path = Path::new(cwd);
            if !cwd_path.is_absolute() {
                return None;
            }
            Some(cwd_path.join(p))
        }
    }
}

/// Resolve `raw` against `cwd` (for relative paths) or `$HOME` (for `~` / `~/`).
/// Returns `None` if `raw` is empty, contains NUL, or cannot be anchored.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn resolve_path(raw: &str, cwd: Option<&str>) -> Option<PathBuf> {
    join_path(raw, cwd).map(|joined| normalize_lexical(&joined))
}

/// Resolve `raw` for filesystem access. Paths containing `..` use kernel resolution
/// via `canonicalize` so symlinks in `cwd` or intermediate components resolve physically;
/// all other paths preserve logical symlink names.
fn resolve_for_io(raw: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let joined = join_path(raw, cwd)?;
    if joined.components().any(|c| matches!(c, Component::ParentDir)) {
        Some(std::fs::canonicalize(&joined).unwrap_or(joined))
    } else {
        Some(normalize_lexical(&joined))
    }
}

/// Stat the first existing regular file or directory among `candidates`.
pub(crate) fn stat_path(daemon: &Daemon, session: Option<SessionId>, candidates: &[String]) -> Option<RemotePathInfo> {
    let cwd = session.and_then(|id| daemon.get(id)).and_then(|s| s.current_cwd());
    for raw in candidates.iter().take(MAX_CANDIDATES) {
        let Some(resolved) = resolve_for_io(raw, cwd.as_deref()) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&resolved) else {
            continue;
        };
        if !meta.is_file() && !meta.is_dir() {
            continue;
        }
        return Some(RemotePathInfo { path: resolved.to_string_lossy().into_owned(), is_dir: meta.is_dir(), size: meta.len() });
    }
    None
}

/// Read up to `max_bytes` from a regular file on the daemon's host.
pub(crate) fn read_file(req: ReqId, raw_path: &str, max_bytes: u64) -> ServerMsg {
    match read_file_inner(raw_path, max_bytes) {
        Ok((path, data)) => ServerMsg::FileData { req, path, data },
        Err(e) => ServerMsg::Error { req: Some(req), message: format!("{e:#}") },
    }
}

fn read_file_inner(raw_path: &str, max_bytes: u64) -> Result<(String, Vec<u8>)> {
    let resolved = resolve_for_io(raw_path, None).with_context(|| format!("invalid remote path: {raw_path}"))?;
    let limit = max_bytes.min(HARD_MAX_FILE_BYTES);

    let pre = std::fs::metadata(&resolved).with_context(|| format!("cannot access {}", resolved.display()))?;
    if !pre.is_file() {
        bail!("{} is not a regular file", resolved.display());
    }
    if pre.len() > limit {
        bail!("{} is {} bytes (limit is {limit} bytes)", resolved.display(), pre.len());
    }

    // Open non-blocking without acquiring a controlling tty so FIFOs or device
    // nodes raced into place cannot stall the worker thread.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(&resolved)
        .with_context(|| format!("cannot open {}", resolved.display()))?;

    let meta = file.metadata().with_context(|| format!("cannot stat {}", resolved.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", resolved.display());
    }
    if meta.len() > limit {
        bail!("{} is {} bytes (limit is {limit} bytes)", resolved.display(), meta.len());
    }

    let cap = usize::try_from(meta.len().min(limit).saturating_add(1)).unwrap_or(usize::MAX);
    let mut data = Vec::with_capacity(cap);
    file.take(limit.saturating_add(1)).read_to_end(&mut data).with_context(|| format!("cannot read {}", resolved.display()))?;
    if data.len() as u64 > limit {
        bail!("{} exceeds the {limit}-byte limit", resolved.display());
    }

    Ok((resolved.to_string_lossy().into_owned(), data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalization_handles_dots_and_root() {
        assert_eq!(normalize_lexical(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(normalize_lexical(Path::new("/../../etc/passwd")), PathBuf::from("/etc/passwd"));
        assert_eq!(normalize_lexical(Path::new("/")), PathBuf::from("/"));
    }

    #[test]
    fn resolve_path_handles_abs_relative_and_invalid() {
        assert_eq!(resolve_path("/var/log/../log/syslog", Some("/tmp")), Some(PathBuf::from("/var/log/syslog")));
        assert_eq!(resolve_path("src/main.rs", Some("/home/user/repo")), Some(PathBuf::from("/home/user/repo/src/main.rs")));
        assert_eq!(resolve_path("../other.rs", Some("/home/user/repo")), Some(PathBuf::from("/home/user/other.rs")));
        assert_eq!(resolve_path("src/main.rs", None), None);
        assert_eq!(resolve_path("", Some("/tmp")), None);
        assert_eq!(resolve_path("bad\0path", Some("/tmp")), None);
        assert_eq!(resolve_path("rel", Some("not-abs")), None);
    }

    #[test]
    fn resolve_for_io_handles_symlinked_cwd_with_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("srv/proj");
        let real_sub = real_parent.join("sub");
        let link_parent = dir.path().join("home");
        std::fs::create_dir_all(&real_sub).unwrap();
        std::fs::create_dir_all(&link_parent).unwrap();
        std::fs::write(real_parent.join("sibling.txt"), "real-sibling").unwrap();
        std::fs::write(link_parent.join("sibling.txt"), "wrong-sibling").unwrap();

        let sym = link_parent.join("link");
        std::os::unix::fs::symlink(&real_sub, &sym).unwrap();

        let resolved = resolve_for_io("../sibling.txt", Some(sym.to_str().unwrap())).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(real_parent.join("sibling.txt")).unwrap());
        assert_eq!(std::fs::read_to_string(&resolved).unwrap(), "real-sibling");
    }

    #[test]
    fn read_file_rejects_sparse_file_above_hard_max_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("sparse.bin");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(HARD_MAX_FILE_BYTES + 1).unwrap();

        let err = read_file_inner(big.to_str().unwrap(), u64::MAX).unwrap_err();
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
    }

    #[test]
    fn read_file_handles_zero_stat_size_proc_files() {
        let proc_stat = Path::new("/proc/self/stat");
        if !proc_stat.exists() {
            return;
        }
        let (path, data) = read_file_inner("/proc/self/stat", 64 * 1024).unwrap();
        assert_eq!(path, "/proc/self/stat");
        assert!(!data.is_empty());

        let err = read_file_inner("/proc/self/stat", 1).unwrap_err();
        assert!(format!("{err:#}").contains("exceeds"), "{err:#}");
    }
}
