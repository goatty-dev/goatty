//! Palette commands (`[[command]]`, #91): a plugin's entries in goatty's
//! command palette. A command asks for its `[[command.input]]` values one
//! after another (label, placeholder, type, checks), then types its
//! `run` line into the focused terminal, where the user sees it run.
//!
//! Like everything else in a manifest, a command is data: `run` is a list
//! of words whose first one is a program the plugin provides or requires,
//! and every value the user typed becomes (part of) one quoted word. A
//! value can never turn into shell syntax.
//!
//! ```toml
//! [[command]]
//! id = "exec"                                # palette id: docker.exec
//! title = "Open a shell in a container"      # shown as "docker: Open a shell…"
//! keywords = ["exec", "attach"]
//! run = ["docker", "exec", "-it", "{container}", "{shell}"]
//! # when = "some_setting"                    # optional condition on plugin settings
//!
//! [[command.input]]
//! id = "container"
//! label = "Container"
//! placeholder = "name or id"
//! pattern = "^[a-zA-Z0-9][a-zA-Z0-9_.-]*$"
//! pattern_help = "letters, digits, _ . and -"
//!
//! [[command.input]]
//! id = "shell"
//! label = "Shell"
//! type = "choice"
//! default = "sh"
//! choices = [{ value = "sh" }, { value = "bash" }]
//! ```
//!
//! An input can list what to pick from by running a program on the tab's
//! host, like a snippet input's source (`docs/snippets.md`): `run` (argv,
//! no shell; the first word a program the plugin provides or requires,
//! the others may use earlier inputs), how to read its output, a time
//! limit. With `strict = true` only its suggestions are answers, so the
//! input is a list to pick from:
//!
//! ```toml
//! [[command.input]]
//! id = "branch"
//! label = "Branch"
//! strict = true
//! [command.input.source]
//! run = ["git", "branch", "--format=%(refname:short)"]
//! ```
//!
//! `[[command]].when` gates the command on plugin settings (like
//! `[[alias]].when`), hiding it from the palette and right-click menu when
//! false. A command can also be in the terminal's right-click menu (#186),
//! under the plugin's name, with one of goatty's icons ([`MENU_ICONS`]).
//! There, `[command.menu].when` controls terminal context (`always`,
//! `selection` when text is selected, or `prompt`), and `fill` names the
//! input the selected text goes into (still asked, so the user sees and
//! can change it):
//!
//! ```toml
//! [command.menu]
//! icon = "search"
//! when = "selection"     # always (default) | selection | prompt
//! fill = "container"
//! ```
//!
//! Instead of `run`, a command can send keys into the focused pane (#240),
//! to the program running there: `keys` is text with vim-style key names
//! (`<Esc>`, `<Enter>`, `<C-c>`, `<lt>` for a literal `<`) and `{input}`
//! values, which are typed as text. `[command.pane]` says where a command
//! applies: while one of `programs` is in the pane's foreground, or `at` a
//! shell prompt or inside a program. Elsewhere it isn't offered, and a
//! shortcut bound to it goes to the terminal as a normal key:
//!
//! ```toml
//! [[command]]
//! id = "write-quit"
//! title = "Save and exit Vim"
//! keys = ["<Esc>", ":wq", "<Enter>"]
//! [command.pane]
//! programs = ["vim", "nvim"]
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, Problem};
use crate::template::{Part, parts};

/// A command in the palette.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteCommand {
    /// Lowercase letters, digits and `-`; the palette's id is `<plugin>.<id>`.
    pub id: String,
    /// What it does, as a short imperative ("Open a shell in a container").
    pub title: String,
    /// Other words people might search for.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// The command line as words. The first is one of the plugin's package
    /// executables or required commands (no template); the others may use
    /// `{input}` (the value, or nothing for an empty optional value as a
    /// whole word), inside a word too (`--name={name}`). Empty when the
    /// command sends `keys` instead.
    #[serde(default)]
    pub run: Vec<String>,
    /// Instead of `run`: keys sent to the program in the focused pane, as
    /// text with `<Key>` names and `{input}` values (see [`strokes`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Where the command applies (see [`PaneContext`]); elsewhere it's
    /// hidden and a shortcut bound to it passes its key to the terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneContext>,
    /// Asked in this order.
    #[serde(default, rename = "input")]
    pub inputs: Vec<CommandInput>,
    /// Condition on settings (see [`Condition`](crate::manifest::Condition)):
    /// when false, the command isn't offered in the palette or menu.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Where this command opens: typed into the current pane's prompt
    /// (default), or launched directly in a new split or tab (#241).
    /// Skipped on serialization so `Installed.manifest` never emits `open_in`
    /// to older clients whose `PaletteCommand` denies unknown fields; wire
    /// replies carry non-default placements on `Installed::command_open_in`.
    #[serde(default, skip_serializing)]
    pub open_in: OpenIn,
    /// Also in the terminal's right-click menu.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<CommandMenu>,
}

/// Where a palette command opens when run (#241).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenIn {
    /// Type the command line into the focused pane's idle shell prompt.
    #[default]
    Current,
    /// Launch the program directly in a new split pane to the right.
    SplitRight,
    /// Launch the program directly in a new split pane below.
    SplitDown,
    /// Launch the program directly in a new split pane to the left.
    SplitLeft,
    /// Launch the program directly in a new split pane above.
    SplitUp,
    /// Launch the program directly in a new tab on the same host.
    Tab,
}

impl OpenIn {
    pub const ALL: [Self; 6] = [Self::Current, Self::SplitRight, Self::SplitDown, Self::SplitLeft, Self::SplitUp, Self::Tab];

    pub fn is_default(&self) -> bool {
        *self == Self::Current
    }

    /// Whether this placement spawns the program directly in a new pane or tab
    /// instead of typing into the active pane's shell prompt.
    pub fn launches_direct(self) -> bool {
        self != Self::Current
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Current => "Current pane",
            Self::SplitRight => "Split right",
            Self::SplitDown => "Split down",
            Self::SplitLeft => "Split left",
            Self::SplitUp => "Split up",
            Self::Tab => "New tab",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Current => "current pane",
            Self::SplitRight => "split right",
            Self::SplitDown => "split down",
            Self::SplitLeft => "split left",
            Self::SplitUp => "split up",
            Self::Tab => "new tab",
        }
    }
}

/// Where in a pane a command applies, from what the focused pane runs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneContext {
    /// One of these programs is in the foreground (`vim`, `nvim`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub programs: Vec<String>,
    /// At a shell prompt, or inside a program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<PaneAt>,
}

/// A shell waits at its prompt, or another program runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneAt {
    Prompt,
    Program,
}

impl PaneContext {
    /// Whether a pane whose foreground program is `process` (when the host
    /// can tell), `at_prompt` or not, is one this command applies to.
    pub fn matches(&self, process: Option<&str>, at_prompt: bool) -> bool {
        let program = self.programs.is_empty() || process.is_some_and(|p| self.programs.iter().any(|q| q == p));
        let at = match self.at {
            None => true,
            Some(PaneAt::Prompt) => at_prompt,
            Some(PaneAt::Program) => !at_prompt,
        };
        program && at
    }

    /// "while vim or nvim runs", for the install plan and messages.
    pub fn describe(&self) -> String {
        match (self.programs.as_slice(), self.at) {
            ([], Some(PaneAt::Prompt)) => "at a shell prompt".into(),
            ([], _) => "while a program runs".into(),
            ([one], _) => format!("while {one} runs"),
            ([init @ .., last], _) => format!("while {} or {last} runs", init.join(", ")),
        }
    }
}

/// One step of a command's `keys`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stroke {
    /// Typed as text (the author's, or an input's value).
    Text(String),
    /// A key press (`<Esc>`, `<C-c>`).
    Press(KeyPress),
}

/// A key with modifiers, as named in `keys` (`<C-S-Tab>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPress {
    pub key: KeyName,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// The keys `keys` can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyName {
    /// A character key (`<C-c>`: `c`).
    Char(char),
    Enter,
    Escape,
    Tab,
    Backspace,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 to F12.
    F(u8),
}

/// The key names `keys` understands (case doesn't matter), for messages.
const KEY_NAMES: &str = "Esc, Enter, Tab, BS, Space, Up, Down, Left, Right, Home, End, PageUp, PageDown, Insert, Del, F1-F12, \
lt; modifiers C- (Ctrl), A- or M- (Alt), S- (Shift)";

fn key_name(name: &str) -> Option<KeyName> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return (!c.is_control()).then_some(KeyName::Char(c));
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "esc" | "escape" => KeyName::Escape,
        "enter" | "cr" | "return" => KeyName::Enter,
        "tab" => KeyName::Tab,
        "bs" | "backspace" => KeyName::Backspace,
        "space" => KeyName::Space,
        "up" => KeyName::Up,
        "down" => KeyName::Down,
        "left" => KeyName::Left,
        "right" => KeyName::Right,
        "home" => KeyName::Home,
        "end" => KeyName::End,
        "pageup" => KeyName::PageUp,
        "pagedown" => KeyName::PageDown,
        "insert" => KeyName::Insert,
        "del" | "delete" => KeyName::Delete,
        f => match f.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            Some(n @ 1..=12) => KeyName::F(n),
            _ => return None,
        },
    })
}

/// `<C-S-Tab>`'s inside (`C-S-Tab`) as a key press.
fn key_press(spec: &str) -> Option<KeyPress> {
    let mut p = KeyPress { key: KeyName::Escape, ctrl: false, alt: false, shift: false };
    let mut rest = spec;
    while rest.len() > 2 && rest.as_bytes()[1] == b'-' {
        match rest.as_bytes()[0].to_ascii_lowercase() {
            b'c' => p.ctrl = true,
            b'a' | b'm' => p.alt = true,
            b's' => p.shift = true,
            _ => break,
        }
        rest = &rest[2..];
    }
    p.key = key_name(rest)?;
    Some(p)
}

/// One `keys` entry split into text (still with `{input}`s) and key
/// presses. `<lt>` is a literal `<`.
fn lex_keys(entry: &str) -> Result<Vec<Result<String, KeyPress>>, String> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = entry;
    while let Some(i) = rest.find('<') {
        text.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let spec = tail.find('>').map(|end| &tail[..end]);
        match spec {
            Some(s) if s.eq_ignore_ascii_case("lt") => text.push('<'),
            Some(s) => match key_press(s) {
                Some(k) => {
                    if !text.is_empty() {
                        out.push(Ok(std::mem::take(&mut text)));
                    }
                    out.push(Err(k));
                }
                None => return Err(format!("<{s}>: not a key ({KEY_NAMES}; write a literal < as <lt>)")),
            },
            None => return Err(format!("{entry:?}: '<' without '>' (write a literal < as <lt>)")),
        }
        rest = &tail[spec.map_or(0, |s| s.len() + 1)..];
    }
    text.push_str(rest);
    if !text.is_empty() {
        out.push(Ok(text));
    }
    Ok(out)
}

/// What `c`'s `keys` send for `values` (by input id): the author's text
/// and each value as typed text, and the named keys as presses.
pub fn strokes(c: &PaletteCommand, values: &BTreeMap<String, String>) -> Vec<Stroke> {
    let mut out = Vec::new();
    for entry in &c.keys {
        // Checked at validation.
        let Ok(segments) = lex_keys(entry) else { continue };
        for seg in segments {
            match seg {
                Err(k) => out.push(Stroke::Press(k)),
                Ok(t) => {
                    let Ok(ps) = parts(&t) else { continue };
                    let text: String = ps
                        .iter()
                        .map(|p| match p {
                            Part::Text(s) => s.as_str(),
                            Part::Var(v) => values.get(*v).map(String::as_str).unwrap_or_default(),
                        })
                        .collect();
                    if text.is_empty() {
                        continue;
                    }
                    match out.last_mut() {
                        Some(Stroke::Text(prev)) => prev.push_str(&text),
                        _ => out.push(Stroke::Text(text)),
                    }
                }
            }
        }
    }
    out
}

/// A command's entry in the terminal's right-click menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CommandMenu {
    /// One of [`MENU_ICONS`]; none: the plugin icon.
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub when: MenuWhen,
    /// `when = "selection"`: the input the selected text fills in.
    #[serde(default)]
    pub fill: Option<String>,
}

/// When a command shows in the right-click menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MenuWhen {
    #[default]
    Always,
    /// While text is selected in the pane.
    Selection,
    /// While a shell waits at a prompt in the pane (nothing else runs there).
    Prompt,
}

/// The icons a command's menu entry may use (goatty's own, by name).
pub const MENU_ICONS: &[&str] = &[
    "activity",
    "book-open",
    "bug",
    "code",
    "copy",
    "download",
    "external-link",
    "file-text",
    "folder-open",
    "git-commit",
    "globe",
    "history",
    "link",
    "package",
    "puzzle",
    "refresh",
    "search",
    "server",
    "shield-check",
    "terminal",
    "trash",
    "upload",
    "zap",
];

/// One value a command asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandInput {
    /// Lowercase letters, digits and `_`; `{id}` in `run`.
    pub id: String,
    /// What's asked ("Container").
    pub label: String,
    /// Shown greyed in the empty field: an example ("name or id").
    #[serde(default)]
    pub placeholder: String,
    /// One more line under the field.
    #[serde(default)]
    pub help: String,
    #[serde(default, rename = "type")]
    pub kind: InputKind,
    /// Filled in to start with.
    #[serde(default)]
    pub default: Option<String>,
    /// May be left empty.
    #[serde(default)]
    pub optional: bool,
    /// `integer`/`number`: bounds (inclusive).
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// `text`: a regular expression the whole value must match.
    #[serde(default)]
    pub pattern: Option<String>,
    /// What `pattern` allows, in words ("letters and digits").
    #[serde(default)]
    pub pattern_help: Option<String>,
    /// `choice`: the values to pick from.
    #[serde(default)]
    pub choices: Vec<InputChoice>,
    /// `path`/`folder`: it must exist (checked when the host is this computer).
    #[serde(default)]
    pub must_exist: bool,
    /// `text`: suggestions from a program on the tab's host, run when the
    /// input is asked (`[command.input.source]`, the same format as a
    /// snippet input's source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::snippets::Source>,
    /// With a source: only its suggestions are answers (pick from a list).
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputKind {
    /// Any text (`pattern` restricts it).
    #[default]
    Text,
    /// A whole number.
    Integer,
    /// A number, decimals allowed.
    Number,
    /// A file or folder path; `~` is the home directory.
    Path,
    /// A folder path; suggestions list folders only.
    Folder,
    /// One of `choices`.
    Choice,
    /// Snippets only: a yes/no question whose "yes" inserts the input's
    /// `on` text ("Include subfolders?" → `-r`).
    Flag,
}

impl InputKind {
    pub fn label(self) -> &'static str {
        match self {
            InputKind::Text => "text",
            InputKind::Integer => "integer",
            InputKind::Number => "number",
            InputKind::Path => "path",
            InputKind::Folder => "folder",
            InputKind::Choice => "choice",
            InputKind::Flag => "flag",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputChoice {
    pub value: String,
    /// Shown instead of the value.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub help: String,
}

impl InputChoice {
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.value)
    }
}

pub(crate) fn is_command_id(s: &str) -> bool {
    (1..=40).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && s.as_bytes()[0].is_ascii_lowercase()
}

pub(crate) fn is_input_id(s: &str) -> bool {
    (1..=40).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && s.as_bytes()[0].is_ascii_lowercase()
}

/// Check `text` against `input`'s type and limits, without touching the
/// file system. Returns the value to use (trimmed numbers; text as typed).
pub fn check_value(input: &CommandInput, text: &str) -> Result<String, String> {
    if text.is_empty() {
        return if input.optional { Ok(String::new()) } else { Err("Required".into()) };
    }
    if text.contains(['\n', '\r']) {
        return Err("Must be a single line".into());
    }
    // Control characters would act as keys when the line is typed (Ctrl+U…).
    if text.chars().any(char::is_control) {
        return Err("Can't contain control characters".into());
    }
    let bounds = |n: f64| -> Result<(), String> {
        match (input.min, input.max) {
            (Some(lo), Some(hi)) if n < lo || n > hi => Err(format!("Must be between {} and {}", fmt_num(lo), fmt_num(hi))),
            (Some(lo), _) if n < lo => Err(format!("Must be at least {}", fmt_num(lo))),
            (_, Some(hi)) if n > hi => Err(format!("Must be at most {}", fmt_num(hi))),
            _ => Ok(()),
        }
    };
    match input.kind {
        InputKind::Integer => {
            let t = text.trim();
            let n: i64 = t.parse().map_err(|_| "Must be a whole number".to_string())?;
            bounds(n as f64)?;
            Ok(n.to_string())
        }
        InputKind::Number => {
            let t = text.trim();
            let n: f64 = t.parse().ok().filter(|n: &f64| n.is_finite()).ok_or_else(|| "Must be a number".to_string())?;
            bounds(n)?;
            Ok(t.to_string())
        }
        InputKind::Choice => {
            input.choices.iter().find(|c| c.value == text).map(|c| c.value.clone()).ok_or_else(|| "Pick one from the list".to_string())
        }
        InputKind::Flag => match text.trim() {
            "yes" | "on" | "true" => Ok("yes".into()),
            "no" | "off" | "false" => Ok("no".into()),
            _ => Err("Answer yes or no".into()),
        },
        InputKind::Path | InputKind::Folder => Ok(text.to_string()),
        InputKind::Text => {
            if let Some(p) = &input.pattern {
                // Checked at validation; a bad one here rejects nothing.
                if let Ok(re) = regex::Regex::new(&anchored(p))
                    && !re.is_match(text)
                {
                    return Err(match &input.pattern_help {
                        Some(h) => format!("Use {h}"),
                        None => format!("Must match {p}"),
                    });
                }
            }
            Ok(text.to_string())
        }
    }
}

/// `pattern` must match the whole value.
pub fn anchored(p: &str) -> String {
    format!("^(?:{p})$")
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { n.to_string() }
}

/// The line `c` types into the terminal for `values` (by input id), each
/// word quoted for a POSIX shell where needed.
pub fn command_line(c: &PaletteCommand, values: &BTreeMap<String, String>) -> String {
    let mut words = Vec::new();
    for (i, t) in c.run.iter().enumerate() {
        if i == 0 {
            words.push(crate::zsh::word(t));
            continue;
        }
        let Ok(ps) = parts(t) else { continue };
        if let [Part::Var(v)] = ps.as_slice() {
            let value = values.get(*v).map(String::as_str).unwrap_or_default();
            // A whole-word `{x}` with an empty value drops the word.
            if value.is_empty() {
                continue;
            }
            // A path's `~/` stays unquoted, so the shell expands it.
            let is_path = c.inputs.iter().any(|i| i.id == *v && matches!(i.kind, InputKind::Path | InputKind::Folder));
            if is_path && (value == "~" || value.starts_with("~/")) {
                let rest = &value[1..].trim_start_matches('/');
                words.push(if rest.is_empty() { "~".into() } else { format!("~/{}", crate::zsh::word(rest)) });
                continue;
            }
        }
        // Inside a word (`--out={dir}`), a path's `~` becomes "$HOME": the
        // shell doesn't expand a tilde after `=`. Each literal run is quoted.
        let mut runs: Vec<String> = Vec::new();
        let mut text = String::new();
        for p in &ps {
            match p {
                Part::Text(s) => text.push_str(s),
                Part::Var(v) => {
                    let value = values.get(*v).map(String::as_str).unwrap_or_default();
                    let is_path = c.inputs.iter().any(|i| i.id == *v && matches!(i.kind, InputKind::Path | InputKind::Folder));
                    match value.strip_prefix('~').filter(|rest| is_path && (rest.is_empty() || rest.starts_with('/'))) {
                        Some(rest) => {
                            if !text.is_empty() {
                                runs.push(crate::zsh::word(&std::mem::take(&mut text)));
                            }
                            runs.push("\"$HOME\"".into());
                            text.push_str(rest);
                        }
                        None => text.push_str(value),
                    }
                }
            }
        }
        if !text.is_empty() || runs.is_empty() {
            runs.push(crate::zsh::word(&text));
        }
        words.push(runs.concat());
    }
    words.join(" ")
}

/// The direct `argv` `c` spawns (without a shell) for `values` (by input id)
/// when opening in a split or new tab (#241). Whole-word `{input}` with an
/// empty optional value is omitted. Leading `~` / `~/` in `path` and `folder`
/// inputs is expanded against `home` when known.
pub fn command_argv(c: &PaletteCommand, values: &BTreeMap<String, String>, home: Option<&str>) -> Vec<String> {
    let mut argv = Vec::new();
    for (i, t) in c.run.iter().enumerate() {
        if i == 0 {
            argv.push(t.clone());
            continue;
        }
        let Ok(ps) = parts(t) else { continue };
        if let [Part::Var(v)] = ps.as_slice() {
            let value = values.get(*v).map(String::as_str).unwrap_or_default();
            if value.is_empty() {
                continue;
            }
            let is_path = c.inputs.iter().any(|i| i.id == *v && matches!(i.kind, InputKind::Path | InputKind::Folder));
            argv.push(if is_path { expand_tilde(value, home) } else { value.to_string() });
            continue;
        }
        let mut word = String::new();
        for p in &ps {
            match p {
                Part::Text(s) => word.push_str(s),
                Part::Var(v) => {
                    let value = values.get(*v).map(String::as_str).unwrap_or_default();
                    let is_path = c.inputs.iter().any(|i| i.id == *v && matches!(i.kind, InputKind::Path | InputKind::Folder));
                    if is_path {
                        word.push_str(&expand_tilde(value, home));
                    } else {
                        word.push_str(value);
                    }
                }
            }
        }
        argv.push(word);
    }
    argv
}

/// Whether any `path` or `folder` input value in `values` starts with `~` or
/// `~/` (so direct launch benefits from resolving the host's `$HOME`).
pub fn needs_home_expansion(c: &PaletteCommand, values: &BTreeMap<String, String>) -> bool {
    c.inputs
        .iter()
        .filter(|i| matches!(i.kind, InputKind::Path | InputKind::Folder))
        .any(|i| values.get(&i.id).is_some_and(|v| v == "~" || v.starts_with("~/")))
}

fn expand_tilde(value: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return value.to_string();
    };
    if value == "~" {
        home.to_string()
    } else if let Some(rest) = value.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else {
        value.to_string()
    }
}

/// What `input`'s source runs for the earlier `values` (argv, never a
/// shell); `Err` says what to fill in first.
pub fn source_argv(c: &PaletteCommand, input: &CommandInput, values: &BTreeMap<String, String>) -> Result<Vec<String>, String> {
    let src = input.source.as_ref().ok_or_else(|| format!("{} has no suggestions", input.label))?;
    if src.sh.is_some() {
        return Err("plugins can't run a shell".into());
    }
    // Unanswered, or empty without being optional (an optional one left
    // empty is simply left out of the arguments).
    let optional = |d: &str| c.inputs.iter().any(|i| i.id == d && i.optional);
    let missing =
        crate::snippets::source_depends(&src.run).into_iter().find(|d| values.get(d).is_none_or(|v| v.is_empty() && !optional(d)));
    if let Some(d) = missing {
        let label = c.inputs.iter().find(|i| i.id == d).map_or(d.clone(), |i| i.label.clone());
        return Err(format!("Fill in {label} first"));
    }
    crate::snippets::source_argv(&src.run, values)
}

/// The command line with `{input}` placeholders (or the keys it sends,
/// and where), for the install plan.
pub fn describe(c: &PaletteCommand) -> String {
    let what = if c.keys.is_empty() { c.run.join(" ") } else { format!("sends the keys {}", c.keys.concat()) };
    match &c.pane {
        Some(p) => format!("{what} ({})", p.describe()),
        None => what,
    }
}

/// A program name in `[command.pane].programs`.
fn is_program_name(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

fn check_keys(c: &PaletteCommand, inputs: &BTreeSet<&str>, at: &str) -> Vec<Problem> {
    let mut out = Vec::new();
    for (k, entry) in c.keys.iter().enumerate() {
        let at = format!("{at}.keys[{k}]");
        if entry.is_empty() {
            out.push(Problem { path: at.clone(), message: "empty".into() });
        }
        if entry.chars().any(char::is_control) {
            out.push(Problem { path: at.clone(), message: "control characters: name the key instead (<Esc>, <Enter>, <C-c>)".into() });
        }
        let segments = match lex_keys(entry) {
            Ok(s) => s,
            Err(e) => {
                out.push(Problem { path: at, message: e });
                continue;
            }
        };
        for t in segments.iter().filter_map(|s| s.as_ref().ok()) {
            match parts(t) {
                Err(e) => out.push(Problem { path: at.clone(), message: e }),
                Ok(ps) => {
                    for p in ps {
                        if let Part::Var(v) = p
                            && !inputs.contains(v)
                        {
                            out.push(Problem { path: at.clone(), message: format!("{{{v}}}: no input {v:?}") });
                        }
                    }
                }
            }
        }
    }
    out
}

fn check_pane(c: &PaletteCommand, at: &str) -> Vec<Problem> {
    let mut out = Vec::new();
    let Some(pane) = &c.pane else { return out };
    let mut err = |message: &str| out.push(Problem { path: format!("{at}.pane"), message: message.into() });
    if pane.programs.is_empty() && pane.at.is_none() {
        err("say which programs, or at = \"prompt\" or \"program\"");
    }
    if !pane.programs.is_empty() && pane.at == Some(PaneAt::Prompt) {
        err("programs run inside a program, not at a prompt");
    }
    if let Some(p) = pane.programs.iter().find(|p| !is_program_name(p)) {
        err(&format!("{p:?}: a program's name (letters, digits, . _ + -), not a path"));
    }
    if c.keys.is_empty() && !c.open_in.launches_direct() && (!pane.programs.is_empty() || pane.at == Some(PaneAt::Program)) {
        err("a run line is typed at a shell prompt; inside a program, send keys instead");
    }
    out
}

/// Problems with one input's definition (labels, limits, pattern,
/// choices, default), shared by `[[command]]` and `[[snippet]]` inputs.
pub(crate) fn check_input(input: &CommandInput, at: &str) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut err = |path: String, message: String| out.push(Problem { path, message });
    if input.label.trim().is_empty() {
        err(format!("{at}.label"), "empty".into());
    }
    let numeric = matches!(input.kind, InputKind::Integer | InputKind::Number);
    if !numeric && (input.min.is_some() || input.max.is_some()) {
        err(format!("{at}.min"), "min and max are for integer and number inputs".into());
    }
    if let (Some(lo), Some(hi)) = (input.min, input.max)
        && lo > hi
    {
        err(format!("{at}.max"), "smaller than min".into());
    }
    match (&input.pattern, input.kind) {
        (Some(_), k) if k != InputKind::Text => err(format!("{at}.pattern"), "pattern is for text inputs".into()),
        (Some(p), _) => {
            if let Err(e) = regex::Regex::new(&anchored(p)) {
                err(format!("{at}.pattern"), format!("not a regular expression: {e}"));
            }
        }
        (None, _) if input.pattern_help.is_some() => err(format!("{at}.pattern_help"), "without a pattern".into()),
        _ => {}
    }
    if (input.kind == InputKind::Choice) == input.choices.is_empty() {
        err(
            format!("{at}.choices"),
            if input.choices.is_empty() { "a choice input needs choices" } else { "only choice inputs have choices" }.into(),
        );
    }
    let mut values = BTreeSet::new();
    for ch in &input.choices {
        if ch.value.is_empty() || !values.insert(ch.value.as_str()) {
            err(format!("{at}.choices"), format!("{:?}: empty or listed twice", ch.value));
        }
    }
    if input.must_exist && !matches!(input.kind, InputKind::Path | InputKind::Folder) {
        err(format!("{at}.must_exist"), "only for path and folder inputs".into());
    }
    if let Some(d) = &input.default
        && let Err(e) = check_value(input, d)
    {
        err(format!("{at}.default"), format!("{d:?}: {e}"));
    }
    out
}

pub(crate) fn check(m: &Manifest, programs: &BTreeSet<&str>) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut err = |path: String, message: String| out.push(Problem { path, message });
    let mut ids = BTreeSet::new();
    for (i, c) in m.commands.iter().enumerate() {
        let at = format!("command[{i}]");
        if !is_command_id(&c.id) {
            err(format!("{at}.id"), format!("{:?}: lowercase letters, digits and '-'", c.id));
        } else if !ids.insert(c.id.as_str()) {
            err(format!("{at}.id"), format!("{:?} is used twice", c.id));
        }
        if c.title.trim().is_empty() {
            err(format!("{at}.title"), "empty".into());
        }
        let mut inputs = BTreeSet::new();
        for (j, input) in c.inputs.iter().enumerate() {
            let at = format!("{at}.input[{j}]");
            if !is_input_id(&input.id) {
                err(format!("{at}.id"), format!("{:?}: lowercase letters, digits and '_'", input.id));
            } else if !inputs.insert(input.id.as_str()) {
                err(format!("{at}.id"), format!("{:?} is used twice", input.id));
            }
            if input.kind == InputKind::Flag {
                err(format!("{at}.type"), "flag inputs are for snippets".into());
            }
            for p in check_input(input, &at) {
                err(p.path, p.message);
            }
            if input.strict && input.source.is_none() {
                err(format!("{at}.strict"), "only for inputs with a source".into());
            }
            if let Some(src) = &input.source {
                if input.kind != InputKind::Text {
                    err(
                        format!("{at}.source"),
                        format!("{} inputs list their own values; a source is for text inputs", input.kind.label()),
                    );
                }
                let earlier: Vec<&str> = c.inputs[..j].iter().map(|i| i.id.as_str()).collect();
                let all: Vec<&str> = c.inputs.iter().map(|i| i.id.as_str()).collect();
                for p in crate::snippets::check_plugin_source(src, &earlier, &all, programs, &format!("{at}.source")) {
                    err(p.path, p.message);
                }
            }
        }
        if let Some(w) = &c.when
            && let Err(e) = crate::manifest::Condition::parse(w).and_then(|cond| cond.check(m))
        {
            err(format!("{at}.when"), e);
        }
        if let Some(menu) = &c.menu {
            if let Some(icon) = &menu.icon
                && !MENU_ICONS.contains(&icon.as_str())
            {
                err(format!("{at}.menu.icon"), format!("{icon:?} isn't one of goatty's icons ({})", MENU_ICONS.join(", ")));
            }
            match (&menu.fill, menu.when) {
                (Some(_), w) if w != MenuWhen::Selection => err(format!("{at}.menu.fill"), "needs when = \"selection\"".into()),
                (Some(f), _) => match c.inputs.iter().find(|i| &i.id == f) {
                    None => err(format!("{at}.menu.fill"), format!("no input {f:?}")),
                    Some(i) if matches!(i.kind, InputKind::Choice | InputKind::Integer | InputKind::Number) => {
                        err(format!("{at}.menu.fill"), format!("{f:?}: selected text can only fill text, path and folder inputs"))
                    }
                    Some(_) => {}
                },
                _ => {}
            }
        }
        for p in check_pane(c, &at).into_iter().chain(check_keys(c, &inputs, &at)) {
            err(p.path, p.message);
        }
        if !c.keys.is_empty() && c.open_in.launches_direct() {
            err(format!("{at}.open_in"), "keys are sent to the current pane; remove open_in".into());
        }
        match c.run.first() {
            None if !c.keys.is_empty() => {}
            Some(_) if !c.keys.is_empty() => err(format!("{at}.run"), "run and keys: a command does one of them".into()),
            None => err(format!("{at}.run"), "empty (or send keys instead)".into()),
            Some(p) if !programs.contains(p.as_str()) => {
                err(format!("{at}.run"), format!("{p:?} is neither a package executable nor in requires.commands"))
            }
            Some(_) => {}
        }
        for t in c.run.iter().skip(1) {
            match parts(t) {
                Err(e) => err(format!("{at}.run"), e),
                Ok(ps) => {
                    for p in ps {
                        if let Part::Var(v) = p
                            && !inputs.contains(v)
                        {
                            err(format!("{at}.run"), format!("{{{v}}}: no input {v:?}"));
                        }
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{parse, tests::EXAMPLE};

    const COMMANDS: &str = r#"
[[command]]
id = "tree"
title = "Show a folder as a tree"
keywords = ["list"]
run = ["eza", "--tree", "--level={depth}", "{dir}", "{filter}"]

[[command.input]]
id = "dir"
label = "Folder"
type = "folder"
default = "."

[[command.input]]
id = "depth"
label = "Depth"
type = "integer"
min = 1
max = 9
default = "2"

[[command.input]]
id = "filter"
label = "Only names matching"
placeholder = "*.rs"
optional = true
pattern = "[^/]*"
pattern_help = "a name pattern without /"
"#;

    fn with_commands(extra: &str) -> Result<Manifest, crate::manifest::Invalid> {
        // [permissions] must stay last in EXAMPLE: put the commands before it.
        let (head, perms) = EXAMPLE.split_once("[permissions]").unwrap();
        parse(&format!("{head}{extra}\n[permissions]{perms}"), None)
    }

    #[test]
    fn parses_and_renders() {
        let m = with_commands(COMMANDS).unwrap();
        let c = &m.commands[0];
        assert_eq!(c.inputs.len(), 3);
        let mut v = BTreeMap::new();
        v.insert("dir".to_string(), "my dir/it's".to_string());
        v.insert("depth".to_string(), "3".to_string());
        v.insert("filter".to_string(), String::new());
        assert_eq!(command_line(c, &v), r"eza --tree --level=3 'my dir/it'\''s'");
        v.insert("dir".to_string(), "~/my src".to_string());
        assert_eq!(command_line(c, &v), r"eza --tree --level=3 ~/'my src'", "~/ expands in the shell");
        v.insert("dir".to_string(), "~".to_string());
        assert_eq!(command_line(c, &v), r"eza --tree --level=3 ~");
        v.insert("dir".to_string(), "my dir/it's".to_string());
        v.insert("filter".to_string(), "~/x".to_string());
        assert_eq!(command_line(c, &v), r"eza --tree --level=3 'my dir/it'\''s' '~/x'", "only paths expand ~");
        v.insert("filter".to_string(), "$(rm -rf ~)".to_string());
        let inline = PaletteCommand { run: vec!["eza".into(), "--out={dir}".into(), "x{depth}".into()], ..c.clone() };
        let mut w = v.clone();
        w.insert("dir".to_string(), "~/my src".to_string());
        assert_eq!(command_line(&inline, &w), r#"eza --out="$HOME"'/my src' x3"#, "~ inside a word");
        w.insert("dir".to_string(), "~".to_string());
        assert_eq!(command_line(&inline, &w), r#"eza --out="$HOME" x3"#);
        assert_eq!(command_line(c, &v), r"eza --tree --level=3 'my dir/it'\''s' '$(rm -rf ~)'");
        assert_eq!(describe(c), "eza --tree --level={depth} {dir} {filter}");
    }

    #[test]
    fn checks_values() {
        let m = with_commands(COMMANDS).unwrap();
        let [dir, depth, filter] = &m.commands[0].inputs[..] else { panic!() };
        assert_eq!(check_value(depth, " 4 "), Ok("4".into()));
        assert_eq!(check_value(depth, "0"), Err("Must be between 1 and 9".into()));
        assert_eq!(check_value(depth, "two"), Err("Must be a whole number".into()));
        assert_eq!(check_value(dir, ""), Err("Required".into()));
        assert_eq!(check_value(filter, ""), Ok(String::new()));
        assert_eq!(check_value(filter, "a/b"), Err("Use a name pattern without /".into()));
        assert_eq!(check_value(filter, "*.rs"), Ok("*.rs".into()));
        assert_eq!(check_value(filter, "a\nb"), Err("Must be a single line".into()));
        assert_eq!(check_value(filter, "a\u{15}b"), Err("Can't contain control characters".into()), "Ctrl+U");
        assert_eq!(check_value(filter, "a\u{1b}[A"), Err("Can't contain control characters".into()));
        let choice = CommandInput {
            kind: InputKind::Choice,
            choices: vec![InputChoice { value: "sh".into(), label: None, help: String::new() }],
            ..depth.clone()
        };
        let choice = CommandInput { min: None, max: None, ..choice };
        assert_eq!(check_value(&choice, "sh"), Ok("sh".into()));
        assert!(check_value(&choice, "zsh").is_err());
        let number = CommandInput { kind: InputKind::Number, min: Some(0.5), max: None, ..depth.clone() };
        assert_eq!(check_value(&number, "0.25"), Err("Must be at least 0.5".into()));
        assert_eq!(check_value(&number, "1.5"), Ok("1.5".into()));
        assert_eq!(check_value(&number, "NaN"), Err("Must be a number".into()));
    }

    #[test]
    fn validation() {
        let bad = r#"
[[command]]
id = "Bad"
title = ""
run = ["rm", "{nope}", "{"]

[[command.input]]
id = "n"
label = "N"
type = "integer"
min = 5
max = 1
default = "x"
pattern = "a"

[[command.input]]
id = "n"
label = "Pick"
type = "choice"
"#;
        let p: Vec<String> = with_commands(bad).unwrap_err().0.iter().map(|p| format!("{}: {}", p.path, p.message)).collect();
        let has = |s: &str| p.iter().any(|x| x.contains(s));
        assert!(has("command[0].id: \"Bad\""), "{p:?}");
        assert!(has("command[0].title: empty"), "{p:?}");
        assert!(has("command[0].run: \"rm\" is neither"), "{p:?}");
        assert!(has("{nope}: no input"), "{p:?}");
        assert!(has("unclosed"), "{p:?}");
        assert!(has("input[0].max: smaller than min"), "{p:?}");
        assert!(has("input[0].default"), "{p:?}");
        assert!(has("input[0].pattern: pattern is for text inputs"), "{p:?}");
        assert!(has("input[1].id: \"n\" is used twice"), "{p:?}");
        assert!(has("input[1].choices: a choice input needs choices"), "{p:?}");
    }

    #[test]
    fn sources_list_what_to_pick() {
        let src = r#"
[[command]]
id = "pick"
title = "Show a file in a folder"
run = ["eza", "{dir}", "{file}"]

[[command.input]]
id = "dir"
label = "Folder"

[[command.input]]
id = "file"
label = "File"
strict = true
[command.input.source]
run = ["eza", "-1", "--only-files", "{dir}"]
"#;
        let perms = "\n[permissions]\nsnippet-sources = \"eza lists the folder's files to pick from.\"\n";
        let m = with_commands(src).map(|_| ()).unwrap_err();
        assert!(m.0.iter().any(|p| p.path == "permissions.snippet-sources"), "the source needs a reason: {m:?}");
        let (head, rest) = EXAMPLE.split_once("[permissions]").unwrap();
        let m = parse(&format!("{head}{src}{perms}{}", rest.trim_start_matches('\n')), None).unwrap();
        let c = &m.commands[0];
        let file = &c.inputs[1];
        assert!(file.strict);
        let mut v = BTreeMap::new();
        assert_eq!(source_argv(c, file, &v), Err("Fill in Folder first".into()));
        v.insert("dir".to_string(), "my dir".to_string());
        assert_eq!(source_argv(c, file, &v), Ok(vec!["eza".into(), "-1".into(), "--only-files".into(), "my dir".into()]));
        // An optional earlier input left empty is left out, not missing.
        let mut opt = c.clone();
        opt.inputs[0].optional = true;
        v.insert("dir".to_string(), String::new());
        assert_eq!(source_argv(&opt, &opt.inputs[1], &v), Ok(vec!["eza".into(), "-1".into(), "--only-files".into()]));
        assert_eq!(source_argv(c, file, &v), Err("Fill in Folder first".into()), "a required one is still missing");
        use crate::permissions::{Permission, derived, describe};
        assert!(derived(&m).contains(&Permission::SnippetSources));
        let g = describe(&m).into_iter().find(|g| g.permission == Permission::SnippetSources).unwrap();
        assert_eq!(g.details, ["Show a file in a folder → File: eza -1 --only-files {dir}"]);
        // An older goatty refuses unknown fields: nothing new without a source.
        let plain = with_commands(COMMANDS).unwrap();
        let json = serde_json::to_string(&plain.commands[0]).unwrap();
        assert!(!json.contains("source") && !json.contains("strict"), "{json}");

        let bad = |input: &str| -> Vec<String> {
            let toml = format!(
                "[[command]]\nid = \"x\"\ntitle = \"X\"\nrun = [\"eza\"]\n[[command.input]]\nid = \"a\"\nlabel = \"A\"\n[[command.input]]\nid = \"b\"\nlabel = \"B\"\n{input}"
            );
            with_commands(&toml).unwrap_err().0.iter().map(|p| format!("{}: {}", p.path, p.message)).collect()
        };
        let has = |p: &[String], s: &str| p.iter().any(|x| x.contains(s));
        let p = bad("strict = true");
        assert!(has(&p, "input[1].strict: only for inputs with a source"), "{p:?}");
        let p = bad("[command.input.source]\nrun = [\"rm\", \"{c}\"]");
        assert!(has(&p, "input[1].source.run: \"rm\" is neither"), "{p:?}");
        assert!(has(&p, "{c}: no input"), "{p:?}");
        let p = bad("[command.input.source]\nsh = \"ls | head\"");
        assert!(has(&p, "input[1].source.sh: plugins list a program"), "{p:?}");
        let p = bad("type = \"integer\"\n[command.input.source]\nrun = [\"eza\"]");
        assert!(has(&p, "input[1].source: integer inputs list their own values"), "{p:?}");
        // Only earlier inputs: `a` may use nothing asked after it.
        let toml = "[[command]]\nid = \"x\"\ntitle = \"X\"\nrun = [\"eza\"]\n[[command.input]]\nid = \"a\"\nlabel = \"A\"\n[command.input.source]\nrun = [\"eza\", \"{b}\"]\n[[command.input]]\nid = \"b\"\nlabel = \"B\"\n";
        let p: Vec<String> = with_commands(toml).unwrap_err().0.iter().map(|p| format!("{}: {}", p.path, p.message)).collect();
        assert!(has(&p, "{b} is asked later"), "{p:?}");
    }

    #[test]
    fn menu_entries() {
        let menu = r#"
[command.menu]
icon = "search"
when = "selection"
fill = "filter"
"#;
        let m = with_commands(&format!("{COMMANDS}{menu}")).unwrap();
        let got = m.commands[0].menu.clone().unwrap();
        assert_eq!(got, CommandMenu { icon: Some("search".into()), when: MenuWhen::Selection, fill: Some("filter".into()) });
        // Only serialized when there is one (older goatty refuses unknown fields).
        let plain = with_commands(COMMANDS).unwrap();
        assert!(!serde_json::to_string(&plain.commands[0]).unwrap().contains("menu"));

        let bad = |menu: &str| -> Vec<String> {
            with_commands(&format!("{COMMANDS}\n[command.menu]\n{menu}\n"))
                .unwrap_err()
                .0
                .iter()
                .map(|p| format!("{}: {}", p.path, p.message))
                .collect()
        };
        assert!(bad("icon = \"rocket\"")[0].starts_with("command[0].menu.icon: \"rocket\" isn't one of"));
        assert_eq!(bad("fill = \"dir\""), ["command[0].menu.fill: needs when = \"selection\""]);
        assert_eq!(bad("when = \"selection\"\nfill = \"nope\""), ["command[0].menu.fill: no input \"nope\""]);
        assert!(bad("when = \"selection\"\nfill = \"depth\"")[0].contains("can only fill text"));
    }

    #[test]
    fn commands_are_a_permission() {
        use crate::permissions::{Permission, derived, describe};
        let m = with_commands(COMMANDS).unwrap();
        assert!(derived(&m).contains(&Permission::Commands));
        let g = describe(&m).into_iter().find(|g| g.permission == Permission::Commands).unwrap();
        assert_eq!(g.details, ["Show a folder as a tree: eza --tree --level={depth} {dir} {filter}"]);
        assert!(!derived(&parse(EXAMPLE, None).unwrap()).contains(&Permission::Commands));
    }

    const VIM: &str = r#"
[[command]]
id = "quit"
title = "Exit Vim"
keys = ["<Esc>", ":{how}", "<Enter>"]
[command.pane]
programs = ["vim", "nvim"]

[[command.input]]
id = "how"
label = "How"
type = "choice"
choices = [{ value = "q" }, { value = "wq" }, { value = "q!" }]
"#;

    #[test]
    fn keys_go_to_the_program_in_the_pane() {
        let m = with_commands(VIM).unwrap();
        let c = &m.commands[0];
        let v = BTreeMap::from([("how".to_string(), "wq".to_string())]);
        let esc = KeyPress { key: KeyName::Escape, ctrl: false, alt: false, shift: false };
        let enter = KeyPress { key: KeyName::Enter, ..esc };
        assert_eq!(strokes(c, &v), [Stroke::Press(esc), Stroke::Text(":wq".into()), Stroke::Press(enter)]);

        // Key names, modifiers, a literal `<`, values typed as they are.
        let k = PaletteCommand { keys: vec!["a<lt>b <C-c><m-S-tab><F5><C-->{how}".into()], ..c.clone() };
        let v = BTreeMap::from([("how".to_string(), "<Esc>".to_string())]);
        let ctrl = |key| Stroke::Press(KeyPress { key, ctrl: true, alt: false, shift: false });
        assert_eq!(
            strokes(&k, &v),
            [
                Stroke::Text("a<b ".into()),
                ctrl(KeyName::Char('c')),
                Stroke::Press(KeyPress { key: KeyName::Tab, ctrl: false, alt: true, shift: true }),
                Stroke::Press(KeyPress { key: KeyName::F(5), ..esc }),
                ctrl(KeyName::Char('-')),
                Stroke::Text("<Esc>".into()),
            ],
            "a value is text, never a key"
        );
        let only_value = PaletteCommand { keys: vec!["{how}".into()], ..c.clone() };
        assert!(strokes(&only_value, &BTreeMap::from([("how".to_string(), String::new())])).is_empty(), "the app says so instead");

        // Where it applies.
        let pane = c.pane.as_ref().unwrap();
        assert!(pane.matches(Some("nvim"), false));
        assert!(!pane.matches(Some("zsh"), true) && !pane.matches(None, false));
        let prompt = PaneContext { programs: vec![], at: Some(PaneAt::Prompt) };
        assert!(prompt.matches(Some("zsh"), true) && !prompt.matches(Some("htop"), false));
        let program = PaneContext { programs: vec![], at: Some(PaneAt::Program) };
        assert!(program.matches(Some("htop"), false) && !program.matches(None, true));

        // The install plan says what's sent and where.
        use crate::permissions::{Permission, describe};
        let g = describe(&m).into_iter().find(|g| g.permission == Permission::Commands).unwrap();
        assert_eq!(g.details, ["Exit Vim: sends the keys <Esc>:{how}<Enter> (while vim or nvim runs)"]);

        // Round-trips (the catalog goes to clients as JSON); `run` commands keep their shape.
        let json = serde_json::to_string(c).unwrap();
        assert_eq!(&serde_json::from_str::<PaletteCommand>(&json).unwrap(), c);
        let plain = serde_json::to_value(&with_commands(COMMANDS).unwrap().commands[0]).unwrap();
        assert!(plain.get("keys").is_none() && plain.get("pane").is_none());
    }

    #[test]
    fn keys_validation() {
        let bad = |extra: &str| -> Vec<String> {
            let toml = format!("[[command]]\nid = \"x\"\ntitle = \"X\"\n{extra}\n");
            with_commands(&toml).unwrap_err().0.iter().map(|p| format!("{}: {}", p.path, p.message)).collect()
        };
        let has = |p: &[String], s: &str| p.iter().any(|x| x.contains(s));
        let p = bad("keys = [\"<Esc>\", \"<Bogus>\", \"a < b\", \"{nope}\", \"\\u001b\"]");
        assert!(has(&p, "keys[1]: <Bogus>: not a key"), "{p:?}");
        assert!(has(&p, "keys[2]: \"a < b\": '<' without '>'"), "{p:?}");
        assert!(has(&p, "keys[3]: {nope}: no input"), "{p:?}");
        assert!(has(&p, "keys[4]: control characters"), "{p:?}");
        assert!(has(&bad("run = [\"eza\"]\nkeys = [\"q\"]"), "run and keys"));
        assert!(has(&bad(""), "command[0].run: empty"));
        let p = bad("run = [\"eza\"]\n[command.pane]\nprograms = [\"vim\"]");
        assert!(has(&p, "pane: a run line is typed at a shell prompt"), "{p:?}");
        let p = bad("keys = [\"q\"]\n[command.pane]\nprograms = [\"/usr/bin/vim\"]\nat = \"prompt\"");
        assert!(has(&p, "not a path") && has(&p, "not at a prompt"), "{p:?}");
        assert!(has(&bad("keys = [\"q\"]\n[command.pane]"), "say which programs"));
        assert!(has(&bad("keys = [\"\"]"), "keys[0]: empty"));
        assert!(has(&bad("keys = [\"q\"]\nopen_in = \"split-right\""), "open_in: keys are sent to the current pane"));
        // A run command in a new split/tab may still target a foreground program.
        with_commands(
            "[[command]]\nid = \"x\"\ntitle = \"X\"\nrun = [\"eza\"]\nopen_in = \"split-right\"\n[command.pane]\nprograms = [\"vim\"]\n",
        )
        .unwrap();
        // A run command may be for prompts only; keys need no program on the host.
        with_commands("[[command]]\nid = \"x\"\ntitle = \"X\"\nrun = [\"eza\"]\n[command.pane]\nat = \"prompt\"\n").unwrap();
        with_commands("[[command]]\nid = \"x\"\ntitle = \"X\"\nkeys = [\"q\"]\n").unwrap();
    }

    #[test]
    fn when_conditions() {
        let m = with_commands("[[command]]\nid = \"t\"\ntitle = \"T\"\nrun = [\"eza\"]\nwhen = \"replace_ls\"\n").unwrap();
        assert_eq!(m.commands[0].when.as_deref(), Some("replace_ls"));
        let plain = with_commands(COMMANDS).unwrap();
        assert!(!serde_json::to_string(&plain.commands[0]).unwrap().contains("when"));

        let bad = |w: &str| -> Vec<String> {
            with_commands(&format!("[[command]]\nid = \"t\"\ntitle = \"T\"\nrun = [\"eza\"]\nwhen = {w:?}\n"))
                .unwrap_err()
                .0
                .iter()
                .map(|p| format!("{}: {}", p.path, p.message))
                .collect()
        };
        assert_eq!(bad("nope"), ["command[0].when: no setting \"nope\""]);
        assert!(bad("icons")[0].starts_with("command[0].when: icons is not a bool"));
        assert!(bad("icons=nope")[0].contains("not one of icons's choices"));
        assert_eq!(bad(""), ["command[0].when: empty condition"]);
    }

    #[test]
    fn quoted_lines_survive_a_real_shell() {
        // The rendered line, run by sh with `printf '%s\n'` in place of
        // the program, prints each value back unchanged.
        let Ok(out) = std::process::Command::new("sh").arg("-c").arg("true").output() else { return };
        assert!(out.status.success());
        let c = PaletteCommand {
            id: "x".into(),
            title: "x".into(),
            keywords: Vec::new(),
            run: vec!["printf".into(), "%s\\n".into(), "{a}".into(), "pre-{b}".into()],
            keys: Vec::new(),
            pane: None,
            inputs: Vec::new(),
            when: None,
            open_in: OpenIn::Current,
            menu: None,
        };
        let nasty = ["it's \"$HOME\" `id` $(echo hi); ls *", "\\ ' \" ! ~ # &"];
        let mut v = BTreeMap::new();
        v.insert("a".to_string(), nasty[0].to_string());
        v.insert("b".to_string(), nasty[1].to_string());
        let line = command_line(&c, &v);
        let out = std::process::Command::new("sh").arg("-c").arg(&line).output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("{}\npre-{}\n", nasty[0], nasty[1]));
    }

    #[test]
    fn open_in_and_direct_argv() {
        let toml = r#"
[[command]]
id = "lazygit"
title = "Open Lazygit"
run = ["eza", "--level={depth}", "{dir}", "{filter}"]
open_in = "split-right"

[[command.input]]
id = "dir"
label = "Folder"
type = "folder"
default = "."

[[command.input]]
id = "depth"
label = "Depth"
type = "integer"
default = "2"

[[command.input]]
id = "filter"
label = "Filter"
optional = true
"#;
        let m = with_commands(toml).unwrap();
        let c = &m.commands[0];
        assert_eq!(c.open_in, OpenIn::SplitRight);
        assert!(c.open_in.launches_direct());
        // `open_in` is never serialized inside `PaletteCommand` (wire replies
        // carry it on `Installed::command_open_in`) so older clients whose
        // `PaletteCommand` denies unknown fields still decode the catalog.
        assert!(!serde_json::to_string(c).unwrap().contains("open_in"));

        let plain = with_commands(COMMANDS).unwrap();
        assert_eq!(plain.commands[0].open_in, OpenIn::Current);
        assert!(!serde_json::to_string(&plain.commands[0]).unwrap().contains("open_in"));

        // Permissions description notes the split/tab placement.
        let perms = crate::permissions::describe(&m);
        let g = perms.into_iter().find(|g| g.permission == crate::permissions::Permission::Commands).unwrap();
        assert_eq!(g.details, ["Open Lazygit: eza --level={depth} {dir} {filter} (in split right)"]);

        // Direct argv: no shell quoting, optional empty omitted, path ~ expanded.
        let mut v = BTreeMap::new();
        v.insert("dir".into(), "~/my repo".into());
        v.insert("depth".into(), "3".into());
        v.insert("filter".into(), String::new());
        assert!(needs_home_expansion(c, &v));
        assert_eq!(command_argv(c, &v, Some("/home/u")), ["eza", "--level=3", "/home/u/my repo"]);
        // Non-path input keeps literal ~ and shell metacharacters intact.
        v.insert("dir".into(), "~".into());
        v.insert("filter".into(), "~/$(id)".into());
        assert_eq!(command_argv(c, &v, Some("/home/u")), ["eza", "--level=3", "/home/u", "~/$(id)"]);

        // Invalid open_in value is rejected.
        let bad = with_commands("[[command]]\nid = \"x\"\ntitle = \"X\"\nrun = [\"eza\"]\nopen_in = \"floating\"\n");
        assert!(bad.is_err());
    }
}
