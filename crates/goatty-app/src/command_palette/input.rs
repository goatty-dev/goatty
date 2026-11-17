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
//! A list to pick from is a field too ([`Field::list`], or [`Ask::pick`]
//! for a one-step pick): the palette lists the items with the same fuzzy
//! search, the current value selected. [`Field::on_select`] runs whenever
//! the selection moves (a live preview: the theme switcher shows each
//! theme as you go), and [`Ask::on_cancel`] when the palette closes
//! without an answer (Esc puts the old theme back). A field's items can
//! also come from a [`Loader`] (a plugin's `[command.input.source]` runs
//! on the host).
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

/// Code that runs with the window later (going back to the snippet picker).
pub type WindowAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// An extra check: `Some(message)` when the value can't be used.
pub type Check = Rc<dyn Fn(&str) -> Option<String>>;

/// Suggestions that take a while (a program on the host): given the
/// answers so far, the work to do off the UI thread.
pub type Loader = Rc<dyn Fn(&Answers) -> Job>;
pub type Job = Box<dyn FnOnce() -> Loaded + Send>;

/// What a [`Loader`]'s job found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub items: Vec<Suggestion>,
    /// Shown under the field (why there are none, "Showing the first ones").
    pub note: Option<String>,
}

/// Runs when the selection in a list moves, with the selected value: a
/// live preview. Not for answering (that's [`Ask::then`]).
pub type OnSelect = Rc<dyn Fn(&str, &mut App)>;

/// Runs when the palette closes without an answer: undo a preview.
pub type OnCancel = Rc<dyn Fn(&mut App)>;

/// The finished line shown above the fields while they're asked, updated
/// as you type: (answers so far, the field being asked, its text).
pub type Preview = Rc<dyn Fn(&Answers, &str, &str) -> Vec<goatty_plugin::snippets::Seg>>;

/// A small label next to the title (a snippet's "Deletes data").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub text: SharedString,
    pub tone: Tone,
    pub tooltip: SharedString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Danger,
}

/// The field id of [`Ask::pick`]'s one field.
const PICKED: &str = "picked";

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
    /// Leading and trailing spaces aren't part of the answer (hosts).
    /// Text and paths keep theirs; numbers are trimmed by their check.
    pub trim: bool,
    /// Suggestions fetched when the field is asked (they replace `suggestions`).
    pub loader: Option<Loader>,
    /// Takes several values: Enter adds one, Enter on an empty field goes
    /// on. The answer is the values separated by `\n`.
    pub multiple: bool,
    /// A pick-from-list field's live preview (see [`OnSelect`]).
    pub on_select: Option<OnSelect>,
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
                source: None,
                strict: false,
            },
            check: None,
            suggestions: Vec::new(),
            custom: true,
            trim: false,
            loader: None,
            multiple: false,
            on_select: None,
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

    /// One of `items`, picked from a list (only these are answers).
    pub fn list(id: &str, label: &str, items: Vec<Suggestion>) -> Self {
        let mut f = Self::new(id, label, InputKind::Choice);
        f.spec.choices = items
            .iter()
            .map(|s| InputChoice { value: s.value.clone(), label: Some(s.label.clone()), help: s.detail.clone().unwrap_or_default() })
            .collect();
        f.suggestions = items;
        f.custom = false;
        f
    }

    /// One of the items `loader` finds (fetched when the field is asked).
    pub fn loaded_list(id: &str, label: &str, loader: Loader) -> Self {
        let mut f = Self::text(id, label);
        f.loader = Some(loader);
        f.custom = false;
        f
    }

    /// An ssh destination (`user@host`, an alias); `known`: the hosts to
    /// suggest (host, label).
    pub fn host(id: &str, label: &str, known: Vec<(String, String)>) -> Self {
        let mut f = Self::text(id, label);
        f.trim = true;
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

    /// A field from a plugin's `[[command.input]]`. `loader` fetches its
    /// source's suggestions (`[command.input.source]`, run on the host).
    pub fn from_plugin(spec: &CommandInput, loader: Option<Loader>) -> Self {
        let mut f = Self::new(&spec.id, &spec.label, spec.kind);
        f.spec = spec.clone();
        if spec.kind == InputKind::Choice {
            f.suggestions = choice_suggestions(&spec.choices);
            f.custom = false;
        }
        if spec.source.is_some() {
            f.loader = loader;
            f.custom = !spec.strict;
        }
        f
    }

    /// A field for a snippet's input: choices and flags pick from a list,
    /// a source's suggestions come from `loader`.
    pub fn from_snippet(input: &goatty_plugin::snippets::SnippetInput, loader: Option<Loader>) -> Self {
        let mut f = Self::from_plugin(&input.spec(), None);
        if input.kind == InputKind::Flag {
            let on = input.on.clone().unwrap_or_default();
            f.suggestions = vec![
                Suggestion { value: "yes".into(), label: "Yes".into(), detail: Some(format!("adds {on}")) },
                Suggestion { value: "no".into(), label: "No".into(), detail: None },
            ];
            f.custom = false;
            if f.spec.default.is_none() {
                f.spec.default = Some("no".into());
            }
        }
        f.multiple = input.multiple;
        if input.source.is_some() {
            f.loader = loader;
            f.custom = !input.strict;
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
        let text = if self.trim { text.trim() } else { text };
        let value = check_value(&self.spec, text)?;
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
    /// What relative paths are relative to: the directory of the terminal
    /// the answer is for (set by the workspace when it's on this computer).
    /// Without one, relative paths aren't looked up.
    pub base: Option<PathBuf>,
    pub then: AnswerHandler,
    /// The finished line, shown above the fields.
    pub preview: Option<Preview>,
    pub badge: Option<Badge>,
    /// Esc on the first field calls this instead of just closing (back to
    /// the snippet picker).
    pub back: Option<WindowAction>,
    /// What Enter on the last field does ("insert").
    pub done_label: Option<SharedString>,
    /// Answers to start with (going back into a filled-in snippet).
    pub answers: Answers,
    /// Runs when the palette closes without answering (Esc, a click
    /// outside, another palette replacing it): undo a live preview.
    pub on_cancel: Option<OnCancel>,
}

impl Ask {
    pub fn new(title: impl Into<SharedString>, fields: Vec<Field>, then: impl Fn(&Answers, &mut Window, &mut App) + 'static) -> Self {
        Self {
            title: title.into(),
            fields,
            local: true,
            base: None,
            then: Rc::new(then),
            preview: None,
            badge: None,
            back: None,
            done_label: None,
            answers: Answers::new(),
            on_cancel: None,
        }
    }

    /// Pick one of `items` (`current` selected to start with), then
    /// `then` with its value: a command's second step ("Switch theme…").
    pub fn pick(
        title: impl Into<SharedString>,
        label: &str,
        items: Vec<Suggestion>,
        current: Option<&str>,
        then: impl Fn(&str, &mut Window, &mut App) + 'static,
    ) -> Self {
        let mut field = Field::list(PICKED, label, items);
        field.spec.default = current.map(str::to_string);
        Self::new(title, vec![field], move |answers, window, cx| {
            if let Some(v) = answers.get(PICKED) {
                then(v, window, cx);
            }
        })
    }

    /// Call `f` when the palette closes without an answer.
    pub fn on_cancel(mut self, f: impl Fn(&mut App) + 'static) -> Self {
        self.on_cancel = Some(Rc::new(f));
        self
    }

    /// The first field's live preview (see [`Field::on_select`]).
    pub fn on_select(mut self, f: impl Fn(&str, &mut App) + 'static) -> Self {
        if let Some(field) = self.fields.first_mut() {
            field.on_select = Some(Rc::new(f));
        }
        self
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
/// computer; a relative one in `base` (the terminal's directory, where the
/// answer is used), or not at all without one. Blocking: call it off the
/// UI thread.
pub fn missing_path(spec: &CommandInput, value: &str, home: Option<&Path>, base: Option<&Path>) -> Option<String> {
    if !spec.must_exist || value.is_empty() {
        return None;
    }
    let mut p = expand_home(value, home);
    if p.is_relative() {
        p = base?.join(p);
    }
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
        assert_eq!(Field::text("t", "T").validate(" a "), Ok(" a ".into()), "text keeps its spaces");
        assert_eq!(Field::folder("d", "D", false).validate("/tmp/build "), Ok("/tmp/build ".into()), "paths too");
    }

    #[test]
    fn lists_pick_one_of_their_items() {
        let item = |v: &str, l: &str| Suggestion { value: v.into(), label: l.into(), detail: None };
        let f = Field::list("t", "Theme", vec![item("dracula", "Dracula"), item("nord", "Nord")]);
        assert!(f.strict());
        assert_eq!(f.validate("nord"), Ok("nord".into()));
        assert_eq!(f.validate("Nord"), Err("Pick one from the list".into()), "values, not labels");
        let ask = Ask::pick("Switch theme", "Theme", vec![item("nord", "Nord")], Some("nord"), |_, _, _| {}).on_select(|_, _| {});
        assert_eq!(ask.fields[0].spec.default.as_deref(), Some("nord"));
        assert!(ask.fields[0].on_select.is_some() && ask.on_cancel.is_none());
        let loaded = Field::loaded_list("c", "Container", Rc::new(|_: &Answers| -> Job { Box::new(Loaded::default) }));
        assert!(loaded.strict() && loaded.loader.is_some());
        assert_eq!(loaded.validate("anything"), Ok("anything".into()), "checked against what the loader found");
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
        assert_eq!(missing_path(&f, &at("sub"), None, None), None);
        assert_eq!(missing_path(&f, &at("file"), None, None).as_deref(), Some("That's a file, not a folder"));
        assert_eq!(missing_path(&f, &at("nope"), None, None).as_deref(), Some("Doesn't exist"));
        assert_eq!(missing_path(&Field::folder("d", "Folder", false).spec, &at("nope"), None, None), None);
        // Relative: in the terminal's directory, or not looked up without one.
        assert_eq!(missing_path(&f, "sub", None, Some(tmp.path())), None);
        assert_eq!(missing_path(&f, "nope", None, Some(tmp.path())).as_deref(), Some("Doesn't exist"));
        assert_eq!(missing_path(&f, "nope", None, None), None);
        let listed: Vec<String> = list_folder(&format!("{}/", tmp.path().display()), None, false).into_iter().map(|s| s.label).collect();
        assert_eq!(listed, [format!("{}/sub/", tmp.path().display()), format!("{}/file", tmp.path().display())]);
    }
}
