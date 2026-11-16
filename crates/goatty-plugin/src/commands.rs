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
//! A command can also be in the terminal's right-click menu (#186), under
//! the plugin's name, with one of goatty's icons ([`MENU_ICONS`]). With
//! `when = "selection"` it shows only while text is selected, and `fill`
//! names the input the selected text goes into (still asked, so the user
//! sees and can change it):
//!
//! ```toml
//! [command.menu]
//! icon = "search"
//! when = "selection"     # always (default) | selection | prompt
//! fill = "container"
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
    /// whole word), inside a word too (`--name={name}`).
    pub run: Vec<String>,
    /// Asked in this order.
    #[serde(default, rename = "input")]
    pub inputs: Vec<CommandInput>,
    /// Also in the terminal's right-click menu.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<CommandMenu>,
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

/// The command line with `{input}` placeholders, for the install plan.
pub fn describe(c: &PaletteCommand) -> String {
    c.run.join(" ")
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
        match c.run.first() {
            None => err(format!("{at}.run"), "empty".into()),
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
            inputs: Vec::new(),
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
}
