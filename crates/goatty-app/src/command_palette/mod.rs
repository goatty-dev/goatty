//! The command palette (Ctrl+Shift+P): search goatty's commands by name and
//! run one, like VS Code's or Obsidian's.
//!
//! [`CommandPalette`] is an overlay the [`Workspace`](crate::workspace::Workspace)
//! shows above its panes. It snapshots the [`CommandRegistry`] when it opens,
//! ranks the commands with [`fuzzy`] on every keystroke (recently used ones
//! first while the query is empty, and slightly ahead when scores tie), and
//! emits [`PaletteEvent::Run`] with the chosen command. The workspace then
//! closes it, focuses the terminal again, and runs the command there.
//!
//! The hosts to connect to (`crate::hosts`) are commands too, listed once
//! something is typed. [`Scope::Hosts`] (Ctrl+Shift+H) is the host
//! switcher (#4): this computer and the hosts, with user@host, OS and
//! round-trip time for the ones goatty is connected to, and a typed
//! `user@host` that isn't one of them. Picking a host lists its sessions
//! ([`Scope::Sessions`], fetched off the UI thread, connecting first when
//! needed): first the tabs detached there (or left by a window that
//! closed) to restore all at once, one entry per window's worth (#252);
//! then a new session, "reattach every detached session" (a tab each), and
//! each session. Enter opens one where `ssh.open_in` says (a new tab or
//! window), Shift+Enter the other way. Shift+Delete on a "Restore N tabs"
//! entry (twice) ends those tabs' shells. Shift+Delete (or the row's
//! trash button), pressed twice, ends a session without opening it: the
//! daemon keeps sessions even across its own restarts, so old ones pile up
//! otherwise. Escape goes back to the hosts.
//!
//! The palette also asks for input ([`input`], #91): a command (or any
//! code) hands it an [`input::Ask`], and it shows one field at a time with
//! a label, placeholder, live check and suggestions, then emits
//! [`PaletteEvent::Answered`]. A field can be a list to pick from (#79):
//! moving through it calls the field's live preview, and closing the
//! palette without an answer calls the asker's `on_cancel` (Esc restores).

pub mod fuzzy;
pub mod input;
pub mod registry;

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use goatty_client::Connection;
use goatty_config::{CommandHistory, OpenIn};
use goatty_proto::{SessionId, SessionInfo};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, KeyBinding, ScrollStrategy, SharedString,
    StyledText, Subscription, Task, UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};

pub use input::{Answers, Ask, Field, ask};
pub use registry::{Command, CommandRegistry, register_builtins};

use crate::hosts::{self, HostList, HostRef, OpenRequest};
use crate::session::AttachMode;
use crate::settings::AppSettings;
use crate::ui::controls::pretty_keys;
use crate::ui::icons::Icon;
use crate::ui::style::Ui;
use crate::ui::text_input::{TextInput, TextInputEvent};

pub const CONTEXT: &str = "CommandPalette";

const ROW_HEIGHT: f32 = 32.;
const MAX_VISIBLE_ROWS: usize = 12;
const WIDTH: f32 = 620.;
/// Shortcuts shown per command (some have one per keyboard layout).
const MAX_SHORTCUTS: usize = 2;
/// A query word that only matches a keyword ("preferences" → Open settings)
/// counts this much less than one matching the label.
const KEYWORD_PENALTY: i32 = 12;
/// Score bonus for the most recently used command, decreasing by one per
/// step back in the history: enough to break near-ties, not to beat a
/// clearly better match.
const RECENT_BONUS: i32 = 8;
/// A query word that matches the title (not just the category) is more
/// specific: "sett" prefers "Settings: Open settings" over "Settings: Shell".
const TITLE_BONUS: i32 = 10;

actions!(
    command_palette,
    [SelectNext, SelectPrevious, SelectFirst, SelectLast, PageDown, PageUp, Dismiss, ConfirmOther, Complete, EndSession]
);

/// Key bindings inside the palette. `toggle_keys` are the keystrokes bound
/// to opening it, which close it again.
pub fn bindings(toggle_keys: &[String], cx: &App) -> Vec<KeyBinding> {
    let c = Some(CONTEXT);
    let mut v = vec![
        KeyBinding::new("down", SelectNext, c),
        KeyBinding::new("up", SelectPrevious, c),
        KeyBinding::new("ctrl-n", SelectNext, c),
        KeyBinding::new("ctrl-p", SelectPrevious, c),
        KeyBinding::new("tab", Complete, c),
        KeyBinding::new("shift-tab", SelectPrevious, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("ctrl-home", SelectFirst, c),
        KeyBinding::new("ctrl-end", SelectLast, c),
        KeyBinding::new("escape", Dismiss, c),
        KeyBinding::new("shift-enter", ConfirmOther, c),
        KeyBinding::new("shift-delete", EndSession, c),
    ];
    for k in toggle_keys {
        match KeyBinding::load(k, Box::new(Dismiss), gpui::KeyBindingContextPredicate::parse(CONTEXT).ok().map(Rc::new), false, None, cx) {
            Ok(b) => v.push(b),
            Err(e) => log::debug!("palette: can't bind {k} to close: {e}"),
        }
    }
    v
}

/// What the palette lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every command, hosts once something is typed.
    All,
    /// Only the hosts to connect to (this computer first).
    Hosts,
    /// The sessions on a host, and a new one.
    Sessions(HostRef),
}

/// The "new session" entry of [`Scope::Sessions`].
const NEW_SESSION: &str = "session.new";

/// The "every detached session" entry of [`Scope::Sessions`].
const ALL_DETACHED: &str = "session.detached";

/// What a session entry's id says to open.
fn session_choice(id: &str) -> Option<AttachMode> {
    if id == NEW_SESSION {
        return Some(AttachMode::New);
    }
    if id == ALL_DETACHED {
        return Some(AttachMode::AllDetached);
    }
    if let Some(layout) = id.strip_prefix("layout:") {
        return layout.parse().ok().map(AttachMode::Layout);
    }
    id.strip_prefix("session:")?.parse().ok().map(AttachMode::Id)
}

/// A session's name in lists: its title, else its program.
fn session_title(s: &SessionInfo) -> String {
    let process = s.process.as_deref().filter(|p| !p.is_empty());
    if !s.title.trim().is_empty() { s.title.trim().to_string() } else { process.unwrap_or("shell").to_string() }
}

/// `1 tab`, `3 tabs`.
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// The entry that restores saved layout `info`: its tabs with their names.
fn layout_command(info: &goatty_proto::LayoutInfo, list: &[SessionInfo], now: u64) -> Option<Command> {
    let title_of = |id: SessionId| list.iter().find(|s| s.id == id).map(session_title);
    let (tabs, panes, names) = crate::workspace::saved::summary(info, title_of)?;
    let mut title = format!("Restore {}", count(tabs, "tab", "tabs"));
    if panes > tabs {
        title.push_str(&format!(" ({} in all)", count(panes, "pane", "panes")));
    }
    let mut c = Command::handler(&format!("layout:{}", info.id), "Restore", &title, |_, _| {});
    let words: Vec<&str> = names.iter().map(String::as_str).chain(["restore", "detached", "tabs", "window"]).collect();
    c = c.keywords(&words);
    const SHOWN: usize = 4;
    let mut detail = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > SHOWN {
        detail.push_str(&format!(" +{}", names.len() - SHOWN));
    }
    detail.push_str(if info.saved_unix == 0 { " · just detached" } else { " · detached" });
    if info.saved_unix != 0 {
        detail.push_str(&format!(" {}", age(info.saved_unix, now)));
    }
    c.detail = Some(detail.into());
    Some(c)
}

/// `5 min`, `3 h`, `2 d` since `created` (unix seconds).
fn age(created: u64, now: u64) -> String {
    let secs = now.saturating_sub(created);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// The palette entry for a session: its title (else its program), with
/// the program, directory, whether a window shows it and its age.
fn session_command(s: &SessionInfo, now: u64) -> Command {
    let process = s.process.as_deref().filter(|p| !p.is_empty());
    let title = session_title(s);
    let mut c = Command::handler(&format!("session:{}", s.id), "Session", &title, |_, _| {});
    let mut words: Vec<&str> = Vec::new();
    words.extend(process);
    words.extend(s.cwd.as_deref());
    c = c.keywords(&words);
    let mut detail: Vec<String> = Vec::new();
    if let Some(p) = process.filter(|p| !title.contains(p)) {
        detail.push(p.to_string());
    }
    detail.push(if s.attached > 0 { "open".into() } else { "detached".into() });
    detail.push(age(s.created_unix, now));
    c.detail = Some(detail.join(" · ").into());
    c
}

/// The sessions picker's entries: the saved layouts to restore (newest
/// first), a new session, every loose detached session at once (when there
/// are several), then the sessions, newest first. `layouts`: `None` when
/// they couldn't be listed (then which sessions are loose is unknown: no
/// bulk entry).
fn session_commands(
    mut list: Vec<SessionInfo>,
    layouts: Option<&[goatty_proto::LayoutInfo]>,
    host: &HostRef,
    now: u64,
) -> Vec<Rc<Command>> {
    list.sort_by(|a, b| b.created_unix.cmp(&a.created_unix).then(b.id.cmp(&a.id)));
    let mut out: Vec<Command> = layouts.unwrap_or_default().iter().filter_map(|l| layout_command(l, &list, now)).collect();
    let mut new = Command::handler(NEW_SESSION, "Session", &format!("New session on {}", host.label()), |_, _| {});
    new.detail = Some("a fresh shell".into());
    out.push(new);
    let loose = layouts.map_or(0, |l| crate::workspace::saved::loose_detached(&list, l).len());
    if loose > 1 {
        let mut all = Command::handler(ALL_DETACHED, "Session", &format!("Reattach all {loose} detached sessions"), |_, _| {});
        all.detail = Some("a tab each".into());
        out.push(all.keywords(&["restore", "every", "detached"]));
    }
    out.extend(list.iter().map(|s| session_command(s, now)));
    out.into_iter().map(Rc::new).collect()
}

/// What ending sessions from the picker left.
struct Ended {
    /// `None`: couldn't be listed.
    layouts: Option<Vec<goatty_proto::LayoutInfo>>,
    list: Vec<SessionInfo>,
    /// How many kill requests failed, and the first error.
    failed: Option<(usize, String)>,
    /// Some of them hadn't exited yet when the list was taken.
    still_running: bool,
}

/// End `ids` on `conn` (blocks). For saved layout `layout`, the layout is
/// claimed first, so no window restores it meanwhile; only its sessions
/// that were confirmed and are still detached end, and whatever else it
/// holds is released again.
fn end_sessions(conn: &Connection, layout: Option<goatty_proto::LayoutId>, ids: Vec<SessionId>) -> anyhow::Result<Ended> {
    let ids = match layout {
        None => ids,
        Some(l) => {
            let Some(info) = conn.claim_layout_id(l)? else {
                anyhow::bail!("those tabs were restored or changed elsewhere meanwhile; nothing was ended");
            };
            let detached: Vec<SessionId> = conn.list_sessions()?.iter().filter(|s| s.attached == 0).map(|s| s.id).collect();
            let (end, keep): (Vec<SessionId>, Vec<SessionId>) = info.sessions.iter().partition(|s| ids.contains(s) && detached.contains(s));
            if keep.is_empty() {
                conn.save_layout(l, Vec::new(), String::new());
            } else {
                conn.release_claimed_layout(goatty_proto::LayoutInfo { sessions: keep, ..info });
            }
            end
        }
    };
    let mut failed: Vec<String> = Vec::new();
    for &id in &ids {
        // One that fails (or exited meanwhile) doesn't stop the others.
        if let Err(e) = conn.kill(id) {
            failed.push(format!("{e:#}").lines().next().unwrap_or_default().to_string());
        }
    }
    // They're gone once the shells exited; list what's left then.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let list = loop {
        let list = conn.list_sessions()?;
        if !list.iter().any(|s| ids.contains(&s.id)) || std::time::Instant::now() > deadline {
            break list;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let still_running = list.iter().any(|s| ids.contains(&s.id));
    // A kill that failed because the session was gone already isn't a failure.
    let failed = if still_running && !failed.is_empty() { Some((failed.len(), failed[0].clone())) } else { None };
    let layouts = conn.list_layouts().ok().map(|l| crate::workspace::saved::live_layouts(l, &list));
    Ok(Ended { layouts, list, failed, still_running })
}

/// The settings page for hosts, listed last in [`Scope::Hosts`].
const MANAGE_HOSTS: &str = "settings.hosts";

/// What the palette tells the workspace.
pub enum PaletteEvent {
    /// Close without running anything.
    Dismissed,
    /// Close and run this command.
    Run(Rc<Command>),
    /// Close and call the asker's handler with the answers.
    Answered(input::AnswerHandler, Answers),
    /// Close and go back to where the asking started (the snippet picker).
    Back(input::WindowAction),
}

/// Asking for input: the [`input::Ask`] and where it's at.
struct Asking {
    title: SharedString,
    fields: Vec<Field>,
    local: bool,
    then: input::AnswerHandler,
    step: usize,
    answers: Answers,
    /// Under the field: (text, is an error).
    message: Option<(SharedString, bool)>,
    /// Enter was pressed on this step, so "Required" shows too.
    tried: bool,
    /// The selected suggestion is what Enter answers (the user moved to
    /// it, or the field only takes its choices); otherwise the text is.
    picked: bool,
    home: Option<PathBuf>,
    /// What relative paths are relative to (`Ask::base`).
    base: Option<PathBuf>,
    /// Folder listing and existence check for the current text.
    _work: Option<Task<()>>,
    preview: Option<input::Preview>,
    badge: Option<input::Badge>,
    back: Option<input::WindowAction>,
    done_label: Option<SharedString>,
    /// A `multiple` field's values so far.
    chosen: Vec<String>,
    /// The user moved through the suggestions with the keyboard since the
    /// last value was added (a `multiple` field's Enter then adds it).
    moved: bool,
    /// A loader's results per step, with the answers they were for.
    loaded: std::collections::BTreeMap<usize, (String, input::Loaded)>,
    /// Shown under the field instead of its help: the loader's note.
    note: Option<SharedString>,
    _load: Option<Task<()>>,
    /// Runs when the palette goes away without an answer.
    on_cancel: Option<input::OnCancel>,
    answered: bool,
    /// The step is set up: selection moves from now on are the user's
    /// (and preview).
    ready: bool,
    /// The value the live preview shows now.
    previewed: Option<String>,
    /// A pick-from-list step's starting value (its default, or the answer
    /// when going back): selected whenever the list (re)appears, also
    /// once a loader's items arrive, until the user moves or types.
    want: Option<String>,
}

impl Asking {
    fn field(&self) -> &Field {
        &self.fields[self.step]
    }
}

/// One ranked result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Index into the palette's command list.
    pub index: usize,
    /// Matched characters of the label, for highlighting.
    pub positions: Vec<usize>,
    pub recent: bool,
}

/// What ranking needs to know about a command.
pub struct Entry<'a> {
    /// "Category: Title".
    pub label: &'a str,
    /// Byte offset of the title in `label` (0 without a category).
    pub title_start: usize,
    pub keywords: &'a [SharedString],
    pub recent_rank: Option<usize>,
    /// Left out while the query is empty, unless recently used.
    pub only_when_searched: bool,
}

/// Whether `c` applies to the focused pane (`pane`: its program and
/// whether it's at a prompt; `None`: no pane, so only commands for any).
fn fits_pane(c: &Command, pane: Option<&(Option<String>, bool)>) -> bool {
    match (&c.pane, pane) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(ctx), Some((process, at_prompt))) => ctx.matches(process.as_deref(), *at_prompt),
    }
}

/// Rank `entries` for `query`. An empty query lists recently used commands
/// (most recent first), then the rest alphabetically.
/// [`rank`], except that a host's sessions keep their own order until
/// something is typed: saved tabs to restore (newest first), then a new
/// session, then the sessions (newest first).
pub fn rank_in(scope: &Scope, entries: &[Entry<'_>], query: &str) -> Vec<Hit> {
    if matches!(scope, Scope::Sessions(_)) && query.trim().is_empty() {
        return (0..entries.len()).map(|index| Hit { index, positions: Vec::new(), recent: false }).collect();
    }
    rank(entries, query)
}

pub fn rank(entries: &[Entry<'_>], query: &str) -> Vec<Hit> {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        let mut idx: Vec<usize> =
            (0..entries.len()).filter(|&i| !entries[i].only_when_searched || entries[i].recent_rank.is_some()).collect();
        idx.sort_by(|&a, &b| {
            let (ea, eb) = (&entries[a], &entries[b]);
            (ea.recent_rank.is_none(), ea.recent_rank, ea.label.to_lowercase()).cmp(&(
                eb.recent_rank.is_none(),
                eb.recent_rank,
                eb.label.to_lowercase(),
            ))
        });
        return idx.into_iter().map(|index| Hit { index, positions: Vec::new(), recent: entries[index].recent_rank.is_some() }).collect();
    }
    let mut scored: Vec<(i32, Hit)> = Vec::new();
    'entries: for (index, e) in entries.iter().enumerate() {
        let mut score = 0;
        let mut positions = Vec::new();
        let title = &e.label[e.title_start..];
        let title_chars = e.label[..e.title_start].chars().count();
        for w in &words {
            let label = fuzzy::fuzzy_match(w, e.label);
            let in_title = if e.title_start > 0 { fuzzy::fuzzy_match(w, title) } else { None };
            // Prefer the title's alignment (and highlight it), with a bonus.
            let label = match (label, in_title) {
                (Some(l), Some(t)) => Some(fuzzy::Match {
                    score: l.score.max(t.score) + TITLE_BONUS,
                    positions: t.positions.iter().map(|p| p + title_chars).collect(),
                }),
                (l, _) => l,
            };
            let keyword = e.keywords.iter().filter_map(|k| fuzzy::fuzzy_match(w, k)).map(|m| m.score - KEYWORD_PENALTY).max();
            match (label, keyword) {
                (Some(l), Some(k)) if k > l.score => score += k,
                (Some(l), _) => {
                    score += l.score;
                    positions.extend(l.positions);
                }
                (None, Some(k)) => score += k,
                (None, None) => continue 'entries,
            }
        }
        if let Some(r) = e.recent_rank {
            score += RECENT_BONUS.saturating_sub(r as i32).max(0);
        }
        positions.sort_unstable();
        positions.dedup();
        scored.push((score, Hit { index, positions, recent: e.recent_rank.is_some() }));
    }
    scored.sort_by(|(sa, a), (sb, b)| sb.cmp(sa).then_with(|| entries[a.index].label.len().cmp(&entries[b.index].label.len())));
    scored.into_iter().map(|(_, h)| h).collect()
}

/// Pretty shortcuts of `c` ("Ctrl+,").
fn shortcuts(c: &Command, overrides: &std::collections::BTreeMap<String, Option<String>>) -> Vec<SharedString> {
    c.key_action()
        .map(|a| {
            goatty_config::keys::keys_for(a, overrides).iter().take(MAX_SHORTCUTS).map(|k| SharedString::from(pretty_keys(k))).collect()
        })
        .unwrap_or_default()
}

/// The host typed into [`Scope::Hosts`], when it can be one and isn't
/// listed already (`ids`: the listed commands').
pub fn typed_host<'a>(query: &str, mut ids: impl Iterator<Item = &'a str>) -> Option<String> {
    let host = query.trim();
    if goatty_config::ssh_host_problem(host).is_some() {
        return None;
    }
    let id = format!("ssh.host:{host}");
    (!ids.any(|i| i == id)).then(|| host.to_string())
}

/// How part of a label is drawn (the rest is plain text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// The "Category: " prefix, muted so the title stands out.
    Category,
    /// Letters the query matched.
    Match,
}

/// Non-overlapping, sorted styled ranges of a label: the matched `matches`,
/// and the category prefix (`..title_start`) around them.
pub fn label_spans(title_start: usize, matches: &[Range<usize>]) -> Vec<(Range<usize>, Span)> {
    let mut out = Vec::new();
    let mut at = 0;
    for m in matches {
        if at < m.start.min(title_start) {
            out.push((at..m.start.min(title_start), Span::Category));
        }
        out.push((m.clone(), Span::Match));
        at = m.end;
    }
    if at < title_start {
        out.push((at..title_start, Span::Category));
    }
    out
}

pub struct CommandPalette {
    scope: Scope,
    input: Entity<TextInput>,
    /// The listed commands, then (in [`Scope::Hosts`]) the typed host.
    commands: Vec<Rc<Command>>,
    /// How many of `commands` are listed ones.
    listed: usize,
    labels: Vec<SharedString>,
    /// Pretty shortcuts per command ("Ctrl+,").
    shortcuts: Vec<Vec<SharedString>>,
    hits: Vec<Hit>,
    selected: usize,
    query: String,
    scroll: UniformListScrollHandle,
    history: CommandHistory,
    history_path: Option<PathBuf>,
    /// [`Scope::Sessions`]: the host's connection once made, its entries,
    /// what's going on (`loading`: with a spinner), the fetch.
    conn: Option<Arc<Connection>>,
    sessions: Vec<Rc<Command>>,
    /// The host's saved layouts no window holds (restored from the list).
    layouts: Vec<goatty_proto::LayoutInfo>,
    /// `layouts` could be listed.
    layouts_known: bool,
    status: Option<(SharedString, bool)>,
    _load: Option<Task<()>>,
    /// The session the next Shift+Delete ends (the first one asks).
    ending: Option<SharedString>,
    asking: Option<Asking>,
    /// The focused pane's foreground program and whether a shell waits at
    /// its prompt, for commands that apply only in some panes (#240).
    pane: Option<(Option<String>, bool)>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl CommandPalette {
    /// A palette over `scope`, with the user's history.
    pub fn new(scope: Scope, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        Self::with_history(scope, goatty_config::command_history::history_file(), window, cx)
    }

    pub fn with_history(scope: Scope, history_path: Option<PathBuf>, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        // A small file, read once per opening (like the color picker's history).
        let history = history_path.as_deref().map(CommandHistory::load).unwrap_or_default();
        let input = cx.new(|cx| TextInput::new(placeholder(&scope), "", cx));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &TextInputEvent, _window, cx| match ev {
            TextInputEvent::Changed(q) => this.set_query(q, cx),
            TextInputEvent::Submitted(_) => this.confirm(cx),
        });
        // `~/.ssh/config` is re-read in the background; list what it has now.
        let hosts_changed = cx.observe_global::<HostList>(|this, cx| this.reload_commands(cx));
        let tailnet_changed = cx.observe_global::<crate::tailnet::TailnetList>(|this, cx| this.reload_commands(cx));
        // Plugin commands arrive after the host's catalog is fetched.
        let commands_changed = cx.observe_global::<CommandRegistry>(|this, cx| this.reload_commands(cx));
        hosts::refresh(cx);
        input.focus_handle(cx).focus(window, cx);
        let mut this = Self {
            scope,
            input,
            commands: Vec::new(),
            listed: 0,
            labels: Vec::new(),
            shortcuts: Vec::new(),
            hits: Vec::new(),
            selected: 0,
            query: String::new(),
            scroll: UniformListScrollHandle::new(),
            history,
            history_path,
            conn: None,
            sessions: Vec::new(),
            layouts: Vec::new(),
            layouts_known: false,
            status: None,
            _load: None,
            ending: None,
            asking: None,
            pane: None,
            _subscriptions: vec![sub, hosts_changed, tailnet_changed, commands_changed],
        };
        // A host's sessions are fetched; the other scopes list what's here.
        match this.scope.clone() {
            Scope::Sessions(host) => this.show_sessions(host, cx),
            _ => {
                this.load_commands(cx);
                this.set_query("", cx);
            }
        }
        this
    }

    /// A palette that asks for `ask`'s fields.
    pub fn asking(ask: Ask, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let mut this = Self::new(Scope::All, window, cx);
        this.asking = Some(Asking {
            title: ask.title,
            fields: ask.fields,
            local: ask.local,
            then: ask.then,
            step: 0,
            answers: Answers::new(),
            message: None,
            tried: false,
            picked: false,
            home: std::env::var_os("HOME").map(PathBuf::from),
            base: ask.base,
            _work: None,
            preview: ask.preview,
            badge: ask.badge,
            back: ask.back,
            done_label: ask.done_label,
            chosen: Vec::new(),
            moved: false,
            loaded: Default::default(),
            note: None,
            _load: None,
            on_cancel: ask.on_cancel,
            answered: false,
            ready: false,
            previewed: None,
            want: None,
        });
        if let Some(a) = &mut this.asking {
            a.answers = ask.answers;
        }
        // However the palette closes (Esc, a click outside, another
        // palette replacing it), an unanswered ask undoes its preview.
        let release = cx.on_release(|this, cx| {
            if let Some(a) = &this.asking
                && !a.answered
                && let Some(cancel) = a.on_cancel.clone()
            {
                cancel(cx);
            }
        });
        this._subscriptions.push(release);
        if this.asking.as_ref().is_some_and(|a| a.fields.is_empty()) {
            // Nothing to ask: answer right away (after the palette is set up).
            cx.spawn(async move |this, cx| this.update(cx, |this, cx| this.finish_ask(cx)).ok()).detach();
        } else {
            this.start_step(0, None, cx);
        }
        this
    }

    /// List only the commands that apply to the focused pane: one running
    /// `process`, `at_prompt` or not (#240).
    pub fn set_pane(&mut self, process: Option<String>, at_prompt: bool, cx: &mut Context<'_, Self>) {
        self.pane = Some((process, at_prompt));
        self.reload_commands(cx);
    }

    /// The registry or the hosts changed: list them again, keeping the
    /// query (not while asking: the suggestions are the field's).
    fn reload_commands(&mut self, cx: &mut Context<'_, Self>) {
        if self.asking.is_some() {
            return;
        }
        self.load_commands(cx);
        let q = std::mem::take(&mut self.query);
        self.set_query(&q, cx);
    }

    /// The commands to list: the registry's and the hosts, or only the hosts.
    fn load_commands(&mut self, cx: &mut Context<'_, Self>) {
        if self.asking.is_some() {
            return;
        }
        let registry = cx.try_global::<CommandRegistry>().map(|r| r.commands().to_vec()).unwrap_or_default();
        let tailnet: Vec<_> = crate::tailnet::current(cx).iter().map(|e| Rc::new(crate::tailnet::command(e))).collect();
        let hosts = hosts::current(cx).into_iter().map(|e| Rc::new(hosts::command(&e))).chain(tailnet);
        self.commands = match self.scope {
            Scope::All => registry.into_iter().filter(|c| fits_pane(c, self.pane.as_ref())).chain(hosts).collect(),
            Scope::Hosts => std::iter::once(Rc::new(hosts::local_command()))
                .chain(hosts)
                .chain(registry.into_iter().filter(|c| c.id == MANAGE_HOSTS))
                .collect(),
            Scope::Sessions(_) => self.sessions.clone(),
        };
        self.listed = self.commands.len();
        let overrides = &cx.global::<AppSettings>().client.keybindings;
        self.labels = self.commands.iter().map(|c| SharedString::from(c.label())).collect();
        self.shortcuts = self.commands.iter().map(|c| shortcuts(c, overrides)).collect();
    }

    fn set_query(&mut self, query: &str, cx: &mut Context<'_, Self>) {
        if self.asking.is_some() {
            self.ask_changed(query, cx);
            return;
        }
        self.commands.truncate(self.listed);
        self.labels.truncate(self.listed);
        self.shortcuts.truncate(self.listed);
        let entries: Vec<Entry<'_>> = self
            .commands
            .iter()
            .zip(&self.labels)
            .map(|(c, label)| Entry {
                label,
                title_start: c.title_start(),
                keywords: &c.keywords,
                recent_rank: self.history.rank(&c.id),
                only_when_searched: c.only_when_searched && self.scope == Scope::All,
            })
            .collect();
        self.hits = rank_in(&self.scope, &entries, query);
        if self.scope == Scope::Hosts {
            // Adding hosts comes after the hosts.
            if let Some(i) = self.hits.iter().position(|h| self.commands[h.index].id == MANAGE_HOSTS) {
                let manage = self.hits.remove(i);
                self.hits.push(manage);
            }
            if let Some(host) = typed_host(query, self.commands.iter().map(|c| c.id.as_ref())) {
                let c = Rc::new(hosts::typed_command(&host));
                self.labels.push(c.label().into());
                self.shortcuts.push(Vec::new());
                self.commands.push(c);
                let hit = Hit { index: self.commands.len() - 1, positions: Vec::new(), recent: false };
                // `user@host` is clearly an address; a bare word may be a
                // typo of a listed host, which stays first.
                let first = host.contains('@') || self.hits.iter().all(|h| self.commands[h.index].id == MANAGE_HOSTS);
                self.hits.insert(if first { 0 } else { self.hits.len() }, hit);
            }
        }
        self.query = query.to_string();
        self.select(0, cx);
    }

    fn select(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        let ix = ix.min(self.hits.len().saturating_sub(1));
        if ix != self.selected || self.ending.as_ref().is_some_and(|id| self.endable_at(ix).map(|(e, _)| e).as_ref() != Some(id)) {
            self.disarm_end();
        }
        self.selected = ix;
        if !self.hits.is_empty() {
            self.scroll.scroll_to_item(self.selected, ScrollStrategy::Nearest);
        }
        self.preview_selection(cx);
        cx.notify();
    }

    /// The selected entry of a pick-from-list field, if any.
    fn selected_value(&self) -> Option<String> {
        self.hits.get(self.selected).map(|h| self.commands[h.index].id.to_string())
    }

    /// Show the selected entry with the field's live preview (once per
    /// value; not while a step is being set up, which selects its current
    /// value).
    fn preview_selection(&mut self, cx: &mut Context<'_, Self>) {
        let value = self.selected_value();
        let Some(a) = &mut self.asking else { return };
        if !a.ready || !a.field().strict() || a.field().multiple {
            return;
        }
        let Some(on_select) = a.field().on_select.clone() else { return };
        let Some(value) = value else { return };
        if a.previewed.as_ref() == Some(&value) {
            return;
        }
        a.previewed = Some(value.clone());
        on_select(&value, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.pick();
        // Wraps around, like VS Code.
        let n = self.hits.len().max(1);
        self.select((self.selected + 1) % n, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.pick();
        let n = self.hits.len().max(1);
        self.select((self.selected + n - 1) % n, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.pick();
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.pick();
        self.select(self.hits.len().saturating_sub(1), cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.pick();
        self.select(self.selected + MAX_VISIBLE_ROWS - 1, cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.pick();
        self.select(self.selected.saturating_sub(MAX_VISIBLE_ROWS - 1), cx);
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<'_, Self>) {
        // A field back to the one before it.
        if let Some(a) = &self.asking
            && a.step > 0
        {
            let prev = a.step - 1;
            let text = a.answers.get(&a.fields[prev].spec.id).cloned();
            self.start_step(prev, text, cx);
            return;
        }
        // Back to where the asking started.
        if let Some(back) = self.asking.as_ref().and_then(|a| a.back.clone()) {
            cx.emit(PaletteEvent::Back(back));
            return;
        }
        // From a host's sessions back to the hosts.
        if matches!(self.scope, Scope::Sessions(_)) {
            self.show_scope(Scope::Hosts, cx);
            return;
        }
        cx.emit(PaletteEvent::Dismissed);
    }

    fn confirm(&mut self, cx: &mut Context<'_, Self>) {
        if self.asking.is_some() {
            self.submit_answer(cx);
            return;
        }
        self.run_hit(self.selected, false, cx);
    }

    fn confirm_other(&mut self, _: &ConfirmOther, _: &mut Window, cx: &mut Context<'_, Self>) {
        if self.asking.is_some() {
            self.submit_answer(cx);
            return;
        }
        self.run_hit(self.selected, true, cx);
    }

    /// Tab: while asking, fill in the selected (else the first)
    /// suggestion; otherwise the next command.
    fn complete(&mut self, _: &Complete, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.asking.is_none() {
            self.select_next(&SelectNext, window, cx);
            return;
        }
        let ix = if self.asking.as_ref().is_some_and(|a| a.picked) { self.selected } else { 0 };
        self.fill_suggestion(ix, cx);
    }

    // ---- asking for input ----------------------------------------------------

    /// The user chose a suggestion with the keyboard.
    fn pick(&mut self) {
        if let Some(a) = &mut self.asking {
            a.picked = true;
            a.moved = true;
            a.want = None;
        }
    }

    /// Whether the selected row is drawn as selected.
    fn shows_selection(&self) -> bool {
        self.asking.as_ref().is_none_or(|a| a.picked)
    }

    /// Show field `step`, starting with `text` (else its default).
    fn start_step(&mut self, step: usize, text: Option<String>, cx: &mut Context<'_, Self>) {
        let Some(a) = &mut self.asking else { return };
        a.step = step;
        a.ready = false;
        a.tried = false;
        a.picked = false;
        a.message = None;
        a._work = None;
        a.moved = false;
        a.note = None;
        let multiple = a.fields[step].multiple;
        // A multiple field's values are chips; the text starts empty.
        a.chosen = if multiple {
            text.iter().flat_map(|t| t.split('\n')).filter(|v| !v.is_empty()).map(str::to_string).collect()
        } else {
            Vec::new()
        };
        let text = if multiple { Some(String::new()) } else { text };
        let spec = &a.fields[step].spec;
        let text = text.or_else(|| spec.default.clone()).unwrap_or_default();
        // A pick-from-list field lists everything with its value selected;
        // the text only filters.
        let (text, preselect) = if a.fields[step].strict() && !multiple { (String::new(), Some(text)) } else { (text, None) };
        let placeholder = if spec.placeholder.is_empty() { format!("{}…", spec.label) } else { spec.placeholder.clone() };
        let label = spec.label.clone();
        a.want = preselect;
        self.input.update(cx, |input, cx| {
            input.set_text(text.clone(), cx);
            input.set_placeholder(placeholder, cx);
            input.set_label(label);
            // Typing replaces a suggested value.
            input.select_all_text(cx);
        });
        self.ask_changed(&text, cx);
        // The current value (selected by `ask_changed`) is what's shown
        // already; anything else previews once selected.
        let selected = self.selected_value();
        let shown = self.asking.as_ref().and_then(|a| a.want.clone()).filter(|w| selected.as_ref() == Some(w));
        if let Some(a) = &mut self.asking {
            a.ready = true;
            a.previewed = shown;
        }
        self.load_suggestions(step, cx);
    }

    /// Fetch the suggestions of field `step` if it has a loader (once per
    /// step and earlier answers).
    fn load_suggestions(&mut self, step: usize, cx: &mut Context<'_, Self>) {
        let Some(a) = &mut self.asking else { return };
        let Some(loader) = a.fields[step].loader.clone() else { return };
        let key: String = a.fields[..step].iter().map(|f| format!("{}={:?};", f.spec.id, a.answers.get(&f.spec.id))).collect();
        if let Some((k, loaded)) = a.loaded.get(&step)
            && *k == key
        {
            let loaded = loaded.clone();
            self.show_loaded(step, loaded, cx);
            return;
        }
        let job = loader(&a.answers);
        self.status = Some(("Looking for suggestions…".into(), true));
        a._load = Some(cx.spawn(async move |this, cx| {
            let loaded = cx.background_executor().spawn(async move { job() }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(a) = &mut this.asking else { return };
                if a.step != step {
                    return;
                }
                a.loaded.insert(step, (key, loaded.clone()));
                this.status = None;
                this.show_loaded(step, loaded, cx);
            });
        }));
        cx.notify();
    }

    fn show_loaded(&mut self, step: usize, loaded: input::Loaded, cx: &mut Context<'_, Self>) {
        let Some(a) = &mut self.asking else { return };
        a.fields[step].suggestions = loaded.items;
        a.note = loaded.note.map(Into::into);
        let q = self.query.clone();
        self.ask_changed(&q, cx);
    }

    /// The field's text changed: check it, update the suggestions.
    fn ask_changed(&mut self, text: &str, cx: &mut Context<'_, Self>) {
        self.query = text.to_string();
        let Some(a) = &mut self.asking else { return };
        if !text.trim().is_empty() {
            a.want = None;
        }
        a.picked = a.field().strict();
        let field = a.field().clone();
        a.message = message_for(&field, text, a.tried);
        if let Some(note) = a.note.clone()
            && a.message.as_ref().is_none_or(|(_, err)| !err)
        {
            a.message = Some((note, false));
        }
        if field.is_path() {
            self.show_suggestions(Vec::new(), "", cx);
            let Some(a) = &mut self.asking else { return };
            if !a.local {
                return;
            }
            let (home, base, text_owned, step) = (a.home.clone(), a.base.clone(), text.to_string(), a.step);
            let folders = field.spec.kind == goatty_plugin::commands::InputKind::Folder;
            // Only a value that passes the other checks is looked up.
            let value = field.validate(text).ok().map(|v| input::trim_completed(&v).to_string());
            let spec = field.spec.clone();
            a._work = Some(cx.spawn(async move |this, cx| {
                let t = text_owned.clone();
                let (list, missing) = cx
                    .background_executor()
                    .spawn(async move {
                        let list = input::list_folder(&t, home.as_deref(), folders);
                        let missing = value.and_then(|v| input::missing_path(&spec, &v, home.as_deref(), base.as_deref()));
                        (list, missing)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.query != text_owned || this.asking.as_ref().is_none_or(|a| a.step != step) {
                        return;
                    }
                    if let (Some(a), Some(m)) = (&mut this.asking, missing)
                        && a.message.as_ref().is_none_or(|(_, err)| !err)
                    {
                        a.message = Some((m.into(), true));
                    }
                    this.show_suggestions(list, "", cx);
                });
            }));
            return;
        }
        let mut list = field.suggestions.clone();
        list.retain(|s| !a.chosen.contains(&s.value));
        self.show_suggestions(list, text, cx);
        // A default or earlier answer selects its own choice, not the
        // first fuzzy match (a label may look like another's value).
        if field.strict() {
            let want = self.asking.as_ref().and_then(|a| a.want.clone());
            let at = |v: &str| self.hits.iter().position(|h| self.commands[h.index].id == v);
            if let Some(ix) = at(text).or_else(|| want.as_deref().and_then(at)) {
                self.select(ix, cx);
            }
        }
    }

    /// List `list` under the field, filtered by `query` (fuzzy, by label
    /// or value; everything for an empty query, in order).
    fn show_suggestions(&mut self, list: Vec<input::Suggestion>, query: &str, cx: &mut Context<'_, Self>) {
        self.commands = list
            .into_iter()
            .map(|s| {
                let mut c = Command::handler(&s.value, "", &s.label, |_, _| {});
                c.detail = s.detail.map(Into::into);
                Rc::new(c)
            })
            .collect();
        self.listed = self.commands.len();
        self.labels = self.commands.iter().map(|c| SharedString::from(c.label())).collect();
        self.shortcuts = vec![Vec::new(); self.commands.len()];
        // Found by label, value or detail (a process by its name or id).
        let values: Vec<Vec<SharedString>> =
            self.commands.iter().map(|c| std::iter::once(c.id.clone()).chain(c.detail.clone()).collect()).collect();
        let entries: Vec<Entry<'_>> = self
            .labels
            .iter()
            .zip(&values)
            .map(|(label, value)| Entry { label, title_start: 0, keywords: value, recent_rank: None, only_when_searched: false })
            .collect();
        self.hits = if query.trim().is_empty() {
            (0..entries.len()).map(|index| Hit { index, positions: Vec::new(), recent: false }).collect()
        } else {
            rank(&entries, query)
        };
        self.select(0, cx);
    }

    /// Put suggestion `ix` into the field (Tab, or clicking a folder).
    fn fill_suggestion(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        let Some(hit) = self.hits.get(ix) else { return };
        let value = self.commands[hit.index].id.to_string();
        self.input.update(cx, |input, cx| input.set_text(value.clone(), cx));
        self.ask_changed(&value, cx);
    }

    /// Enter: check the answer, then the next field or done.
    fn submit_answer(&mut self, cx: &mut Context<'_, Self>) {
        let Some(a) = &mut self.asking else { return };
        let picked = a.picked.then(|| self.hits.get(self.selected).map(|h| self.commands[h.index].id.to_string())).flatten();
        let field = a.field().clone();
        if field.multiple {
            // Enter adds the selected suggestion (or the text); on an empty
            // field it goes on with the values so far.
            let typed = !self.query.trim().is_empty();
            let candidate = match picked {
                Some(v) if a.moved || typed => Some(v),
                _ if typed && !field.strict() => Some(self.query.clone()),
                _ if typed => {
                    a.tried = true;
                    a.message = Some(("Pick one from the list".into(), true));
                    cx.notify();
                    return;
                }
                _ => None,
            };
            if let Some(v) = candidate {
                match field.validate(&v) {
                    Ok(v) if !v.is_empty() => {
                        if !a.chosen.contains(&v) {
                            a.chosen.push(v);
                        }
                        a.moved = false;
                        a.tried = false;
                        self.input.update(cx, |input, cx| input.set_text("", cx));
                        self.ask_changed("", cx);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        a.tried = true;
                        a.message = Some((e.into(), true));
                        cx.notify();
                    }
                }
                return;
            }
            if a.chosen.is_empty() && !field.spec.optional {
                a.tried = true;
                a.message = Some(("Add at least one".into(), true));
                cx.notify();
                return;
            }
            let value = a.chosen.join("\n");
            self.accept(value, cx);
            return;
        }
        let text = match picked {
            Some(v) => v,
            None if field.strict() => {
                a.tried = true;
                a.message = Some(("Pick one from the list".into(), true));
                cx.notify();
                return;
            }
            None => self.query.clone(),
        };
        let text = if field.is_path() { input::trim_completed(&text).to_string() } else { text };
        let value = match field.validate(&text) {
            Ok(v) => v,
            Err(e) => {
                a.tried = true;
                a.message = Some((e.into(), true));
                cx.notify();
                return;
            }
        };
        if !(field.is_path() && a.local && field.spec.must_exist) || value.is_empty() {
            self.accept(value, cx);
            return;
        }
        let (home, base, step, spec) = (a.home.clone(), a.base.clone(), a.step, field.spec.clone());
        a._work = Some(cx.spawn(async move |this, cx| {
            let v = value.clone();
            let missing =
                cx.background_executor().spawn(async move { input::missing_path(&spec, &v, home.as_deref(), base.as_deref()) }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(a) = &mut this.asking else { return };
                if a.step != step {
                    return;
                }
                match missing {
                    Some(m) => {
                        a.tried = true;
                        a.message = Some((m.into(), true));
                        cx.notify();
                    }
                    None => this.accept(value, cx),
                }
            });
        }));
    }

    /// Remove a `multiple` field's value (its chip was clicked).
    fn unchoose(&mut self, value: &str, cx: &mut Context<'_, Self>) {
        let Some(a) = &mut self.asking else { return };
        a.chosen.retain(|v| v != value);
        let q = self.query.clone();
        self.ask_changed(&q, cx);
    }

    /// Keep `value` for the current field and go on.
    fn accept(&mut self, value: String, cx: &mut Context<'_, Self>) {
        let Some(a) = &mut self.asking else { return };
        a.answers.insert(a.field().spec.id.clone(), value);
        let next = a.step + 1;
        if next < a.fields.len() {
            let text = a.answers.get(&a.fields[next].spec.id).cloned();
            self.start_step(next, text, cx);
        } else {
            self.finish_ask(cx);
        }
    }

    fn finish_ask(&mut self, cx: &mut Context<'_, Self>) {
        let Some(a) = &mut self.asking else { return };
        a.answered = true;
        cx.emit(PaletteEvent::Answered(a.then.clone(), a.answers.clone()));
    }

    /// Switch to `scope` with an empty query.
    fn show_scope(&mut self, scope: Scope, cx: &mut Context<'_, Self>) {
        self.input.update(cx, |input, cx| {
            input.set_text("", cx);
            input.set_placeholder(placeholder(&scope), cx);
        });
        self.scope = scope;
        self.conn = None;
        self.sessions.clear();
        self.status = None;
        self._load = None;
        self.load_commands(cx);
        self.set_query("", cx);
    }

    /// The sessions picker for `host`: a new session right away, the
    /// host's sessions once fetched (connecting first if no tab is on it).
    fn show_sessions(&mut self, host: HostRef, cx: &mut Context<'_, Self>) {
        let now = unix_now();
        self.show_scope(Scope::Sessions(host.clone()), cx);
        self.layouts.clear();
        self.sessions = session_commands(Vec::new(), Some(&[]), &host, now);
        let endpoint = host.endpoint();
        let label = if hosts::pool::live(&endpoint).is_some() {
            "Loading sessions…".to_string()
        } else {
            format!("Connecting to {}…", host.label())
        };
        self.status = Some((label.into(), true));
        self.load_commands(cx);
        self.set_query("", cx);
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let conn = hosts::pool::open(endpoint)?;
                    let list = conn.list_sessions()?;
                    let layouts = match conn.list_layouts() {
                        Ok(l) => Some(crate::workspace::saved::live_layouts(l, &list)),
                        Err(e) => {
                            log::warn!("listing the saved layouts: {e:#}");
                            None
                        }
                    };
                    anyhow::Ok((conn, list, layouts))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.scope != Scope::Sessions(host.clone()) {
                    return;
                }
                match result {
                    Ok((conn, list, layouts)) => {
                        this.status = if layouts.is_none() {
                            Some(("Couldn't list the detached tabs; their sessions are listed one by one.".into(), false))
                        } else {
                            list.is_empty().then(|| (format!("No sessions on {} yet", host.label()).into(), false))
                        };
                        this.sessions = session_commands(list, layouts.as_deref(), &host, unix_now());
                        this.layouts_known = layouts.is_some();
                        this.layouts = layouts.unwrap_or_default();
                        this.conn = Some(conn);
                    }
                    Err(e) => {
                        let first = format!("{e:#}").lines().next().unwrap_or_default().to_string();
                        let msg = format!("Couldn't list the sessions: {first}. A new session shows what goatty can do about it.");
                        this.status = Some((msg.into(), false));
                    }
                }
                this.load_commands(cx);
                let q = std::mem::take(&mut this.query);
                this.set_query(&q, cx);
            });
        }));
    }

    /// What Shift+Delete at hit `ix` ends ([`Scope::Sessions`]): the
    /// entry's id and its sessions (a session's one, a saved layout's all).
    fn endable_at(&self, ix: usize) -> Option<(SharedString, Vec<SessionId>)> {
        let hit = self.hits.get(ix)?;
        let id = self.commands[hit.index].id.clone();
        let sessions = match session_choice(&id)? {
            AttachMode::Id(s) => vec![s],
            AttachMode::Layout(l) => self.layouts.iter().find(|x| x.id == l)?.sessions.clone(),
            _ => return None,
        };
        Some((id, sessions))
    }

    /// Forget a pending "end this session?".
    fn disarm_end(&mut self) {
        if self.ending.take().is_some() {
            self.status = None;
        }
    }

    fn end_selected(&mut self, _: &EndSession, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.end_session_at(self.selected, cx);
    }

    /// End the session at hit `ix` without opening it: the first time
    /// asks, the second ends it.
    fn end_session_at(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        let Scope::Sessions(host) = self.scope.clone() else { return };
        if self.asking.is_some() {
            return;
        }
        let (Some((key, ids)), Some(conn)) = (self.endable_at(ix), self.conn.clone()) else { return };
        let title = self.commands[self.hits[ix].index].title.clone();
        // A saved layout's entry: "Restore 3 tabs" → "3 tabs".
        let what = match title.strip_prefix("Restore ") {
            None => format!("“{title}”"),
            Some(t) if t.starts_with("1 tab") => "that tab".to_string(),
            Some(t) => format!("those {}", t.split(" (").next().unwrap_or(t)),
        };
        if self.selected != ix {
            self.select(ix, cx);
        }
        if self.ending.as_ref() != Some(&key) {
            self.ending = Some(key);
            let msg = if ids.len() == 1 {
                format!("Shift+Delete again ends {what}: whatever runs in it stops, and a window showing it closes the pane.")
            } else {
                format!("Shift+Delete again ends {what} ({} shells): whatever runs in them stops.", ids.len())
            };
            self.status = Some((msg.into(), false));
            cx.notify();
            return;
        }
        self.ending = None;
        self.status = Some((format!("Ending {what}…").into(), true));
        cx.notify();
        let layout = match session_choice(&self.commands[self.hits[ix].index].id) {
            Some(AttachMode::Layout(l)) => Some(l),
            _ => None,
        };
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { end_sessions(&conn, layout, ids) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.scope != Scope::Sessions(host.clone()) {
                    return;
                }
                match result {
                    Ok(Ended { layouts, list, failed, still_running }) => {
                        let msg = match (&failed, still_running) {
                            (Some((n, first)), _) => format!("Couldn't end {n} of {what}: {first}"),
                            (None, true) => format!("Asked {what} to end; some are still exiting"),
                            (None, false) => format!("Ended {what}"),
                        };
                        this.status = Some((msg.into(), false));
                        this.layouts_known = layouts.is_some();
                        this.layouts = layouts.unwrap_or_default();
                        let known = this.layouts_known.then_some(&this.layouts[..]);
                        this.sessions = session_commands(list, known, &host, unix_now());
                    }
                    Err(e) => {
                        let first = format!("{e:#}").lines().next().unwrap_or_default().to_string();
                        this.status = Some((format!("Couldn't end {what}: {first}").into(), false));
                    }
                }
                this.load_commands(cx);
                let q = std::mem::take(&mut this.query);
                this.set_query(&q, cx);
            });
        }));
    }

    fn run_hit(&mut self, ix: usize, other_place: bool, cx: &mut Context<'_, Self>) {
        // A suggestion: a folder completes, anything else is the answer.
        if let Some(a) = &mut self.asking {
            if a.field().is_path() {
                self.fill_suggestion(ix, cx);
            } else {
                a.picked = true;
                a.moved = true;
                self.select(ix, cx);
                self.submit_answer(cx);
            }
            return;
        }
        let Some(hit) = self.hits.get(ix) else { return };
        let command = self.commands[hit.index].clone();
        match &self.scope {
            Scope::Sessions(host) => {
                let Some(attach) = session_choice(&command.id) else { return };
                let open_in = cx.global::<AppSettings>().client.ssh.open_in;
                let place = if other_place { open_in.flip() } else { open_in };
                let host = host.clone();
                hosts::open(cx, OpenRequest { host, attach, conn: self.conn.clone(), place });
                cx.emit(PaletteEvent::Dismissed);
                return;
            }
            // A host: its sessions next (plain SSH hosts have none to list).
            Scope::Hosts => {
                if let Some(host) = HostRef::from_command_id(&command.id) {
                    let plain = matches!(&host, HostRef::Ssh(h) if cx.global::<AppSettings>().client.ssh.plain(h));
                    if !plain {
                        self.remember(&command.id, cx);
                        self.show_sessions(host, cx);
                        return;
                    }
                }
            }
            Scope::All => {}
        }
        self.remember(&command.id, cx);
        cx.emit(PaletteEvent::Run(command));
    }

    /// Put `id` first in the recently used list.
    fn remember(&mut self, id: &str, cx: &mut Context<'_, Self>) {
        if self.history.push(id)
            && let Some(path) = self.history_path.clone()
        {
            let history = self.history.clone();
            cx.background_spawn(async move {
                if let Err(e) = history.save(&path) {
                    log::warn!("couldn't save {}: {e}", path.display());
                }
            })
            .detach();
        }
    }

    fn render_rows(&mut self, range: std::ops::Range<usize>, ui: Ui, cx: &mut Context<'_, Self>) -> Vec<gpui::AnyElement> {
        range
            .filter_map(|ix| {
                let hit = self.hits.get(ix)?;
                let label = self.labels[hit.index].clone();
                let selected = ix == self.selected && self.shows_selection();
                let matched = HighlightStyle { color: Some(ui.accent), font_weight: Some(FontWeight::BOLD), ..Default::default() };
                let category = HighlightStyle { color: Some(ui.muted), ..Default::default() };
                let matches = fuzzy::byte_ranges(&label, &hit.positions);
                let spans = label_spans(self.commands[hit.index].title_start(), &matches);
                let text = StyledText::new(label.clone()).with_highlights(spans.into_iter().map(|(r, span)| {
                    (
                        r,
                        match span {
                            Span::Category => category,
                            Span::Match => matched,
                        },
                    )
                }));
                let keys = self.shortcuts[hit.index].clone();
                let detail = self.commands[hit.index].detail.clone();
                let recent = hit.recent && self.query.trim().is_empty();
                // Sessions can be ended from the list (twice: ask, then end).
                let endable_at = self.endable_at(ix);
                let endable = self.conn.is_some() && endable_at.is_some();
                let armed = endable && self.ending.is_some() && self.ending == endable_at.map(|(e, _)| e);
                Some(
                    div()
                        .id(ix)
                        .w_full()
                        .h(px(ROW_HEIGHT))
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(ui.surface_hover))
                        .when(!selected, |d| d.hover(|s| s.bg(ui.surface)))
                        .on_click(cx.listener(move |this, _, _, cx| this.run_hit(ix, false, cx)))
                        .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().child(text))
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .gap_1()
                                .items_center()
                                .text_xs()
                                .text_color(ui.muted)
                                .when_some(detail, |d, text| d.child(text))
                                .when(recent, |d| d.child("recently used"))
                                .children(
                                    keys.into_iter()
                                        .map(|k| div().px_1p5().rounded_sm().border_1().border_color(ui.border).bg(ui.surface).child(k)),
                                )
                                .when(endable, |d| {
                                    d.child(
                                        div()
                                            .id(("end-session", ix))
                                            .p_1()
                                            .rounded_sm()
                                            .text_color(if armed { ui.danger } else { ui.muted })
                                            .when(armed, |d| d.bg(ui.surface))
                                            .hover(|s| s.bg(ui.surface).text_color(ui.danger))
                                            .tooltip(crate::ui::controls::tooltip(if armed {
                                                "Click again to end it"
                                            } else {
                                                "End this session  (Shift+Delete)"
                                            }))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.end_session_at(ix, cx);
                                            }))
                                            .child(Icon::Trash.svg().size(px(13.)).text_color(if armed { ui.danger } else { ui.muted })),
                                    )
                                }),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let ui_font = cx.global::<AppSettings>().ui_font.clone();
        let rows = self.hits.len().min(MAX_VISIBLE_ROWS);
        let list = if self.hits.is_empty() && self.asking.is_some() {
            div().into_any_element()
        } else if self.hits.is_empty() {
            let empty = match self.scope {
                Scope::All => "No matching commands",
                Scope::Hosts => "No hosts yet: type user@host, or add hosts in Settings → Hosts",
                Scope::Sessions(_) => "No matching sessions",
            };
            div().px_4().py_3().text_color(ui.muted).child(empty).into_any_element()
        } else {
            let list =
                uniform_list("commands", self.hits.len(), cx.processor(move |this, range, _window, cx| this.render_rows(range, ui, cx)))
                    .track_scroll(&self.scroll)
                    .w_full()
                    .h(px(ROW_HEIGHT * rows as f32));
            div().px_1().child(list).into_any_element()
        };
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::confirm_other))
            .on_action(cx.listener(Self::complete))
            .on_action(cx.listener(Self::end_selected))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismissed)))
            .occlude()
            .w(px(WIDTH))
            .max_w_full()
            .flex()
            .flex_col()
            .gap_1()
            .pb_1()
            .font_family(ui_font)
            .text_sm()
            .text_color(ui.text)
            .bg(ui.bg)
            .border_1()
            .border_color(ui.border)
            .rounded_lg()
            .shadow_lg()
            .overflow_hidden()
            .when_some(self.asking.as_ref(), |d, a| {
                let mono = cx.global::<AppSettings>().client.font.family.clone().unwrap_or_else(|| "monospace".into());
                // What a pick-from-list field answers is the selected entry, not the filter text.
                let text = if a.field().strict() {
                    self.hits.get(self.selected).map(|h| self.commands[h.index].id.to_string()).unwrap_or_default()
                } else {
                    self.query.clone()
                };
                d.child(ask_header(a, &text, ui, mono.into()))
            })
            .child(div().p_2().when(self.asking.is_some(), |d| d.pt_0()).child(self.input.clone()))
            .when_some(self.asking.as_ref().filter(|a| !a.chosen.is_empty()), |d, a| {
                d.child(div().px_3().flex().flex_wrap().gap_1().children(a.chosen.iter().enumerate().map(|(i, v)| {
                    let value = v.clone();
                    div()
                        .id(("chosen", i))
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .py_0p5()
                        .rounded(px(crate::ui::style::radius::SMALL))
                        .bg(ui.accent_soft)
                        .text_color(ui.accent_text)
                        .text_xs()
                        .cursor_pointer()
                        .tooltip(crate::ui::controls::tooltip("Remove"))
                        .on_click(cx.listener(move |this, _, _, cx| this.unchoose(&value, cx)))
                        .child(v.clone())
                        .child(Icon::Close.svg().size(px(10.)).text_color(ui.accent_text))
                })))
            })
            .when_some(self.asking.as_ref().and_then(|a| a.message.clone()), |d, (text, error)| {
                d.child(
                    div()
                        .id("ask-message")
                        .px_4()
                        .text_xs()
                        .text_color(if error { ui.danger } else { ui.muted })
                        .when(error, |d| d.role(gpui::Role::Alert))
                        .child(text),
                )
            })
            .child(list)
            .when_some(self.status.clone(), |d, (text, loading)| {
                d.child(
                    div()
                        .px_4()
                        .py_1()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(ui.muted)
                        .when(loading, |d| d.child(Icon::Loader.svg().size(px(12.)).text_color(ui.accent)))
                        .child(text),
                )
            })
            .when_some(self.asking.as_ref(), |d, a| {
                let last = a.step + 1 == a.fields.len();
                let done = a.done_label.clone().unwrap_or_else(|| "done".into());
                let next: SharedString = if last { done } else { "next".into() };
                let back = if a.step > 0 || a.back.is_some() { "back" } else { "cancel" };
                let mut hint = if a.field().multiple {
                    format!("Enter: add · Enter on an empty field: {next} · Esc: {back}")
                } else {
                    format!("Enter: {next} · Esc: {back}")
                };
                if !self.hits.is_empty() {
                    hint.push_str(if a.field().strict() { " · ↑↓: pick" } else { " · ↑↓: pick · Tab: fill in" });
                }
                d.child(div().px_4().pt_1().border_t_1().border_color(ui.border).text_xs().text_color(ui.muted).child(hint))
            })
            .when(self.asking.is_none() && matches!(self.scope, Scope::Sessions(_)), |d| {
                let open_in = cx.global::<AppSettings>().client.ssh.open_in;
                let name = |o: OpenIn| match o {
                    OpenIn::Tab => "a new tab",
                    OpenIn::Window => "a new window",
                };
                let hint = format!(
                    "Enter: open in {} · Shift+Enter: {} · Shift+Delete: end it · Esc: back to the hosts",
                    name(open_in),
                    name(open_in.flip())
                );
                d.child(div().px_4().pt_1().border_t_1().border_color(ui.border).text_xs().text_color(ui.muted).child(hint))
            })
    }
}

/// What a field shows under it for `text`: the check's complaint, else its
/// help. An empty field only complains once Enter was pressed (`tried`).
fn message_for(field: &Field, text: &str, tried: bool) -> Option<(SharedString, bool)> {
    let help = (!field.spec.help.is_empty()).then(|| (SharedString::from(field.spec.help.clone()), false));
    // A strict field's text only filters its choices.
    if field.strict() || (text.trim().is_empty() && !tried) {
        return help;
    }
    match field.validate(text) {
        Err(e) => Some((e.into(), true)),
        Ok(_) => help,
    }
}

/// The title of what's being asked (with its badge), the finished line
/// so far, the field's label and the step.
fn ask_header(a: &Asking, text: &str, ui: Ui, mono: SharedString) -> impl IntoElement {
    let steps = (a.fields.len() > 1).then(|| format!("{} of {}", a.step + 1, a.fields.len()));
    let current = a.field().spec.id.clone();
    let preview = a.preview.as_ref().map(|p| {
        // The field being typed shows its text as it'll be inserted.
        let segs = p(&a.answers, &current, text);
        preview_line(&segs, ui, mono)
    });
    div()
        .px_4()
        .pt_2()
        .pb_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(div().text_xs().text_color(ui.muted).child(a.title.clone()))
                .when_some(a.badge.clone(), |d, b| d.child(badge(&b, ui))),
        )
        .when_some(preview, |d, p| d.child(p))
        .child(
            div()
                .flex()
                .justify_between()
                .items_baseline()
                .gap_2()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(a.field().spec.label.clone()))
                .when_some(steps, |d, s| d.child(div().text_xs().text_color(ui.muted).child(s))),
        )
}

/// A command line with its blanks: filled values stand out, the blank
/// being asked is highlighted, the others are dim.
pub fn preview_line(segs: &[goatty_plugin::snippets::Seg], ui: Ui, mono: SharedString) -> gpui::Div {
    use goatty_plugin::snippets::Seg;
    let mut text = String::new();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for seg in segs {
        let start = text.len();
        text.push_str(&seg.text());
        let style = match seg {
            Seg::Text(_) => None,
            Seg::Value { .. } => {
                Some(HighlightStyle { color: Some(ui.accent_text), font_weight: Some(FontWeight::SEMIBOLD), ..Default::default() })
            }
            Seg::Blank { current: true, .. } => {
                Some(HighlightStyle { color: Some(ui.on_accent), background_color: Some(ui.accent), ..Default::default() })
            }
            Seg::Blank { .. } => Some(HighlightStyle { color: Some(ui.faint), ..Default::default() }),
        };
        if let Some(st) = style
            && text.len() > start
        {
            highlights.push((start..text.len(), st));
        }
    }
    div()
        .w_full()
        .px_2()
        .py_1p5()
        .rounded(px(crate::ui::style::radius::SMALL))
        .bg(ui.field)
        .font_family(mono)
        .text_sm()
        .flex()
        .gap_2()
        .child(div().flex_none().text_color(ui.faint).child("$"))
        .child(div().flex_1().min_w_0().text_color(ui.text).child(StyledText::new(text).with_highlights(highlights)))
}

/// A small colored label.
pub fn badge(b: &input::Badge, ui: Ui) -> impl IntoElement {
    let color = match b.tone {
        input::Tone::Good => ui.success,
        input::Tone::Warn => ui.warning,
        input::Tone::Danger => ui.danger,
    };
    div()
        .id(SharedString::from(format!("badge-{}", b.text)))
        .flex_none()
        .px_1p5()
        .rounded(px(crate::ui::style::radius::SMALL))
        .border_1()
        .border_color(color.opacity(0.5))
        .bg(color.opacity(0.12))
        .text_color(color)
        .text_size(px(crate::ui::style::text::CAPTION))
        .tooltip(crate::ui::controls::tooltip(b.tooltip.clone()))
        .child(b.text.clone())
}

fn placeholder(scope: &Scope) -> SharedString {
    match scope {
        Scope::All => "Type a command…".into(),
        Scope::Hosts => "Connect to… (a host below, or user@host)".into(),
        Scope::Sessions(host) => format!("Sessions on {}…", host.label()).into(),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_for_other_panes_are_left_out() {
        let mut vim = Command::handler("plugin.vim.quit", "Vim", "Exit", |_, _| {});
        vim.pane = Some(goatty_plugin::commands::PaneContext { programs: vec!["vim".into(), "nvim".into()], at: None });
        let plain = Command::handler("x.y", "X", "Y", |_, _| {});
        let nvim = (Some("nvim".to_string()), false);
        let zsh = (Some("zsh".to_string()), true);
        assert!(fits_pane(&vim, Some(&nvim)));
        assert!(!fits_pane(&vim, Some(&zsh)));
        assert!(!fits_pane(&vim, None), "no pane: only commands for any");
        assert!(fits_pane(&plain, Some(&zsh)) && fits_pane(&plain, None));
    }

    fn entries<'a>(labels: &'a [(&'a str, &'a [SharedString], Option<usize>)]) -> Vec<Entry<'a>> {
        labels
            .iter()
            .map(|(label, keywords, recent_rank)| Entry {
                label,
                title_start: label.find(": ").map_or(0, |i| i + 2),
                keywords,
                recent_rank: *recent_rank,
                only_when_searched: false,
            })
            .collect()
    }

    fn labels(e: &[Entry<'_>], hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|h| e[h.index].label.to_string()).collect()
    }

    #[test]
    fn empty_query_lists_recent_then_alphabetical() {
        let none: &[SharedString] = &[];
        let data = [("b", none, None), ("c", none, Some(1)), ("a", none, None), ("d", none, Some(0))];
        let e = entries(&data);
        let hits = rank(&e, "");
        assert_eq!(labels(&e, &hits), ["d", "c", "a", "b"]);
        assert!(hits[0].recent && !hits[2].recent);
    }

    #[test]
    fn keywords_find_commands_without_highlighting() {
        let kw = [SharedString::from("preferences")];
        let none: &[SharedString] = &[];
        let data = [("Settings: Open settings", &kw[..], None), ("Terminal: Paste", none, None)];
        let e = entries(&data);
        let hits = rank(&e, "prefs");
        assert_eq!(labels(&e, &hits), ["Settings: Open settings"]);
        assert!(hits[0].positions.is_empty());
        // A label match is highlighted.
        let hits = rank(&e, "paste");
        assert_eq!(hits[0].positions, [10, 11, 12, 13, 14]);
    }

    #[test]
    fn label_matches_beat_keyword_matches() {
        let kw = [SharedString::from("theme")];
        let none: &[SharedString] = &[];
        let data = [("Settings: Appearance", &kw[..], None), ("Themes: Open the themes folder", none, None)];
        let e = entries(&data);
        assert_eq!(labels(&e, &rank(&e, "theme"))[0], "Themes: Open the themes folder");
    }

    #[test]
    fn recency_breaks_ties() {
        let none: &[SharedString] = &[];
        let data = [("View: Bigger text", none, None), ("View: Bigger text!", none, Some(0))];
        let e = entries(&data);
        assert_eq!(labels(&e, &rank(&e, "bigger"))[0], "View: Bigger text!");
    }

    #[test]
    fn hosts_show_once_searched_or_used() {
        let none: &[SharedString] = &[];
        let data = [("Tabs: New tab", none, None), ("SSH: Connect to box", none, None), ("SSH: Connect to pi", none, Some(0))];
        let mut e = entries(&data);
        e[1].only_when_searched = true;
        e[2].only_when_searched = true;
        assert_eq!(labels(&e, &rank(&e, "")), ["SSH: Connect to pi", "Tabs: New tab"]);
        assert_eq!(labels(&e, &rank(&e, "box")), ["SSH: Connect to box"]);
    }

    #[test]
    fn session_entries() {
        let info = |id, title: &str, process: Option<&str>, attached, created| SessionInfo {
            id,
            pid: None,
            title: title.into(),
            cwd: Some("/srv".into()),
            cols: 80,
            rows: 24,
            attached,
            created_unix: created,
            process: process.map(Into::into),
        };
        let host = HostRef::Ssh("box".into());
        let list = vec![info(3, "", Some("zsh"), 0, 1_000), info(7, "build", Some("cargo"), 1, 4_000)];
        let c = session_commands(list.clone(), Some(&[]), &host, 4_000 + 7_200);
        let ids: Vec<_> = c.iter().map(|c| c.id.to_string()).collect();
        assert_eq!(ids, ["session.new", "session:7", "session:3"], "new first, then newest");
        assert_eq!(c[0].title.as_ref(), "New session on box");
        assert_eq!((c[1].title.as_ref(), c[1].detail.as_deref()), ("build", Some("cargo · open · 2 h")));
        assert_eq!((c[2].title.as_ref(), c[2].detail.as_deref()), ("zsh", Some("detached · 2 h")));
        assert!(matches!(session_choice("session.new"), Some(AttachMode::New)));
        assert!(matches!(session_choice("session:7"), Some(AttachMode::Id(7))));
        assert!(session_choice("ssh.host:box").is_none());
        assert!(matches!(session_choice("layout:12"), Some(AttachMode::Layout(12))));
        assert!(matches!(session_choice("session.detached"), Some(AttachMode::AllDetached)));
        assert!(session_choice("layout:x").is_none());

        // Saved layouts come first, then the new session, every loose
        // detached session (when there are two), then each session.
        let mut list = list;
        list.push(info(9, "", Some("htop"), 0, 2_000));
        list.push(info(11, "logs", Some("tail"), 0, 3_000));
        let json = |ids: &[u64]| {
            let tabs: Vec<String> = ids.iter().map(|id| format!(r#"{{"tree":{{"leaf":{id}}},"focused":{id}}}"#)).collect();
            format!(r#"{{"version":1,"tabs":[{}],"active":0}}"#, tabs.join(","))
        };
        let layouts = [goatty_proto::LayoutInfo { id: 5, json: json(&[3, 11]), sessions: vec![3, 11], saved_unix: 4_000 }];
        // Unknown layouts: no restore entries, and no bulk reattach (two
        // loose-looking sessions may be one saved split).
        let unknown: Vec<_> = session_commands(list.clone(), None, &host, 4_000 + 600).iter().map(|c| c.id.to_string()).collect();
        assert_eq!(unknown, ["session.new", "session:7", "session:11", "session:9", "session:3"]);
        let c = session_commands(list, Some(&layouts), &host, 4_000 + 600);
        let ids: Vec<_> = c.iter().map(|c| c.id.to_string()).collect();
        assert_eq!(
            ids,
            ["layout:5", "session.new", "session:7", "session:11", "session:9", "session:3"],
            "one loose session: no bulk entry"
        );
        assert_eq!((c[0].title.as_ref(), c[0].detail.as_deref()), ("Restore 2 tabs", Some("zsh, logs · detached 10 min")));
        // Listed in that order (not alphabetically) while nothing is typed.
        let labels: Vec<SharedString> = c.iter().map(|c| c.label().into()).collect();
        let entries: Vec<Entry<'_>> = c
            .iter()
            .zip(&labels)
            .map(|(c, label)| Entry {
                label,
                title_start: c.title_start(),
                keywords: &c.keywords,
                recent_rank: None,
                only_when_searched: false,
            })
            .collect();
        let order: Vec<usize> = rank_in(&Scope::Sessions(host.clone()), &entries, " ").iter().map(|h| h.index).collect();
        assert_eq!(order, (0..c.len()).collect::<Vec<_>>());
        assert_eq!(age(100, 130), "just now");
        assert_eq!(age(0, 300), "5 min");
        assert_eq!(age(0, 3 * 86_400), "3 d");
    }

    #[test]
    fn typed_hosts() {
        let ids = ["ssh.host:box", "settings.hosts"];
        assert_eq!(typed_host(" pi@10.0.0.5 ", ids.into_iter()).as_deref(), Some("pi@10.0.0.5"));
        assert_eq!(typed_host("box", ids.into_iter()), None, "listed already");
        assert_eq!(typed_host("", ids.into_iter()), None);
        assert_eq!(typed_host("two words", ids.into_iter()), None);
        assert_eq!(typed_host("-oProxyCommand=x", ids.into_iter()), None);
    }

    #[test]
    fn category_is_muted_around_matches() {
        use Span::*;
        // "Settings: Open settings", title at 10.
        assert_eq!(label_spans(10, &[]), [(0..10, Category)]);
        assert_eq!(label_spans(10, std::slice::from_ref(&(15..19))), [(0..10, Category), (15..19, Match)]);
        assert_eq!(label_spans(10, &[0..4, 15..17]), [(0..4, Match), (4..10, Category), (15..17, Match)]);
        assert_eq!(label_spans(10, &[2..3, 6..12]), [(0..2, Category), (2..3, Match), (3..6, Category), (6..12, Match)]);
        assert_eq!(label_spans(0, std::slice::from_ref(&(1..2))), [(1..2, Match)], "no category");
    }

    #[test]
    fn builtins_rank_sensibly() {
        let cmds = registry::builtins();
        let label_strings: Vec<String> = cmds.iter().map(Command::label).collect();
        let e: Vec<Entry<'_>> = cmds
            .iter()
            .zip(&label_strings)
            .map(|(c, l)| Entry {
                label: l,
                title_start: c.title_start(),
                keywords: &c.keywords,
                recent_rank: None,
                only_when_searched: c.only_when_searched,
            })
            .collect();
        let top = |q: &str| e[rank(&e, q).first().unwrap_or_else(|| panic!("nothing for {q:?}")).index].label.to_string();
        assert_eq!(top("open settings"), "Settings: Open settings");
        assert_eq!(top("sett"), "Settings: Open settings");
        assert_eq!(top("settings shell"), "Settings: Shell");
        assert_eq!(top("shortcuts"), "Settings: Keyboard shortcuts");
        assert_eq!(top("keybind"), "Settings: Keyboard shortcuts");
        assert_eq!(top("reload"), "Shell: Reload running shells");
        assert_eq!(top("rel sh"), "Shell: Reload running shells");
        assert_eq!(top("zoom in"), "View: Bigger text");
        assert_eq!(top("paste"), "Terminal: Paste");
        assert_eq!(top("theme"), "Themes: Switch theme…");
        assert_eq!(top("themes folder"), "Themes: Open the themes folder");
        assert_eq!(top("dark mode"), "Themes: Dark, light or follow the system…");
        assert_eq!(top("install tool"), "Shell: Install a built-in tool…");
        assert_eq!(top("quit"), "goatty: Quit");
        assert_eq!(top("ssh"), "SSH: Connect to a host…");
        assert_eq!(top("connect"), "SSH: Connect to a host…");
        assert_eq!(top("custom host"), "SSH: Connect to a custom host…");
        assert_eq!(top("user@host"), "SSH: Connect to a custom host…");
        assert_eq!(top("add host"), "SSH: Add a host to the list…");
        assert_eq!(top("add path"), "Shell: Add a folder to PATH…");
        assert_eq!(top("path"), "Shell: Add a folder to PATH…");
        assert_eq!(top("remove path"), "Shell: Remove a folder from PATH…");
        assert_eq!(top("set text size"), "View: Set the text size…");
        assert!(rank(&e, "qqqq").is_empty());
    }
}
