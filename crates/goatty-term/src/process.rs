//! Which program runs in the foreground of a PTY: the shell while it waits
//! at the prompt, `nvim` while that runs. Tabs show it (title fallback and
//! icon), `goattyd list` reports it.
//!
//! The foreground process group comes from the PTY (`tcgetpgrp`), the name
//! from `/proc` on Linux and libproc on macOS (other systems report nothing).
//!
//! [`ForegroundWatch`] checks again a little after something happened on the
//! PTY (output, input), so it costs nothing while the session is idle and at
//! most a couple of `/proc` reads per second during a flood.

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
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// The working directory of process `pid` (`/proc/<pid>/cwd`; libproc on
/// macOS).
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
    #[cfg(not(target_os = "macos"))]
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
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
    let base = argv0.rsplit('/').next().unwrap_or_default().trim_start_matches('-').to_string();
    let comm = comm.trim_start_matches('-');
    let name = if comm.is_empty() || (comm.len() >= 15 && base.starts_with(comm)) { base } else { comm.to_string() };
    if is_runtime(&name)
        && let Some(agent) = agent_package(cmdline)
    {
        return Some(agent.to_string());
    }
    (!name.is_empty()).then_some(name)
}

/// Interpreters that run other programs' code.
fn is_runtime(name: &str) -> bool {
    let name = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    matches!(name, "node" | "nodejs" | "bun" | "deno" | "python" | "uv" | "uvx")
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

/// The agent whose package an argument of the command line is in.
fn agent_package(cmdline: &[u8]) -> Option<&'static str> {
    let args = cmdline.split(|&b| b == 0).skip(1).take(8).map(String::from_utf8_lossy);
    for arg in args {
        if let Some((_, name)) = AGENT_PACKAGES.iter().find(|(dir, _)| arg.contains(dir)) {
            return Some(name);
        }
    }
    None
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
