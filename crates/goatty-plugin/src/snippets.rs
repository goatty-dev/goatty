//! Snippets and expansions (#89): a guided command helper for people who
//! don't know a command by heart yet.
//!
//! A **snippet** is a command line with blanks: you search for what you
//! want to do ("Switch to a branch"), goatty asks for each blank in turn
//! (with suggestions, like the repository's branches), shows the finished
//! line and puts it on your prompt for you to read and run. Plugins ship
//! them (`[[snippet]]`), and the user keeps their own in `host.json`
//! (`snippets.<id>`, same format without the `id`).
//!
//! ```toml
//! [[snippet]]
//! id          = "switch-branch"
//! title       = "Switch to a branch"
//! description = "Moves your working copy to another branch."
//! tags        = ["git", "branch"]
//! safety      = "changes"              # safe | changes | destructive
//! line        = "git switch {branch}"  # {input} placeholders are the only variable parts
//!
//! [[snippet.input]]
//! id    = "branch"
//! label = "Branch"
//! [snippet.input.source]               # suggestions, run on the tab's host
//! run   = ["git", "for-each-ref", "--format=%(refname:short)", "refs/heads"]
//! ```
//!
//! The line is the author's shell text and stays as it is; every `{input}`
//! becomes **one quoted shell word** ([`Snippet::render`]). A small shell
//! lexer ([`parse_line`]) makes sure a placeholder never sits inside
//! quotes, so a value can never turn into shell syntax. Only `choice` and
//! `flag` values, which come from the author's own list, go in as written.
//!
//! An **expansion** rewrites plain words while you type: `| first 200
//! lines` + Space becomes `| head -n 200`. Expansions compile to a data
//! table of regular expressions ([`compile`], [`render_table`]) that a
//! static zsh widget reads; no author-written regex or code reaches zsh.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::commands::{CommandInput, InputChoice, InputKind, check_input, check_value, is_command_id, is_input_id};
use crate::manifest::Problem;
use crate::template::{Part, parts};
use crate::zsh::word;

/// Values by input id. A `multiple` input's values are separated by `\n`;
/// a flag's value is `yes` or `no`; an optional input left empty is `""`.
pub type Answers = BTreeMap<String, String>;

/// Most suggestions a source returns.
pub const MAX_ITEMS: usize = 1000;
/// Most bytes of a source's output that are read.
pub const MAX_OUTPUT: usize = 64 * 1024;
/// A source's time limit when it doesn't set one, and the most it may set (seconds).
pub const DEFAULT_TIMEOUT: f64 = 2.0;
pub const MAX_TIMEOUT: f64 = 10.0;

/// A command line with blanks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snippet {
    /// Lowercase letters, digits and `-` (the key in `host.json` for the user's own).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// What it does, as a short imperative ("Find big files").
    pub title: String,
    /// One or two plain sentences: what happens when it runs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Topics to filter by ("git", "files").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Other words people might search for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// What running it does to your files. Without one, goatty guesses
    /// from the line ([`Snippet::safety`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety: Option<Safety>,
    /// The command line, one line of shell text with `{input}` placeholders.
    /// `{{` and `}}` are literal braces; braces that don't hold an input id
    /// (`{}`, `{a,b}`, `${HOME}`, `awk '{print $1}'`) stay as they are.
    pub line: String,
    /// Asked in this order.
    #[serde(default, rename = "input", skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<SnippetInput>,
}

/// What running a snippet does, shown as a colored badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Safety {
    /// Only looks at things.
    Safe,
    /// Changes files, settings or running programs.
    Changes,
    /// Deletes or overwrites something; there's no undo.
    Destructive,
}

impl Safety {
    pub fn key(self) -> &'static str {
        match self {
            Safety::Safe => "safe",
            Safety::Changes => "changes",
            Safety::Destructive => "destructive",
        }
    }

    /// The badge's text.
    pub fn label(self) -> &'static str {
        match self {
            Safety::Safe => "Only reads",
            Safety::Changes => "Makes changes",
            Safety::Destructive => "Deletes data",
        }
    }

    /// One sentence for a tooltip or the details.
    pub fn meaning(self) -> &'static str {
        match self {
            Safety::Safe => "Only looks at things: nothing on your computer changes.",
            Safety::Changes => "Changes files, settings or running programs.",
            Safety::Destructive => "Deletes or overwrites something, and there's no undo. Read the line before you press Enter.",
        }
    }
}

/// One blank of a snippet: a [`CommandInput`] (type, label, checks) plus
/// suggestions from a program, several values, and yes/no flags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnippetInput {
    /// Lowercase letters, digits and `_`; `{id}` in `line`.
    pub id: String,
    /// What's asked ("Branch").
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub placeholder: String,
    /// One more line under the field.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub help: String,
    #[serde(default, rename = "type", skip_serializing_if = "is_text")]
    pub kind: InputKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern_help: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<InputChoice>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub must_exist: bool,
    /// Suggestions from a program on the tab's host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// With a source: only its suggestions are answers.
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict: bool,
    /// A choice or source input that takes several values; each becomes
    /// its own quoted word (`git add {files}`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub multiple: bool,
    /// `flag`: what "yes" inserts (`-r`); "no" inserts nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
}

fn is_text(k: &InputKind) -> bool {
    *k == InputKind::Text
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Where a snippet's suggestions come from: a program and how to read
/// its output. The host runs it (never a shell for a plugin's), in the
/// pane's directory, with a time limit and an output cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The program and its arguments. The first word is one of the
    /// plugin's package executables or required commands; the others may
    /// use earlier inputs' values (`{branch}`), each as one argument.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run: Vec<String>,
    /// Your own snippets only: a shell pipeline (`git branch | cut -c3-`),
    /// run by `sh -c`. `{input}` values are quoted like in `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sh: Option<String>,
    /// Splits each line into columns: a literal string (`"\t"`), or `" "`
    /// for runs of spaces. Without it each line is one value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
    /// The column holding the value (from 1; default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// The column shown dimmed next to it (default: the other columns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<usize>,
    /// The column shown (and searched) instead of the value: a process
    /// name for a process id (the value is still what's inserted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<usize>,
    /// Header lines to skip (`docker ps` has one).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skip: usize,
    #[serde(default, skip_serializing_if = "is_pane")]
    pub cwd: SourceCwd,
    /// Shown under the field when the program fails or prints nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fail: Option<String>,
    /// Seconds before it's stopped (default 2, at most 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_pane(c: &SourceCwd) -> bool {
    *c == SourceCwd::Pane
}

/// The directory a source runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceCwd {
    /// The focused pane's directory (falls back to the home directory).
    #[default]
    Pane,
    Home,
}

impl Source {
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.timeout.unwrap_or(DEFAULT_TIMEOUT).clamp(0.1, MAX_TIMEOUT))
    }

    /// For the install plan and the editor: `git for-each-ref … refs/heads`.
    pub fn describe(&self) -> String {
        match &self.sh {
            Some(sh) => format!("sh -c {}", crate::zsh::quote(sh)),
            None => self.run.iter().map(|w| if w.contains('{') { w.clone() } else { word(w) }).collect::<Vec<_>>().join(" "),
        }
    }
}

impl SnippetInput {
    /// A plain text input (the editor's starting point for a new `{name}`).
    pub fn text(id: &str, label: &str) -> Self {
        SnippetInput {
            id: id.into(),
            label: label.into(),
            placeholder: String::new(),
            help: String::new(),
            kind: InputKind::Text,
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
            multiple: false,
            on: None,
        }
    }

    /// The type and checks, in the format palette commands use.
    pub fn spec(&self) -> CommandInput {
        CommandInput {
            id: self.id.clone(),
            label: self.label.clone(),
            placeholder: self.placeholder.clone(),
            help: self.help.clone(),
            kind: self.kind,
            default: self.default.clone(),
            optional: self.optional,
            min: self.min,
            max: self.max,
            pattern: self.pattern.clone(),
            pattern_help: self.pattern_help.clone(),
            choices: self.choices.clone(),
            must_exist: self.must_exist,
        }
    }

    /// Check one value (one entry of a `multiple` input).
    pub fn check(&self, text: &str) -> Result<String, String> {
        check_value(&self.spec(), text)
    }

    /// The earlier inputs its source needs (`{branch}` in `run`).
    pub fn depends(&self) -> Vec<String> {
        let Some(src) = &self.source else { return Vec::new() };
        let mut out: Vec<String> = Vec::new();
        let texts: Vec<&str> = match &src.sh {
            Some(sh) => vec![sh.as_str()],
            None => src.run.iter().skip(1).map(String::as_str).collect(),
        };
        for t in texts {
            for p in parts(t).unwrap_or_default() {
                if let Part::Var(v) = p
                    && is_input_id(v)
                    && !out.iter().any(|o| o == v)
                {
                    out.push(v.to_string());
                }
            }
        }
        out
    }

    /// Values go in as written: they come from the author's own list.
    fn raw(&self) -> bool {
        matches!(self.kind, InputKind::Choice | InputKind::Flag)
    }
}

// ---- the line: lexing, rendering, previews -----------------------------------

/// A piece of a snippet's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// The author's shell text, as written (`{{` already a `{`).
    Text(String),
    /// `{name}`. `start`/`end`: it begins/ends a shell word (nothing but
    /// whitespace or an operator next to it), so an empty value can drop
    /// out and several values can become several words.
    Var { name: String, start: bool, end: bool },
}

/// Characters that end a shell word.
fn breaks_word(c: char) -> bool {
    c.is_whitespace() || "|&;()<>".contains(c)
}

/// Split `line` into text and placeholders. `is_input` says which names are
/// inputs: `{name}` outside quotes must be one (a typo otherwise), inside
/// quotes it must not be (goatty quotes values itself), and braces around
/// anything else are the shell's.
pub fn parse_line(line: &str, is_input: impl Fn(&str) -> bool) -> Result<Vec<Piece>, String> {
    #[derive(PartialEq, Clone, Copy)]
    enum Q {
        None,
        Single,
        Double,
        AnsiC,
    }
    if line.contains(['\n', '\r']) {
        return Err("a snippet is one line".into());
    }
    if line.chars().any(|c| c.is_control() && c != '\t') {
        return Err("control characters aren't allowed".into());
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<Piece> = Vec::new();
    let mut text = String::new();
    let mut q = Q::None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // Literal braces.
        if (c == '{' && next == Some('{')) || (c == '}' && next == Some('}')) {
            text.push(c);
            i += 2;
            continue;
        }
        if c == '{' && chars.get(i.wrapping_sub(1)) != Some(&'$') {
            let close = chars[i + 1..].iter().position(|&c| c == '}').map(|p| p + i + 1);
            if let Some(end) = close {
                let name: String = chars[i + 1..end].iter().collect();
                if is_input_id(&name) {
                    let known = is_input(&name);
                    if q != Q::None {
                        if known {
                            return Err(format!("{{{name}}} is inside quotes: put it outside them, goatty quotes the value"));
                        }
                    } else {
                        if !known {
                            return Err(format!("{{{name}}}: no input {name:?} (write {{{{{name}}}}} for literal braces)"));
                        }
                        let start = match text.chars().last() {
                            Some(c) => breaks_word(c),
                            None => out.is_empty(),
                        };
                        let end_ok = chars.get(end + 1).is_none_or(|&c| breaks_word(c));
                        if !text.is_empty() {
                            out.push(Piece::Text(std::mem::take(&mut text)));
                        }
                        out.push(Piece::Var { name, start, end: end_ok });
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        match (q, c) {
            (Q::None | Q::Double | Q::AnsiC, '\\') => {
                let Some(n) = next else { return Err("ends with a lone \\".into()) };
                text.push(c);
                text.push(n);
                i += 2;
                continue;
            }
            (Q::None, '\'') if text.ends_with('$') => q = Q::AnsiC,
            (Q::None, '\'') => q = Q::Single,
            (Q::None, '"') => q = Q::Double,
            (Q::Single, '\'') | (Q::AnsiC, '\'') | (Q::Double, '"') => q = Q::None,
            _ => {}
        }
        text.push(c);
        i += 1;
    }
    if q != Q::None {
        return Err("a quote isn't closed".into());
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    // A placeholder right after another one doesn't start a word.
    for k in 1..out.len() {
        if matches!(out[k - 1], Piece::Var { .. })
            && let Piece::Var { start, .. } = &mut out[k]
        {
            *start = false;
        }
    }
    for k in 0..out.len().saturating_sub(1) {
        if matches!(out[k + 1], Piece::Var { .. })
            && let Piece::Var { end, .. } = &mut out[k]
        {
            *end = false;
        }
    }
    Ok(out)
}

/// How a part of a [`Snippet::preview`] looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// The author's text.
    Text(String),
    /// A filled-in value (quoted as it'll be inserted), for input `id`.
    Value { id: String, text: String },
    /// A blank not filled in yet: `‹Branch›`. `current`: the one being asked.
    Blank { id: String, label: String, current: bool },
}

impl Seg {
    pub fn text(&self) -> String {
        match self {
            Seg::Text(t) | Seg::Value { text: t, .. } => t.clone(),
            Seg::Blank { label, .. } => format!("‹{label}›"),
        }
    }
}

impl Snippet {
    pub fn input(&self, id: &str) -> Option<&SnippetInput> {
        self.inputs.iter().find(|i| i.id == id)
    }

    /// The line split into text and placeholders.
    pub fn pieces(&self) -> Result<Vec<Piece>, String> {
        parse_line(&self.line, |n| self.input(n).is_some())
    }

    /// The line as typed into the terminal for `values`: each value one
    /// quoted word (several for a `multiple` input), `~/` kept expandable,
    /// empty optional values left out with their space.
    pub fn render(&self, values: &Answers) -> String {
        let segs = self.segments(values, None, true);
        tidy(&segs.iter().map(Seg::text).collect::<String>())
    }

    /// The line for showing while values are asked: filled values, blanks
    /// as `‹label›` (`current` marks the one being asked).
    pub fn preview(&self, values: &Answers, current: Option<&str>) -> Vec<Seg> {
        self.segments(values, current, false)
    }

    fn segments(&self, values: &Answers, current: Option<&str>, done: bool) -> Vec<Seg> {
        let Ok(pieces) = self.pieces() else { return vec![Seg::Text(self.line.clone())] };
        let mut out: Vec<Seg> = Vec::new();
        for p in pieces {
            match p {
                Piece::Text(t) => out.push(Seg::Text(t)),
                Piece::Var { name, start, end } => {
                    let Some(input) = self.input(&name) else { continue };
                    let value = values.get(&name);
                    let text = value.map(|v| render_value(input, v, start)).unwrap_or_default();
                    if text.is_empty() {
                        let answered = value.is_some() && (done || input.optional || input.kind == InputKind::Flag);
                        if answered || done {
                            // Left out, and its space with it when it's a whole word.
                            if start
                                && end
                                && let Some(Seg::Text(t)) = out.last_mut()
                                && t.ends_with(' ')
                            {
                                t.pop();
                            }
                            continue;
                        }
                        out.push(Seg::Blank { label: input.label.clone(), current: current == Some(name.as_str()), id: name });
                    } else {
                        out.push(Seg::Value { id: name, text });
                    }
                }
            }
        }
        out
    }

    /// What running it does: the author's word for it, else a guess.
    pub fn safety(&self) -> Safety {
        self.safety.unwrap_or_else(|| guess_safety(&self.line))
    }

    /// The programs the line runs (the first word of each command), for
    /// "needs docker".
    pub fn programs(&self) -> Vec<String> {
        programs(&self.line)
    }

    /// One of the user's snippets from `host.json` (`snippets.<id>`).
    pub fn from_user(id: &str, value: &serde_json::Value) -> Result<Snippet, String> {
        let mut s: Snippet = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        s.id = id.to_string();
        Ok(s)
    }

    /// As stored in `host.json` (without the id, which is the key).
    pub fn to_user(&self) -> serde_json::Value {
        let mut s = self.clone();
        s.id.clear();
        serde_json::to_value(&s).unwrap_or_default()
    }

    /// The text search looks at: title, description, tags, keywords, line.
    pub fn haystack(&self) -> String {
        let mut v = vec![self.title.clone(), self.description.clone(), self.line.clone()];
        v.extend(self.tags.iter().cloned());
        v.extend(self.keywords.iter().cloned());
        v.join(" ")
    }
}

/// One value as it goes into the line.
fn render_value(input: &SnippetInput, value: &str, word_start: bool) -> String {
    match input.kind {
        InputKind::Flag => {
            if value == "yes" {
                input.on.clone().unwrap_or_default()
            } else {
                String::new()
            }
        }
        _ if input.multiple => value.split('\n').filter(|v| !v.is_empty()).map(|v| one_value(input, v, true)).collect::<Vec<_>>().join(" "),
        _ if value.is_empty() => String::new(),
        _ => one_value(input, value, word_start),
    }
}

fn one_value(input: &SnippetInput, value: &str, word_start: bool) -> String {
    if input.raw() {
        return value.to_string();
    }
    let path = matches!(input.kind, InputKind::Path | InputKind::Folder);
    if let Some(rest) = value.strip_prefix('~').filter(|r| path && (r.is_empty() || r.starts_with('/'))) {
        let rest = rest.trim_start_matches('/');
        // A tilde only expands at the start of a word.
        let home = if word_start { "~" } else { "\"$HOME\"" };
        return if rest.is_empty() { home.to_string() } else { format!("{home}/{}", word(rest)) };
    }
    word(value)
}

/// Collapse the doubled spaces a left-out value leaves at the ends.
fn tidy(s: &str) -> String {
    s.trim().to_string()
}

/// The words of `line` outside quotes, with whether each starts a command.
fn command_words(line: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut q: Option<char> = None;
    let mut starts = true;
    let mut backtick = false;
    let mut chars = line.chars().peekable();
    let push = |cur: &mut String, quoted: &mut bool, starts: &mut bool, out: &mut Vec<(String, bool)>| {
        if !cur.is_empty() || *quoted {
            out.push((std::mem::take(cur), *starts));
            *starts = false;
        }
        *quoted = false;
    };
    while let Some(c) = chars.next() {
        match q {
            Some(open) => {
                if c == open {
                    q = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    q = Some(c);
                    quoted = true;
                }
                '\\' => cur.extend(chars.next()),
                c if c.is_whitespace() => push(&mut cur, &mut quoted, &mut starts, &mut out),
                '|' | '&' | ';' | '(' => {
                    push(&mut cur, &mut quoted, &mut starts, &mut out);
                    starts = true;
                }
                ')' => push(&mut cur, &mut quoted, &mut starts, &mut out),
                '`' => {
                    push(&mut cur, &mut quoted, &mut starts, &mut out);
                    backtick = !backtick;
                    starts = backtick;
                }
                '$' if chars.peek() == Some(&'(') => {
                    chars.next();
                    push(&mut cur, &mut quoted, &mut starts, &mut out);
                    starts = true;
                }
                '>' | '<' => {
                    push(&mut cur, &mut quoted, &mut starts, &mut out);
                    let mut op = c.to_string();
                    // `>>`, and `>&2`/`2>&1`: a file descriptor, not a file.
                    while let Some(&n) = chars.peek().filter(|&&n| n == '>' || n == '&') {
                        op.push(n);
                        chars.next();
                    }
                    out.push((op, false));
                }
                _ => cur.push(c),
            },
        }
    }
    push(&mut cur, &mut quoted, &mut starts, &mut out);
    out
}

/// Words that run the next word as a command.
const PREFIXES: &[&str] = &["sudo", "doas", "env", "time", "nohup", "nice", "command", "exec", "xargs", "watch"];

/// The programs `line` runs: the first word of each command (after
/// `sudo`, `env`, `VAR=value`), when it's a plain name.
pub fn programs(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut want = false;
    for (w, starts) in command_words(line) {
        if starts {
            want = true;
        }
        if !want {
            continue;
        }
        if w.contains('=') && !w.starts_with('=') || w.starts_with('-') {
            continue;
        }
        let plain = !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "._+-/".contains(c)) && !w.contains("..");
        // `env`, `time`, … are everywhere; `sudo` is worth knowing about.
        let listed = !PREFIXES.contains(&w.as_str()) || matches!(w.as_str(), "sudo" | "doas" | "watch");
        if plain && listed && !out.contains(&w) {
            out.push(w.clone());
        }
        want = PREFIXES.contains(&w.as_str());
    }
    out
}

/// What running `line` probably does, from its commands. Only a guess for
/// snippets that don't say (`safety`): the editor shows it, the author can
/// pick another.
pub fn guess_safety(line: &str) -> Safety {
    let words = command_words(line);
    let mut worst = Safety::Safe;
    let mut i = 0;
    for (k, (w, _)) in words.iter().enumerate() {
        if w == ">" || w == ">>" {
            // `> /dev/null` and `2>&1` change nothing.
            let target = words.get(k + 1).map(|(t, _)| t.as_str()).unwrap_or_default();
            if target != "/dev/null" && !target.starts_with('&') && !target.is_empty() {
                worst = worst.max(Safety::Changes);
            }
        }
    }
    while i < words.len() {
        let (_, starts) = &words[i];
        if !starts {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < words.len() && (PREFIXES.contains(&words[j].0.as_str()) || words[j].0.contains('=')) {
            if words[j].0 == "sudo" || words[j].0 == "doas" {
                worst = worst.max(Safety::Changes);
            }
            j += 1;
        }
        let end = (j + 1..words.len()).find(|&k| words[k].1).unwrap_or(words.len());
        let cmd: Vec<&str> =
            words[j.min(words.len())..end].iter().map(|(w, _)| w.as_str()).filter(|w| !w.starts_with(['>', '<'])).collect();
        worst = worst.max(command_safety(&cmd));
        i = end.max(i + 1);
    }
    worst
}

fn command_safety(cmd: &[&str]) -> Safety {
    let Some(&prog) = cmd.first() else { return Safety::Safe };
    let prog = prog.rsplit('/').next().unwrap_or(prog);
    let args = &cmd[1..];
    let has = |s: &str| args.contains(&s);
    let any_flag = |c: char| args.iter().any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(c));
    let sub = args.iter().find(|a| !a.starts_with('-')).copied().unwrap_or_default();
    match prog {
        "rm" | "rmdir" | "shred" | "dd" | "wipefs" | "fdisk" | "parted" | "truncate" | "srm" => Safety::Destructive,
        p if p.starts_with("mkfs") => Safety::Destructive,
        "git" => match sub {
            "reset" if has("--hard") => Safety::Destructive,
            "clean" => Safety::Destructive,
            "push" if has("--force") || has("-f") || args.iter().any(|a| a.starts_with("--force-with-lease")) => Safety::Destructive,
            "branch" if has("-D") || has("-d") || has("--delete") => Safety::Destructive,
            "stash" if args.iter().any(|a| *a == "drop" || *a == "clear") => Safety::Destructive,
            "checkout" if has("--") || has(".") => Safety::Destructive,
            "restore" => Safety::Destructive,
            "status" | "log" | "diff" | "show" | "blame" | "grep" | "ls-files" | "shortlog" | "describe" | "reflog" | "for-each-ref"
            | "rev-parse" | "help" => Safety::Safe,
            "branch" | "tag" | "remote" | "stash" | "config" if args.len() <= 1 => Safety::Safe,
            _ => Safety::Changes,
        },
        "docker" | "podman" => match sub {
            "rm" | "rmi" | "prune" => Safety::Destructive,
            "system" | "volume" | "image" | "container" if has("prune") || has("rm") => Safety::Destructive,
            "ps" | "images" | "logs" | "inspect" | "stats" | "top" | "version" | "info" => Safety::Safe,
            _ => Safety::Changes,
        },
        "mv" | "cp" | "mkdir" | "touch" | "chmod" | "chown" | "chgrp" | "ln" | "install" | "rsync" | "unzip" | "gunzip" | "gzip"
        | "zip" | "bunzip2" | "bzip2" | "xz" | "unxz" | "zstd" | "7z" | "tee" | "kill" | "pkill" | "killall" | "patch" | "crontab"
        | "passwd" | "chsh" | "useradd" | "usermod" | "ssh-keygen" | "mount" | "umount" | "reboot" | "shutdown" | "poweroff" => {
            Safety::Changes
        }
        "tar" if any_flag('x') || any_flag('c') || has("--extract") || has("--create") => Safety::Changes,
        "sed" if args.iter().any(|a| a.starts_with("-i") || a.starts_with("--in-place")) => Safety::Changes,
        "curl" if has("-o") || has("-O") || has("--output") || has("--remote-name") => Safety::Changes,
        "wget" => Safety::Changes,
        "find" if has("-delete") => Safety::Destructive,
        "find" if has("-exec") || has("-execdir") => Safety::Changes,
        "systemctl" | "service" if !matches!(sub, "status" | "list-units" | "list-unit-files" | "is-active" | "is-enabled" | "cat") => {
            Safety::Changes
        }
        "apt" | "apt-get" | "dnf" | "yum" | "pacman" | "zypper" | "brew" | "snap" | "flatpak" | "pip" | "pip3" | "pipx" | "npm"
        | "pnpm" | "yarn" | "cargo" | "gem" | "nix-env"
            if args.iter().any(|a| {
                matches!(
                    *a,
                    "install"
                        | "remove"
                        | "uninstall"
                        | "upgrade"
                        | "update"
                        | "purge"
                        | "autoremove"
                        | "add"
                        | "-S"
                        | "-R"
                        | "-Syu"
                        | "-Rs"
                )
            }) =>
        {
            Safety::Changes
        }
        _ => Safety::Safe,
    }
}

/// `title` as an id for the user's own: `Find big files` →
/// `find-big-files`, `-2`, `-3`… when `taken`.
pub fn id_from(title: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let mut s = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let mut s: String = s.trim_end_matches('-').chars().take(32).collect();
    if s.is_empty() || !s.as_bytes()[0].is_ascii_lowercase() {
        s.insert_str(0, "my-");
    }
    let s = s.trim_end_matches('-').to_string();
    if !taken(&s) {
        return s;
    }
    (2..).map(|n| format!("{s}-{n}")).find(|c| !taken(c)).unwrap_or(s)
}

/// Snippets and expansions as a file (`goattyd snippet export`, the
/// settings window's export): the manifest's `[[snippet]]` and
/// `[[expansion]]` tables on their own.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pack {
    #[serde(default, rename = "snippet", skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<Snippet>,
    #[serde(default, rename = "expansion", skip_serializing_if = "Vec::is_empty")]
    pub expansions: Vec<Expansion>,
}

impl Pack {
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    pub fn from_toml(text: &str) -> Result<Pack, String> {
        toml::from_str(text).map_err(|e| e.message().to_string())
    }
}

// ---- sources -------------------------------------------------------------------

/// One suggestion from a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub value: String,
    /// Shown instead of the value (the source's `label` column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Shown dimmed next to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What to run for a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A program and its arguments (no shell involved).
    Argv(Vec<String>),
    /// `sh -c` with this text (your own snippets only).
    Shell(String),
}

/// The command for `input`'s source with the earlier `values`; `Err` names
/// an input that has no value yet.
pub fn source_command(s: &Snippet, input: &SnippetInput, values: &Answers) -> Result<Command, String> {
    let src = input.source.as_ref().ok_or_else(|| format!("{} has no suggestions", input.id))?;
    let missing = input.depends().into_iter().find(|d| values.get(d).is_none_or(String::is_empty));
    if let Some(d) = missing {
        let label = s.input(&d).map_or(d.clone(), |i| i.label.clone());
        return Err(format!("Fill in {label} first"));
    }
    if let Some(sh) = &src.sh {
        let tmp = Snippet { line: sh.clone(), ..s.clone() };
        return Ok(Command::Shell(tmp.render(values)));
    }
    let mut argv = Vec::new();
    for (i, t) in src.run.iter().enumerate() {
        if i == 0 {
            argv.push(t.clone());
            continue;
        }
        let ps = parts(t)?;
        if let [Part::Var(v)] = ps.as_slice() {
            // A whole-argument placeholder: one argument per value.
            argv.extend(values.get(*v).into_iter().flat_map(|x| x.split('\n')).filter(|x| !x.is_empty()).map(str::to_string));
            continue;
        }
        let mut a = String::new();
        for p in ps {
            match p {
                Part::Text(t) => a.push_str(&t),
                Part::Var(v) => a.push_str(&values.get(v).cloned().unwrap_or_default().replace('\n', " ")),
            }
        }
        argv.push(a);
    }
    Ok(Command::Argv(argv))
}

/// Read a source's output: skip the header lines, split each line into
/// columns, take the value (and the detail), drop empty and repeated
/// values. Returns the items and whether there were more than [`MAX_ITEMS`].
pub fn parse_output(stdout: &str, src: &Source) -> (Vec<Item>, bool) {
    let mut out: Vec<Item> = Vec::new();
    let mut seen = BTreeSet::new();
    let col = src.column.unwrap_or(1).max(1) - 1;
    for line in stdout.lines().skip(src.skip) {
        let line = strip_ansi(line.trim_end_matches('\r'));
        let cols: Vec<&str> = match src.split.as_deref() {
            None if src.column.is_none() => vec![line.trim()],
            Some(" ") | None => line.split_whitespace().collect(),
            Some(d) => line.split(d).collect(),
        };
        let Some(value) = cols.get(col).map(|v| v.trim()).filter(|v| !v.is_empty()) else { continue };
        if value.chars().any(char::is_control) || !seen.insert(value.to_string()) {
            continue;
        }
        let label_col = src.label.map(|l| l.max(1) - 1);
        let label = label_col.and_then(|l| cols.get(l)).map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
        let detail = match src.detail {
            Some(d) => cols.get(d.max(1) - 1).map(|d| d.trim().to_string()),
            None if cols.len() > 1 => Some(
                cols.iter()
                    .enumerate()
                    .filter(|(i, _)| *i != col && Some(*i) != label_col)
                    .map(|(_, c)| c.trim())
                    .collect::<Vec<_>>()
                    .join("  "),
            ),
            None => None,
        }
        .filter(|d| !d.is_empty());
        if out.len() == MAX_ITEMS {
            return (out, true);
        }
        out.push(Item { value: value.to_string(), label, detail });
    }
    (out, false)
}

/// Without terminal color codes (`ESC [ … m`) and other control characters.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

// ---- validation ----------------------------------------------------------------

/// Whose snippet it is: a plugin's may only run its own programs and no
/// shell; the user's own may run anything (they wrote it).
#[derive(Debug, Clone, Copy)]
pub enum Owner<'a> {
    /// The plugin's package executables and required commands.
    Plugin(&'a BTreeSet<&'a str>),
    User,
}

/// Problems with one snippet; paths start with `at` (`snippet[2]`).
pub fn check_snippet(s: &Snippet, at: &str, owner: Owner<'_>) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut err = |path: String, message: String| out.push(Problem { path, message });
    if !is_command_id(&s.id) {
        err(format!("{at}.id"), format!("{:?}: lowercase letters, digits and '-', starting with a letter", s.id));
    }
    if s.title.trim().is_empty() {
        err(format!("{at}.title"), "empty".into());
    }
    if s.title.len() > 80 {
        err(format!("{at}.title"), "keep it under 80 characters".into());
    }
    for t in &s.tags {
        if t.is_empty() || t.len() > 24 || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
            err(format!("{at}.tags"), format!("{t:?}: a short word"));
        }
    }
    if s.line.trim().is_empty() {
        err(format!("{at}.line"), "empty".into());
    }
    let mut ids = BTreeSet::new();
    for (j, input) in s.inputs.iter().enumerate() {
        let at = format!("{at}.input[{j}]");
        if !is_input_id(&input.id) {
            err(format!("{at}.id"), format!("{:?}: lowercase letters, digits and '_'", input.id));
        } else if !ids.insert(input.id.as_str()) {
            err(format!("{at}.id"), format!("{:?} is used twice", input.id));
        }
        for p in check_input(&input.spec(), &at) {
            err(p.path, p.message);
        }
        match (input.kind, &input.on) {
            (InputKind::Flag, None) => err(format!("{at}.on"), "a flag needs `on`: the text \"yes\" inserts".into()),
            (InputKind::Flag, Some(on)) if on.trim().is_empty() || on.contains(['\n', '\r', '{', '}']) => {
                err(format!("{at}.on"), "one line of shell text without braces".into())
            }
            (k, Some(_)) if k != InputKind::Flag => err(format!("{at}.on"), "only flags have `on`".into()),
            _ => {}
        }
        if input.kind == InputKind::Flag && input.optional {
            err(format!("{at}.optional"), "a flag is always answered (yes or no)".into());
        }
        if input.multiple && input.source.is_none() && input.kind != InputKind::Choice {
            err(format!("{at}.multiple"), "only for choice inputs and inputs with a source".into());
        }
        if input.strict && input.source.is_none() {
            err(format!("{at}.strict"), "only for inputs with a source".into());
        }
        if let Some(src) = &input.source {
            if matches!(input.kind, InputKind::Choice | InputKind::Flag) {
                err(format!("{at}.source"), format!("{} inputs list their own values", input.kind.label()));
            }
            check_source(s, j, src, &format!("{at}.source"), owner, &mut err);
        }
    }
    match s.pieces() {
        Err(e) => err(format!("{at}.line"), e),
        Ok(pieces) => {
            let used: BTreeSet<&str> = pieces
                .iter()
                .filter_map(|p| match p {
                    Piece::Var { name, .. } => Some(name.as_str()),
                    Piece::Text(_) => None,
                })
                .collect();
            // An input may only narrow a later input's suggestions (a branch, then its commits).
            let feeds: BTreeSet<String> = s.inputs.iter().flat_map(SnippetInput::depends).collect();
            for input in &s.inputs {
                if !used.contains(input.id.as_str()) && !feeds.contains(&input.id) {
                    err(format!("{at}.line"), format!("doesn't use input {:?}: add {{{}}} where it goes", input.id, input.id));
                }
            }
            for p in &pieces {
                if let Piece::Var { name, start, end } = p
                    && let Some(input) = s.input(name)
                    && input.multiple
                    && !(*start && *end)
                {
                    err(format!("{at}.line"), format!("{{{name}}} takes several values: give it a word of its own"));
                }
            }
        }
    }
    out
}

fn check_source(s: &Snippet, index: usize, src: &Source, at: &str, owner: Owner<'_>, err: &mut impl FnMut(String, String)) {
    match (&src.sh, src.run.first(), owner) {
        (Some(_), _, Owner::Plugin(_)) => {
            err(format!("{at}.sh"), "plugins list a program and its arguments in `run`; `sh` is for your own snippets".into())
        }
        (Some(_), Some(_), _) => err(format!("{at}.run"), "use `run` or `sh`, not both".into()),
        (None, None, _) => err(format!("{at}.run"), "the program and its arguments".into()),
        (None, Some(p), Owner::Plugin(programs)) if !programs.contains(p.as_str()) => {
            err(format!("{at}.run"), format!("{p:?} is neither a package executable nor in requires.commands"))
        }
        (None, Some(p), _) if p.is_empty() || p.contains(['{', '}', '/', ' ']) && !p.starts_with('/') => {
            err(format!("{at}.run"), format!("{p:?}: the first word is a program's name"))
        }
        (Some(sh), None, Owner::User) => {
            let tmp = Snippet { line: sh.clone(), ..s.clone() };
            if let Err(e) = tmp.pieces() {
                err(format!("{at}.sh"), e);
            }
        }
        _ => {}
    }
    for t in src.run.iter().skip(1).chain(src.sh.iter()) {
        if t.contains('\0') || t.contains(['\n', '\r']) {
            err(format!("{at}.run"), "arguments are single lines".into());
        }
        let Ok(ps) = parts(t) else {
            if src.sh.is_none() {
                err(format!("{at}.run"), format!("{t:?}: unclosed '{{' (write {{{{ for a literal one)"));
            }
            continue;
        };
        for p in ps {
            if let Part::Var(v) = p
                && is_input_id(v)
            {
                match s.inputs.iter().position(|i| i.id == v) {
                    Some(k) if k < index => {}
                    Some(_) => err(format!("{at}.run"), format!("{{{v}}} is asked later: a source can only use earlier inputs")),
                    None if src.sh.is_none() => err(format!("{at}.run"), format!("{{{v}}}: no input {v:?}")),
                    None => {}
                }
            }
        }
    }
    if src.split.as_deref() == Some("") {
        err(format!("{at}.split"), "empty: leave it out for one value per line".into());
    }
    if src.column == Some(0) || src.detail == Some(0) || src.label == Some(0) {
        err(format!("{at}.column"), "columns count from 1".into());
    }
    if let Some(t) = src.timeout
        && !(t > 0.0 && t <= MAX_TIMEOUT)
    {
        err(format!("{at}.timeout"), format!("between 0 and {MAX_TIMEOUT} seconds"));
    }
}

// ---- expansions ----------------------------------------------------------------

/// Words you type that turn into a command while you type (on Space).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expansion {
    /// What you type: words and `{name}` placeholders, ending in a word
    /// (`| first {n} lines`).
    pub say: String,
    /// What it becomes, with the placeholders' text (`| head -n {n}`).
    pub to: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The placeholders' types (default `word`).
    #[serde(default, rename = "input", skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, ExpansionInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpansionInput {
    #[serde(default, rename = "type")]
    pub kind: ExpansionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExpansionKind {
    /// Anything without spaces, as typed (quotes included).
    #[default]
    Word,
    /// Digits.
    Integer,
}

/// An expansion as the zsh widget uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    /// A POSIX extended regular expression matching the end of the text
    /// left of the cursor: `(^|[[:space:]])` (group 1), then the words.
    pub re: String,
    /// `to` with each placeholder as `\x01N\x01` (N: its group, from 2).
    pub to: String,
    /// The last plain word of `say`: the widget only tries rules whose
    /// last word (or, with `trailing`, the word before it) was just typed.
    pub last: String,
    /// `say` ends with one placeholder after `last` (`| only lines with {text}`).
    pub trailing: bool,
    /// `say`, for the message under the prompt.
    pub say: String,
}

/// Separates `to`'s literal text from group numbers in [`Compiled::to`].
pub const GROUP_MARK: char = '\u{1}';

fn ere_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Compile an expansion, or say what's wrong with it.
pub fn compile(e: &Expansion) -> Result<Compiled, String> {
    let bad = |s: &str| s.chars().any(|c| c.is_control()) || s.contains(['\'', '"', '\\', '`']);
    let words: Vec<&str> = e.say.split_whitespace().collect();
    if words.len() < 2 {
        return Err("`say` needs at least two words, so ordinary commands never expand".into());
    }
    if e.say.len() > 200 || e.to.len() > 500 {
        return Err("too long".into());
    }
    let mut names: Vec<&str> = Vec::new();
    let mut re = String::from("(^|[[:space:]])");
    let mut last = String::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            re.push_str("[[:space:]]+");
        }
        let ps = parts(w).map_err(|e| format!("say: {e}"))?;
        match ps.as_slice() {
            [Part::Var(v)] => {
                if !is_input_id(v) {
                    return Err(format!("say: {{{v}}} is not a placeholder name (lowercase letters, digits, _)"));
                }
                if names.contains(v) {
                    return Err(format!("say: {{{v}}} appears twice"));
                }
                names.push(v);
                let kind = e.inputs.get(*v).map(|i| i.kind).unwrap_or_default();
                re.push_str(match kind {
                    ExpansionKind::Word => "([^[:space:]]+)",
                    ExpansionKind::Integer => "([0-9]+)",
                });
                last.clear();
            }
            ps if ps.iter().any(|p| matches!(p, Part::Var(_))) => {
                return Err(format!("say: {w:?}: a placeholder is a word of its own"));
            }
            _ => {
                let text: String = ps.iter().map(|p| if let Part::Text(t) = p { t.as_str() } else { "" }).collect();
                if bad(&text) {
                    return Err(format!("say: {w:?}: no quotes, backslashes or control characters"));
                }
                re.push_str(&ere_escape(&text));
                last = text;
            }
        }
    }
    let trailing = last.is_empty();
    if trailing {
        // One placeholder may come last; the word before it is the key.
        last = match words.len().checked_sub(2).map(|i| words[i]) {
            Some(w) if !(w.starts_with('{') && w.ends_with('}')) => w.replace("{{", "{").replace("}}", "}"),
            _ => return Err("`say` ends with at most one placeholder, after a word".into()),
        };
    }
    re.push('$');
    for k in e.inputs.keys() {
        if !names.contains(&k.as_str()) {
            return Err(format!("input.{k}: not a placeholder in `say`"));
        }
    }
    if e.to.trim().is_empty() {
        return Err("`to` is empty".into());
    }
    if e.to.chars().any(|c| c.is_control()) {
        return Err("`to`: no control characters".into());
    }
    let mut to = String::new();
    for p in parts(&e.to).map_err(|e| format!("to: {e}"))? {
        match p {
            Part::Text(t) => to.push_str(&t),
            Part::Var(v) => {
                let k = names.iter().position(|n| *n == v).ok_or_else(|| format!("to: {{{v}}} isn't in `say`"))?;
                to.push(GROUP_MARK);
                to.push_str(&(k + 2).to_string());
                to.push(GROUP_MARK);
            }
        }
    }
    Ok(Compiled { re, to, last, trailing, say: words.join(" ") })
}

impl Compiled {
    /// What the widget does to `left` (the text left of the cursor), done
    /// in Rust: the same regular expression and replacement. `None` when
    /// it doesn't match. For tests and the settings window's "try it".
    pub fn apply(&self, left: &str) -> Option<String> {
        let re = regex::Regex::new(&self.re).ok()?;
        let caps = re.captures(left)?;
        let whole = caps.get(0)?;
        let mut out = left[..whole.start()].to_string();
        out.push_str(caps.get(1).map_or("", |m| m.as_str()));
        for (i, chunk) in self.to.split(GROUP_MARK).enumerate() {
            if i % 2 == 0 {
                out.push_str(chunk);
            } else {
                let n: usize = chunk.parse().ok()?;
                out.push_str(caps.get(n).map_or("", |m| m.as_str()));
            }
        }
        Some(out)
    }
}

/// zsh's `$'…'` quoting: control characters as `\xNN`.
fn ansi_c(s: &str) -> String {
    let mut out = String::from("$'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `expansions.zsh`: the table the widget (`goatty-shell/zsh/expand.zsh`)
/// reads, in priority order (the first matching rule wins). Data only:
/// every string is quoted.
pub fn render_table(rules: &[Compiled]) -> String {
    let mut out = String::from(
        "# goatty expansions: generated from the plugins' [[expansion]] and host.json's expansions.\n# Change them in goatty's settings window (Snippets).\n",
    );
    let list = |f: &dyn Fn(&Compiled) -> String| rules.iter().map(f).collect::<Vec<_>>().join(" ");
    out.push_str(&format!("typeset -ga _goatty_exp_re=({})\n", list(&|r| crate::zsh::quote(&r.re))));
    out.push_str(&format!("typeset -ga _goatty_exp_to=({})\n", list(&|r| ansi_c(&r.to))));
    out.push_str(&format!("typeset -ga _goatty_exp_say=({})\n", list(&|r| crate::zsh::quote(&r.say))));
    // Rules by their last word, and (ending in a placeholder) by the word before.
    for (name, trailing) in [("_goatty_exp_last", false), ("_goatty_exp_last2", true)] {
        let mut by: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (i, r) in rules.iter().enumerate().filter(|(_, r)| r.trailing == trailing) {
            by.entry(r.last.as_str()).or_default().push((i + 1).to_string());
        }
        let pairs: Vec<String> = by.iter().map(|(k, v)| format!("{} {}", crate::zsh::quote(k), crate::zsh::quote(&v.join(" ")))).collect();
        out.push_str(&format!("typeset -gA {name}=({})\n", pairs.join(" ")));
    }
    out
}

/// Problems with a plugin's `[[snippet]]`s and `[[expansion]]`s.
pub(crate) fn check(m: &crate::manifest::Manifest, programs: &BTreeSet<&str>) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut ids = BTreeSet::new();
    for (i, s) in m.snippets.iter().enumerate() {
        let at = format!("snippet[{i}]");
        if !ids.insert(s.id.as_str()) {
            out.push(Problem { path: format!("{at}.id"), message: format!("{:?} is used twice", s.id) });
        }
        out.extend(check_snippet(s, &at, Owner::Plugin(programs)));
    }
    let mut says = BTreeSet::new();
    for (i, e) in m.expansions.iter().enumerate() {
        let at = format!("expansion[{i}]");
        match compile(e) {
            Err(msg) => out.push(Problem { path: at, message: msg }),
            Ok(c) => {
                if !says.insert(c.say) {
                    out.push(Problem { path: format!("{at}.say"), message: "the same words twice".into() });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
