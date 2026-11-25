//! Windows NTFS junctions and hard-link binary activation without elevation
//! or Developer Mode (#278).
//!
//! * Directory links (`current`, `plugins/<id>`) use NTFS mount points
//!   (`IO_REPARSE_TAG_MOUNT_POINT`), which require no elevated privileges.
//!   Updating an existing junction calls `FSCTL_SET_REPARSE_POINT` in place so
//!   there is no window where the link is absent.
//! * Executable links (`bin/<exe>.exe`) use NTFS hard links with a copy
//!   fallback when hard-linking is unavailable (e.g. cross-volume or FAT32).

#![cfg(windows)]

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: `self.0` is a valid Win32 handle owned exclusively by this guard.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn wide_nul(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Strip the Win32 verbatim prefix (`\\?\` or `\\?\UNC\`) so paths compare
/// cleanly against normal `PathBuf`s.
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// Build the `REPARSE_DATA_BUFFER` payload for `IO_REPARSE_TAG_MOUNT_POINT`.
fn mount_point_buffer(target: &Path) -> Result<Vec<u8>> {
    let abs =
        std::fs::canonicalize(target).or_else(|_| std::path::absolute(target)).with_context(|| format!("resolve {}", target.display()))?;
    let clean = strip_verbatim(&abs);
    let clean_str = clean.to_string_lossy();
    let sub_str = if let Some(unc) = clean_str.strip_prefix(r"\\") { format!(r"\??\UNC\{unc}") } else { format!(r"\??\{clean_str}") };
    let sub_u16: Vec<u16> = OsStr::new(&sub_str).encode_wide().collect();
    let print_u16: Vec<u16> = OsStr::new(clean_str.as_ref()).encode_wide().collect();
    let sub_bytes = u16::try_from(sub_u16.len() * 2).context("junction target path is too long")?;
    let print_bytes = u16::try_from(print_u16.len() * 2).context("junction print path is too long")?;
    let path_buf_bytes = usize::from(sub_bytes) + 2 + usize::from(print_bytes) + 2;
    let reparse_data_len = u16::try_from(8 + path_buf_bytes).context("junction reparse data is too large")?;

    let mut buf = Vec::with_capacity(8 + usize::from(reparse_data_len));
    buf.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buf.extend_from_slice(&reparse_data_len.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    buf.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    buf.extend_from_slice(&sub_bytes.to_le_bytes()); // SubstituteNameLength
    buf.extend_from_slice(&(sub_bytes + 2).to_le_bytes()); // PrintNameOffset
    buf.extend_from_slice(&print_bytes.to_le_bytes()); // PrintNameLength
    for unit in sub_u16 {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    buf.extend_from_slice(&0u16.to_le_bytes());
    for unit in print_u16 {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    buf.extend_from_slice(&0u16.to_le_bytes());
    Ok(buf)
}

fn set_reparse_point(dir: &Path, buf: &[u8]) -> io::Result<()> {
    let w = wide_nul(dir.as_os_str());
    // SAFETY: `w` is a NUL-terminated UTF-16 path; `CreateFileW` opens an
    // existing directory/reparse point handle for setting its reparse data.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(h);
    let mut returned: u32 = 0;
    // SAFETY: `handle.0` is valid and `buf` is a well-formed REPARSE_DATA_BUFFER
    // of length `buf.len()`.
    let ok = unsafe {
        DeviceIoControl(handle.0, FSCTL_SET_REPARSE_POINT, buf.as_ptr().cast(), buf.len() as u32, null_mut(), 0, &mut returned, null_mut())
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn is_reparse_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && (m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0)
}

/// Create or atomically retarget an NTFS directory junction at `link` pointing to `target`.
pub fn replace_junction(target: &Path, link: &Path) -> Result<()> {
    let dir = link.parent().context("junction has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let buf = mount_point_buffer(target)?;

    if is_reparse_dir(link) {
        return set_reparse_point(link, &buf).with_context(|| format!("update junction {}", link.display()));
    }
    if let Ok(meta) = std::fs::symlink_metadata(link) {
        if meta.is_dir() {
            bail!("{} already exists and is a regular directory", link.display());
        }
        let _ = std::fs::remove_file(link);
    }

    let name = link.file_name().context("junction has no name")?.to_string_lossy();
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir(&tmp);
    let tmp_w = wide_nul(tmp.as_os_str());
    // SAFETY: `tmp_w` is NUL-terminated UTF-16; creates an empty staging directory.
    let created = unsafe { CreateDirectoryW(tmp_w.as_ptr(), null()) };
    if created == 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("create {}", tmp.display()));
    }
    if let Err(e) = set_reparse_point(&tmp, &buf) {
        let _ = std::fs::remove_dir(&tmp);
        return Err(e).with_context(|| format!("set junction on {}", tmp.display()));
    }
    match std::fs::rename(&tmp, link) {
        Ok(()) => Ok(()),
        Err(_) if is_reparse_dir(link) => {
            let _ = std::fs::remove_dir(&tmp);
            set_reparse_point(link, &buf).with_context(|| format!("update junction {}", link.display()))
        }
        Err(e) => {
            let _ = std::fs::remove_dir(&tmp);
            Err(e).with_context(|| format!("activate junction {}", link.display()))
        }
    }
}

/// Read the target of an NTFS junction (or symlink), stripping any `\\?\` prefix.
pub fn read_junction(link: &Path) -> io::Result<PathBuf> {
    std::fs::read_link(link).map(|p| strip_verbatim(&p))
}

/// Remove an NTFS junction without touching its target directory's contents.
pub fn remove_junction(link: &Path) -> io::Result<()> {
    std::fs::remove_dir(link)
}

/// Place executable `target` at `link` via an NTFS hard link (falling back to
/// copying when hard-linking fails), replacing any existing file at `link`.
pub fn replace_bin(target: &Path, link: &Path) -> Result<()> {
    let dir = link.parent().context("link has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = link.file_name().context("link has no name")?.to_string_lossy();
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(target, &tmp).is_err() {
        std::fs::copy(target, &tmp).with_context(|| format!("copy {} to {}", target.display(), tmp.display()))?;
    }
    if let Err(first_err) = std::fs::rename(&tmp, link) {
        // On Windows a running `.exe` cannot be overwritten directly, but it
        // can be renamed aside so the new binary takes its place.
        let old = dir.join(format!(".{name}.old-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_file(&old);
        if std::fs::rename(link, &old).is_ok() {
            match std::fs::rename(&tmp, link) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&old);
                    return Ok(());
                }
                Err(e) => {
                    let _ = std::fs::rename(&old, link);
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e).with_context(|| format!("activate {}", link.display()));
                }
            }
        }
        let _ = std::fs::remove_file(&tmp);
        return Err(first_err).with_context(|| format!("activate {}", link.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junction_create_retarget_read_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("gen1");
        let d2 = tmp.path().join("gen2");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d1.join("a.txt"), "one").unwrap();
        std::fs::write(d2.join("a.txt"), "two").unwrap();

        let link = tmp.path().join("current");
        replace_junction(&d1, &link).unwrap();
        assert_eq!(std::fs::read_to_string(link.join("a.txt")).unwrap(), "one");
        assert_eq!(read_junction(&link).unwrap().file_name().unwrap(), "gen1");

        // In-place retargeting.
        replace_junction(&d2, &link).unwrap();
        assert_eq!(std::fs::read_to_string(link.join("a.txt")).unwrap(), "two");
        assert_eq!(read_junction(&link).unwrap().file_name().unwrap(), "gen2");

        // Removing the junction leaves the target directory untouched.
        remove_junction(&link).unwrap();
        assert!(!link.exists());
        assert_eq!(std::fs::read_to_string(d2.join("a.txt")).unwrap(), "two");
    }

    #[test]
    fn hardlink_binary_activation() {
        let tmp = tempfile::tempdir().unwrap();
        let src1 = tmp.path().join("v1.exe");
        let src2 = tmp.path().join("v2.exe");
        std::fs::write(&src1, b"v1").unwrap();
        std::fs::write(&src2, b"v2").unwrap();
        let bin = tmp.path().join("bin/tool.exe");

        replace_bin(&src1, &bin).unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), b"v1");
        replace_bin(&src2, &bin).unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), b"v2");
    }
}
