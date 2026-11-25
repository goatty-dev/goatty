//! `goattyd proxy`: the remote end of every non-local transport.

use std::io::{Read, Write};

use anyhow::Result;

/// `goattyd proxy`: relay stdio ⇄ the daemon socket/pipe, starting the daemon
/// if needed. This is the remote end of every non-local transport
/// (`ssh host goattyd proxy`). It never parses the protocol.
pub fn proxy() -> Result<()> {
    let exe = std::env::current_exe().ok();
    let sock = goatty_client::connect_or_spawn_daemon(exe)?;
    let mut to_daemon = sock.try_clone()?;
    let mut from_daemon = sock;
    let (mut stdin, mut stdout) = raw_stdio()?;
    let up = std::thread::spawn(move || {
        relay(&mut *stdin, &mut to_daemon);
        // Signals write-EOF to the daemon on both Unix sockets (`SHUT_WR`) and
        // Windows message-mode Named Pipes (zero-length EOF message), so the
        // daemon finishes any in-flight replies and closes its end cleanly.
        // If signaling write-EOF fails (e.g. timed out on a stalled pipe), fail
        // closed by shutting down both directions so the proxy never hangs.
        if let Err(e) = to_daemon.shutdown(std::net::Shutdown::Write) {
            log::warn!("proxy: write-EOF failed ({e}); closing daemon connection");
            let _ = to_daemon.shutdown(std::net::Shutdown::Both);
        }
    });
    relay(&mut from_daemon, &mut *stdout);
    // Daemon closed the connection: we're done, whatever stdin is doing.
    drop(up);
    Ok(())
}

/// Raw, unbuffered binary stdin/stdout handles that must not be closed on drop.
#[cfg(unix)]
fn raw_stdio() -> Result<(std::mem::ManuallyDrop<std::fs::File>, std::mem::ManuallyDrop<std::fs::File>)> {
    use std::os::fd::FromRawFd;
    // Raw fds: std's Stdout is line-buffered, which would hold binary
    // frames back until a 0x0A byte happens to pass through.
    // SAFETY: fds 0/1 are owned by this process for its whole life and
    // nothing else in `proxy` uses them.
    let stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    // SAFETY: as above.
    let stdout = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) });
    Ok((stdin, stdout))
}

/// Raw binary stdin/stdout handles on Windows (`GetStdHandle` + CRT
/// `_setmode(O_BINARY)` so `\n` ↔ `\r\n` translation never corrupts binary
/// postcard frames).
#[cfg(windows)]
fn raw_stdio() -> Result<(std::mem::ManuallyDrop<std::fs::File>, std::mem::ManuallyDrop<std::fs::File>)> {
    use anyhow::Context;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};

    const O_BINARY: i32 = 0x8000;
    unsafe extern "C" {
        fn _setmode(fd: i32, mode: i32) -> i32;
    }

    // SAFETY: fds 0 and 1 are the CRT standard input/output descriptors; setting
    // binary mode prevents CR/LF translation on binary frames.
    unsafe {
        let _ = _setmode(0, O_BINARY);
        let _ = _setmode(1, O_BINARY);
    }

    // SAFETY: `GetStdHandle` has no preconditions; returned handles are validated below.
    let in_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if in_handle.is_null() || in_handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("GetStdHandle(STD_INPUT_HANDLE)");
    }
    // SAFETY: as above.
    let out_handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if out_handle.is_null() || out_handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("GetStdHandle(STD_OUTPUT_HANDLE)");
    }

    // SAFETY: standard handles live for the process's lifetime and `ManuallyDrop`
    // ensures `File::drop` never closes them prematurely.
    let stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(in_handle) });
    // SAFETY: as above.
    let stdout = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(out_handle) });
    Ok((stdin, stdout))
}

/// Plain read/write relay. Deliberately not `std::io::copy`: on Linux it
/// uses `splice()`, and data spliced from a pipe into a Unix socket can sit
/// in the kernel instead of being delivered (observed: the client's Hello
/// never reached the daemon), which would deadlock the handshake.
fn relay(from: &mut impl Read, to: &mut impl Write) {
    let mut buf = vec![0u8; 64 << 10];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}
