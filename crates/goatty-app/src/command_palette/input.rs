//! Commands that ask for input (#91), like VS Code's input boxes: the
//! palette turns into one field at a time, with a label, a placeholder, a
//! starting value, a check that runs as you type, and suggestions to pick
//! from (choices, known hosts, folders on this computer).
//!
//! Anything can ask, a built-in command, a plugin's `[[command]]` or code
//! like the host list: build an [`Ask`] and call [`ask`]. The palette
//! (re)opens in the window, asks each [`Field`] in turn (Enter: next, Esc:
//! back, Tab: complete), and calls [`Ask::then`] with the [`Answers`]
//! after it closed, so the answer handler can act on the focused terminal.
//!
//! The checks are `goatty_plugin::commands::check_value` (types, limits,
//! patterns, choices: the same for plugins and built-ins) plus an optional
//! extra check per field. Paths are only looked at on disk when the host
//! the answer is for is this computer ([`Ask::local`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use goatty_plugin::commands::{CommandInput, InputChoice, InputKind, check_value};
use gpui::{App, SharedString, Window};

/// Values by field id. Optional fields left empty are `""`.
pub type Answers = BTreeMap<String, String>;

/// What runs with the answers, after the palette closed.
pub type AnswerHandler = Rc<dyn Fn(&Answers, &mut Window, &mut App)>;

/// An extra check: `Some(message)` when the value can't be used.
pub type Check = Rc<dyn Fn(&str) -> Option<String>>;

/// Most suggestions listed for a folder.
const MAX_PATH_SUGGESTIONS: usize = 200;

/// One thing to ask.
#[derive(Clone)]
pub struct Field {
    /// The type, limits and texts (the format plugins use).
    pub spec: CommandInput,
    /// Run after the type check.
    pub check: Option<Check>,
    /// Pick from these; with `custom`, anything else that passes the
    /// checks is fine too (hosts), without it only these (choices).
    pub suggestions: Vec<Suggestion>,
    pub custom: bool,
}

/// An entry under the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// What picking it answers (or, for a folder, completes to).
    pub value: String,
    pub label: String,
    pub detail: Option<String>,
}

impl Field {
    fn new(id: &str, label: &str, kind: InputKind) -> Self {
        Self {
            spec: CommandInput {
                id: id.into(),
                label: label.into(),
                placeholder: String::new(),
                help: String::new(),
                kind,
                default: None,
                optional: false,
                min: None,
                max: None,
                pattern: None,
                pattern_help: None,
                choices: Vec::new(),
                must_exist: false,
            },
            check: None,
            suggestions: Vec::new(),
            custom: true,
        }
    }

    /// Any text.
    pub fn text(id: &str, label: &str) -> Self {
        Self::new(id, label, InputKind::Text)
    }

    /// A whole number between `min` and `max`.
    #[cfg_attr(not(test), expect(dead_code, reason = "API for commands; plugins use it through from_plugin"))]
    pub fn integer(id: &str, label: &str, min: Option<i64>, max: Option<i64>) -> Self {
        let mut f = Self::new(id, label, InputKind::Integer);
        f.spec.min = min.map(|n| n as f64);
        f.spec.max = max.map(|n| n as f64);
        f
    }

    /// A number (decimals allowed) between `min` and `max`.
    pub fn number(id: &str, label: &str, min: Option<f64>, max: Option<f64>) -> Self {
        let mut f = Self::new(id, label, InputKind::Number);
        f.spec.min = min;
        f.spec.max = max;
        f
    }

    /// A folder; `must_exist` is checked on this computer only.
    pub fn folder(id: &str, label: &str, must_exist: bool) -> Self {
        let mut f = Self::new(id, label, InputKind::Folder);
        f.spec.must_exist = must_exist;
        f
    }

    /// One of `choices` (value, label, description).
    pub fn choice(id: &str, label: &str, choices: Vec<(String, String, String)>) -> Self {
        let mut f = Self::new(id, label, InputKind::Choice);
        f.spec.choices = choices.into_iter().map(|(value, label, help)| InputChoice { value, label: Some(label), help }).collect();
        f.suggestions = choice_suggestions(&f.spec.choices);
        f.custom = false;
        f
    }

    /// An ssh destination (`user@host`, an alias); `known`: the hosts to
    /// suggest (host, label).
    pub fn host(id: &str, label: &str, known: Vec<(String, String)>) -> Self {
        let mut f = Self::text(id, label);
        f.check = Some(Rc::new(|h: &str| goatty_config::ssh_host_problem(h.trim()).map(|p| format!("Not a host: {p}"))));
        f.suggestions = known
            .into_iter()
            .map(|(host, label)| {
                let detail = (label != host).then(|| host.clone());
                Suggestion { value: host, label, detail }
            })
            .collect();
        f
    }

    /// A field from a plugin's `[[command.input]]`.
    pub fn from_plugin(spec: &CommandInput) -> Self {
        let mut f = Self::new(&spec.id, &spec.label, spec.kind);
        f.spec = spec.clone();
        if spec.kind == InputKind::Choice {
            f.suggestions = choice_suggestions(&spec.choices);
            f.custom = false;
        }
        f
    }

    pub fn placeholder(mut self, text: impl Into<String>) -> Self {
        self.spec.placeholder = text.into();
        self
    }

    pub fn help(mut self, text: impl Into<String>) -> Self {
        self.spec.help = text.into();
        self
    }

    /// The text the field starts with.
    pub fn initial(mut self, text: impl Into<String>) -> Self {
        self.spec.default = Some(text.into());
        self
    }

    pub fn optional(mut self) -> Self {
        self.spec.optional = true;
        self
    }

    pub fn check(mut self, f: impl Fn(&str) -> Option<String> + 'static) -> Self {
        self.check = Some(Rc::new(f));
        self
    }

    /// Paths are completed from the file system (on this computer).
    pub fn is_path(&self) -> bool {
        matches!(self.spec.kind, InputKind::Path | InputKind::Folder)
    }

    /// Only the listed choices are answers.
    pub fn strict(&self) -> bool {
        !self.custom
    }

    /// The value to answer with for `text`, or what's wrong with it. Paths
    /// that must exist are checked separately ([`missing_path`]).
    pub fn validate(&self, text: &str) -> Result<String, String> {
        let text = if self.spec.kind == InputKind::Choice { text.to_string() } else { text.trim().to_string() };
        let value = check_value(&self.spec, &text)?;
        if value.is_empty() {
            return Ok(value);
        }
        if let Some(problem) = self.check.as_ref().and_then(|c| c(&value)) {
            return Err(problem);
        }
        Ok(value)
    }
}

fn choice_suggestions(choices: &[InputChoice]) -> Vec<Suggestion> {
    choices
        .iter()
        .map(|c| Suggestion { value: c.value.clone(), label: c.label().to_string(), detail: (!c.help.is_empty()).then(|| c.help.clone()) })
        .collect()
}

/// A request for input.
pub struct Ask {
    /// What it's for: the command ("Add a folder to PATH").
    pub title: SharedString,
    pub fields: Vec<Field>,
    /// The answers are for this computer: paths can be checked and
    /// completed here.
    pub local: bool,
    pub then: AnswerHandler,
}

impl Ask {
    pub fn new(title: impl Into<SharedString>, fields: Vec<Field>, then: impl Fn(&Answers, &mut Window, &mut App) + 'static) -> Self {
        Self { title: title.into(), fields, local: true, then: Rc::new(then) }
    }

    /// The answers are for another host: don't look at this computer's files.
    pub fn remote(mut self, remote: bool) -> Self {
        self.local = !remote;
        self
    }
}

/// Ask in `window`'s palette. Deferred, so it can be called from a command
/// the palette is running (which runs inside the workspace).
pub fn ask(window: &mut Window, cx: &mut App, ask: Ask) {
    window.defer(cx, move |window, cx| {
        let Some(Some(ws)) = window.root::<crate::workspace::Workspace>() else {
            log::warn!("can't ask \"{}\": not a terminal window", ask.title);
            return;
        };
        ws.update(cx, |ws, cx| ws.open_ask(ask, window, cx));
    });
}

/// `~` and `~/…` with `home`.
pub fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

/// `path` with the home directory as `~` (for showing and storing).
pub fn contract_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Why `value` (a path field's answer) isn't there, looked up on this
/// computer. Blocking: call it off the UI thread.
pub fn missing_path(spec: &CommandInput, value: &str, home: Option<&Path>) -> Option<String> {
    if !spec.must_exist || value.is_empty() {
        return None;
    }
    let p = expand_home(value, home);
    match std::fs::metadata(&p) {
        Ok(m) if spec.kind == InputKind::Folder && !m.is_dir() => Some("That's a file, not a folder".into()),
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some("Doesn't exist".into()),
        Err(e) => Some(format!("Can't open it: {e}")),
    }
}

/// Which folder to list for `text` and the name prefix to filter by:
/// everything up to and including the last `/`. `None` for text that
/// isn't a path yet (no `/`, other than a lone `~`).
pub fn listing_for(text: &str) -> Option<(String, String)> {
    if text == "~" {
        return Some(("~/".into(), String::new()));
    }
    let i = text.rfind('/')?;
    if !(text.starts_with('/') || text.starts_with("~/")) {
        return None;
    }
    Some((text[..=i].to_string(), text[i + 1..].to_string()))
}

/// The suggestions for a folder's `entries` (name, is a folder): names
/// starting with `prefix` (case-insensitive; hidden ones only for a `.`
/// prefix), folders first, each completing `base` + name (+ `/`).
pub fn path_suggestions(base: &str, prefix: &str, mut entries: Vec<(String, bool)>, folders_only: bool) -> Vec<Suggestion> {
    let lower = prefix.to_lowercase();
    entries.retain(|(name, dir)| {
        (*dir || !folders_only) && (prefix.starts_with('.') || !name.starts_with('.')) && name.to_lowercase().starts_with(&lower)
    });
    entries.sort_by(|(a, ad), (b, bd)| bd.cmp(ad).then_with(|| a.to_lowercase().cmp(&b.to_lowercase())));
    entries
        .into_iter()
        .take(MAX_PATH_SUGGESTIONS)
        .map(|(name, dir)| {
            let value = format!("{base}{name}{}", if dir { "/" } else { "" });
            Suggestion { label: value.clone(), value, detail: dir.then(|| "folder".into()) }
        })
        .collect()
}

/// Read the folder for `text` (blocking). Unreadable folders list nothing.
pub fn list_folder(text: &str, home: Option<&Path>, folders_only: bool) -> Vec<Suggestion> {
    let Some((base, prefix)) = listing_for(text) else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(expand_home(&base, home)) else { return Vec::new() };
    // Big folders: stop reading at some point; the prefix narrows it down.
    let entries = rd
        .filter_map(Result::ok)
        .take(20_000)
        .map(|e| {
            let dir = e.file_type().is_ok_and(|t| t.is_dir() || (t.is_symlink() && e.path().is_dir()));
            (e.file_name().to_string_lossy().into_owned(), dir)
        })
        .collect();
    path_suggestions(&base, &prefix, entries, folders_only)
}

/// The value as a path field would store it: a trailing `/` from
/// completing removed.
pub fn trim_completed(value: &str) -> &str {
    if value.len() > 1 { value.strip_suffix('/').unwrap_or(value) } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_check_their_type_and_extra_rules() {
        let n = Field::integer("n", "N", Some(1), Some(8));
        assert_eq!(n.validate(" 3 "), Ok("3".into()));
        assert_eq!(n.validate("9"), Err("Must be between 1 and 8".into()));
        assert_eq!(n.validate(""), Err("Required".into()));
        let f = Field::number("s", "Size", Some(6.0), Some(48.0));
        assert_eq!(f.validate("12.5"), Ok("12.5".into()));
        assert!(f.validate("big").is_err());
        let h = Field::host("h", "Host", vec![("pi".into(), "Raspberry".into())]);
        assert_eq!(h.validate(" me@box "), Ok("me@box".into()));
        assert_eq!(h.validate("-oProxyCommand=x"), Err("Not a host: it can't start with -".into()));
        assert_eq!(h.suggestions[0].detail.as_deref(), Some("pi"));
        assert!(!h.strict());
        let c = Field::choice("c", "Pick", vec![("a".into(), "Apple".into(), String::new())]);
        assert!(c.strict());
        assert_eq!(c.validate("a"), Ok("a".into()));
        assert!(c.validate("Apple").is_err(), "values, not labels");
        let opt = Field::text("t", "T").optional().check(|v| v.contains(' ').then(|| "No spaces".into()));
        assert_eq!(opt.validate(""), Ok(String::new()), "the extra check skips empty optional values");
        assert_eq!(opt.validate("a b"), Err("No spaces".into()));
    }

    #[test]
    fn home_in_paths() {
        let home = Some(Path::new("/home/me"));
        assert_eq!(expand_home("~/bin", home), PathBuf::from("/home/me/bin"));
        assert_eq!(expand_home("~", home), PathBuf::from("/home/me"));
        assert_eq!(expand_home("~other/x", home), PathBuf::from("~other/x"));
        assert_eq!(expand_home("/opt", home), PathBuf::from("/opt"));
        assert_eq!(contract_home(Path::new("/home/me/src/x"), home), "~/src/x");
        assert_eq!(contract_home(Path::new("/home/me"), home), "~");
        assert_eq!(contract_home(Path::new("/home/meow"), home), "/home/meow");
    }

    #[test]
    fn completes_paths() {
        assert_eq!(listing_for("/usr/lo"), Some(("/usr/".into(), "lo".into())));
        assert_eq!(listing_for("~"), Some(("~/".into(), String::new())));
        assert_eq!(listing_for("~/"), Some(("~/".into(), String::new())));
        assert_eq!(listing_for("bin"), None);
        assert_eq!(listing_for("a/b"), None, "relative paths aren't listed");
        let entries =
            vec![("Zeta".into(), true), ("local".into(), true), ("lib".into(), false), (".hidden".into(), true), ("Lab".into(), true)];
        let v: Vec<String> = path_suggestions("/usr/", "l", entries.clone(), false).into_iter().map(|s| s.value).collect();
        assert_eq!(v, ["/usr/Lab/", "/usr/local/", "/usr/lib"], "folders first, case-insensitive");
        let v: Vec<String> = path_suggestions("/usr/", "", entries.clone(), true).into_iter().map(|s| s.value).collect();
        assert_eq!(v, ["/usr/Lab/", "/usr/local/", "/usr/Zeta/"], "no files, no hidden");
        let v: Vec<String> = path_suggestions("/usr/", ".", entries, true).into_iter().map(|s| s.value).collect();
        assert_eq!(v, ["/usr/.hidden/"]);
        assert_eq!(trim_completed("/usr/local/"), "/usr/local");
        assert_eq!(trim_completed("/"), "/");
    }

    #[test]
    fn looks_at_the_disk() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("file"), "").unwrap();
        let f = Field::folder("d", "Folder", true).spec;
        let at = |n: &str| tmp.path().join(n).display().to_string();
        assert_eq!(missing_path(&f, &at("sub"), None), None);
        assert_eq!(missing_path(&f, &at("file"), None).as_deref(), Some("That's a file, not a folder"));
        assert_eq!(missing_path(&f, &at("nope"), None).as_deref(), Some("Doesn't exist"));
        assert_eq!(missing_path(&Field::folder("d", "Folder", false).spec, &at("nope"), None), None);
        let listed: Vec<String> = list_folder(&format!("{}/", tmp.path().display()), None, false).into_iter().map(|s| s.label).collect();
        assert_eq!(listed, [format!("{}/sub/", tmp.path().display()), format!("{}/file", tmp.path().display())]);
    }
}
