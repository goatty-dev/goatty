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
                check_private(path)?;
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

/// Refuse a key file someone else could have planted or can read: it must
/// belong to this user and be readable by nobody else.
#[cfg(unix)]
fn check_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    if m.uid() != me {
        bail!("{} belongs to another user (uid {}): refusing to use it as this computer's goatty key", path.display(), m.uid());
    }
    if m.mode() & 0o077 != 0 {
        bail!(
            "{} is readable by other users (mode {:o}): refusing to use it as this computer's goatty key; `chmod 600` it if it is yours",
            path.display(),
            m.mode() & 0o777
        );
    }
    Ok(())
}

#[cfg(windows)]
mod win_acl {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;

    use anyhow::{Context, Result, bail};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_SUCCESS, FALSE, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetAclInformation, GetTokenInformation, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, TOKEN_QUERY,
        TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_READ_DATA};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 6;
    const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
    const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
    const ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 12;

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid Win32 handle owned exclusively by `self`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct LocalMem(*mut std::ffi::c_void);
    impl Drop for LocalMem {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was allocated by a Win32 API requiring `LocalFree`.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    struct TokenUserBuf(Vec<TOKEN_USER>);
    impl TokenUserBuf {
        fn current_user() -> io::Result<Self> {
            let mut token: HANDLE = std::ptr::null_mut();
            // SAFETY: `GetCurrentProcess` returns a pseudo-handle; `token` is a writable out-pointer.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = OwnedHandle(token);
            let mut needed = 0u32;
            // SAFETY: probes the required buffer length in bytes.
            unsafe {
                GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &raw mut needed);
            }
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let elem_size = std::mem::size_of::<TOKEN_USER>();
            let count = (needed as usize).div_ceil(elem_size).max(1);
            let byte_len = u32::try_from(count.checked_mul(elem_size).ok_or_else(|| io::Error::other("TOKEN_USER size overflow"))?)
                .map_err(|_| io::Error::other("TOKEN_USER size overflow"))?;
            // SAFETY: `TOKEN_USER` is plain C data; all-zeros is valid.
            let mut buf: Vec<TOKEN_USER> = vec![unsafe { std::mem::zeroed() }; count];
            // SAFETY: `buf` is aligned for `TOKEN_USER` and has `byte_len >= needed` bytes.
            if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), byte_len, &raw mut needed) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if (needed as usize) < elem_size {
                return Err(io::Error::other("short TOKEN_USER buffer"));
            }
            Ok(Self(buf))
        }

        fn sid(&self) -> *mut std::ffi::c_void {
            self.0[0].User.Sid
        }
    }

    fn to_wide_nul(path: &Path) -> Vec<u16> {
        OsStr::new(path.as_os_str()).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn well_known_sid(kind: i32) -> io::Result<Vec<u64>> {
        let mut size = SECURITY_MAX_SID_SIZE;
        let words = (size as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buf = vec![0u64; words];
        // SAFETY: `buf` is 8-byte aligned and holds `size` writable bytes.
        if unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &raw mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf)
    }

    pub(super) fn current_user_sid_string() -> Result<String> {
        let me = TokenUserBuf::current_user().context("read current user SID")?;
        let mut sid_wide: *mut u16 = std::ptr::null_mut();
        // SAFETY: `me.sid()` lives for the duration of `me`.
        if unsafe { ConvertSidToStringSidW(me.sid(), &raw mut sid_wide) } == 0 || sid_wide.is_null() {
            return Err(io::Error::last_os_error()).context("ConvertSidToStringSidW");
        }
        let _guard = LocalMem(sid_wide.cast());
        let mut len = 0usize;
        // SAFETY: `ConvertSidToStringSidW` returns a NUL-terminated UTF-16 string.
        while unsafe { *sid_wide.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` stops before the terminating NUL.
        Ok(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_wide, len) }))
    }

    /// Create a brand-new file exclusively (`dwShareMode = 0`) with a protected
    /// owner-only DACL (`D:P(A;;GA;;;<USER_SID>)`) applied atomically at
    /// creation time so no other account can ever open the file during a
    /// create-then-chmod window.
    pub(crate) fn create_private_file(path: &Path) -> Result<std::fs::File> {
        let sid_str = current_user_sid_string()?;
        let sddl: Vec<u16> = OsStr::new(&format!("D:P(A;;GA;;;{sid_str})")).encode_wide().chain(std::iter::once(0)).collect();
        let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated UTF-16 and `sd` is a valid out-pointer.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &raw mut sd, std::ptr::null_mut())
        } == 0
            || sd.is_null()
        {
            return Err(io::Error::last_os_error()).context("ConvertStringSecurityDescriptorToSecurityDescriptorW");
        }
        let _sd_guard = LocalMem(sd);
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: FALSE,
        };
        let wide_path = to_wide_nul(path);
        // SAFETY: `wide_path` is NUL-terminated UTF-16 and `sa.lpSecurityDescriptor`
        // stays alive for the duration of `CreateFileW`.
        let handle = unsafe {
            CreateFileW(wide_path.as_ptr(), GENERIC_WRITE, 0, &raw const sa, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut())
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(io::Error::last_os_error()).with_context(|| format!("create private file {}", path.display()));
        }
        // SAFETY: `handle` is a newly created, valid Win32 file handle owned exclusively by us.
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }

    pub(crate) fn check_private(path: &Path) -> Result<()> {
        let me = TokenUserBuf::current_user().context("read current user SID")?;
        let system = well_known_sid(WinLocalSystemSid).context("build LocalSystem SID")?;
        let admins = well_known_sid(WinBuiltinAdministratorsSid).context("build Administrators SID")?;

        let wide_path = to_wide_nul(path);
        let mut owner: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `wide_path` is NUL-terminated UTF-16; `sd` is freed via `LocalFree` below.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &raw mut owner,
                std::ptr::null_mut(),
                &raw mut dacl,
                std::ptr::null_mut(),
                &raw mut sd,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32)).with_context(|| format!("read ACL of {}", path.display()));
        }
        let _sd_guard = LocalMem(sd);

        let is_allowed_sid = |sid: *mut std::ffi::c_void| -> bool {
            if sid.is_null() {
                return false;
            }
            // SAFETY: all SID pointers remain alive for this closure.
            unsafe {
                EqualSid(sid, me.sid()) != 0
                    || EqualSid(sid, system.as_ptr().cast_mut().cast()) != 0
                    || EqualSid(sid, admins.as_ptr().cast_mut().cast()) != 0
            }
        };

        if !is_allowed_sid(owner) {
            bail!("{} belongs to another user: refusing to use it as this computer's goatty key", path.display());
        }
        // A NULL DACL grants Everyone full access on Windows.
        if dacl.is_null() {
            bail!("{} has no DACL (world-accessible): refusing to use it as this computer's goatty key", path.display());
        }

        let mut info = ACL_SIZE_INFORMATION { AceCount: 0, AclBytesInUse: 0, AclBytesFree: 0 };
        // SAFETY: `dacl` points inside `sd` and `info` has exact size of `ACL_SIZE_INFORMATION`.
        if unsafe {
            GetAclInformation(dacl, (&raw mut info).cast(), std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32, AclSizeInformation)
        } == 0
        {
            return Err(io::Error::last_os_error()).with_context(|| format!("inspect ACL of {}", path.display()));
        }

        let read_mask = FILE_READ_DATA | FILE_GENERIC_READ | GENERIC_READ | GENERIC_ALL;
        for i in 0..info.AceCount {
            let mut ace_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `i < info.AceCount` and `dacl` is valid.
            if unsafe { GetAce(dacl, i, &raw mut ace_ptr) } == 0 || ace_ptr.is_null() {
                return Err(io::Error::last_os_error()).with_context(|| format!("read ACE {i} of {}", path.display()));
            }
            // SAFETY: every ACE starts with `ACE_HEADER`; both `ACCESS_ALLOWED_ACE`
            // and `ACCESS_ALLOWED_CALLBACK_ACE` share the exact leading layout
            // (`ACE_HEADER`, `Mask: u32`, `SidStart: u32`).
            unsafe {
                let header = *(ace_ptr.cast::<windows_sys::Win32::Security::ACE_HEADER>());
                match header.AceType {
                    ACCESS_DENIED_ACE_TYPE
                    | ACCESS_DENIED_OBJECT_ACE_TYPE
                    | ACCESS_DENIED_CALLBACK_ACE_TYPE
                    | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {}
                    ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {
                        let allowed = ace_ptr.cast::<ACCESS_ALLOWED_ACE>();
                        let mask = (*allowed).Mask;
                        let sid = (&raw mut (*allowed).SidStart).cast::<std::ffi::c_void>();
                        if (mask & read_mask) != 0 && !is_allowed_sid(sid) {
                            bail!("{} is readable by other users: refusing to use it as this computer's goatty key", path.display());
                        }
                    }
                    other => {
                        bail!(
                            "{} has unsupported access-control entry (type {other}): refusing to use it as this computer's goatty key",
                            path.display()
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) use win_acl::create_private_file;

#[cfg(windows)]
fn check_private(path: &Path) -> Result<()> {
    win_acl::check_private(path)
}

#[cfg(not(any(unix, windows)))]
fn check_private(path: &Path) -> Result<()> {
    let _ = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
    Ok(())
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tailnet/device.key");
        let a = Identity::load_or_create(&path).unwrap();
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public(), b.public());
        assert!(is_public_key(&a.public()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
            // A key others can read isn't used.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let err = Identity::load_or_create(&path).unwrap_err();
            assert!(err.to_string().contains("readable by other users"), "{err:#}");
        }
        #[cfg(windows)]
        {
            use std::ffi::OsStr;
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
            use windows_sys::Win32::Security::Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
            };
            use windows_sys::Win32::Security::{
                ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
            };

            // 1. Creation-time DACL isolation: even inside a directory where
            // Everyone has inheritable full control, `load_or_create` creates
            // the key with a protected owner-only DACL at `CreateFileW` time.
            let shared_dir = dir.path().join("shared-parent");
            std::fs::create_dir_all(&shared_dir).unwrap();
            let out = std::process::Command::new("icacls.exe")
                .arg(&shared_dir)
                .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
                .output()
                .expect("run icacls.exe");
            assert!(out.status.success(), "icacls shared dir failed: {}", String::from_utf8_lossy(&out.stderr));
            let shared_key = shared_dir.join("device.key");
            let created = Identity::load_or_create(&shared_key).unwrap();
            let reloaded = Identity::load_or_create(&shared_key).unwrap();
            assert_eq!(created.public(), reloaded.public());

            // 2. Standard allow ACE: granting Everyone (S-1-1-0) read access is rejected.
            let out = std::process::Command::new("icacls.exe").arg(&path).args(["/grant", "*S-1-1-0:R"]).output().expect("run icacls.exe");
            assert!(out.status.success(), "icacls failed: {}", String::from_utf8_lossy(&out.stderr));
            let err = Identity::load_or_create(&path).unwrap_err();
            assert!(err.to_string().contains("readable by other users"), "{err:#}");

            // 3. Conditional allow ACE (`ACCESS_ALLOWED_CALLBACK_ACE_TYPE`, SDDL `XA`)
            // granting Everyone read access is also rejected.
            let me_sid = win_acl::current_user_sid_string().unwrap();
            let cond_sddl: Vec<u16> = OsStr::new(&format!("D:P(A;;GA;;;{me_sid})(XA;;FR;;;WD;(Member_of {{SID(WD)}}))"))
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `cond_sddl` is NUL-terminated UTF-16 and `sd` is freed via `LocalFree` below.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(cond_sddl.as_ptr(), SDDL_REVISION_1, &raw mut sd, std::ptr::null_mut())
            };
            assert_ne!(ok, 0, "ConvertStringSecurityDescriptorToSecurityDescriptorW failed: {}", std::io::Error::last_os_error());
            let mut dacl_present = 0;
            let mut dacl_defaulted = 0;
            let mut dacl: *mut ACL = std::ptr::null_mut();
            // SAFETY: `sd` is a valid security descriptor allocated above.
            let got_dacl = unsafe { GetSecurityDescriptorDacl(sd, &raw mut dacl_present, &raw mut dacl, &raw mut dacl_defaulted) };
            assert_ne!(got_dacl, 0);
            assert!(dacl_present != 0 && !dacl.is_null());
            let wide_path: Vec<u16> = OsStr::new(shared_key.as_os_str()).encode_wide().chain(std::iter::once(0)).collect();
            // SAFETY: `wide_path` is NUL-terminated UTF-16 and `dacl` lives inside `sd`.
            let status = unsafe {
                SetNamedSecurityInfoW(
                    wide_path.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    dacl,
                    std::ptr::null(),
                )
            };
            // SAFETY: `sd` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
            unsafe {
                LocalFree(sd);
            }
            assert_eq!(status, ERROR_SUCCESS, "SetNamedSecurityInfoW failed: {status}");
            let err = Identity::load_or_create(&shared_key).unwrap_err();
            assert!(err.to_string().contains("readable by other users"), "{err:#}");
        }
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
