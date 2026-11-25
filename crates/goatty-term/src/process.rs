//! Which program runs in the foreground of a PTY: the shell while it waits
//! at the prompt, `nvim` while that runs. Tabs show it (title fallback and
//! icon), `goattyd list` reports it.
//!
//! On Unix, the foreground process group comes from the PTY (`tcgetpgrp`), the
//! name from `/proc` on Linux and libproc on macOS. On Windows, we walk the
//! session shell's Win32 process tree (`CreateToolhelp32Snapshot` +
//! `QueryFullProcessImageNameW` + optional PEB command-line inspection for
//! coding agents under `node`/`python`) and strip `.exe`.
//!
//! [`ForegroundWatch`] checks again a little after something happened on the
//! PTY (output, input), so it costs nothing while the session is idle and at
//! most a couple of probes per second during a flood.

use std::time::{Duration, Instant};

/// After a poke: a quick check (a command that just started), then a later
/// one (a command that starts slowly or prints nothing).
const FIRST_CHECK: Duration = Duration::from_millis(40);
const SECOND_CHECK: Duration = Duration::from_millis(450);

/// The name to show for process `pid`, if it can be found.
pub fn process_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok();
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        display_name(comm.as_deref().unwrap_or("").trim_end_matches('\n'), &cmdline)
    }
    // macOS: the kernel's name (`proc_name`, up to 32 bytes); no argv, so
    // a coding agent under a runtime shows as the runtime.
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 64];
        // SAFETY: `buf` is valid for writes of the size passed.
        let n = unsafe { libc::proc_name(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
        display_name(&String::from_utf8_lossy(&buf[..(n.max(0) as usize).min(buf.len())]), &[])
    }
    #[cfg(windows)]
    {
        windows_impl::process_name(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

/// The working directory of process `pid` (`/proc/<pid>/cwd`; libproc on
/// macOS; `None` on Windows where goatty relies on OSC 7).
pub fn cwd(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `info` is plain data valid for writes of the size passed.
        let info = unsafe {
            let mut info: libc::proc_vnodepathinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
            if libc::proc_pidinfo(pid as i32, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut info).cast(), size) != size {
                return None;
            }
            info
        };
        // SAFETY: vip_path is a NUL-terminated C string of MAXPATHLEN bytes.
        let path = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
        use std::os::unix::ffi::OsStrExt;
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
        (!path.as_os_str().is_empty()).then(|| path.to_path_buf())
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Strip a trailing `.exe` (case-insensitive) from a Windows image name so tab
/// icons and shell checks match Unix names (`pwsh`, `nvim`, `node`, `cmd`).
#[cfg(any(windows, test))]
pub(crate) fn strip_exe_suffix(name: &str) -> &str {
    if name.len() >= 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") { &name[..name.len() - 4] } else { name }
}

/// Pick a name from `/proc/<pid>/comm` and `/proc/<pid>/cmdline`.
///
/// `comm` is usually right: for a script it's the script's name (`ranger`,
/// not `python3`), and programs that rename themselves (Node's
/// `process.title`) update it. But the kernel cuts it to 15 bytes, so a
/// longer name comes from `argv[0]` when that starts with it. A login
/// shell's leading `-` (`-zsh`) is dropped. A runtime (`node`, `bun`,
/// `python3`…) running a known coding agent's package is reported as the
/// agent (`cursor-agent` runs as `node …/cursor-agent/…/index.js`).
pub fn display_name(comm: &str, cmdline: &[u8]) -> Option<String> {
    let argv0 = cmdline.split(|&b| b == 0).next().map(String::from_utf8_lossy).unwrap_or_default();
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or_default().trim_start_matches('-').to_string();
    let comm = comm.trim_start_matches('-');
    let name = if comm.is_empty() || (comm.len() >= 15 && base.starts_with(comm)) { base } else { comm.to_string() };
    if is_runtime(&name)
        && let Some(agent) = agent_package(cmdline)
    {
        return Some(agent.to_string());
    }
    (!name.is_empty()).then_some(name)
}

/// Resolve a Windows image filename and optional raw command-line string into
/// the display name shown on tabs.
#[cfg(any(windows, test))]
pub(crate) fn display_name_windows(image_name: &str, cmdline: Option<&str>) -> Option<String> {
    let leaf = image_name.rsplit(['/', '\\']).next().unwrap_or_default();
    let name = strip_exe_suffix(leaf);
    if name.is_empty() {
        return None;
    }
    if is_runtime(name)
        && let Some(cmd) = cmdline
        && let Some(agent) = agent_package_str(cmd)
    {
        return Some(agent.to_string());
    }
    Some(name.to_string())
}

/// Interpreters that run other programs' code.
fn is_runtime(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    matches!(stem, "node" | "nodejs" | "bun" | "deno" | "python" | "uv" | "uvx")
}

/// Where coding agents installed as packages live, and the name each goes
/// by: npm/bun packages under a `node_modules` folder, Cursor's installer's
/// versions folder. Only an installed package matches, not a project
/// folder that happens to have the name.
const AGENT_PACKAGES: &[(&str, &str)] = &[
    ("/node_modules/@anthropic-ai/claude-code/", "claude"),
    ("/node_modules/@openai/codex/", "codex"),
    ("/node_modules/opencode-ai/", "opencode"),
    ("/node_modules/@google/gemini-cli/", "gemini"),
    ("/node_modules/@github/copilot/", "copilot"),
    ("/cursor-agent/versions/", "cursor-agent"),
    ("/node_modules/@mariozechner/pi-coding-agent/", "pi"),
    ("/node_modules/@sourcegraph/amp/", "amp"),
    ("/node_modules/@qwen-code/qwen-code/", "qwen"),
];

/// Match a single path/argument against [`AGENT_PACKAGES`], normalizing
/// backslashes so Windows paths match too.
fn match_agent_path(arg: &str) -> Option<&'static str> {
    if arg.contains('\\') {
        let norm = arg.replace('\\', "/");
        AGENT_PACKAGES.iter().find(|(dir, _)| norm.contains(dir)).map(|(_, name)| *name)
    } else {
        AGENT_PACKAGES.iter().find(|(dir, _)| arg.contains(dir)).map(|(_, name)| *name)
    }
}

/// The agent whose package an argument of a NUL-separated command line is in.
fn agent_package(cmdline: &[u8]) -> Option<&'static str> {
    let args = cmdline.split(|&b| b == 0).skip(1).take(8).map(String::from_utf8_lossy);
    for arg in args {
        if let Some(name) = match_agent_path(&arg) {
            return Some(name);
        }
    }
    None
}

/// The agent whose package appears in a Windows command-line string.
#[cfg(any(windows, test))]
fn agent_package_str(cmdline: &str) -> Option<&'static str> {
    // Split on whitespace/quotes conservatively: skip argv[0] by taking tokens
    // after the first.
    let mut parts = split_windows_cmdline(cmdline).into_iter().skip(1).take(8);
    parts.find_map(|arg| match_agent_path(&arg))
}

#[cfg(any(windows, test))]
fn split_windows_cmdline(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in cmd.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(windows)]
mod windows_impl {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ, QueryFullProcessImageNameW,
    };

    use super::display_name_windows;

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid Win32 handle owned exclusively by `self`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn is_console_helper(exe_name: &str) -> bool {
        exe_name.eq_ignore_ascii_case("conhost.exe") || exe_name.eq_ignore_ascii_case("openconsole.exe")
    }

    fn process_creation_time(pid: u32) -> u64 {
        // SAFETY: plain Win32 call; handle is checked and closed.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if raw.is_null() {
            return 0;
        }
        let handle = OwnedHandle(raw);
        let mut created = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut exit = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut kernel = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut user = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        // SAFETY: `handle.0` is open and all four `FILETIME` pointers are valid writable locals.
        let ok = unsafe { GetProcessTimes(handle.0, &raw mut created, &raw mut exit, &raw mut kernel, &raw mut user) };
        if ok == 0 {
            return 0;
        }
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
    }

    /// Walk the Win32 process tree rooted at `shell_pid` and return the most
    /// likely active foreground descendant (or `shell_pid` itself when idle).
    pub(crate) fn foreground_pid(shell_pid: u32) -> Option<u32> {
        // SAFETY: plain Win32 snapshot call; returned handle is validated before use.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return Some(shell_pid);
        }
        let snap = OwnedHandle(snap);

        // SAFETY: `PROCESSENTRY32W` is plain C data; zero-initialization + `dwSize` is required.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut children: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        let mut shell_seen = false;

        // SAFETY: `snap.0` is a valid snapshot handle and `entry` has `dwSize` set.
        let mut ok = unsafe { Process32FirstW(snap.0, &raw mut entry) };
        while ok != 0 {
            let pid = entry.th32ProcessID;
            let parent = entry.th32ParentProcessID;
            if pid == shell_pid {
                shell_seen = true;
            }
            let nul = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = OsString::from_wide(&entry.szExeFile[..nul]).to_string_lossy().into_owned();
            if pid != 0 && pid != parent {
                children.entry(parent).or_default().push((pid, name));
            }
            // SAFETY: same valid snapshot handle and `entry`.
            ok = unsafe { Process32NextW(snap.0, &raw mut entry) };
        }

        if !shell_seen {
            return None;
        }

        // BFS from `shell_pid` to find the deepest non-helper descendant; break
        // ties by latest process creation time, then highest PID.
        let mut best: Option<(usize, u64, u32)> = None;
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        visited.insert(shell_pid);
        queue.push_back((shell_pid, 0usize));

        while let Some((cur, depth)) = queue.pop_front() {
            if let Some(kids) = children.get(&cur) {
                for (kid_pid, kid_name) in kids {
                    if !visited.insert(*kid_pid) {
                        continue;
                    }
                    if is_console_helper(kid_name) {
                        continue;
                    }
                    let next_depth = depth + 1;
                    let key = (next_depth, process_creation_time(*kid_pid), *kid_pid);
                    if best.is_none_or(|b| key > b) {
                        best = Some(key);
                    }
                    queue.push_back((*kid_pid, next_depth));
                }
            }
        }

        Some(best.map_or(shell_pid, |(_, _, pid)| pid))
    }

    fn query_image_path(handle: HANDLE) -> Option<String> {
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `handle` is open and `buf` has space for `len` UTF-16 code units.
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &raw mut len) };
        if ok == 0 || len == 0 {
            return None;
        }
        Some(OsString::from_wide(&buf[..len as usize]).to_string_lossy().into_owned())
    }

    /// Best-effort read of another same-bitness process's command line from its
    /// PEB via `NtQueryInformationProcess` + `ReadProcessMemory`. Returns `None`
    /// on access denied or architecture mismatch.
    fn read_peb_cmdline(pid: u32) -> Option<String> {
        #[repr(C)]
        struct ProcessBasicInformation {
            _exit_status: isize,
            peb_base_address: usize,
            _affinity_mask: usize,
            _base_priority: isize,
            _unique_process_id: usize,
            _inherited_from_unique_process_id: usize,
        }

        #[repr(C)]
        struct UnicodeString {
            length: u16,
            _maximum_length: u16,
            #[cfg(target_pointer_width = "64")]
            _pad: u32,
            buffer: usize,
        }

        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtQueryInformationProcess(
                process_handle: HANDLE,
                process_information_class: u32,
                process_information: *mut std::ffi::c_void,
                process_information_length: u32,
                return_length: *mut u32,
            ) -> i32;
        }

        // SAFETY: plain Win32 call; returned handle is validated and closed on drop.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, FALSE, pid) };
        if raw.is_null() {
            return None;
        }
        let handle = OwnedHandle(raw);

        // SAFETY: `ProcessBasicInformation` is plain C data; all zeros is valid.
        let mut pbi: ProcessBasicInformation = unsafe { std::mem::zeroed() };
        let mut ret_len = 0u32;
        // ProcessBasicInformation = 0
        // SAFETY: `pbi` is valid for its exact size and `handle.0` is open.
        let status = unsafe {
            NtQueryInformationProcess(
                handle.0,
                0,
                (&raw mut pbi).cast(),
                std::mem::size_of::<ProcessBasicInformation>() as u32,
                &raw mut ret_len,
            )
        };
        if status < 0 || pbi.peb_base_address == 0 {
            return None;
        }

        // PEB.ProcessParameters offset (0x20 on 64-bit, 0x10 on 32-bit Windows).
        let proc_params_offset: usize = if cfg!(target_pointer_width = "64") { 0x20 } else { 0x10 };
        let proc_params_addr = pbi.peb_base_address.checked_add(proc_params_offset)?;
        let mut proc_params: usize = 0;
        let mut read = 0usize;
        // SAFETY: reading a pointer-sized integer into our local `proc_params`.
        let ok = unsafe {
            ReadProcessMemory(
                handle.0,
                proc_params_addr as *const _,
                (&raw mut proc_params).cast(),
                std::mem::size_of::<usize>(),
                &raw mut read,
            )
        };
        if ok == 0 || proc_params == 0 {
            return None;
        }

        // RTL_USER_PROCESS_PARAMETERS.CommandLine offset (0x70 on 64-bit, 0x40 on 32-bit).
        let cmdline_offset: usize = if cfg!(target_pointer_width = "64") { 0x70 } else { 0x40 };
        let cmdline_addr = proc_params.checked_add(cmdline_offset)?;
        // SAFETY: `UnicodeString` is plain C data.
        let mut us: UnicodeString = unsafe { std::mem::zeroed() };
        // SAFETY: reading `UnicodeString` into our local `us`.
        let ok = unsafe {
            ReadProcessMemory(handle.0, cmdline_addr as *const _, (&raw mut us).cast(), std::mem::size_of::<UnicodeString>(), &raw mut read)
        };
        if ok == 0 || us.length == 0 || us.buffer == 0 {
            return None;
        }

        // Cap at 8 KiB (4096 UTF-16 code units) — plenty for agent package paths.
        let byte_len = (us.length as usize).min(8192);
        let word_len = byte_len / 2;
        if word_len == 0 {
            return None;
        }
        let mut wide = vec![0u16; word_len];
        // SAFETY: reading `word_len * 2` bytes into `wide` which has that exact capacity.
        let ok = unsafe { ReadProcessMemory(handle.0, us.buffer as *const _, wide.as_mut_ptr().cast(), word_len * 2, &raw mut read) };
        if ok == 0 {
            return None;
        }
        let actual_words = (read / 2).min(wide.len());
        Some(OsString::from_wide(&wide[..actual_words]).to_string_lossy().into_owned())
    }

    pub(crate) fn process_name(pid: u32) -> Option<String> {
        // SAFETY: plain Win32 call; handle is validated and closed on drop.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if raw.is_null() {
            return None;
        }
        let handle = OwnedHandle(raw);
        let image = query_image_path(handle.0)?;
        let leaf = image.rsplit(['/', '\\']).next().unwrap_or_default();
        let stem = super::strip_exe_suffix(leaf);
        let cmdline = if super::is_runtime(stem) { read_peb_cmdline(pid) } else { None };
        display_name_windows(&image, cmdline.as_deref())
    }
}

#[cfg(windows)]
pub(crate) fn windows_foreground_pid(shell_pid: u32) -> Option<u32> {
    windows_impl::foreground_pid(shell_pid)
}

/// Watches a PTY's foreground process on its own thread.
///
/// The thread sleeps until [`poke`](Self::poke)d, then checks after
/// [`FIRST_CHECK`] and again after [`SECOND_CHECK`] (repeating while pokes
/// keep coming), calling `on_change` whenever the name differs from the last
/// one it reported. Dropping the watch ends the thread.
pub struct ForegroundWatch {
    tx: flume::Sender<()>,
}

impl ForegroundWatch {
    /// `probe` returns the current foreground name. `on_change` gets the
    /// first value right away (on the watch thread) and every change after.
    pub fn spawn(
        thread_name: String,
        probe: impl Fn() -> Option<String> + Send + 'static,
        on_change: impl Fn(Option<String>) + Send + 'static,
    ) -> std::io::Result<Self> {
        // One pending poke is enough: they all mean "look again soon".
        let (tx, rx) = flume::bounded::<()>(1);
        std::thread::Builder::new().name(thread_name).spawn(move || {
            let mut last = probe();
            on_change(last.clone());
            let mut check = || {
                let now = probe();
                if now != last {
                    last = now.clone();
                    on_change(now);
                }
            };
            while rx.recv().is_ok() {
                loop {
                    if wait(&rx, FIRST_CHECK).is_none() {
                        return;
                    }
                    check();
                    let Some(poked) = wait(&rx, SECOND_CHECK) else { return };
                    check();
                    if !poked {
                        break;
                    }
                }
            }
        })?;
        Ok(Self { tx })
    }

    /// Something happened on the PTY; look again soon. Cheap and never
    /// blocks (call it on every read).
    pub fn poke(&self) {
        let _ = self.tx.try_send(());
    }
}

/// Sleep for `d`, swallowing pokes. `Some(poked)`, or `None` once the watch
/// was dropped.
fn wait(rx: &flume::Receiver<()>, d: Duration) -> Option<bool> {
    let deadline = Instant::now() + d;
    let mut poked = false;
    loop {
        match rx.recv_deadline(deadline) {
            Ok(()) => poked = true,
            Err(flume::RecvTimeoutError::Timeout) => return Some(poked),
            Err(flume::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn names() {
        assert_eq!(display_name("nvim", b"nvim\0file.rs\0").as_deref(), Some("nvim"));
        // Scripts: comm is the script, argv[0] the interpreter.
        assert_eq!(display_name("ranger", b"/usr/bin/python3\0/usr/bin/ranger\0").as_deref(), Some("ranger"));
        // Login shells.
        assert_eq!(display_name("zsh", b"-zsh\0").as_deref(), Some("zsh"));
        assert_eq!(display_name("", b"-zsh\0").as_deref(), Some("zsh"));
        // comm is cut to 15 bytes.
        assert_eq!(display_name("rust-analyzer-p", b"/x/rust-analyzer-proc-macro-srv\0").as_deref(), Some("rust-analyzer-proc-macro-srv"));
        assert_eq!(display_name("", b"").as_deref(), None);
        // Coding agents under a runtime go by their own name.
        let cursor = b"/home/me/.local/share/cursor-agent/versions/1/node\0--use-system-ca\0/home/me/.local/share/cursor-agent/versions/1/index.js\0";
        assert_eq!(display_name("node", cursor).as_deref(), Some("cursor-agent"));
        let gemini = b"node\0/usr/lib/node_modules/@google/gemini-cli/dist/index.js\0";
        assert_eq!(display_name("node", gemini).as_deref(), Some("gemini"));
        assert_eq!(display_name("node", b"node\0server.js\0").as_deref(), Some("node"));
        let project = b"node\0/home/me/src/cursor-agent/index.js\0/home/me/opencode-ai/x.js\0";
        assert_eq!(display_name("node", project).as_deref(), Some("node"), "a project folder isn't the package");
        assert_eq!(display_name("vim", b"vim\0/x/@openai/codex/a.js\0").as_deref(), Some("vim"), "only runtimes");
    }

    #[test]
    fn windows_names_strip_exe_and_detect_agents() {
        assert_eq!(display_name_windows(r"C:\Program Files\PowerShell\7\pwsh.exe", None).as_deref(), Some("pwsh"));
        assert_eq!(display_name_windows(r"C:\Windows\System32\CMD.EXE", None).as_deref(), Some("CMD"));
        assert_eq!(
            display_name_windows(
                r"C:\Program Files\nodejs\node.exe",
                Some(
                    r#""C:\Program Files\nodejs\node.exe" "C:\Users\me\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\cli.js""#
                )
            )
            .as_deref(),
            Some("claude")
        );
        assert_eq!(
            display_name_windows(r"C:\Program Files\nodejs\node.exe", Some(r#"node.exe C:\Users\me\app\server.js"#)).as_deref(),
            Some("node")
        );
    }

    #[test]
    fn watch_reports_changes_after_a_poke() {
        let current = Arc::new(Mutex::new(Some("zsh".to_string())));
        let (tx, rx) = flume::unbounded();
        let probe = {
            let c = current.clone();
            move || c.lock().unwrap().clone()
        };
        let watch = ForegroundWatch::spawn("fg-test".into(), probe, move |n| tx.send(n).unwrap()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().as_deref(), Some("zsh"));
        *current.lock().unwrap() = Some("vim".into());
        // Nothing happens without a poke.
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
        watch.poke();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().as_deref(), Some("vim"));
        // No change, no report.
        watch.poke();
        assert!(rx.recv_timeout(Duration::from_millis(700)).is_err());
        drop(watch);
    }
}
