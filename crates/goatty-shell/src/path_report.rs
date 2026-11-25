//! What's on PATH in goatty's shell and where each folder came from (#90),
//! for the GUI's PATH page.
//!
//! goatty's startup files note `$PATH` after each step (`_goatty_path_marks`:
//! the system's zshenv, the user's `.zshenv`, …, what was imported from the
//! user's shell, goatty's settings, its
//! tools, plugins, the user's additions). A folder came from the first step
//! whose PATH has it; what was there before the first step was inherited
//! from goattyd's environment. The probe starts a real managed shell the
//! way a session would, so this is the PATH a new tab gets.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::doctor;
use crate::service::HostService;
use crate::setup::tilde;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Prints `step<TAB>PATH` per startup step, then `final<TAB>PATH` (zsh).
pub const COMMAND: &str = r#"builtin print -rl -- "${_goatty_path_marks[@]}" "final"$'\t'"$PATH""#;

/// Prints `step<TAB>PATH` per startup step, then `final<TAB>PATH` (PowerShell).
pub const PS_COMMAND: &str = "foreach ($m in $_goatty_path_marks) { Write-Output $m }; Write-Output \"final`t$env:PATH\"";

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Report {
    /// The shell's PATH, in order.
    pub entries: Vec<Entry>,
    /// `shell.path` as the host applied it, and what became of each folder.
    pub folders: Vec<Folder>,
    /// `$HOME` on the host (`~/…` in `shell.path` means this).
    pub home: Option<String>,
    /// `entries` come from a real managed shell. `false`: from goattyd's
    /// own environment (`problem` says why), all inherited.
    pub probed: bool,
    pub problem: Option<String>,
    /// Sessions run goatty's managed zsh, the only shell that adds `folders`.
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// As it is on PATH.
    pub dir: String,
    /// `~/…` under the home directory.
    pub shown: String,
    pub origin: Origin,
    /// It's a folder (else commands in it can't be found).
    pub exists: bool,
}

/// Which startup step put a folder on PATH.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// Already there when the shell started (goattyd's environment: the
    /// login session, systemd).
    Inherited,
    /// A system-wide startup file (`/etc/zsh/zprofile`, …).
    System { file: String },
    /// One of the user's files (`~/.zprofile`, `~/.config/goatty/zshrc`).
    User { file: String },
    /// goatty's settings (`shell.path`): the user can change these.
    Settings,
    /// goatty's folder of installed tools.
    Tools,
    /// A plugin.
    Plugins,
    /// After the startup files (a plugin that loads last).
    Later,
}

impl Origin {
    /// For people: "your ~/.zprofile", "goatty's settings".
    pub fn label(&self) -> String {
        match self {
            Origin::Inherited => "inherited".into(),
            Origin::System { file } => file.clone(),
            Origin::User { file } => file.clone(),
            Origin::Settings => "goatty's settings".into(),
            Origin::Tools => "goatty's tools".into(),
            Origin::Plugins => "a plugin".into(),
            Origin::Later => "after startup".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// As in the settings (`~/bin`).
    pub entry: String,
    /// With `~` expanded.
    pub dir: String,
    pub state: FolderState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FolderState {
    /// On PATH because of the setting.
    Used,
    /// Doesn't exist: shells skip it.
    Missing,
    /// Exists, but isn't a folder: shells skip it.
    NotAFolder,
    /// On PATH already, from somewhere earlier (so the setting changes nothing).
    Earlier { origin: Origin },
    /// The folder is there but PATH doesn't have it: something removed it
    /// after goatty added it.
    Dropped,
    /// The shell couldn't be asked, or isn't goatty's managed zsh.
    Unchecked,
}

impl HostService {
    /// The PATH report. Starts a shell: call off the UI thread.
    pub fn path_report(&self) -> Report {
        let host = self.settings();
        let launch = self.launcher.launch(&host);
        let inherited = launch.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()).or_else(|| std::env::var("PATH").ok());
        let inherited = inherited.unwrap_or_default();
        let home = crate::dirs::Dirs::home_dir();
        let mut report = Report { home: home.as_ref().map(|h| h.display().to_string()), managed: launch.managed, ..Report::default() };

        let steps = if launch.managed {
            let cmd = if crate::launch::is_powershell(&launch.program) { PS_COMMAND } else { COMMAND };
            match doctor::run(&launch, cmd, PROBE_TIMEOUT) {
                Ok(ran) => match parse_marks(&ran.report) {
                    Some(marks) => Some(marks),
                    None => {
                        report.problem = Some("The shell didn't report its PATH (an older goatty's startup files?)".into());
                        None
                    }
                },
                Err(e) => {
                    report.problem = Some(format!("Couldn't ask the shell: {e:#}"));
                    None
                }
            }
        } else {
            let shell_name = if cfg!(windows) { "PowerShell" } else { "zsh" };
            report.problem = Some(format!("goatty's managed {shell_name} isn't used here, so this is goattyd's own PATH"));
            None
        };
        report.probed = steps.is_some();
        let mut steps = steps.unwrap_or_else(|| vec![("final".into(), inherited.clone())]);
        steps.insert(0, ("inherited".into(), inherited));

        let files = Files::find(self);
        report.entries = attribute(&steps)
            .into_iter()
            .map(|(dir, step)| Entry {
                exists: dir.is_empty() || Path::new(&dir).is_dir(),
                shown: if dir.is_empty() { ".".into() } else { tilde(Path::new(&dir)) },
                origin: files.origin(step),
                dir,
            })
            .collect();
        report.folders = host
            .value
            .shell
            .path
            .iter()
            .map(|entry| {
                let dir = expand(entry, home.as_deref());
                let state = folder_state(Path::new(&dir), report.probed && report.managed, &report.entries);
                Folder { entry: entry.clone(), dir, state }
            })
            .collect();
        report
    }
}

/// `~`, `~/…`, and `~\…` under `home`, like the managed shell expands `shell.path`.
pub fn expand(entry: &str, home: Option<&Path>) -> String {
    match (entry.strip_prefix('~'), home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => {
            format!("{}{rest}", h.display())
        }
        _ => entry.to_owned(),
    }
}

fn folder_state(dir: &Path, checked: bool, entries: &[Entry]) -> FolderState {
    if !dir.exists() {
        return FolderState::Missing;
    }
    if !dir.is_dir() {
        return FolderState::NotAFolder;
    }
    if !checked {
        return FolderState::Unchecked;
    }
    let same = |e: &&Entry| {
        if cfg!(windows) {
            e.dir.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(dir.to_string_lossy().trim_end_matches(['\\', '/']))
        } else {
            Path::new(&e.dir) == dir
        }
    };
    match entries.iter().find(same) {
        Some(e) if e.origin == Origin::Settings => FolderState::Used,
        Some(e) => FolderState::Earlier { origin: e.origin.clone() },
        None => FolderState::Dropped,
    }
}

/// `step<TAB>PATH` lines, the last one `final`. `None` when the shell
/// didn't print its marks.
pub fn parse_marks(text: &str) -> Option<Vec<(String, String)>> {
    let marks: Vec<(String, String)> =
        text.lines().filter_map(|l| l.split_once('\t')).map(|(step, path)| (step.to_owned(), path.to_owned())).collect();
    (marks.len() > 1 && marks.last().is_some_and(|(s, _)| s == "final")).then_some(marks)
}

/// The final PATH's folders, each with the first step whose PATH has it.
pub fn attribute(steps: &[(String, String)]) -> Vec<(String, &str)> {
    attribute_with(steps, if cfg!(windows) { ';' } else { ':' })
}

/// [`attribute`] with an explicit PATH separator (`:` on Unix, `;` on Windows).
pub fn attribute_with(steps: &[(String, String)], sep: char) -> Vec<(String, &str)> {
    let Some((_, last)) = steps.last() else { return Vec::new() };
    let mut seen = std::collections::HashSet::new();
    let norm = |d: &str| if sep == ';' { d.trim_end_matches(['\\', '/']).to_ascii_lowercase() } else { d.to_owned() };
    last.split(sep)
        .filter(|dir| !(sep == ';' && dir.is_empty()) && seen.insert(norm(dir)))
        .map(|dir| {
            let key = norm(dir);
            let step = steps.iter().find(|(_, p)| p.split(sep).any(|d| norm(d) == key)).map_or("final", |(s, _)| s.as_str());
            (dir.to_owned(), step)
        })
        .collect()
}

/// The files behind the steps, as the user knows them.
struct Files {
    zdotdir: PathBuf,
    additions: String,
    /// Where imported PATH folders came from (`~/.zshrc (imported)`).
    imported: String,
}

impl Files {
    fn find(svc: &HostService) -> Self {
        let zdotdir = std::env::var_os("ZDOTDIR").map(PathBuf::from).or_else(crate::dirs::Dirs::home_dir).unwrap_or_default();
        let additions = if cfg!(windows) {
            svc.launcher.dirs().map_or_else(|| r"~\AppData\Roaming\goatty\profile.ps1".into(), |d| tilde(&d.user_ps_profile()))
        } else {
            svc.launcher.dirs().map_or_else(|| "~/.config/goatty/zshrc".into(), |d| tilde(&d.user_zshrc()))
        };
        let imported = svc.launcher.dirs().map_or_else(|| "your shell's startup files (imported)".into(), crate::import::path_origin);
        Files { zdotdir, additions, imported }
    }

    fn origin(&self, step: &str) -> Origin {
        let user = |name: &str| Origin::User { file: tilde(&self.zdotdir.join(name)) };
        match step {
            "inherited" => Origin::Inherited,
            "etc-zshenv" => Origin::System { file: system_file("zshenv") },
            "etc-zprofile" => Origin::System { file: system_file("zprofile") },
            "etc-zshrc" => Origin::System { file: system_file("zshrc") },
            "zshenv" => user(".zshenv"),
            "zprofile" => user(".zprofile"),
            "zshrc" => user(".zshrc"),
            "profile" => Origin::User { file: "$PROFILE".into() },
            // A User origin (not a new kind), so older windows can read it.
            "imported" => Origin::User { file: self.imported.clone() },
            "settings" => Origin::Settings,
            "tools" => Origin::Tools,
            "plugins" => Origin::Plugins,
            "additions" => Origin::User { file: self.additions.clone() },
            _ => Origin::Later,
        }
    }
}

/// zsh's system-wide startup file `name`: `/etc/zsh/…` (Debian, Arch) or `/etc/…`.
fn system_file(name: &str) -> String {
    let debian = format!("/etc/zsh/{name}");
    if Path::new(&debian).exists() { debian } else { format!("/etc/{name}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(s, p)| ((*s).to_owned(), (*p).to_owned())).collect()
    }

    #[test]
    fn folders_come_from_the_first_step_that_has_them() {
        let s = steps(&[
            ("inherited", "/usr/bin:/bin"),
            ("etc-zshenv", "/usr/bin:/bin"),
            // /etc/profile resets PATH: /usr/bin is still inherited.
            ("etc-zprofile", "/usr/local/bin:/usr/bin"),
            ("zprofile", "/home/u/.cargo/bin:/usr/local/bin:/usr/bin"),
            ("settings", "/home/u/.cargo/bin:/usr/local/bin:/usr/bin:/opt/x/bin"),
            ("final", "/home/u/.cargo/bin:/usr/local/bin:/usr/bin:/opt/x/bin:/late:/usr/bin"),
        ]);
        assert_eq!(
            attribute_with(&s, ':'),
            [
                ("/home/u/.cargo/bin".to_owned(), "zprofile"),
                ("/usr/local/bin".to_owned(), "etc-zprofile"),
                ("/usr/bin".to_owned(), "inherited"),
                ("/opt/x/bin".to_owned(), "settings"),
                ("/late".to_owned(), "final"),
            ],
            "duplicates show once"
        );
    }

    #[test]
    fn windows_semicolon_path_attribution() {
        let s = steps(&[
            ("inherited", r"C:\Windows\System32;C:\Program Files\PowerShell\7"),
            ("settings", r"C:\Windows\System32;C:\Program Files\PowerShell\7;C:\Users\u\bin"),
            ("tools", r"C:\Windows\System32;C:\Program Files\PowerShell\7;C:\Users\u\bin;C:\Users\u\AppData\Local\goatty\bin"),
            (
                "final",
                r"C:\Windows\System32;C:\Program Files\PowerShell\7;C:\Users\u\bin;C:\Users\u\AppData\Local\goatty\bin;c:\windows\system32\",
            ),
        ]);
        assert_eq!(
            attribute_with(&s, ';'),
            [
                (r"C:\Windows\System32".to_owned(), "inherited"),
                (r"C:\Program Files\PowerShell\7".to_owned(), "inherited"),
                (r"C:\Users\u\bin".to_owned(), "settings"),
                (r"C:\Users\u\AppData\Local\goatty\bin".to_owned(), "tools"),
            ],
            "case-insensitive dedup with trailing slash normalization"
        );
    }

    #[test]
    fn marks_need_the_final_line() {
        assert_eq!(
            parse_marks("etc-zshenv\t/bin\nnoise\nfinal\t/bin:/x\n"),
            Some(vec![("etc-zshenv".into(), "/bin".into()), ("final".into(), "/bin:/x".into())])
        );
        assert_eq!(parse_marks("final\t/bin\n"), None, "no marks: startup files without them");
        assert_eq!(parse_marks("etc-zshenv\t/bin\n"), None);
    }

    #[test]
    fn tilde_expands_like_the_shell() {
        let home = Path::new("/home/u");
        assert_eq!(expand("~", Some(home)), "/home/u");
        assert_eq!(expand("~/bin", Some(home)), "/home/u/bin");
        assert_eq!(expand(r"~\bin", Some(Path::new(r"C:\Users\u"))), r"C:\Users\u\bin");
        assert_eq!(expand("~bin", Some(home)), "~bin");
        assert_eq!(expand("/opt/bin", Some(home)), "/opt/bin");
        assert_eq!(expand("~/bin", None), "~/bin");
    }

    #[test]
    fn folder_states() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        std::fs::create_dir(&dir).unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, "").unwrap();
        let entry = |origin| Entry { dir: dir.display().to_string(), shown: String::new(), origin, exists: true };
        assert_eq!(folder_state(&tmp.path().join("nope"), true, &[]), FolderState::Missing);
        assert_eq!(folder_state(&file, true, &[]), FolderState::NotAFolder);
        assert_eq!(folder_state(&dir, false, &[]), FolderState::Unchecked);
        assert_eq!(folder_state(&dir, true, &[]), FolderState::Dropped);
        assert_eq!(folder_state(&dir, true, &[entry(Origin::Settings)]), FolderState::Used);
        let zprofile = Origin::User { file: "~/.zprofile".into() };
        assert_eq!(folder_state(&dir, true, &[entry(zprofile.clone())]), FolderState::Earlier { origin: zprofile });
    }
}
