//! Snippets and expansions on this host (#89): the user's own from
//! `host.json` and the enabled plugins', listed for the picker
//! ([`list`]), suggestion sources run for it ([`suggest`]; plugins'
//! palette commands' too, [`suggest_command`]), and the
//! expansion table the managed zsh reads ([`render_expansions`]).
//!
//! The format is `goatty_plugin::snippets`. What this module adds is the
//! host's side of the trust rule: a client only ever names a snippet and
//! an input; the program that runs is looked up here, in a plugin's
//! lock-verified manifest or in the user's own `host.json`, and runs
//! without a shell (a plugin's), in the pane's directory, on goatty's
//! managed PATH, with a time limit and an output cap.

use std::collections::BTreeSet;
#[cfg(not(windows))]
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::Stdio;
use std::time::Duration;
#[cfg(not(windows))]
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use goatty_config::HostSettings;
use goatty_plugin::api::{HostExpansion, HostSnippet, Origin, SnippetList, Suggestions};
use goatty_plugin::commands::InputKind;
use goatty_plugin::snippets::{self, Command, Compiled, Expansion, Owner, Snippet, SnippetInput};

use crate::dirs::Dirs;
use crate::plugins::{self, Active};

/// The owner of the user's own snippets in [`Origin`] and `Suggest`.
pub const MINE: &str = "";

fn yours() -> Origin {
    Origin { owner: MINE.into(), name: "Yours".into() }
}

fn origin_of(a: &Active) -> Origin {
    Origin { owner: a.manifest.id.clone(), name: a.manifest.name.clone() }
}

/// The user's snippets from `host.json`, checked; the broken ones are left
/// out and reported.
pub fn user_snippets(settings: &HostSettings) -> (Vec<Snippet>, Vec<String>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    for (id, v) in &settings.snippets {
        match Snippet::from_user(id, v) {
            Err(e) => issues.push(format!("snippets.{id}: {e}; left out")),
            Ok(s) => {
                let problems = snippets::check_snippet(&s, &format!("snippets.{id}"), Owner::User);
                if problems.is_empty() {
                    out.push(s);
                } else {
                    issues.extend(problems.iter().map(|p| format!("{p}; left out")));
                }
            }
        }
    }
    (out, issues)
}

/// The user's expansions from `host.json`: (id, expansion, enabled).
pub fn user_expansions(settings: &HostSettings) -> (Vec<(String, Expansion, bool)>, Vec<String>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    for (id, v) in &settings.expansions {
        let mut v = v.clone();
        let enabled = v.as_object_mut().and_then(|o| o.remove("enabled")).and_then(|e| e.as_bool()).unwrap_or(true);
        match serde_json::from_value::<Expansion>(v) {
            Err(e) => issues.push(format!("expansions.{id}: {e}; left out")),
            Ok(e) => match snippets::compile(&e) {
                Err(why) => issues.push(format!("expansions.{id}: {why}; left out")),
                Ok(_) => out.push((id.clone(), e, enabled)),
            },
        }
    }
    (out, issues)
}

/// `expansions.zsh` for `settings` and the active plugins: the user's
/// enabled expansions first (they win), then the plugins' in load order.
/// Empty when `shell.expansions` is off. Returns the problems too.
pub fn render_expansions(settings: &HostSettings, active: &[Active]) -> (String, Vec<String>) {
    let mut rules: Vec<Compiled> = Vec::new();
    let mut issues = Vec::new();
    if settings.shell.expansions {
        let (mine, problems) = user_expansions(settings);
        issues.extend(problems);
        rules.extend(mine.iter().filter(|(_, _, on)| *on).filter_map(|(_, e, _)| snippets::compile(e).ok()));
        for a in active {
            rules.extend(a.manifest.expansions.iter().filter_map(|e| snippets::compile(e).ok()));
        }
        // The same words twice: the first (the user's) wins.
        let mut seen = BTreeSet::new();
        rules.retain(|r| seen.insert(r.say.clone()));
    }
    (snippets::render_table(&rules), issues)
}

/// zsh builtins and keywords a snippet line may use: never "missing".
const BUILTINS: &[&str] = &[
    "cd", "echo", "printf", "print", "export", "source", "alias", "unalias", "eval", "exec", "exit", "history", "jobs", "fg", "bg", "kill",
    "pwd", "read", "set", "unset", "test", "type", "wait", "which", "where", "whence", "pushd", "popd", "dirs", "time", "ulimit", "umask",
    "true", "false", "for", "while", "if", "then", "do", "done", "fi", "case", "esac", "noglob", "sudo", "builtin", "command", "local",
    "typeset", "fc", "r", "z",
];

/// The PATH a new managed shell gets, closely enough to tell which programs
/// it finds: the folders the import adds, goattyd's own PATH, `shell.path`
/// and goatty's tools.
pub fn managed_path(dirs: &Dirs, settings: &HostSettings) -> String {
    let env_path = std::env::var("PATH").ok();
    managed_path_with(dirs, settings, env_path.as_deref())
}

pub(crate) fn managed_path_with(dirs: &Dirs, settings: &HostSettings, env_path: Option<&str>) -> String {
    let mut folders: Vec<String> = Vec::new();
    if settings.shell.import.enabled && !settings.shell.import.skip_vars.iter().any(|v| v == "PATH") {
        let record = crate::import::Record::load(dirs);
        if let Some(h) = &record.harvest {
            folders.extend(h.added_path().into_iter().map(str::to_string));
        }
    }
    folders.extend(
        std::env::split_paths(env_path.unwrap_or_default()).filter(|d| !d.as_os_str().is_empty()).map(|d| d.to_string_lossy().into_owned()),
    );
    let home = std::env::var_os("HOME").map(PathBuf::from);
    folders.extend(settings.shell.path.iter().map(|p| crate::path_report::expand(p, home.as_deref())));
    folders.push(dirs.bin_dir().to_string_lossy().into_owned());
    let mut seen = BTreeSet::new();
    folders.retain(|f| seen.insert(f.clone()));
    let sep = if cfg!(windows) { ";" } else { ":" };
    folders.join(sep)
}

fn is_executable(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c_path) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `c_path` is a NUL-terminated path passed to POSIX `faccessat` for reading.
        unsafe { libc::faccessat(libc::AT_FDCWD, c_path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
    }
    #[cfg(not(unix))]
    true
}

/// `program` in `path` (or itself when it's a path), requiring execute permission.
pub(crate) fn find_in(path: &str, program: &str) -> Option<PathBuf> {
    find_in_with_cwd(path, program, None)
}

/// Like [`find_in`], resolving relative paths containing a path separator against `cwd`.
pub(crate) fn find_in_with_cwd(path: &str, program: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let has_sep = program.contains('/') || (cfg!(windows) && program.contains('\\'));
    if has_sep {
        let p = Path::new(program);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(dir) = cwd.filter(|d| !d.is_empty()) {
            Path::new(dir).join(p)
        } else {
            p.to_path_buf()
        };
        return is_executable(&resolved).then_some(resolved);
    }
    #[cfg(windows)]
    let exts: Vec<String> = if Path::new(program).extension().is_some() {
        Vec::new()
    } else {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect()
    };
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let direct = dir.join(program);
        if is_executable(&direct) {
            return Some(direct);
        }
        #[cfg(windows)]
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Every snippet and expansion on this host, with the programs each
/// snippet needs that aren't here.
pub fn list(dirs: &Dirs, settings: &HostSettings) -> SnippetList {
    // Problems of the plugins themselves show on the Plugins page.
    let (active, _) = plugins::active(dirs, settings);
    let mut issues = Vec::new();
    let path = managed_path(dirs, settings);
    let mut found: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();
    let mut missing_of = |s: &Snippet| -> Vec<String> {
        s.programs()
            .into_iter()
            .filter(|p| !BUILTINS.contains(&p.as_str()))
            .filter(|p| !*found.entry(p.clone()).or_insert_with(|| find_in(&path, p).is_some()))
            .collect()
    };
    let mut list = SnippetList { expansions_on: settings.shell.expansions, ..SnippetList::default() };
    let (mine, problems) = user_snippets(settings);
    issues.extend(problems);
    for s in mine {
        list.snippets.push(HostSnippet { origin: yours(), missing: missing_of(&s), snippet: s });
    }
    for a in &active {
        for s in &a.manifest.snippets {
            list.snippets.push(HostSnippet { origin: origin_of(a), missing: missing_of(s), snippet: s.clone() });
        }
    }
    let (mine, problems) = user_expansions(settings);
    issues.extend(problems);
    for (id, e, enabled) in mine {
        list.expansions.push(HostExpansion { origin: yours(), id, expansion: e, enabled });
    }
    for a in &active {
        for e in &a.manifest.expansions {
            list.expansions.push(HostExpansion { origin: origin_of(a), id: String::new(), expansion: e.clone(), enabled: true });
        }
    }
    list.issues = issues;
    list
}

/// Find a snippet and its input on this host: the user's own (`owner` ""),
/// or an enabled plugin's.
fn find(dirs: &Dirs, settings: &HostSettings, owner: &str, snippet: &str, input: &str) -> Result<(Snippet, SnippetInput, bool)> {
    let (s, user) = if owner == MINE {
        let (mine, _) = user_snippets(settings);
        (mine.into_iter().find(|s| s.id == snippet).with_context(|| format!("you have no snippet {snippet:?} on this host"))?, true)
    } else {
        let (active, _) = plugins::active(dirs, settings);
        let a = active.iter().find(|a| a.manifest.id == owner).with_context(|| format!("plugin {owner:?} isn't on (or installed) here"))?;
        let s = a.manifest.snippets.iter().find(|s| s.id == snippet).with_context(|| format!("{owner} has no snippet {snippet:?}"))?;
        (s.clone(), false)
    };
    let i = s.input(input).cloned().with_context(|| format!("{} has no input {input:?}", s.title))?;
    Ok((s, i, user))
}

/// Run `input`'s source (looked up on this host) with the earlier
/// `values`, in `cwd` (the pane's directory; the home directory when it's
/// missing or gone).
pub fn suggest(
    dirs: &Dirs,
    settings: &HostSettings,
    owner: &str,
    snippet: &str,
    input: &str,
    values: &snippets::Answers,
    cwd: Option<&str>,
) -> Result<Suggestions> {
    let (s, i, user) = find(dirs, settings, owner, snippet, input)?;
    let src = i.source.clone().ok_or_else(|| anyhow!("{} has no suggestions", i.label))?;
    let values = expand_paths(values, |id| s.input(id).map(|i| i.kind));
    let cmd = match snippets::source_command(&s, &i, &values) {
        Ok(c) => c,
        Err(why) => return Ok(Suggestions { items: Vec::new(), note: Some(why), truncated: false }),
    };
    // The manifest was checked when it was installed; check again what runs.
    if !user && matches!(cmd, Command::Shell(_)) {
        bail!("plugins can't run a shell");
    }
    run_source(dirs, settings, &src, &cmd, cwd)
}

/// Run the source of input `input` of plugin `plugin`'s palette command
/// `command` (`[command.input.source]`, looked up in the enabled plugin's
/// manifest here) with the earlier `values`, in `cwd`.
pub fn suggest_command(
    dirs: &Dirs,
    settings: &HostSettings,
    plugin: &str,
    command: &str,
    input: &str,
    values: &snippets::Answers,
    cwd: Option<&str>,
) -> Result<Suggestions> {
    let (active, _) = plugins::active(dirs, settings);
    let a = active.iter().find(|a| a.manifest.id == plugin).with_context(|| format!("plugin {plugin:?} isn't on (or installed) here"))?;
    let c = a.manifest.commands.iter().find(|c| c.id == command).with_context(|| format!("{plugin} has no command {command:?}"))?;
    let i = c.inputs.iter().find(|i| i.id == input).with_context(|| format!("{} has no input {input:?}", c.title))?;
    let src = i.source.clone().ok_or_else(|| anyhow!("{} has no suggestions", i.label))?;
    let values = expand_paths(values, |id| c.inputs.iter().find(|i| i.id == id).map(|i| i.kind));
    let argv = match goatty_plugin::commands::source_argv(c, i, &values) {
        Ok(argv) => argv,
        Err(why) => return Ok(Suggestions { items: Vec::new(), note: Some(why), truncated: false }),
    };
    run_source(dirs, settings, &src, &Command::Argv(argv), cwd)
}

/// The answers with a leading `~` of path and folder values (`kind_of`
/// an input's type) made the home directory: a source runs without a
/// shell, which would have expanded it. The stored answers keep their `~`.
fn expand_paths(values: &snippets::Answers, kind_of: impl Fn(&str) -> Option<InputKind>) -> snippets::Answers {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    values
        .iter()
        .map(|(id, v)| {
            let path = matches!(kind_of(id), Some(InputKind::Path | InputKind::Folder));
            let v = if path {
                v.split('\n').map(|p| crate::path_report::expand(p, home.as_deref())).collect::<Vec<_>>().join("\n")
            } else {
                v.clone()
            };
            (id.clone(), v)
        })
        .collect()
}

/// Run a source's `cmd` in `cwd` (the pane's directory; the home directory
/// when it's missing, gone, or the source says so) and read its output.
fn run_source(dirs: &Dirs, settings: &HostSettings, src: &snippets::Source, cmd: &Command, cwd: Option<&str>) -> Result<Suggestions> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    let dir = match (src.cwd, cwd) {
        (snippets::SourceCwd::Pane, Some(c)) if Path::new(c).is_dir() => PathBuf::from(c),
        _ => home,
    };
    let path = managed_path(dirs, settings);
    let fail = |what: String| Suggestions { items: Vec::new(), note: Some(src.on_fail.clone().unwrap_or(what)), truncated: false };
    let out = match run(cmd, &dir, &path, src.timeout()) {
        Ok(o) => o,
        Err(e) => return Ok(fail(format!("{e:#}"))),
    };
    if !out.success {
        let first = out.stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it failed").to_string();
        return Ok(fail(first));
    }
    let (items, truncated) = snippets::parse_output(&out.stdout, src);
    if items.is_empty() {
        return Ok(Suggestions { items, note: src.on_fail.clone().or(Some("Nothing to suggest".into())), truncated });
    }
    Ok(Suggestions { items, note: (out.capped || truncated).then(|| "Showing the first ones".into()), truncated: truncated || out.capped })
}

#[derive(Debug)]
struct Output {
    stdout: String,
    stderr: String,
    success: bool,
    /// Stopped reading at [`snippets::MAX_OUTPUT`].
    capped: bool,
}

#[cfg(windows)]
mod win_proc {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::io::{self, Read};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{Context, Result, bail};
    use goatty_plugin::snippets;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation, TRUE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    use super::Output;

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: `self.0` is a valid Win32 handle owned exclusively by `self`.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct JobObject(HANDLE);
    impl JobObject {
        fn new() -> io::Result<Self> {
            // SAFETY: null attributes and unnamed job object are valid.
            let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if h.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(h);
            // SAFETY: plain C struct; zeroed initialization is valid.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `job.0` is valid and `info` has exact size.
            if unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        fn assign(&self, proc_handle: HANDLE) -> io::Result<()> {
            // SAFETY: both `self.0` and `proc_handle` are valid open handles.
            if unsafe { AssignProcessToJobObject(self.0, proc_handle) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn terminate(&self) {
            // SAFETY: `self.0` is a valid job handle owned by `self`.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid handle owned exclusively by `self`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn to_wide_nul(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn quote_windows_arg(arg: &str) -> String {
        if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '"')) {
            return arg.to_string();
        }
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('"');
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    for _ in 0..(backslashes * 2 + 1) {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push('"');
                }
                c => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push(c);
                }
            }
        }
        for _ in 0..(backslashes * 2) {
            out.push('\\');
        }
        out.push('"');
        out
    }

    fn build_cmdline(program: &Path, args: &[String]) -> Vec<u16> {
        let mut line = quote_windows_arg(&program.to_string_lossy());
        for arg in args {
            line.push(' ');
            line.push_str(&quote_windows_arg(arg));
        }
        to_wide_nul(OsStr::new(&line))
    }

    fn build_env_block(path_env: &str) -> Vec<u16> {
        let mut map: BTreeMap<String, (String, String)> = BTreeMap::new();
        for (k, v) in std::env::vars_os() {
            let ks = k.to_string_lossy().into_owned();
            let vs = v.to_string_lossy().into_owned();
            map.insert(ks.to_ascii_lowercase(), (ks, vs));
        }
        map.remove("goatty_socket");
        for (k, v) in [("PATH", path_env), ("GIT_TERMINAL_PROMPT", "0"), ("GIT_PAGER", "cat"), ("PAGER", "cat")] {
            map.insert(k.to_ascii_lowercase(), (k.to_string(), v.to_string()));
        }
        let mut block = Vec::new();
        for (_, (k, v)) in map {
            block.extend(OsStr::new(&k).encode_wide());
            block.push(b'=' as u16);
            block.extend(OsStr::new(&v).encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    fn create_output_pipe() -> io::Result<(std::fs::File, OwnedHandle)> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        let mut read_h: HANDLE = std::ptr::null_mut();
        let mut write_h: HANDLE = std::ptr::null_mut();
        // SAFETY: `read_h`, `write_h`, and `sa` are valid pointers.
        if unsafe { CreatePipe(&raw mut read_h, &raw mut write_h, &raw mut sa, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let read = OwnedHandle(read_h);
        let write = OwnedHandle(write_h);
        // Prevent the child from inheriting the parent's read end of the pipe.
        // SAFETY: `read.0` is a valid pipe handle.
        if unsafe { SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let raw_read = read.0;
        std::mem::forget(read);
        // SAFETY: `raw_read` is a valid, exclusively owned pipe read handle.
        let file = unsafe { std::fs::File::from_raw_handle(raw_read) };
        Ok((file, write))
    }

    fn open_nul_stdin() -> io::Result<OwnedHandle> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        let nul = to_wide_nul(OsStr::new("NUL"));
        // SAFETY: `nul` is NUL-terminated UTF-16 and `sa` stays alive across the call.
        let h = unsafe {
            CreateFileW(
                nul.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &raw const sa,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle(h))
    }

    struct ProcThreadAttrList {
        _buf: Vec<usize>,
        _handles: Box<[HANDLE]>,
        ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
    }

    impl ProcThreadAttrList {
        fn with_handle_list(handles: Vec<HANDLE>) -> io::Result<Self> {
            let mut bytes = 0usize;
            // SAFETY: probing the required buffer size with a null attribute list is documented.
            unsafe {
                InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut bytes);
            }
            if bytes == 0 {
                return Err(io::Error::last_os_error());
            }
            let words = bytes.div_ceil(std::mem::size_of::<usize>()).max(1);
            let mut buf = vec![0usize; words];
            let ptr: LPPROC_THREAD_ATTRIBUTE_LIST = buf.as_mut_ptr().cast();
            // SAFETY: `buf` is pointer-aligned and holds at least `bytes` writable bytes.
            if unsafe { InitializeProcThreadAttributeList(ptr, 1, 0, &raw mut bytes) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let handles = handles.into_boxed_slice();
            let handles_ptr = handles.as_ptr().cast();
            let handles_bytes = std::mem::size_of_val(handles.as_ref());
            // Moving `handles` (a `Box<[HANDLE]>`) into `Self` preserves its heap address
            // until after `Drop::drop` deletes the attribute list.
            let list = Self { _buf: buf, _handles: handles, ptr };
            // SAFETY: `list.ptr` is initialized and `list._handles` stays alive at the same
            // heap address until after `DeleteProcThreadAttributeList` runs in `Drop`.
            if unsafe {
                UpdateProcThreadAttribute(
                    list.ptr,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles_ptr,
                    handles_bytes,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(list)
        }

        fn as_mut_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.ptr
        }
    }

    impl Drop for ProcThreadAttrList {
        fn drop(&mut self) {
            // SAFETY: `self.ptr` was initialized by `InitializeProcThreadAttributeList`;
            // `_buf` and `_handles` are dropped only after `Drop::drop` returns.
            unsafe {
                DeleteProcThreadAttributeList(self.ptr);
            }
        }
    }

    /// Spawn `program` with `CREATE_SUSPENDED`, assign it to a
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job Object before calling
    /// `ResumeThread` (so no descendant can escape job membership), and
    /// terminate the entire job tree on both normal exit and timeout before
    /// joining the stdout/stderr reader threads.
    pub(super) fn run_in_job(program: &Path, args: &[String], dir: &Path, path_env: &str, timeout: Duration) -> Result<Output> {
        let job = JobObject::new().context("create Windows job object")?;
        let (mut stdout, stdout_write) = create_output_pipe().context("create stdout pipe")?;
        let (mut stderr, stderr_write) = create_output_pipe().context("create stderr pipe")?;
        let stdin_nul = open_nul_stdin().context("open NUL for stdin")?;

        // Restrict inheritance strictly to this child's three standard handles so
        // concurrent snippet spawns never inherit another run's pipe write ends.
        let attrs = ProcThreadAttrList::with_handle_list(vec![stdin_nul.0, stdout_write.0, stderr_write.0])
            .context("init PROC_THREAD_ATTRIBUTE_HANDLE_LIST")?;

        // SAFETY: `STARTUPINFOEXW` is plain C data; zero-init + `cb` is required.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdin_nul.0;
        si.StartupInfo.hStdOutput = stdout_write.0;
        si.StartupInfo.hStdError = stderr_write.0;
        si.lpAttributeList = attrs.as_mut_ptr();

        let app_wide = to_wide_nul(program.as_os_str());
        let mut cmdline = build_cmdline(program, args);
        let env_block = build_env_block(path_env);
        let cwd_wide = to_wide_nul(dir.as_os_str());
        // SAFETY: `PROCESS_INFORMATION` is plain C data.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // SAFETY: all wide strings, env block, attribute list, and handles live across `CreateProcessW`.
        let ok = unsafe {
            CreateProcessW(
                app_wide.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                TRUE,
                CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                env_block.as_ptr().cast(),
                cwd_wide.as_ptr(),
                &raw const si.StartupInfo,
                &raw mut pi,
            )
        };
        // Delete the attribute list before closing the handles it referenced,
        // and close our copies of the child's stdio handles immediately so pipes
        // hit EOF as soon as the job's process tree exits.
        drop(attrs);
        drop(stdin_nul);
        drop(stdout_write);
        drop(stderr_write);
        if ok == 0 {
            return Err(io::Error::last_os_error()).context("couldn't start it");
        }
        let proc_handle = OwnedHandle(pi.hProcess);
        let thread_handle = OwnedHandle(pi.hThread);

        // Assign while suspended so every descendant inherits job membership.
        if let Err(e) = job.assign(proc_handle.0) {
            // SAFETY: `proc_handle.0` is a valid suspended process owned by us.
            unsafe {
                TerminateProcess(proc_handle.0, 1);
            }
            return Err(e).context("assign snippet subprocess to Windows job object");
        }
        // SAFETY: `thread_handle.0` is the primary thread of the newly created process.
        if unsafe { ResumeThread(thread_handle.0) } == u32::MAX {
            let err = io::Error::last_os_error();
            // SAFETY: terminate process we failed to resume.
            unsafe {
                TerminateProcess(proc_handle.0, 1);
            }
            return Err(err).context("resume snippet subprocess");
        }
        drop(thread_handle);

        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stdout).take(snippets::MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
            buf
        });
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stderr).take(4096).read_to_end(&mut buf);
            buf
        });

        let wait_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: `proc_handle.0` is a valid waitable process handle.
        let timed_out = match unsafe { WaitForSingleObject(proc_handle.0, wait_ms) } {
            WAIT_OBJECT_0 => false,
            WAIT_TIMEOUT => true,
            _ => {
                job.terminate();
                let _ = out_thread.join();
                let _ = err_thread.join();
                return Err(io::Error::last_os_error()).context("wait for snippet subprocess");
            }
        };

        if timed_out {
            job.terminate();
            // SAFETY: wait briefly for process teardown before joining pipe readers.
            unsafe {
                WaitForSingleObject(proc_handle.0, 5_000);
            }
            let _ = out_thread.join();
            let _ = err_thread.join();
            bail!("it took longer than {} s", timeout.as_secs_f64());
        }

        let mut exit_code = 1u32;
        // SAFETY: `proc_handle.0` has exited and `exit_code` is a valid out-pointer.
        let success = unsafe { GetExitCodeProcess(proc_handle.0, &raw mut exit_code) } != 0 && exit_code == 0;
        // Terminate any lingering descendants that inherited stdout/stderr pipes
        // before joining the reader threads.
        job.terminate();
        let mut out = out_thread.join().unwrap_or_default();
        let capped = out.len() > snippets::MAX_OUTPUT;
        if capped {
            out.truncate(snippets::MAX_OUTPUT);
            if let Some(i) = out.iter().rposition(|&b| b == b'\n') {
                out.truncate(i + 1);
            }
        }
        let err = err_thread.join().unwrap_or_default();
        Ok(Output {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            success,
            capped,
        })
    }
}

/// Run `cmd` in its own process group: no stdin, output capped, killed
/// (with everything it started) at `timeout`.
fn run(cmd: &Command, dir: &Path, path: &str, timeout: Duration) -> Result<Output> {
    #[cfg(windows)]
    {
        match cmd {
            Command::Argv(argv) => {
                let first = argv.first().context("nothing to run")?;
                let program = find_in(path, first).with_context(|| format!("{first} isn't installed here"))?;
                win_proc::run_in_job(&program, &argv[1..], dir, path, timeout)
            }
            Command::Shell(_) => {
                // Snippet shell templates are rendered with POSIX single-quoting,
                // which `cmd.exe` does not treat as quoting (command injection risk).
                bail!("shell snippet sources aren't supported on Windows yet (tracked in #278)")
            }
        }
    }
    #[cfg(not(windows))]
    {
        let mut c = match cmd {
            Command::Argv(argv) => {
                let first = argv.first().context("nothing to run")?;
                let program = find_in(path, first).with_context(|| format!("{first} isn't installed here"))?;
                let mut c = std::process::Command::new(program);
                c.args(&argv[1..]);
                c
            }
            Command::Shell(text) => {
                #[cfg(unix)]
                {
                    let mut c = std::process::Command::new("/bin/sh");
                    c.arg("-c").arg(text);
                    c
                }
                #[cfg(not(unix))]
                {
                    let _ = text;
                    bail!("shell snippet sources aren't supported on this platform")
                }
            }
        };
        c.current_dir(dir)
            .env("PATH", path)
            // Never wait for a password or a pager.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env_remove("GOATTY_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0);
        }
        let mut child = c.spawn().context("couldn't start it")?;
        #[cfg(unix)]
        let pid = child.id();
        let mut stdout = child.stdout.take().context("no stdout")?;
        let mut stderr = child.stderr.take().context("no stderr")?;
        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stdout).take(snippets::MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
            buf
        });
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stderr).take(4096).read_to_end(&mut buf);
            buf
        });
        let deadline = Instant::now() + timeout;
        let kill = |child: &mut std::process::Child| {
            #[cfg(unix)]
            {
                let _ = child;
                // SAFETY: kill(2) with a negative pid signals the process group the
                // child leads (`process_group(0)`); no memory is involved.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
            }
        };
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if Instant::now() >= deadline {
                kill(&mut child);
                let _ = child.wait();
                let _ = out_thread.join();
                let _ = err_thread.join();
                bail!("it took longer than {} s", timeout.as_secs_f64());
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // Whatever it left running in its group (a pager, a subshell) goes too.
        kill(&mut child);
        let mut out = out_thread.join().unwrap_or_default();
        let capped = out.len() > snippets::MAX_OUTPUT;
        if capped {
            out.truncate(snippets::MAX_OUTPUT);
            // Up to the last whole line.
            if let Some(i) = out.iter().rposition(|&b| b == b'\n') {
                out.truncate(i + 1);
            }
        }
        let err = err_thread.join().unwrap_or_default();
        Ok(Output {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            success: status.success(),
            capped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(snippets: serde_json::Value) -> HostSettings {
        HostSettings { snippets: serde_json::from_value(snippets).unwrap(), ..HostSettings::default() }
    }

    #[test]
    fn sources_get_paths_with_the_home_directory() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
        let values: snippets::Answers = [("dir".to_string(), "~/src\n/opt".to_string()), ("text".to_string(), "~/src".to_string())].into();
        let out = expand_paths(&values, |id| Some(if id == "dir" { InputKind::Folder } else { InputKind::Text }));
        assert_eq!(out["dir"], format!("{}/src\n/opt", home.display()));
        assert_eq!(out["text"], "~/src", "only paths");
    }

    #[test]
    fn lists_user_and_builtin_snippets_with_missing_programs() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let h = settings(json!({
            "mine": {"title": "Mine", "line": "goatty-no-such-program {x} | sort", "input": [{"id": "x", "label": "X"}]},
            "broken": {"title": "Broken", "line": "echo '{x}'", "input": [{"id": "x", "label": "X"}]}
        }));
        let l = list(&dirs, &h);
        let m = l.snippets.iter().find(|s| s.origin.mine()).unwrap();
        assert_eq!(m.snippet.id, "mine");
        assert_eq!(m.missing, ["goatty-no-such-program"], "sort is there");
        assert!(l.issues.iter().any(|i| i.starts_with("snippets.broken.line") && i.contains("inside quotes")), "{:?}", l.issues);
        // The basics pack comes with goatty.
        assert!(l.snippets.iter().any(|s| s.origin.owner == "basics"));
        assert!(l.expansions.iter().any(|e| e.origin.owner == "basics" && e.expansion.say == "| first {n} lines"));
    }

    #[cfg(unix)]
    #[test]
    fn suggestions_come_from_the_hosts_own_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("b.txt"), "").unwrap();
        std::fs::write(work.join("a.txt"), "").unwrap();
        let h = settings(json!({
            "files": {"title": "Pick a file", "line": "cat {f}", "input": [{"id": "f", "label": "File", "source": {"run": ["ls", "-1"]}}]},
            "dep": {"title": "Dependent", "line": "echo {a} {b}", "input": [
                {"id": "a", "label": "A"},
                {"id": "b", "label": "B", "source": {"run": ["printf", "%s\\t%s\\n", "{a}", "detail"], "split": "\t"}}
            ]},
            "slow": {"title": "Slow", "line": "echo {x}", "input": [{"id": "x", "label": "X", "source": {"sh": "sleep 5; echo late", "timeout": 0.3}}]},
            "fails": {"title": "Fails", "line": "echo {x}", "input": [{"id": "x", "label": "X", "source": {"sh": "echo nope >&2; exit 3"}}]},
            "polite": {"title": "Polite", "line": "echo {x}", "input": [{"id": "x", "label": "X", "source": {"sh": "exit 1", "on_fail": "Not in a git repository"}}]}
        }));
        let s = |id: &str, input: &str, values: &[(&str, &str)]| {
            let v = values.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
            suggest(&dirs, &h, MINE, id, input, &v, Some(work.to_str().unwrap())).unwrap()
        };
        let r = s("files", "f", &[]);
        assert_eq!(r.items.iter().map(|i| i.value.as_str()).collect::<Vec<_>>(), ["a.txt", "b.txt"], "in the pane's directory");
        assert_eq!(s("dep", "b", &[]).note.as_deref(), Some("Fill in A first"));
        let r = s("dep", "b", &[("a", "x; rm -rf ~ $(id)")]);
        assert_eq!(r.items[0].value, "x; rm -rf ~ $(id)", "one argument, no shell");
        assert_eq!(r.items[0].detail.as_deref(), Some("detail"));
        let t = Instant::now();
        let r = s("slow", "x", &[]);
        assert!(t.elapsed() < Duration::from_secs(3), "killed at the time limit");
        assert!(r.items.is_empty() && r.note.unwrap().contains("longer than"));
        assert_eq!(s("fails", "x", &[]).note.as_deref(), Some("nope"));
        assert_eq!(s("polite", "x", &[]).note.as_deref(), Some("Not in a git repository"));
        // Only what the host has: no such snippet, owner or input.
        let none = snippets::Answers::new();
        assert!(suggest(&dirs, &h, MINE, "nope", "x", &none, None).is_err());
        assert!(suggest(&dirs, &h, "no-such-plugin", "files", "f", &none, None).is_err());
        assert!(suggest(&dirs, &h, MINE, "files", "nope", &none, None).is_err());
    }

    #[test]
    fn expansions_table_puts_the_users_first_and_honors_the_switch() {
        let mut h = HostSettings {
            expansions: serde_json::from_value(json!({
            "first": {"say": "| first {n} lines", "to": "| head -{n}"},
            "off": {"say": "| quietly please", "to": "> /dev/null", "enabled": false},
            "bad": {"say": "x", "to": "y"}
            }))
            .unwrap(),
            ..HostSettings::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let (active, _) = plugins::active(&Dirs::under(tmp.path()), &h);
        let (table, issues) = render_expansions(&h, &active);
        assert!(issues.iter().any(|i| i.starts_with("expansions.bad")), "{issues:?}");
        let first = table.lines().find(|l| l.starts_with("typeset -ga _goatty_exp_to=")).unwrap();
        assert!(first.contains("$'| head -\\x012\\x01'"), "the user's version of the same words wins: {first}");
        assert_eq!(first.matches("| head -").count(), 1, "{first}");
        assert!(!table.contains("quietly"), "switched off");
        h.shell.expansions = false;
        let (table, _) = render_expansions(&h, &active);
        assert!(table.contains("_goatty_exp_re=()"), "{table}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_descendant_holding_inherited_pipe_does_not_hang_run() {
        let tmp = tempfile::tempdir().unwrap();
        let path_env = std::env::var("PATH").unwrap_or_default();
        // Spawn a background descendant (`start /B ping -n 30`) that inherits
        // stdout/stderr handles while the parent `cmd.exe` prints a marker and
        // exits immediately. Because the child is assigned to the Job Object
        // while `CREATE_SUSPENDED` before `ResumeThread`, the descendant joins
        // the job automatically and `job.terminate()` closes its inherited pipe
        // handles so `run` does not hang for 30 seconds.
        let cmd = Command::Argv(vec![
            "cmd.exe".into(),
            "/C".into(),
            "start /B cmd.exe /C \"ping -n 30 127.0.0.1 >nul\" & echo SNIPPET_DESCENDANT_OK".into(),
        ]);
        let start = std::time::Instant::now();
        let out = run(&cmd, tmp.path(), &path_env, Duration::from_secs(10)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(5), "run hung waiting for descendant pipe: {:?}", start.elapsed());
        assert!(out.success, "stderr: {}", out.stderr);
        assert!(out.stdout.contains("SNIPPET_DESCENDANT_OK"), "stdout: {:?}", out.stdout);

        // Also verify timeout terminates the entire process tree promptly.
        let slow = Command::Argv(vec!["cmd.exe".into(), "/C".into(), "ping -n 30 127.0.0.1 >nul".into()]);
        let start = std::time::Instant::now();
        let err = run(&slow, tmp.path(), &path_env, Duration::from_millis(300)).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(5), "timeout cleanup hung: {:?}", start.elapsed());
        assert!(err.to_string().contains("longer than"), "{err:#}");
    }
}
