//! macOS launchd socket activation (`launch_activate_socket(3)`).
//!
//! When `goattyd` is started on demand by the LaunchAgent
//! (`~/Library/LaunchAgents/dev.goatty.goattyd.plist`, see
//! `goatty_client::service`), launchd owns the listening Unix socket and hands
//! it to `goattyd serve` when it checks in under
//! [`goatty_client::service::LAUNCHD_SOCKET_NAME`].
//!
//! Off macOS this is a no-op returning `None`.

#[cfg(unix)]
use std::os::fd::OwnedFd;

/// Claim the listening socket launchd created for this job, marked
/// close-on-exec so spawned shells never inherit it. Only the first call
/// queries launchd.
#[cfg(unix)]
pub(crate) fn activate_socket() -> Option<OwnedFd> {
    #[cfg(target_os = "macos")]
    {
        activate_socket_macos()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn activate_socket_macos() -> Option<OwnedFd> {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};

    unsafe extern "C" {
        fn launch_activate_socket(name: *const libc::c_char, fds: *mut *mut libc::c_int, cnt: *mut libc::size_t) -> libc::c_int;
    }

    static TAKEN: AtomicBool = AtomicBool::new(false);
    if TAKEN.swap(true, Ordering::SeqCst) {
        return None;
    }

    let name = goatty_client::service::LAUNCHD_SOCKET_NAME;
    let c_name = CString::new(name).expect("LAUNCHD_SOCKET_NAME has no NUL");
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut cnt: libc::size_t = 0;

    // SAFETY: `c_name` is a valid NUL-terminated string, and `fds` / `cnt` are
    // valid out-pointers for `launch_activate_socket(3)`.
    let err = unsafe { launch_activate_socket(c_name.as_ptr(), &mut fds, &mut cnt) };
    if err != 0 {
        if !fds.is_null() {
            // SAFETY: defensively free any buffer launchd may have allocated.
            unsafe { libc::free(fds.cast()) };
        }
        match err {
            libc::ESRCH | libc::ENOENT => {
                log::debug!("launch_activate_socket({name:?}): not launched with a launchd socket (errno {err})");
            }
            _ => {
                log::warn!("launch_activate_socket({name:?}) failed: {}", std::io::Error::from_raw_os_error(err));
            }
        }
        return None;
    }

    if fds.is_null() || cnt == 0 {
        if !fds.is_null() {
            // SAFETY: `fds` was allocated by `launch_activate_socket(3)`.
            unsafe { libc::free(fds.cast()) };
        }
        return None;
    }

    // SAFETY: `launch_activate_socket` succeeded and populated `cnt` file
    // descriptors in `fds`.
    let raw_fds: Vec<libc::c_int> = unsafe { std::slice::from_raw_parts(fds, cnt) }.to_vec();
    // SAFETY: `fds` was heap-allocated by `launch_activate_socket(3)` and the
    // caller owns freeing it.
    unsafe { libc::free(fds.cast()) };

    let mut iter = raw_fds.into_iter();
    let first = iter.next()?;
    for extra in iter {
        log::warn!("ignoring extra launchd socket fd {extra}");
        // SAFETY: `extra` is an open fd launchd handed exclusively to us.
        unsafe { libc::close(extra) };
    }

    if let Err(e) = goatty_term::pty::set_cloexec(first, true) {
        log::warn!("launchd socket fd {first} couldn't be marked close-on-exec: {e}");
        // SAFETY: `first` is an open fd launchd handed exclusively to us.
        unsafe { libc::close(first) };
        return None;
    }

    // SAFETY: launchd transferred ownership of `first` to this process and
    // `TAKEN` ensures nothing else adopted it.
    Some(unsafe { OwnedFd::from_raw_fd(first) })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn outside_launchd_returns_none() {
        assert!(activate_socket().is_none());
        // Second call is also a safe no-op.
        assert!(activate_socket().is_none());
    }
}
