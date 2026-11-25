//! Local IPC transport between goatty clients and `goattyd`:
//!
//! * **Unix:** `AF_UNIX` stream sockets (`UnixStream` / `UnixListener`).
//! * **Windows:** Named Pipes (`\\.\pipe\...`) with an owner-SID-only DACL,
//!   `PIPE_REJECT_REMOTE_CLIENTS`, and overlapped I/O so concurrent reads,
//!   writes, timeouts, and shutdowns never deadlock each other.

use std::io;
use std::path::Path;

use anyhow::Result;

#[cfg(unix)]
mod imp {
    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{Result, bail};

    /// A local byte stream to/from `goattyd` (`UnixStream` on Unix).
    #[derive(Debug)]
    pub struct LocalStream(UnixStream);

    impl LocalStream {
        pub fn connect(path: &Path) -> io::Result<Self> {
            UnixStream::connect(path).map(Self)
        }

        pub fn try_clone(&self) -> io::Result<Self> {
            self.0.try_clone().map(Self)
        }

        pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
            self.0.set_read_timeout(dur)
        }

        pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
            self.0.set_write_timeout(dur)
        }

        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.0.set_nonblocking(nonblocking)
        }

        pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
            self.0.shutdown(how)
        }

        /// Connected pair of streams, for unit tests.
        pub fn pair() -> io::Result<(Self, Self)> {
            let (a, b) = UnixStream::pair()?;
            Ok((Self(a), Self(b)))
        }

        /// The PID of the peer process on this connection, verified to belong
        /// to the current user (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` +
        /// `getpeereid` on macOS).
        pub fn peer_owner_pid(&self, path: &Path) -> Result<u32> {
            let (pid, uid) = peer_cred(&self.0)?;
            // SAFETY: getuid has no preconditions.
            let me = unsafe { libc::getuid() };
            if uid != me || pid <= 0 {
                bail!("{} belongs to another user (uid {uid})", path.display());
            }
            Ok(pid as u32)
        }
    }

    impl From<UnixStream> for LocalStream {
        fn from(s: UnixStream) -> Self {
            Self(s)
        }
    }

    impl Read for LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Read for &LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&self.0).read(buf)
        }
    }

    impl Write for LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    impl Write for &LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&self.0).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            (&self.0).flush()
        }
    }

    /// A listening local endpoint (`UnixListener` on Unix).
    #[derive(Debug)]
    pub struct LocalListener(UnixListener);

    impl LocalListener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            UnixListener::bind(path).map(Self)
        }

        pub fn accept(&self) -> io::Result<LocalStream> {
            self.0.accept().map(|(s, _)| LocalStream(s))
        }
    }

    impl From<UnixListener> for LocalListener {
        fn from(l: UnixListener) -> Self {
            Self(l)
        }
    }

    impl From<OwnedFd> for LocalListener {
        fn from(fd: OwnedFd) -> Self {
            Self(UnixListener::from(fd))
        }
    }

    impl AsRawFd for LocalListener {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_raw_fd()
        }
    }

    #[cfg(target_os = "linux")]
    fn peer_cred(stream: &UnixStream) -> Result<(libc::pid_t, libc::uid_t)> {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are valid for writes of the sizes passed, and
        // the fd stays open for the duration of the call.
        let rc = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &raw mut len) };
        if rc != 0 {
            bail!("SO_PEERCRED: {}", io::Error::last_os_error());
        }
        Ok((cred.pid, cred.uid))
    }

    #[cfg(target_os = "macos")]
    fn peer_cred(stream: &UnixStream) -> Result<(libc::pid_t, libc::uid_t)> {
        let fd = stream.as_raw_fd();
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: `pid` and `len` are valid for writes of the sizes passed, and
        // the fd stays open for the duration of the call.
        let rc = unsafe { libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &raw mut len) };
        if rc != 0 {
            bail!("LOCAL_PEERPID: {}", io::Error::last_os_error());
        }
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: `uid` and `gid` are valid for writes; the fd is open.
        if unsafe { libc::getpeereid(fd, &raw mut uid, &raw mut gid) } != 0 {
            bail!("getpeereid: {}", io::Error::last_os_error());
        }
        Ok((pid, uid))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn peer_cred(_stream: &UnixStream) -> Result<(libc::pid_t, libc::uid_t)> {
        bail!("peer credentials aren't supported on this OS")
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use parking_lot::{Mutex, RwLock};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_NOT_CONNECTED, FALSE, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, TRUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{EqualSid, GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe, SetNamedPipeHandleState, WaitNamedPipeW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, INFINITE, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, ResetEvent, SetEvent,
        WaitForMultipleObjects, WaitForSingleObject,
    };

    const PIPE_BUF_SIZE: u32 = 64 * 1024;
    const CONNECT_BUSY_WAIT: Duration = Duration::from_secs(3);

    fn to_wide_nul(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn last_error() -> u32 {
        // SAFETY: `GetLastError` has no preconditions.
        unsafe { GetLastError() }
    }

    struct OwnedHandle(HANDLE);

    // SAFETY: Win32 pipe and event kernel handles are safe to send and share
    // across threads; concurrent access is synchronized by `RwLock` and Win32
    // overlapped I/O.
    unsafe impl Send for OwnedHandle {}
    // SAFETY: see above.
    unsafe impl Sync for OwnedHandle {}

    impl OwnedHandle {
        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: `self.0` is a valid kernel handle owned exclusively by `self`.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn create_manual_event() -> io::Result<OwnedHandle> {
        // SAFETY: null attributes, manual reset (`TRUE`), initial state unsignaled (`FALSE`), unnamed.
        let h = unsafe { CreateEventW(std::ptr::null(), TRUE, FALSE, std::ptr::null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle(h))
    }

    /// Aligned storage holding a `TOKEN_USER` struct plus its trailing variable-length SID.
    pub(crate) struct TokenUserBuf(Vec<TOKEN_USER>);

    impl TokenUserBuf {
        /// Query the `TOKEN_USER` buffer for `process`.
        pub(crate) fn for_process(process: HANDLE) -> io::Result<Self> {
            let mut token: HANDLE = std::ptr::null_mut();
            // SAFETY: `process` is a valid process handle and `token` is a writable out-pointer.
            if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = OwnedHandle(token);
            let mut needed = 0u32;
            // SAFETY: first call probes the required buffer length in bytes.
            unsafe {
                GetTokenInformation(token.raw(), TokenUser, std::ptr::null_mut(), 0, &raw mut needed);
            }
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let elem_size = std::mem::size_of::<TOKEN_USER>();
            let count = (needed as usize).div_ceil(elem_size).max(1);
            let byte_len = u32::try_from(count.checked_mul(elem_size).ok_or_else(|| io::Error::other("TOKEN_USER size overflow"))?)
                .map_err(|_| io::Error::other("TOKEN_USER size overflow"))?;
            // SAFETY: `TOKEN_USER` is plain C data; all-zeros initialization is valid.
            let mut buf: Vec<TOKEN_USER> = vec![unsafe { std::mem::zeroed() }; count];
            // SAFETY: `buf` is aligned to `align_of::<TOKEN_USER>()` and has `byte_len >= needed` writable bytes.
            if unsafe { GetTokenInformation(token.raw(), TokenUser, buf.as_mut_ptr().cast(), byte_len, &raw mut needed) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if (needed as usize) < elem_size {
                return Err(io::Error::other("short TOKEN_USER buffer"));
            }
            Ok(Self(buf))
        }

        /// Pointer to the user SID, valid for as long as `&self` is alive.
        pub(crate) fn sid(&self) -> *mut std::ffi::c_void {
            self.0[0].User.Sid
        }
    }

    /// Build a Windows security descriptor with a protected DACL granting full
    /// access exclusively to the current process's user SID (`D:P(A;;GA;;;<SID>)`).
    struct OwnerSecurityDescriptor {
        sd: *mut std::ffi::c_void,
    }

    impl OwnerSecurityDescriptor {
        fn new() -> io::Result<Self> {
            // SAFETY: `GetCurrentProcess` returns a pseudo-handle that does not need closing.
            let me = unsafe { GetCurrentProcess() };
            let buf = TokenUserBuf::for_process(me)?;
            let sid = buf.sid();
            let mut sid_wide: *mut u16 = std::ptr::null_mut();
            // SAFETY: `sid` points inside `buf`, which lives across this call.
            if unsafe { ConvertSidToStringSidW(sid, &raw mut sid_wide) } == 0 || sid_wide.is_null() {
                return Err(io::Error::last_os_error());
            }
            let sid_str = {
                let mut len = 0usize;
                // SAFETY: `ConvertSidToStringSidW` returns a NUL-terminated UTF-16 string.
                while unsafe { *sid_wide.add(len) } != 0 {
                    len += 1;
                }
                // SAFETY: `len` stops before the terminating NUL within the allocated string.
                let slice = unsafe { std::slice::from_raw_parts(sid_wide, len) };
                let s = String::from_utf16_lossy(slice);
                // SAFETY: `sid_wide` was allocated by `ConvertSidToStringSidW` and must be freed via `LocalFree`.
                unsafe {
                    LocalFree(sid_wide.cast());
                }
                s
            };
            let sddl = to_wide_nul(&format!("D:P(A;;GA;;;{sid_str})"));
            let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `sddl` is a NUL-terminated UTF-16 string and `sd` is a valid out-pointer.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &raw mut sd, std::ptr::null_mut())
            } == 0
                || sd.is_null()
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { sd })
        }

        fn as_attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.sd,
                bInheritHandle: FALSE,
            }
        }
    }

    impl Drop for OwnerSecurityDescriptor {
        fn drop(&mut self) {
            if !self.sd.is_null() {
                // SAFETY: `self.sd` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
                unsafe {
                    LocalFree(self.sd);
                }
            }
        }
    }

    fn create_pipe_instance(wide_name: &[u16], first: bool) -> io::Result<OwnedHandle> {
        let sd = OwnerSecurityDescriptor::new()?;
        let mut sa = sd.as_attributes();
        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // Use message mode so `Shutdown::Write` can transmit a zero-length
        // message as an in-band half-close EOF signal while preserving full
        // byte-stream semantics (`ERROR_MORE_DATA` returns partial chunks).
        let pipe_mode = PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        // SAFETY: `wide_name` is NUL-terminated UTF-16 and `sa` is valid for the duration of the call.
        let h = unsafe {
            CreateNamedPipeW(
                wide_name.as_ptr(),
                open_mode,
                pipe_mode,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUF_SIZE,
                PIPE_BUF_SIZE,
                0,
                &raw mut sa,
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle(h))
    }

    fn timeout_ms(dur: Option<Duration>) -> u32 {
        match dur {
            None => INFINITE,
            Some(d) => u32::try_from(d.as_millis()).unwrap_or(u32::MAX - 1),
        }
    }

    fn is_eof_err(code: u32) -> bool {
        matches!(code, ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
    }

    struct PipeInner {
        /// `None` once `shutdown(Shutdown::Both)` closed the underlying handle.
        handle: RwLock<Option<OwnedHandle>>,
        /// Serializes non-empty writes and the final zero-length EOF marker so
        /// half-close always lands strictly after every preceding payload byte.
        write_mu: Mutex<()>,
        read_closed: AtomicBool,
        write_closed: AtomicBool,
        read_closed_event: OwnedHandle,
        write_closed_event: OwnedHandle,
        both_closed_event: OwnedHandle,
        read_timeout_event: OwnedHandle,
        read_timeout: Mutex<Option<Duration>>,
        write_timeout: Mutex<Option<Duration>>,
        nonblocking: AtomicBool,
    }

    impl PipeInner {
        fn new(handle: OwnedHandle) -> io::Result<Arc<Self>> {
            Ok(Arc::new(Self {
                handle: RwLock::new(Some(handle)),
                write_mu: Mutex::new(()),
                read_closed: AtomicBool::new(false),
                write_closed: AtomicBool::new(false),
                read_closed_event: create_manual_event()?,
                write_closed_event: create_manual_event()?,
                both_closed_event: create_manual_event()?,
                read_timeout_event: create_manual_event()?,
                read_timeout: Mutex::new(None),
                write_timeout: Mutex::new(None),
                nonblocking: AtomicBool::new(false),
            }))
        }

        fn read_buf(&self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let guard = self.handle.read();
            if self.read_closed.load(Ordering::Acquire) {
                return Ok(0);
            }
            let Some(handle) = guard.as_ref() else {
                return Ok(0);
            };
            let raw = handle.raw();

            if self.nonblocking.load(Ordering::Acquire) {
                let mut avail = 0u32;
                // SAFETY: `raw` is a valid pipe handle and `avail` is a valid out-pointer.
                if unsafe { PeekNamedPipe(raw, std::ptr::null_mut(), 0, std::ptr::null_mut(), &raw mut avail, std::ptr::null_mut()) } == 0 {
                    let err = last_error();
                    if is_eof_err(err) {
                        return Ok(0);
                    }
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
                if avail == 0 {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
            }

            let io_event = create_manual_event()?;
            // SAFETY: `OVERLAPPED` is plain C data; zeroed with `hEvent` set is required.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = io_event.raw();
            let to_read = buf.len().min(u32::MAX as usize) as u32;
            // SAFETY: `raw` is open for the duration of `guard`, `buf` has `to_read` writable
            // bytes, and `ov` stays pinned on the stack until the overlapped I/O completes or
            // is cancelled and awaited via `GetOverlappedResult(..., TRUE)`.
            let ok = unsafe { ReadFile(raw, buf.as_mut_ptr(), to_read, std::ptr::null_mut(), &raw mut ov) };
            let mut transferred = 0u32;
            if ok != 0 {
                // SAFETY: `ov` just completed synchronously.
                if unsafe { GetOverlappedResult(raw, &raw const ov, &raw mut transferred, FALSE) } == 0 {
                    let err = last_error();
                    if is_eof_err(err) {
                        return Ok(0);
                    }
                    if err != ERROR_MORE_DATA {
                        return Err(io::Error::from_raw_os_error(err as i32));
                    }
                }
                if transferred == 0 {
                    // Peer's zero-length half-close message.
                    self.read_closed.store(true, Ordering::Release);
                }
                return Ok(transferred as usize);
            }

            let err = last_error();
            if err == ERROR_MORE_DATA {
                // Synchronous partial read of a message larger than `buf`:
                // retrieve how many bytes filled `buf` and let the caller keep reading.
                // SAFETY: `ov` completed synchronously with `ERROR_MORE_DATA`.
                let _ = unsafe { GetOverlappedResult(raw, &raw const ov, &raw mut transferred, FALSE) };
                return Ok(transferred as usize);
            }
            if err != ERROR_IO_PENDING {
                if is_eof_err(err) {
                    return Ok(0);
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }

            // Clear any stale timeout-change wakeup from before this read began.
            // SAFETY: `read_timeout_event` is a valid event handle owned by `self`.
            unsafe {
                ResetEvent(self.read_timeout_event.raw());
            }
            let handles = [io_event.raw(), self.read_closed_event.raw(), self.read_timeout_event.raw()];
            let wait = loop {
                let wait_ms = timeout_ms(*self.read_timeout.lock());
                // SAFETY: all three event handles are valid for the duration of this call.
                let w = unsafe { WaitForMultipleObjects(3, handles.as_ptr(), FALSE, wait_ms) };
                if w == WAIT_OBJECT_0 + 2 {
                    // `set_read_timeout` updated the timeout while we were waiting:
                    // reset and re-wait on the same in-flight overlapped read.
                    // SAFETY: `read_timeout_event` is a valid event handle owned by `self`.
                    unsafe {
                        ResetEvent(self.read_timeout_event.raw());
                    }
                    continue;
                }
                break w;
            };
            if wait == WAIT_OBJECT_0 {
                // SAFETY: I/O event signaled; `GetOverlappedResult` retrieves the result.
                if unsafe { GetOverlappedResult(raw, &raw const ov, &raw mut transferred, FALSE) } == 0 {
                    let err = last_error();
                    if is_eof_err(err) {
                        return Ok(0);
                    }
                    if err != ERROR_MORE_DATA {
                        return Err(io::Error::from_raw_os_error(err as i32));
                    }
                }
                if transferred == 0 {
                    // Peer's zero-length half-close message.
                    self.read_closed.store(true, Ordering::Release);
                }
                return Ok(transferred as usize);
            }

            // Shutdown signaled or timeout: cancel and wait for kernel completion so
            // `buf` and `ov` are never freed while still referenced by the kernel.
            // SAFETY: `raw` is open and `ov` is still alive on this stack frame.
            unsafe {
                CancelIoEx(raw, &raw const ov);
                let completed = GetOverlappedResult(raw, &raw const ov, &raw mut transferred, TRUE);
                if !self.read_closed.load(Ordering::Acquire) {
                    if completed != 0 {
                        if transferred == 0 {
                            self.read_closed.store(true, Ordering::Release);
                        }
                        return Ok(transferred as usize);
                    }
                    if last_error() == ERROR_MORE_DATA && transferred > 0 {
                        return Ok(transferred as usize);
                    }
                }
            }
            if wait == WAIT_TIMEOUT { Err(io::Error::new(io::ErrorKind::TimedOut, "named pipe read timed out")) } else { Ok(0) }
        }

        fn write_buf(&self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let _write_guard = self.write_mu.lock();
            let guard = self.handle.read();
            if self.write_closed.load(Ordering::Acquire) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            let Some(handle) = guard.as_ref() else {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            };
            let raw = handle.raw();

            let io_event = create_manual_event()?;
            // SAFETY: `OVERLAPPED` is plain C data; zeroed with `hEvent` set is required.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = io_event.raw();
            let to_write = buf.len().min(PIPE_BUF_SIZE as usize) as u32;
            // SAFETY: `raw` is open for `guard`, `buf` is valid for `to_write` bytes, and
            // `ov` stays alive until `GetOverlappedResult` completes.
            let ok = unsafe { WriteFile(raw, buf.as_ptr(), to_write, std::ptr::null_mut(), &raw mut ov) };
            let mut transferred = 0u32;
            if ok != 0 {
                // SAFETY: `ov` completed synchronously.
                if unsafe { GetOverlappedResult(raw, &raw const ov, &raw mut transferred, FALSE) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(transferred as usize);
            }

            let err = last_error();
            if err != ERROR_IO_PENDING {
                if is_eof_err(err) {
                    return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }

            let handles = [io_event.raw(), self.write_closed_event.raw()];
            let wait_ms = timeout_ms(*self.write_timeout.lock());
            // SAFETY: both event handles are valid for this call.
            let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, wait_ms) };
            if wait == WAIT_OBJECT_0 {
                // SAFETY: I/O event signaled; retrieve result.
                if unsafe { GetOverlappedResult(raw, &raw const ov, &raw mut transferred, FALSE) } == 0 {
                    let err = last_error();
                    if is_eof_err(err) {
                        return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                    }
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
                return Ok(transferred as usize);
            }

            // SAFETY: cancel pending write and await kernel completion before dropping `buf`/`ov`.
            unsafe {
                CancelIoEx(raw, &raw const ov);
                let completed = GetOverlappedResult(raw, &raw const ov, &raw mut transferred, TRUE);
                if completed != 0 && !self.write_closed.load(Ordering::Acquire) {
                    return Ok(transferred as usize);
                }
            }
            if wait == WAIT_TIMEOUT {
                Err(io::Error::new(io::ErrorKind::TimedOut, "named pipe write timed out"))
            } else {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        }

        /// Transmit a zero-length message on the message-mode pipe to signal
        /// write-direction EOF without closing the reverse read direction.
        fn send_write_eof(&self) -> io::Result<()> {
            let guard = self.handle.read();
            let Some(handle) = guard.as_ref() else {
                // Handle already closed (`Shutdown::Both`): peer already sees EOF.
                return Ok(());
            };
            let raw = handle.raw();
            let io_event = create_manual_event()?;
            // SAFETY: `OVERLAPPED` is plain C data; zeroed with `hEvent` set is required.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = io_event.raw();
            let dummy = [0u8; 1];
            // SAFETY: `raw` is open for `guard`, `dummy` is valid for 0 bytes, and
            // `ov` stays alive until `GetOverlappedResult` completes.
            let ok = unsafe { WriteFile(raw, dummy.as_ptr(), 0, std::ptr::null_mut(), &raw mut ov) };
            let mut transferred = 0u32;
            if ok != 0 {
                // SAFETY: `ov` completed synchronously.
                if unsafe { GetOverlappedResult(raw, &raw const ov, &raw mut transferred, FALSE) } == 0 {
                    let err = last_error();
                    if is_eof_err(err) {
                        return Ok(());
                    }
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
                return Ok(());
            }
            let err = last_error();
            if err != ERROR_IO_PENDING {
                if is_eof_err(err) {
                    return Ok(());
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            let handles = [io_event.raw(), self.both_closed_event.raw()];
            let wait_ms = timeout_ms(Some(self.write_timeout.lock().unwrap_or(Duration::from_secs(5))));
            // SAFETY: both event handles are valid for this call.
            let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, wait_ms) };
            if wait == WAIT_OBJECT_0 {
                // SAFETY: I/O event signaled; finalize overlapped state.
                if unsafe { GetOverlappedResult(raw, &raw const ov, &raw mut transferred, FALSE) } == 0 {
                    let err = last_error();
                    if is_eof_err(err) {
                        return Ok(());
                    }
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
                return Ok(());
            }
            // SAFETY: cancel pending zero-length write before dropping `ov`.
            let completed = unsafe {
                CancelIoEx(raw, &raw const ov);
                GetOverlappedResult(raw, &raw const ov, &raw mut transferred, TRUE)
            };
            if completed != 0 {
                return Ok(());
            }
            if wait == WAIT_TIMEOUT { Err(io::Error::new(io::ErrorKind::TimedOut, "named pipe write-EOF timed out")) } else { Ok(()) }
        }

        fn shutdown(&self, how: Shutdown) -> io::Result<()> {
            match how {
                Shutdown::Read => {
                    self.read_closed.store(true, Ordering::Release);
                    // SAFETY: `read_closed_event` is a valid event handle owned by `self`.
                    unsafe {
                        SetEvent(self.read_closed_event.raw());
                    }
                    Ok(())
                }
                Shutdown::Write => {
                    if self.write_closed.swap(true, Ordering::AcqRel) {
                        return Ok(());
                    }
                    // Wake any in-flight `write_buf` so it releases `write_mu`.
                    // SAFETY: `write_closed_event` is a valid event handle owned by `self`.
                    unsafe {
                        SetEvent(self.write_closed_event.raw());
                    }
                    let _write_guard = self.write_mu.lock();
                    self.send_write_eof()
                }
                Shutdown::Both => {
                    self.read_closed.store(true, Ordering::Release);
                    self.write_closed.store(true, Ordering::Release);
                    // SAFETY: all three event handles are valid and owned by `self`.
                    unsafe {
                        SetEvent(self.read_closed_event.raw());
                        SetEvent(self.write_closed_event.raw());
                        SetEvent(self.both_closed_event.raw());
                    }
                    // Waking the events causes all in-flight overlapped reads/writes to
                    // cancel and drop their `handle` read guards immediately; taking the
                    // handle here closes the pipe so the peer sees EOF right away.
                    let _ = self.handle.write().take();
                    Ok(())
                }
            }
        }
    }

    /// A local byte stream to/from `goattyd` (overlapped Named Pipe on Windows).
    #[derive(Clone)]
    pub struct LocalStream(Arc<PipeInner>);

    impl std::fmt::Debug for LocalStream {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LocalStream").finish_non_exhaustive()
        }
    }

    impl LocalStream {
        pub fn connect(path: &Path) -> io::Result<Self> {
            let pipe_name = goatty_proto::windows_pipe_name(path);
            let wide = to_wide_nul(&pipe_name);
            let deadline = Instant::now() + CONNECT_BUSY_WAIT;
            loop {
                // SAFETY: `wide` is NUL-terminated UTF-16. `SECURITY_SQOS_PRESENT |
                // SECURITY_IDENTIFICATION` prevents a malicious pipe server from
                // impersonating the client token.
                let h = unsafe {
                    CreateFileW(
                        wide.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                        std::ptr::null_mut(),
                    )
                };
                if h != INVALID_HANDLE_VALUE && !h.is_null() {
                    let handle = OwnedHandle(h);
                    let mode = PIPE_READMODE_MESSAGE;
                    // SAFETY: `handle.raw()` is open with `GENERIC_READ | GENERIC_WRITE`
                    // (which includes `FILE_WRITE_ATTRIBUTES`), and `mode` is valid.
                    if unsafe { SetNamedPipeHandleState(handle.raw(), &raw const mode, std::ptr::null(), std::ptr::null()) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    return PipeInner::new(handle).map(Self);
                }
                let err = last_error();
                if err == ERROR_PIPE_BUSY {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::Error::from_raw_os_error(err as i32));
                    }
                    let wait_ms = (left.as_millis() as u32).clamp(1, 250);
                    // SAFETY: `wide` is NUL-terminated UTF-16.
                    unsafe {
                        WaitNamedPipeW(wide.as_ptr(), wait_ms);
                    }
                    continue;
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }
        }

        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(self.clone())
        }

        pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
            *self.0.read_timeout.lock() = dur;
            // Wake any in-flight `read_buf` so the updated timeout takes effect immediately.
            // SAFETY: `read_timeout_event` is a valid event handle owned by `self.0`.
            unsafe {
                SetEvent(self.0.read_timeout_event.raw());
            }
            Ok(())
        }

        pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
            *self.0.write_timeout.lock() = dur;
            Ok(())
        }

        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.0.nonblocking.store(nonblocking, Ordering::Release);
            Ok(())
        }

        pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
            self.0.shutdown(how)
        }

        /// Connected pair of Named Pipe streams, for unit tests.
        pub fn pair() -> io::Result<(Self, Self)> {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!(r"\\.\pipe\goatty-pair-{}-{id}", std::process::id());
            let wide = to_wide_nul(&name);
            let server = create_pipe_instance(&wide, true)?;
            let client = Self::connect(Path::new(&name))?;

            let io_event = create_manual_event()?;
            // SAFETY: `OVERLAPPED` is plain C data; zeroed with `hEvent` set.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = io_event.raw();
            // SAFETY: `server.raw()` is a valid named pipe handle and `ov` stays alive.
            let ok = unsafe { ConnectNamedPipe(server.raw(), &raw mut ov) };
            if ok == 0 {
                let err = last_error();
                if err == ERROR_IO_PENDING {
                    let mut dummy = 0u32;
                    // SAFETY: wait for the pending connect to finish.
                    if unsafe { GetOverlappedResult(server.raw(), &raw const ov, &raw mut dummy, TRUE) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else if err != ERROR_PIPE_CONNECTED {
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
            }
            let server_stream = PipeInner::new(server).map(Self)?;
            Ok((server_stream, client))
        }

        /// Raw server PID reported by `GetNamedPipeServerProcessId` (without
        /// verifying ownership; callers that need ownership verification should
        /// use [`Self::peer_owner_pid`] or check the SID on a retained handle).
        pub(crate) fn server_pid(&self) -> Result<u32> {
            let guard = self.0.handle.read();
            let handle = guard.as_ref().context("pipe is closed")?;
            let mut pid = 0u32;
            // SAFETY: `handle.raw()` is open and `pid` is a valid out-pointer.
            if unsafe { GetNamedPipeServerProcessId(handle.raw(), &raw mut pid) } == 0 || pid == 0 {
                bail!("GetNamedPipeServerProcessId: {}", io::Error::last_os_error());
            }
            Ok(pid)
        }

        /// The PID of the server process listening on this Named Pipe
        /// (`GetNamedPipeServerProcessId`), verified to run under the same
        /// user SID as this process.
        pub fn peer_owner_pid(&self, path: &Path) -> Result<u32> {
            let pid = self.server_pid()?;

            // Verify the server process belongs to the current user SID.
            // SAFETY: `GetCurrentProcess` returns a pseudo-handle.
            let my_buf = TokenUserBuf::for_process(unsafe { GetCurrentProcess() }).context("read current user SID")?;
            // SAFETY: plain Win32 call; handle checked and closed on drop.
            let peer_proc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
            if peer_proc.is_null() {
                bail!("{} belongs to another user or inaccessible process (pid {pid})", path.display());
            }
            let peer_proc = OwnedHandle(peer_proc);
            let peer_buf = TokenUserBuf::for_process(peer_proc.raw())
                .with_context(|| format!("{} belongs to another user or inaccessible process (pid {pid})", path.display()))?;
            // SAFETY: both `TokenUserBuf`s stay alive across `EqualSid`.
            let same = unsafe { EqualSid(my_buf.sid(), peer_buf.sid()) != 0 };
            if !same {
                bail!("{} belongs to another user (pid {pid})", path.display());
            }
            Ok(pid)
        }
    }

    impl Read for LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read_buf(buf)
        }
    }

    impl Read for &LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read_buf(buf)
        }
    }

    impl Write for LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write_buf(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for &LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write_buf(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A listening Windows Named Pipe server (`\\.\pipe\...`).
    pub struct LocalListener {
        wide_name: Vec<u16>,
        pending: Mutex<Option<OwnedHandle>>,
    }

    impl std::fmt::Debug for LocalListener {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LocalListener").finish_non_exhaustive()
        }
    }

    impl LocalListener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            let pipe_name = goatty_proto::windows_pipe_name(path);
            let wide_name = to_wide_nul(&pipe_name);
            let first = create_pipe_instance(&wide_name, true)?;
            Ok(Self { wide_name, pending: Mutex::new(Some(first)) })
        }

        pub fn accept(&self) -> io::Result<LocalStream> {
            let mut slot = self.pending.lock();
            let server = match slot.take() {
                Some(h) => h,
                None => create_pipe_instance(&self.wide_name, false)?,
            };

            let io_event = create_manual_event()?;
            // SAFETY: `OVERLAPPED` is plain C data; zeroed with `hEvent` set.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = io_event.raw();
            // SAFETY: `server.raw()` is a valid pipe handle and `ov` stays alive until wait finishes.
            let ok = unsafe { ConnectNamedPipe(server.raw(), &raw mut ov) };
            if ok == 0 {
                let err = last_error();
                if err == ERROR_IO_PENDING {
                    // SAFETY: wait for the pending connect event.
                    let wait = unsafe { WaitForSingleObject(io_event.raw(), INFINITE) };
                    if wait != WAIT_OBJECT_0 {
                        // SAFETY: cancel before dropping `ov`.
                        unsafe {
                            CancelIoEx(server.raw(), &raw const ov);
                            let mut dummy = 0u32;
                            GetOverlappedResult(server.raw(), &raw const ov, &raw mut dummy, TRUE);
                        }
                        return Err(io::Error::last_os_error());
                    }
                    let mut dummy = 0u32;
                    // SAFETY: connect event signaled; retrieve result.
                    if unsafe { GetOverlappedResult(server.raw(), &raw const ov, &raw mut dummy, FALSE) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else if err != ERROR_PIPE_CONNECTED {
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
            }

            // Immediately create the next listening pipe instance so connecting
            // clients never observe a gap with zero available instances.
            match create_pipe_instance(&self.wide_name, false) {
                Ok(next) => *slot = Some(next),
                Err(e) => log::warn!("failed to pre-create next named pipe instance: {e}"),
            }
            drop(slot);

            PipeInner::new(server).map(LocalStream)
        }
    }
}

#[cfg(windows)]
pub(crate) use imp::TokenUserBuf;
pub use imp::{LocalListener, LocalStream};

/// Iterator over incoming connections from a [`LocalListener`].
pub struct Incoming<'a> {
    listener: &'a LocalListener,
}

impl Iterator for Incoming<'_> {
    type Item = io::Result<LocalStream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept())
    }
}

impl LocalListener {
    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { listener: self }
    }
}

/// Convenience helper for checking whether anything is currently accepting
/// connections at `path` (without keeping the connection open).
pub fn is_listening(path: &Path) -> bool {
    LocalStream::connect(path).is_ok()
}

/// Verify the local peer listening on `path` belongs to this user and return
/// its PID.
pub fn peer_pid(path: &Path) -> Result<u32> {
    let stream = LocalStream::connect(path)?;
    stream.peer_owner_pid(path)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::time::Duration;

    use super::*;

    #[test]
    fn pair_supports_concurrent_duplex_and_shutdown() {
        let (mut a, mut b) = LocalStream::pair().unwrap();
        let a_writer = a.try_clone().unwrap();
        let handle = std::thread::spawn(move || {
            let mut w = a_writer;
            w.write_all(b"ping").unwrap();
            let mut resp = [0u8; 4];
            a.read_exact(&mut resp).unwrap();
            assert_eq!(&resp, b"pong");
            // Wait for peer shutdown -> EOF.
            let mut eof = [0u8; 1];
            assert_eq!(a.read(&mut eof).unwrap(), 0);
        });
        let mut req = [0u8; 4];
        b.read_exact(&mut req).unwrap();
        assert_eq!(&req, b"ping");
        b.write_all(b"pong").unwrap();
        b.shutdown(Shutdown::Both).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn listener_accepts_multiple_clients_and_reports_owner_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc-test.sock");
        let listener = LocalListener::bind(&path).unwrap();
        let p = path.clone();
        let server = std::thread::spawn(move || {
            for i in 0u8..2 {
                let mut s = listener.accept().unwrap();
                s.write_all(&[i + 1]).unwrap();
            }
        });
        for expected in 1u8..=2 {
            let mut client = LocalStream::connect(&p).unwrap();
            assert_eq!(client.peer_owner_pid(&p).unwrap(), std::process::id());
            let mut buf = [0u8; 1];
            client.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], expected);
        }
        server.join().unwrap();
    }

    #[test]
    fn read_timeout_returns_timed_out_without_corrupting_stream() {
        let (a, mut b) = LocalStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_millis(80))).unwrap();
        let mut buf = [0u8; 4];
        let err = b.read(&mut buf).unwrap_err();
        assert!(matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{err:?}");
        // Stream remains usable after a read timeout.
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut a = a;
        a.write_all(b"ok!").unwrap();
        let mut out = [0u8; 3];
        b.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"ok!");
    }

    #[test]
    fn shutdown_write_signals_peer_eof_while_allowing_delayed_replies() {
        let (mut server, mut client) = LocalStream::pair().unwrap();
        let srv = std::thread::spawn(move || {
            let mut req = Vec::new();
            server.read_to_end(&mut req).unwrap();
            assert_eq!(req, b"request");
            // Simulate a daemon taking longer than any short idle timeout
            // before sending its final reply.
            std::thread::sleep(Duration::from_millis(400));
            server.write_all(b"delayed-reply").unwrap();
            drop(server);
        });
        client.write_all(b"request").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"delayed-reply");
        srv.join().unwrap();
    }

    #[test]
    fn shutdown_write_stops_continuous_server_output() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (server, mut client) = LocalStream::pair().unwrap();
        let mut server_reader = server.try_clone().unwrap();
        let mut server_writer = server;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_r = stop.clone();

        // Writer streams continuously until the reader observes client write-EOF,
        // then exits cleanly and drops the server end (mirroring `goattyd`).
        let srv_write = std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if server_writer.write_all(b"tick").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(server_writer);
        });
        let srv_read = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while let Ok(n) = server_reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            stop_r.store(true, Ordering::Release);
            drop(server_reader);
        });

        // Observe at least one chunk of continuous output, then half-close.
        let mut first = [0u8; 4];
        client.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"tick");
        let start = std::time::Instant::now();
        client.shutdown(Shutdown::Write).unwrap();
        let mut tail = Vec::new();
        client.read_to_end(&mut tail).unwrap();
        assert!(start.elapsed() < Duration::from_secs(3), "continuous stream did not terminate promptly after Shutdown::Write");
        srv_read.join().unwrap();
        srv_write.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn shutdown_write_propagates_timeout_when_peer_stalls() {
        let (_stalled_peer, mut client) = LocalStream::pair().unwrap();
        client.set_write_timeout(Some(Duration::from_millis(100))).unwrap();
        // Fill the pipe's 64 KiB buffer without reading from `_stalled_peer`
        // until writes backpressure and time out.
        let chunk = vec![0xAAu8; 64 * 1024];
        let mut filled = false;
        for _ in 0..16 {
            match client.write(&chunk) {
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    filled = true;
                    break;
                }
                Err(e) => panic!("unexpected write error while filling pipe: {e:?}"),
            }
        }
        assert!(filled, "pipe never backpressured");
        let err = client.shutdown(Shutdown::Write).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "expected TimedOut from stalled write-EOF, got {err:?}");
    }

    #[cfg(windows)]
    #[test]
    fn set_read_timeout_wakes_inflight_read() {
        let (mut a, mut b) = LocalStream::pair().unwrap();
        let b_ctl = b.try_clone().unwrap();
        let started = std::sync::Arc::new(std::sync::Barrier::new(2));
        let timed_out = std::sync::Arc::new(std::sync::Barrier::new(2));
        let s = started.clone();
        let t = timed_out.clone();
        let reader = std::thread::spawn(move || {
            s.wait();
            let mut buf = [0u8; 4];
            let err = b.read(&mut buf).unwrap_err();
            assert!(matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{err:?}");
            b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            t.wait();
            let mut msg = [0u8; 4];
            b.read_exact(&mut msg).unwrap();
            assert_eq!(&msg, b"done");
        });
        started.wait();
        // Let the reader enter its blocking read with no timeout, then tighten it.
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        b_ctl.set_read_timeout(Some(Duration::from_millis(80))).unwrap();
        timed_out.wait();
        assert!(start.elapsed() < Duration::from_secs(3), "in-flight read did not wake on timeout update");
        a.write_all(b"done").unwrap();
        reader.join().unwrap();
    }

    #[test]
    fn large_payload_partial_reads_preserve_byte_stream() {
        let (mut server, mut client) = LocalStream::pair().unwrap();
        let payload: Vec<u8> = (0..100_000usize).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();
        let writer = std::thread::spawn(move || {
            server.write_all(&payload).unwrap();
            server.shutdown(Shutdown::Write).unwrap();
        });
        let mut received = Vec::with_capacity(expected.len());
        let mut small = [0u8; 7];
        loop {
            let n = client.read(&mut small).unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&small[..n]);
        }
        writer.join().unwrap();
        assert_eq!(received, expected);
    }
}
