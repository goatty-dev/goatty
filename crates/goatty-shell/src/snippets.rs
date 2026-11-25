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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

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
    folders.extend(env_path.unwrap_or_default().split(':').filter(|d| !d.is_empty()).map(str::to_string));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    folders.extend(settings.shell.path.iter().map(|p| crate::path_report::expand(p, home.as_deref())));
    folders.push(dirs.bin_dir().to_string_lossy().into_owned());
    let mut seen = BTreeSet::new();
    folders.retain(|f| seen.insert(f.clone()));
    folders.join(":")
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

/// Like [`find_in`], resolving relative paths containing `/` against `cwd`.
pub(crate) fn find_in_with_cwd(path: &str, program: &str, cwd: Option<&str>) -> Option<PathBuf> {
    if program.contains('/') {
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
    path.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join(program)).find(|p| is_executable(p))
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

struct Output {
    stdout: String,
    stderr: String,
    success: bool,
    /// Stopped reading at [`snippets::MAX_OUTPUT`].
    capped: bool,
}

#[cfg(windows)]
mod win_job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };

    pub(super) struct JobObject(HANDLE);

    impl JobObject {
        pub(super) fn new() -> std::io::Result<Self> {
            // SAFETY: null attributes and unnamed job object are valid.
            let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if h.is_null() {
                return Err(std::io::Error::last_os_error());
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
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }

        pub(super) fn assign(&self, proc_handle: HANDLE) -> std::io::Result<()> {
            // SAFETY: both `self.0` and `proc_handle` are valid open handles.
            if unsafe { AssignProcessToJobObject(self.0, proc_handle) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        pub(super) fn terminate(&self) {
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
}

/// Run `cmd` in its own process group: no stdin, output capped, killed
/// (with everything it started) at `timeout`.
fn run(cmd: &Command, dir: &Path, path: &str, timeout: Duration) -> Result<Output> {
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
                // Snippet shell templates are rendered with POSIX single-quoting,
                // which `cmd.exe` does not treat as quoting (command injection risk).
                let _ = text;
                bail!("shell snippet sources aren't supported on Windows yet (tracked in #278)")
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
        c.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(windows)]
    let job = win_job::JobObject::new().context("create Windows job object")?;
    let mut child = c.spawn().context("couldn't start it")?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        if let Err(e) = job.assign(child.as_raw_handle())
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e).context("assign snippet subprocess to Windows job object");
        }
    }
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
        #[cfg(windows)]
        {
            job.terminate();
            let _ = child.kill();
        }
        #[cfg(not(any(unix, windows)))]
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
    // Whatever it left running in its group / job (a pager, a subshell) goes too.
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
}
