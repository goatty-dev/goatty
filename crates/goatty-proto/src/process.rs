//! Spawning helper programs (`ssh`, `tailscale`, `--via` commands) without
//! a console window flashing up on Windows.

use std::process::Command;

/// Keep `cmd` from opening a console window of its own.
///
/// On Windows a console program started from a process without a console
/// (the GUI, started from Explorer) gets a new, visible console window. Then
/// this sets `CREATE_NO_WINDOW`. A process that has a console (the CLI in a
/// terminal) leaves `cmd` alone: the child shares that console, so ssh can
/// still ask for a passphrase there. Elsewhere it does nothing.
pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Console::GetConsoleWindow;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        // SAFETY: GetConsoleWindow has no preconditions; it returns null when
        // this process has no console.
        if unsafe { GetConsoleWindow() }.is_null() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }
    cmd
}
