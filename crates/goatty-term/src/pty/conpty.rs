//! Which ConPTY creates goatty's pseudoconsoles (#356).
//!
//! The ConPTY built into Windows 10/11 predates Windows Terminal 1.22: it
//! strips image protocols (kitty, sixel, iTerm2), breaks the screen and cursor
//! when a TUI leaves the alternate screen, and repaints its own reflowed
//! buffer over ours on resize. Windows releases bundle Microsoft's
//! `Microsoft.Windows.Console.ConPTY` package beside `goatty.exe` and
//! `goattyd.exe` (`conpty.dll`, plus `x64\OpenConsole.exe` and
//! `arm64\OpenConsole.exe`, which `conpty.dll` starts for each pseudoconsole;
//! see `scripts/fetch-conpty.ps1`). When it's there it is used; otherwise
//! (dev builds, the standalone `goattyd-x86_64-windows.exe` on a remote host)
//! the in-box API is.
//!
//! `conpty.dll` is only ever loaded from the executable's own folder by full
//! path, and its dependencies only from there and System32: never from `PATH`
//! or the working directory.

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{E_INVALIDARG, HANDLE};
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::core::HRESULT;

/// `CreatePseudoConsole` flag: on resize, don't repaint the reflowed buffer
/// over the terminal's own reflow (microsoft/terminal#16911).
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;

type CreateFn = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type ResizeFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type CloseFn = unsafe extern "system" fn(HPCON);

/// Where the ConPTY in use comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backend {
    /// `conpty.dll` beside the executable (this path).
    Bundled(PathBuf),
    /// The pseudoconsole built into Windows.
    Inbox,
}

struct Api {
    create: CreateFn,
    resize: ResizeFn,
    close: CloseFn,
    backend: Backend,
}

static API: OnceLock<Api> = OnceLock::new();

fn api() -> &'static Api {
    API.get_or_init(|| {
        let api = match load_bundled() {
            Ok(Some(api)) => api,
            Ok(None) => inbox(),
            Err(e) => {
                log::warn!("ConPTY: {e}; using the in-box ConPTY");
                inbox()
            }
        };
        match &api.backend {
            Backend::Bundled(path) => log::info!("ConPTY: bundled {}", path.display()),
            Backend::Inbox => log::info!("ConPTY: in-box (no conpty.dll beside the executable)"),
        }
        api
    })
}

fn inbox() -> Api {
    Api { create: CreatePseudoConsole, resize: ResizePseudoConsole, close: ClosePseudoConsole, backend: Backend::Inbox }
}

/// `<exe dir>\conpty.dll`, if it exists.
fn bundled_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("conpty.dll");
    path.is_file().then_some(path)
}

/// `Ok(None)` when there is no `conpty.dll`; `Err` when there is one that
/// can't be used.
fn load_bundled() -> Result<Option<Api>, String> {
    let Some(path) = bundled_path() else {
        return Ok(None);
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated absolute path. LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
    // resolves the DLL's own dependencies from its folder, SYSTEM32 from System32,
    // and nothing else (no PATH, no working directory).
    let module =
        unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(format!("can't load {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    // The module stays loaded for the life of the process: pseudoconsoles
    // created through it may live that long.
    let symbol = |name: &'static [u8]| {
        // SAFETY: `module` is a loaded module and `name` a NUL-terminated ASCII string.
        unsafe { GetProcAddress(module, name.as_ptr()) }
            .ok_or_else(|| format!("{} has no {}", path.display(), String::from_utf8_lossy(&name[..name.len() - 1])))
    };
    let (create, resize, close) =
        (symbol(b"ConptyCreatePseudoConsole\0")?, symbol(b"ConptyResizePseudoConsole\0")?, symbol(b"ConptyClosePseudoConsole\0")?);
    // SAFETY: conpty.h declares these exports with exactly these signatures
    // (WINAPI = "system"); function pointers of different types share a layout.
    let api = unsafe {
        Api {
            create: std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateFn>(create),
            resize: std::mem::transmute::<unsafe extern "system" fn() -> isize, ResizeFn>(resize),
            close: std::mem::transmute::<unsafe extern "system" fn() -> isize, CloseFn>(close),
            backend: Backend::Bundled(path),
        }
    };
    Ok(Some(api))
}

/// Create a pseudoconsole with [`PSEUDOCONSOLE_RESIZE_QUIRK`], or without it
/// on a Windows that rejects the flag.
///
/// # Safety
/// `input` and `output` must be open pipe handles.
pub(super) unsafe fn create(size: COORD, input: HANDLE, output: HANDLE) -> Result<HPCON, HRESULT> {
    let api = api();
    with_resize_quirk(|flags| {
        let mut hpcon: HPCON = 0;
        // SAFETY: the caller guarantees the handles; `hpcon` is a valid out pointer.
        let hr = unsafe { (api.create)(size, input, output, flags, &raw mut hpcon) };
        if hr < 0 { Err(hr) } else { Ok(hpcon) }
    })
}

/// Calls `create` with [`PSEUDOCONSOLE_RESIZE_QUIRK`], and again with no
/// flags if that is `E_INVALIDARG` (a ConPTY that doesn't know the flag).
fn with_resize_quirk<T>(mut create: impl FnMut(u32) -> Result<T, HRESULT>) -> Result<T, HRESULT> {
    match create(PSEUDOCONSOLE_RESIZE_QUIRK) {
        Err(E_INVALIDARG) => {
            log::info!("ConPTY: PSEUDOCONSOLE_RESIZE_QUIRK not supported; creating without it");
            create(0)
        }
        r => r,
    }
}

/// # Safety
/// `hpcon` must be an open pseudoconsole from [`create`].
pub(super) unsafe fn resize(hpcon: HPCON, size: COORD) -> HRESULT {
    // SAFETY: the caller guarantees `hpcon`.
    unsafe { (api().resize)(hpcon, size) }
}

/// # Safety
/// `hpcon` must be an open pseudoconsole from [`create`]; it is closed
/// afterwards and must not be used again.
pub(super) unsafe fn close(hpcon: HPCON) {
    // SAFETY: the caller guarantees `hpcon`.
    unsafe { (api().close)(hpcon) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resize_quirk_is_dropped_only_when_rejected() {
        let mut seen = Vec::new();
        let r = with_resize_quirk(|f| {
            seen.push(f);
            if f == 0 { Ok(()) } else { Err(E_INVALIDARG) }
        });
        assert_eq!((r, seen), (Ok(()), vec![PSEUDOCONSOLE_RESIZE_QUIRK, 0]));

        let mut seen = Vec::new();
        let r = with_resize_quirk(|f| {
            seen.push(f);
            Err::<(), _>(-1)
        });
        assert_eq!((r, seen), (Err(-1), vec![PSEUDOCONSOLE_RESIZE_QUIRK]));
    }

    /// With `conpty.dll` beside the test binary (CI's Windows job stages it
    /// there with `scripts/fetch-conpty.ps1`), it is the one in use, and it
    /// starts its own `OpenConsole.exe` instead of falling back to the in-box
    /// conhost (which it does silently when `<arch>\OpenConsole.exe` is missing).
    #[test]
    fn a_bundled_conpty_dll_is_used() {
        let Some(path) = bundled_path() else {
            assert_eq!(api().backend, Backend::Inbox);
            return;
        };
        assert_eq!(api().backend, Backend::Bundled(path));

        let cmd = crate::pty::ShellCommand { program: "cmd.exe".into(), args: vec!["/Q".into(), "/K".into()], cwd: None, env: Vec::new() };
        let (mut pty, reader) = crate::pty::Pty::spawn(&cmd, crate::TermSize::new(80, 24)).unwrap();
        let hosts = child_names(std::process::id());
        // Nobody reads the output: drop it so closing the console can't block on it.
        drop(reader);
        pty.kill();
        pty.close_console();
        assert!(hosts.iter().any(|n| n.eq_ignore_ascii_case("OpenConsole.exe")), "no OpenConsole.exe child: {hosts:?}");
    }

    /// Image names of `pid`'s child processes.
    fn child_names(pid: u32) -> Vec<String> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        };
        // SAFETY: plain snapshot call; the handle is checked below and closed at the end.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        assert!(!snap.is_null() && snap != INVALID_HANDLE_VALUE);
        // SAFETY: `PROCESSENTRY32W` is plain C data; zeroed with `dwSize` set is how it's used.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut names = Vec::new();
        // SAFETY: `snap` is an open snapshot and `entry` a valid, sized out struct.
        let mut ok = unsafe { Process32FirstW(snap, &raw mut entry) };
        while ok != 0 {
            if entry.th32ParentProcessID == pid {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                names.push(String::from_utf16_lossy(&entry.szExeFile[..len]));
            }
            // SAFETY: as above.
            ok = unsafe { Process32NextW(snap, &raw mut entry) };
        }
        // SAFETY: `snap` is open and owned here.
        unsafe { CloseHandle(snap) };
        names
    }
}
