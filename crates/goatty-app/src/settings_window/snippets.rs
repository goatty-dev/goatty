//! The settings window's Snippets page (#89): the user's own snippets and
//! expansions on the connected host, and the ones plugins bring (folded,
//! read only, with "make my own copy").
//!
//! Snippets are host settings (`snippets` and `expansions` in `host.json`,
//! `shell.expansions`): edits go into the same draft as the other shell
//! pages and are applied with Preview/Apply. The plugins' snippets come
//! from the host's snippet list (`crate::snippets::Store`).
//!
//! The editor turns every `{name}` typed into the command line into an
//! input row (label, type, default, choices or suggestions), and shows the
//! finished line with its blanks as you type.

use std::collections::HashSet;

use goatty_config::HostSettings;
use goatty_plugin::commands::{InputChoice, InputKind};
use goatty_plugin::snippets::{self, Expansion, Owner, Piece, Safety, Snippet, SnippetInput, Source};
use gpui::{
    AnyElement, App, Context, Entity, Focusable, FontWeight, IntoElement, MouseButton, ParentElement, SharedString, Styled, Subscription,
    Window, div, prelude::*, px,
};

use super::kit::{self, card, chip, divider, field, modal, opt, search_field, section};
use super::{Page, SettingsWindow};
use crate::snippets::State;
use crate::ui::controls::{ButtonKind, Tone, badge, banner, button, button_with, icon_button, segmented, toggle};
use crate::ui::focus::{self, Ring, Scroller};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

/// Which snippets the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Mine,
    Plugins,
}

/// One `{name}` of the line being edited.
struct InputRow {
    id: String,
    /// Everything the form doesn't edit (help, pattern, a source's columns…).
    base: SnippetInput,
    label: Entity<TextInput>,
    kind: InputKind,
    default: Entity<TextInput>,
    /// Choices (comma-separated), a flag's text, or a suggestions command.
    extra: Entity<TextInput>,
    optional: bool,
}

struct SnippetForm {
    /// The id it replaces (`None`: a new one).
    editing: Option<String>,
    title: Entity<TextInput>,
    line: Entity<TextInput>,
    description: Entity<TextInput>,
    tags: Entity<TextInput>,
    safety: Option<Safety>,
    rows: Vec<InputRow>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

struct ExpansionForm {
    editing: Option<String>,
    say: Entity<TextInput>,
    to: Entity<TextInput>,
    description: Entity<TextInput>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

enum Form {
    Snippet(Box<SnippetForm>),
    Expansion(ExpansionForm),
}

/// The page's own state (the snippets themselves are host settings).
pub struct SnippetsPage {
    filter: Entity<TextInput>,
    /// "Try it": text run through the expansions.
    try_it: Entity<TextInput>,
    scope: Scope,
    /// Plugin groups the user unfolded.
    open: HashSet<String>,
    form: Option<Form>,
    pub(super) modal_focus: gpui::FocusHandle,
    opener: Option<gpui::FocusHandle>,
    pub(super) restore: Option<gpui::FocusHandle>,
    _subs: Vec<Subscription>,
}

impl SnippetsPage {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Search snippets", "", cx).bare());
        let try_it = cx.new(|cx| TextInput::new("ls -l | first 5 lines", "", cx).label("Try the expansions"));
        let subs = vec![
            cx.subscribe(&filter, |_, _, _: &TextInputEvent, cx| cx.notify()),
            cx.subscribe(&try_it, |_, _, _: &TextInputEvent, cx| cx.notify()),
        ];
        Self {
            filter,
            try_it,
            scope: Scope::All,
            open: HashSet::new(),
            form: None,
            modal_focus: cx.focus_handle(),
            opener: None,
            restore: None,
            _subs: subs,
        }
    }
}

/// A command line waiting to become a snippet ("Save the last command as
/// a snippet"): the Snippets page opens its editor with it.
#[derive(Default)]
pub struct Pending(pub Option<String>);

impl gpui::Global for Pending {}

/// Open Settings → Snippets with a new snippet for `line`.
pub fn new_from_line(line: String, cx: &mut App) {
    cx.set_global(Pending(Some(line)));
    super::open_at(cx, Some(Page::Snippets));
}

/// The command typed at a prompt line: without the prompt in front of it
/// (`~/src ❯ ls -l` → `ls -l`), when it ends in a usual prompt character.
pub fn strip_prompt(line: &str) -> String {
    let line = line.trim();
    let marks = ["❯ ", "➜ ", "λ ", "$ ", "% ", "# ", "> "];
    let first = marks.iter().filter_map(|m| line.find(m).map(|i| i + m.len())).filter(|&end| end <= 80).min();
    match first {
        Some(end) => line[end..].trim().to_string(),
        None => line.to_string(),
    }
}

/// `title` as an id, unique among `taken` (see `snippets::id_from`).
pub fn slug(title: &str, taken: &dyn Fn(&str) -> bool) -> String {
    snippets::id_from(title, taken)
}

/// The `{name}`s of `line` outside quotes, in order, without repeats;
/// `None` while the line doesn't parse (an open quote while typing).
fn placeholders(line: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for p in snippets::parse_line(line, |_| true).ok()? {
        if let Piece::Var { name, .. } = p
            && !out.contains(&name)
        {
            out.push(name);
        }
    }
    Some(out)
}

/// A placeholder name as a label: `max_depth` → "Max depth".
fn label_for(id: &str) -> String {
    let words = id.replace('_', " ");
    let mut c = words.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// What the user's snippets are, from the draft: (id, parsed or why not).
fn user_snippets(h: &HostSettings) -> Vec<(String, Result<Snippet, String>)> {
    h.snippets
        .iter()
        .map(|(id, v)| {
            let s = Snippet::from_user(id, v).and_then(|s| {
                let problems = snippets::check_snippet(&s, "", Owner::User);
                match problems.first() {
                    None => Ok(s),
                    Some(p) => Err(p.to_string().trim_start_matches('.').to_string()),
                }
            });
            (id.clone(), s)
        })
        .collect()
}

/// The user's expansions from the draft: (id, expansion, enabled).
fn user_expansions(h: &HostSettings) -> Vec<(String, Result<Expansion, String>, bool)> {
    h.expansions
        .iter()
        .map(|(id, v)| {
            let mut v = v.clone();
            let enabled = v.as_object_mut().and_then(|o| o.remove("enabled")).and_then(|e| e.as_bool()).unwrap_or(true);
            let e = serde_json::from_value::<Expansion>(v).map_err(|e| e.to_string()).and_then(|e| snippets::compile(&e).map(|_| e));
            (id.clone(), e, enabled)
        })
        .collect()
}

/// A problem path from `check_snippet`, in words for the form.
fn friendly(problem: &str, s: &Snippet) -> String {
    let mut p = problem.trim_start_matches('.').to_string();
    for (i, input) in s.inputs.iter().enumerate() {
        p = p.replace(&format!("input[{i}]"), &format!("{{{}}}", input.id));
    }
    p.replace("line: ", "Command: ").replace("title: ", "Title: ")
}

impl SettingsWindow {
    /// A line from "Save the last command as a snippet" opens the editor.
    pub(super) fn take_pending_snippet(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.page != Page::Snippets
            || self.host_value().is_none()
            || self.snippets.form.is_some()
            || self.create_theme_open
            || self.aliases_modal_open()
            || self.prompt_modal_open()
        {
            return;
        }
        let Some(line) = cx.try_global::<Pending>().and_then(|p| p.0.clone()) else { return };
        cx.set_global(Pending(None));
        let s = Snippet {
            id: String::new(),
            title: String::new(),
            description: String::new(),
            tags: Vec::new(),
            keywords: Vec::new(),
            safety: None,
            line,
            inputs: Vec::new(),
        };
        self.open_snippet_form(None, s, window, cx);
    }

    fn text_of(&self, e: &Entity<TextInput>, cx: &App) -> String {
        e.read(cx).text().trim().to_owned()
    }

    fn input(&self, cx: &mut Context<'_, Self>, placeholder: &str, label: &str, text: &str) -> Entity<TextInput> {
        let (p, l, t) = (placeholder.to_owned(), label.to_owned(), text.to_owned());
        cx.new(|cx| TextInput::new(p, t, cx).label(l))
    }

    /// Open the snippet editor with `s` (a copy of a plugin's, or the user's
    /// own when `editing`).
    pub(super) fn open_snippet_form(&mut self, editing: Option<String>, s: Snippet, window: &mut Window, cx: &mut Context<'_, Self>) {
        let title = self.input(cx, "Find big files", "Title", &s.title);
        let line = self.input(cx, "du -ah {dir} | sort -rh | head -n {count}", "Command", &s.line);
        let description = self.input(cx, "Optional: what happens when it runs", "Description", &s.description);
        let tags = self.input(cx, "files, disk", "Tags", &s.tags.join(", "));
        let mut subs = Vec::new();
        subs.push(cx.subscribe(&line, |v, _, ev: &TextInputEvent, cx| match ev {
            TextInputEvent::Changed(_) => v.sync_rows(cx),
            TextInputEvent::Submitted(_) => v.save_snippet(cx),
        }));
        for e in [&title, &description, &tags] {
            subs.push(cx.subscribe(e, |v, _, ev: &TextInputEvent, cx| match ev {
                TextInputEvent::Changed(_) => {
                    if let Some(Form::Snippet(f)) = &mut v.snippets.form {
                        f.error = None;
                    }
                    cx.notify();
                }
                TextInputEvent::Submitted(_) => v.save_snippet(cx),
            }));
        }
        let rows = s.inputs.iter().map(|i| self.input_row(i.clone(), cx)).collect();
        let first = if s.title.is_empty() { title.clone() } else { line.clone() };
        self.snippets.form = Some(Form::Snippet(Box::new(SnippetForm {
            editing,
            title,
            line,
            description,
            tags,
            safety: s.safety,
            rows,
            error: None,
            _subs: subs,
        })));
        self.snippets.opener = window.focused(cx);
        window.focus(&first.focus_handle(cx), cx);
        self.sync_rows(cx);
        cx.notify();
    }

    fn input_row(&self, base: SnippetInput, cx: &mut Context<'_, Self>) -> InputRow {
        let extra_text = match base.kind {
            InputKind::Choice => base.choices.iter().map(|c| c.value.clone()).collect::<Vec<_>>().join(", "),
            InputKind::Flag => base.on.clone().unwrap_or_default(),
            _ => base.source.as_ref().map(|s| s.sh.clone().unwrap_or_else(|| s.describe())).unwrap_or_default(),
        };
        let label = self.input(cx, &label_for(&base.id), "Question", &base.label);
        let default = self.input(cx, "Optional", "Starts with", base.default.as_deref().unwrap_or_default());
        let extra = self.input(cx, "", "More", &extra_text);
        InputRow { id: base.id.clone(), kind: base.kind, optional: base.optional, base, label, default, extra }
    }

    /// The line changed: one row per `{name}` in it, in order (rows of
    /// names still there keep what was entered).
    fn sync_rows(&mut self, cx: &mut Context<'_, Self>) {
        let Some(Form::Snippet(f)) = &self.snippets.form else { return };
        // Half-typed (an open quote): keep the rows until it parses again.
        let Some(names) = placeholders(f.line.read(cx).text()) else { return };
        let mut old: Vec<InputRow> = match &mut self.snippets.form {
            Some(Form::Snippet(f)) => std::mem::take(&mut f.rows),
            _ => return,
        };
        let mut rows = Vec::new();
        for n in names {
            match old.iter().position(|r| r.id == n) {
                Some(i) => rows.push(old.remove(i)),
                None => rows.push(self.input_row(SnippetInput::text(&n, &label_for(&n)), cx)),
            }
        }
        if let Some(Form::Snippet(f)) = &mut self.snippets.form {
            f.rows = rows;
            f.error = None;
        }
        cx.notify();
    }

    /// The snippet the form describes.
    fn form_snippet(&self, f: &SnippetForm, cx: &App) -> Snippet {
        let inputs = f
            .rows
            .iter()
            .map(|r| {
                let mut i = r.base.clone();
                i.id = r.id.clone();
                i.label = self.text_of(&r.label, cx);
                if i.label.is_empty() {
                    i.label = label_for(&r.id);
                }
                i.kind = r.kind;
                i.optional = r.optional && r.kind != InputKind::Flag;
                let d = self.text_of(&r.default, cx);
                i.default = (!d.is_empty()).then_some(d);
                let extra = self.text_of(&r.extra, cx);
                i.choices = Vec::new();
                i.on = None;
                match r.kind {
                    InputKind::Choice => {
                        i.choices = extra
                            .split(',')
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(|v| InputChoice { value: v.to_string(), label: None, help: String::new() })
                            .collect();
                        i.source = None;
                    }
                    InputKind::Flag => {
                        i.on = Some(extra);
                        i.source = None;
                        i.multiple = false;
                    }
                    _ if extra.is_empty() => {
                        i.source = None;
                        i.strict = false;
                        i.multiple = false;
                    }
                    _ => {
                        // A typed command runs in a shell (it's the user's own).
                        let base = r.base.source.clone().unwrap_or(Source {
                            run: Vec::new(),
                            sh: None,
                            split: None,
                            column: None,
                            detail: None,
                            label: None,
                            skip: 0,
                            cwd: Default::default(),
                            on_fail: None,
                            timeout: None,
                        });
                        let unchanged = r.base.source.as_ref().is_some_and(|s| s.sh.is_none() && s.describe() == extra);
                        i.source = Some(if unchanged { base } else { Source { run: Vec::new(), sh: Some(extra), ..base } });
                    }
                }
                if !matches!(r.kind, InputKind::Integer | InputKind::Number) {
                    i.min = None;
                    i.max = None;
                }
                if r.kind != InputKind::Text {
                    i.pattern = None;
                    i.pattern_help = None;
                }
                if !matches!(r.kind, InputKind::Path | InputKind::Folder) {
                    i.must_exist = false;
                }
                i
            })
            .collect();
        let tags =
            self.text_of(&f.tags, cx).split(',').map(|t| t.trim().to_lowercase().replace(' ', "-")).filter(|t| !t.is_empty()).collect();
        Snippet {
            id: String::new(),
            title: self.text_of(&f.title, cx),
            description: self.text_of(&f.description, cx),
            tags,
            keywords: Vec::new(),
            safety: f.safety,
            line: self.text_of(&f.line, cx),
            inputs,
        }
    }

    fn save_snippet(&mut self, cx: &mut Context<'_, Self>) {
        let Some(Form::Snippet(f)) = &self.snippets.form else { return };
        let Some(h) = self.host_value() else { return };
        let mut s = self.form_snippet(f, cx);
        let editing = f.editing.clone();
        s.id = match &editing {
            Some(id) => id.clone(),
            None => slug(&s.title, &|c| h.snippets.contains_key(c)),
        };
        if let Some(p) = snippets::check_snippet(&s, "", Owner::User).first() {
            let msg = friendly(&p.to_string(), &s);
            if let Some(Form::Snippet(f)) = &mut self.snippets.form {
                f.error = Some(msg);
            }
            cx.notify();
            return;
        }
        let value = s.to_user();
        let id = s.id.clone();
        self.edit_host(cx, move |h| {
            h.snippets.insert(id, value);
        });
        self.close_snippet_form(false, cx);
    }

    fn open_expansion_form(&mut self, editing: Option<String>, e: Option<Expansion>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let e = e.unwrap_or(Expansion { say: String::new(), to: String::new(), description: String::new(), inputs: Default::default() });
        let say = self.input(cx, "| first {n} lines", "You type", &e.say);
        let to = self.input(cx, "| head -n {n}", "It becomes", &e.to);
        let description = self.input(cx, "Optional", "Description", &e.description);
        let mut subs = Vec::new();
        for i in [&say, &to, &description] {
            subs.push(cx.subscribe(i, |v, _, ev: &TextInputEvent, cx| match ev {
                TextInputEvent::Changed(_) => {
                    if let Some(Form::Expansion(f)) = &mut v.snippets.form {
                        f.error = None;
                    }
                    cx.notify();
                }
                TextInputEvent::Submitted(_) => v.save_expansion(cx),
            }));
        }
        self.snippets.opener = window.focused(cx);
        window.focus(&say.focus_handle(cx), cx);
        self.snippets.form = Some(Form::Expansion(ExpansionForm { editing, say, to, description, error: None, _subs: subs }));
        cx.notify();
    }

    fn form_expansion(&self, f: &ExpansionForm, cx: &App) -> Expansion {
        let say = self.text_of(&f.say, cx);
        // Placeholders named like a number take digits.
        let inputs = placeholders_in_say(&say)
            .into_iter()
            .filter(|n| ["n", "count", "number", "lines", "num"].contains(&n.as_str()))
            .map(|n| (n, snippets::ExpansionInput { kind: snippets::ExpansionKind::Integer }))
            .collect();
        Expansion { say, to: self.text_of(&f.to, cx), description: self.text_of(&f.description, cx), inputs }
    }

    fn save_expansion(&mut self, cx: &mut Context<'_, Self>) {
        let Some(Form::Expansion(f)) = &self.snippets.form else { return };
        let Some(h) = self.host_value() else { return };
        let e = self.form_expansion(f, cx);
        let editing = f.editing.clone();
        if let Err(why) = snippets::compile(&e) {
            if let Some(Form::Expansion(f)) = &mut self.snippets.form {
                f.error = Some(why);
            }
            cx.notify();
            return;
        }
        let id = editing.clone().unwrap_or_else(|| slug(e.say.trim_start_matches(['|', ' ']), &|c| h.expansions.contains_key(c)));
        let enabled = editing.as_ref().and_then(|id| h.expansions.get(id)).and_then(|v| v.get("enabled")).and_then(|v| v.as_bool());
        let mut value = serde_json::to_value(&e).unwrap_or_default();
        if let (Some(false), Some(o)) = (enabled, value.as_object_mut()) {
            o.insert("enabled".into(), false.into());
        }
        self.edit_host(cx, move |h| {
            h.expansions.insert(id, value);
        });
        self.close_snippet_form(false, cx);
    }

    pub(super) fn close_snippet_form(&mut self, to_opener: bool, cx: &mut Context<'_, Self>) {
        self.snippets.form = None;
        let opener = self.snippets.opener.take().filter(|_| to_opener);
        self.snippets.restore = Some(opener.unwrap_or_else(|| self.focus.clone()));
        cx.notify();
    }

    pub(super) fn snippets_modal_open(&self) -> bool {
        self.snippets.form.is_some()
    }

    fn set_expansion_enabled(&mut self, id: String, on: bool, cx: &mut Context<'_, Self>) {
        self.edit_host(cx, move |h| {
            if let Some(o) = h.expansions.get_mut(&id).and_then(|v| v.as_object_mut()) {
                if on {
                    o.remove("enabled");
                } else {
                    o.insert("enabled".into(), false.into());
                }
            }
        });
    }

    /// The snippets the host's plugins bring (from the snippet list).
    fn plugin_snippets(&self, cx: &App) -> Option<std::rc::Rc<goatty_plugin::api::SnippetList>> {
        let store = cx.try_global::<crate::snippets::Store>()?;
        let same = match (&store.link, &self.link) {
            (Some(a), Some(b)) => a.same(b),
            _ => false,
        };
        match &store.state {
            State::Ready(list) if same => Some(list.clone()),
            _ => None,
        }
    }

    // ---- Rendering ----------------------------------------------------------

    pub(super) fn snippets_page(&self, ui: Ui, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        let (notes, h) = self.host_status(ui);
        let mut page = div().flex().flex_col().gap(px(20.)).children(notes);
        let Some(h) = h else { return page.into_any_element() };
        let mono = kit::mono(cx);
        let query = self.snippets.filter.read(cx).text().trim().to_lowercase();
        let mine = user_snippets(&h);
        let list = self.plugin_snippets(cx);
        let theirs: Vec<&goatty_plugin::api::HostSnippet> =
            list.iter().flat_map(|l| l.snippets.iter()).filter(|s| !s.origin.mine()).collect();
        let scope = self.snippets.scope;
        let set_scope = |s: Scope| {
            self.act(cx, move |v, cx| {
                v.snippets.scope = s;
                cx.notify();
            })
        };
        let new_snippet = button_with(
            "snippet-new",
            Some(Icon::Plus),
            "New snippet",
            ButtonKind::Primary,
            ui,
            self.act_window(cx, |v, window, cx| {
                let blank = Snippet {
                    id: String::new(),
                    title: String::new(),
                    description: String::new(),
                    tags: Vec::new(),
                    keywords: Vec::new(),
                    safety: None,
                    line: String::new(),
                    inputs: Vec::new(),
                };
                v.open_snippet_form(None, blank, window, cx)
            }),
        )
        .h(px(38.));
        page = page.child(
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().flex_1().child(search_field(ui, "snippet-search", &self.snippets.filter, window, cx)))
                        .child(new_snippet),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(chip(ui, "snippet-all", "All", Some(mine.len() + theirs.len()), scope == Scope::All, set_scope(Scope::All)))
                        .child(chip(ui, "snippet-mine", "Mine", Some(mine.len()), scope == Scope::Mine, set_scope(Scope::Mine)))
                        .child(chip(
                            ui,
                            "snippet-plugins",
                            "From plugins",
                            Some(theirs.len()),
                            scope == Scope::Plugins,
                            set_scope(Scope::Plugins),
                        )),
                ),
        );
        let matches = |s: &Snippet| query.is_empty() || s.haystack().to_lowercase().contains(&query);
        let mut shown = 0;

        if scope != Scope::Plugins {
            let mut s = section(ui, "My snippets").plain();
            let rows: Vec<&(String, Result<Snippet, String>)> =
                mine.iter().filter(|(id, r)| query.is_empty() || id.contains(&query) || r.as_ref().is_ok_and(matches)).collect();
            if mine.is_empty() && query.is_empty() {
                s = s.child(card(ui).pb(px(20.)).child(kit::empty(
                    ui,
                    Icon::Code,
                    "No snippets of your own yet",
                    "A snippet is a command with blanks: write {name} where a value goes, and goatty asks for it. Ctrl+Shift+S finds and fills them in.",
                )));
            } else if !rows.is_empty() {
                let mut c = card(ui);
                for (k, (id, r)) in rows.iter().enumerate() {
                    if k > 0 {
                        c = c.child(divider(ui));
                    }
                    c = c.child(self.snippet_row(ui, &mono, id, r.as_ref().ok(), r.as_ref().err(), None, cx));
                }
                shown += rows.len();
                s = s.child(c);
            }
            page = page.child(s);
        }

        if scope != Scope::Mine {
            let mut s = section(ui, "From plugins").desc("Make your own copy to change one").plain();
            if list.is_none() {
                s = s.child(kit::hint(ui, "Loading the plugins' snippets…").px(px(4.)));
            }
            let mut groups: Vec<(String, String, Vec<&goatty_plugin::api::HostSnippet>)> = Vec::new();
            for hs in theirs.iter().filter(|hs| matches(&hs.snippet)) {
                match groups.iter_mut().find(|g| g.0 == hs.origin.owner) {
                    Some(g) => g.2.push(hs),
                    None => groups.push((hs.origin.owner.clone(), hs.origin.name.clone(), vec![hs])),
                }
            }
            for (owner, name, rows) in &groups {
                shown += rows.len();
                s = s.child(self.plugin_group(ui, &mono, owner, name, rows, !query.is_empty(), cx));
            }
            if groups.is_empty() && list.is_some() && query.is_empty() {
                s = s.child(
                    card(ui).child(
                        kit::row(ui, "Plugins can bring snippets too")
                            .icon(Icon::Puzzle)
                            .desc("Basics is on by default; git, docker and others add their own")
                            .on_click("snippet-browse", self.listener_go(cx, Page::Plugins)),
                    ),
                );
            }
            page = page.child(s);
        }
        if shown == 0 && !query.is_empty() {
            page = page.child(card(ui).child(kit::empty(
                ui,
                Icon::Search,
                "No snippet matches",
                "Search looks at titles, descriptions, tags and the commands.",
            )));
        }
        page = page.child(self.expansions_section(ui, &h, &mono, list.as_deref(), window, cx));
        page.child(kit::hint(ui, "Ctrl+Shift+S in a terminal finds a snippet and fills it in.").px(px(4.))).into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn snippet_row(
        &self,
        ui: Ui,
        mono: &SharedString,
        id: &str,
        s: Option<&Snippet>,
        error: Option<&String>,
        plugin: Option<&goatty_plugin::api::HostSnippet>,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let gid = SharedString::from(format!("snippet-row-{}-{id}", plugin.map_or("", |p| p.origin.owner.as_str())));
        let eid = |what: &str| SharedString::from(format!("snippet-{what}-{}-{id}", plugin.map_or("", |p| p.origin.owner.as_str())));
        let title = s.map_or_else(|| id.to_string(), |s| s.title.clone());
        let line = s.map(|s| s.line.clone()).unwrap_or_default();
        let actions_id: gpui::ElementId = eid("actions").into();
        let actions = match plugin {
            None => {
                let (edit_id, del_id) = (id.to_string(), id.to_string());
                focus::show_when_focused(div().flex().gap(px(2.)).opacity(0.).group_hover(gid.clone(), |s| s.opacity(1.)), &actions_id)
                    .child(icon_button(
                        eid("edit"),
                        Icon::Pencil,
                        format!("Edit {title}"),
                        ui,
                        self.act_window(cx, move |v, window, cx| {
                            let Some(s) =
                                v.host_value().and_then(|h| h.snippets.get(&edit_id)).and_then(|j| Snippet::from_user(&edit_id, j).ok())
                            else {
                                return;
                            };
                            v.open_snippet_form(Some(edit_id.clone()), s, window, cx);
                        }),
                    ))
                    .child(icon_button(
                        eid("delete"),
                        Icon::Trash,
                        format!("Delete {title}"),
                        ui,
                        self.act_window(cx, move |v, window, cx| {
                            focus::hand_over(&v.focus, focus::Handover::Control("snippet-new".into()), window, cx);
                            let id = del_id.clone();
                            v.edit_host(cx, move |h| {
                                h.snippets.remove(&id);
                            });
                        }),
                    ))
            }
            Some(hs) => {
                let copy = hs.snippet.clone();
                focus::show_when_focused(div().flex().gap(px(2.)).opacity(0.).group_hover(gid.clone(), |s| s.opacity(1.)), &actions_id)
                    .child(icon_button(
                        eid("copy"),
                        Icon::Copy,
                        format!("Make my own copy of {title}"),
                        ui,
                        self.act_window(cx, move |v, window, cx| v.open_snippet_form(None, copy.clone(), window, cx)),
                    ))
            }
        };
        let missing = plugin.map(|p| p.missing.clone()).unwrap_or_default();
        div()
            .id(gid.clone())
            .group(gid.clone())
            .flex()
            .items_center()
            .gap(px(14.))
            .min_h(px(54.))
            .px(px(16.))
            .py(px(8.))
            .hover(|s| s.bg(ui.surface_hover))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div().text_size(px(text::BODY)).font_weight(FontWeight::MEDIUM).truncate().child(SharedString::from(title)),
                            )
                            .children(
                                s.map(|s| s.tags.iter().take(3).map(|t| badge(t.clone(), Tone::Neutral, ui)).collect::<Vec<_>>())
                                    .unwrap_or_default(),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(text::LABEL))
                            .font_family(mono.clone())
                            .text_color(ui.muted)
                            .truncate()
                            .child(SharedString::from(line)),
                    )
                    .when_some(error, |d, e| {
                        d.child(div().text_size(px(text::SMALL)).text_color(ui.danger).child(SharedString::from(format!("Left out: {e}"))))
                    })
                    .when(!missing.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(text::SMALL))
                                .text_color(ui.warning)
                                .child(SharedString::from(format!("Needs {}", missing.join(", ")))),
                        )
                    }),
            )
            .children(s.map(|s| {
                let safety = s.safety();
                let tone = match safety {
                    Safety::Safe => Tone::Success,
                    Safety::Changes => Tone::Warning,
                    Safety::Destructive => Tone::Danger,
                };
                badge(safety.label(), tone, ui)
            }))
            .child(actions)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn plugin_group(
        &self,
        ui: Ui,
        mono: &SharedString,
        owner: &str,
        name: &str,
        rows: &[&goatty_plugin::api::HostSnippet],
        filtering: bool,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let open = filtering || self.snippets.open.contains(owner);
        let header_id: gpui::ElementId = SharedString::from(format!("snippet-fold-{owner}")).into();
        let key = owner.to_string();
        let header = div()
            .id(header_id.clone())
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(name.to_string()))
            .aria_expanded(open)
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(52.))
            .px(px(16.))
            .cursor_pointer()
            .hover(|s| s.bg(ui.surface_hover))
            .rounded_t(px(radius::CARD))
            .when(!open, |d| d.rounded_b(px(radius::CARD)))
            .child((if open { Icon::ChevronDown } else { Icon::ChevronRight }).svg().size(px(15.)).text_color(ui.muted))
            .child(Icon::Puzzle.svg().size(px(15.)).text_color(ui.muted))
            .child(div().text_size(px(text::BODY)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(name.to_string())))
            .child(div().text_size(px(text::SMALL)).text_color(ui.faint).child(SharedString::from(rows.len().to_string())))
            .on_click(cx.listener(move |v, _, _, cx| {
                if !v.snippets.open.remove(&key) {
                    v.snippets.open.insert(key.clone());
                }
                cx.notify();
            }));
        let header = focus::tab(header, &header_id, ui, Ring::Inner, Scroller::Page);
        let mut c = card(ui).child(header);
        if open {
            c = c.child(div().h(px(1.)).bg(ui.divider));
            for (i, hs) in rows.iter().enumerate() {
                if i > 0 {
                    c = c.child(divider(ui));
                }
                c = c.child(self.snippet_row(ui, mono, &hs.snippet.id, Some(&hs.snippet), None, Some(hs), cx));
            }
        }
        c.into_any_element()
    }

    fn expansions_section(
        &self,
        ui: Ui,
        h: &HostSettings,
        mono: &SharedString,
        list: Option<&goatty_plugin::api::SnippetList>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let on = h.shell.expansions;
        let mine = user_expansions(h);
        let theirs: Vec<&goatty_plugin::api::HostExpansion> =
            list.iter().flat_map(|l| l.expansions.iter()).filter(|e| !e.origin.mine()).collect();
        let new = button_with(
            "expansion-new",
            Some(Icon::Plus),
            "New expansion",
            ButtonKind::Normal,
            ui,
            self.act_window(cx, |v, window, cx| v.open_expansion_form(None, None, window, cx)),
        );
        let mut top = card(ui).child(
            kit::row(ui, "Expand plain words while typing")
                .desc("Space turns | first 20 lines into | head -n 20; Backspace undoes it, Enter shows it before running")
                .control(
                    toggle("expansions-on", on, ui, self.host(cx, move |h| h.shell.expansions = !on))
                        .aria_label("Expand plain words while typing"),
                ),
        );
        if on {
            // Try it: the same rules, applied in Rust.
            let rules: Vec<snippets::Compiled> = mine
                .iter()
                .filter(|(_, _, en)| *en)
                .filter_map(|(_, e, _)| e.as_ref().ok().and_then(|e| snippets::compile(e).ok()))
                .chain(theirs.iter().filter_map(|e| snippets::compile(&e.expansion).ok()))
                .collect();
            let typed = self.snippets.try_it.read(cx).text().to_owned();
            let result = expand_all(&rules, &typed);
            top = top.child(divider(ui)).child(
                kit::row(ui, "Try it").desc("Type as you would at the prompt").below(
                    div().flex().flex_col().gap(px(6.)).child(div().font_family(mono.clone()).child(self.snippets.try_it.clone())).child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .px(px(4.))
                            .font_family(mono.clone())
                            .text_size(px(text::LABEL))
                            .child(div().text_color(ui.faint).child("→"))
                            .child(div().text_color(if result.is_some() { ui.accent_text } else { ui.faint }).child(SharedString::from(
                                result.unwrap_or_else(|| if typed.is_empty() { "…".into() } else { "(nothing to expand)".into() }),
                            ))),
                    ),
                ),
            );
        }
        let mut s = section(ui, "Expansions").action(new).child(top);
        let _ = window;
        if !mine.is_empty() {
            let mut c = card(ui);
            for (k, (id, e, enabled)) in mine.iter().enumerate() {
                if k > 0 {
                    c = c.child(divider(ui));
                }
                c = c.child(self.expansion_row(ui, mono, Some(id), e.as_ref().ok(), e.as_ref().err(), *enabled, "Yours", cx));
            }
            s = s.child(c);
        }
        if !theirs.is_empty() {
            let mut c = card(ui);
            for (k, e) in theirs.iter().enumerate() {
                if k > 0 {
                    c = c.child(divider(ui));
                }
                c = c.child(self.expansion_row(ui, mono, None, Some(&e.expansion), None, e.enabled, &e.origin.name, cx));
            }
            s = s.child(c);
        }
        s.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn expansion_row(
        &self,
        ui: Ui,
        mono: &SharedString,
        id: Option<&String>,
        e: Option<&Expansion>,
        error: Option<&String>,
        enabled: bool,
        from: &str,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let key = id.cloned().unwrap_or_else(|| e.map(|e| e.say.clone()).unwrap_or_default());
        let gid = SharedString::from(format!("expansion-row-{key}"));
        let eid = |w: &str| SharedString::from(format!("expansion-{w}-{key}"));
        let say = e.map(|e| e.say.clone()).unwrap_or_else(|| key.clone());
        let to = e.map(|e| e.to.clone()).unwrap_or_default();
        let desc = e.map(|e| e.description.clone()).unwrap_or_default();
        let actions_id: gpui::ElementId = eid("actions").into();
        let mut row = div()
            .id(gid.clone())
            .group(gid.clone())
            .flex()
            .items_center()
            .gap(px(14.))
            .min_h(px(48.))
            .px(px(16.))
            .py(px(6.))
            .hover(|s| s.bg(ui.surface_hover));
        if let Some(id) = id {
            let id2 = id.clone();
            row = row.child(
                toggle(eid("on"), enabled, ui, self.act(cx, move |v, cx| v.set_expansion_enabled(id2.clone(), !enabled, cx)))
                    .aria_label(SharedString::from(format!("Expansion {say}"))),
            );
        }
        row = row.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .font_family(mono.clone())
                        .text_size(px(text::LABEL))
                        .child(div().text_color(ui.text).font_weight(FontWeight::MEDIUM).child(SharedString::from(say.clone())))
                        .child(div().text_color(ui.faint).child("→"))
                        .child(div().text_color(ui.accent_text).truncate().child(SharedString::from(to))),
                )
                .when(!desc.is_empty(), |d| d.child(div().text_size(px(text::SMALL)).text_color(ui.muted).child(SharedString::from(desc))))
                .when_some(error, |d, e| {
                    d.child(div().text_size(px(text::SMALL)).text_color(ui.danger).child(SharedString::from(format!("Left out: {e}"))))
                }),
        );
        match id {
            Some(id) => {
                let (edit_id, del_id, edit_e) = (id.clone(), id.clone(), e.cloned());
                row = row.child(
                    focus::show_when_focused(div().flex().gap(px(2.)).opacity(0.).group_hover(gid.clone(), |s| s.opacity(1.)), &actions_id)
                        .child(icon_button(
                            eid("edit"),
                            Icon::Pencil,
                            format!("Edit {say}"),
                            ui,
                            self.act_window(cx, move |v, window, cx| {
                                v.open_expansion_form(Some(edit_id.clone()), edit_e.clone(), window, cx)
                            }),
                        ))
                        .child(icon_button(
                            eid("delete"),
                            Icon::Trash,
                            format!("Delete {say}"),
                            ui,
                            self.act_window(cx, move |v, window, cx| {
                                focus::hand_over(&v.focus, focus::Handover::Control("expansion-new".into()), window, cx);
                                let id = del_id.clone();
                                v.edit_host(cx, move |h| {
                                    h.expansions.remove(&id);
                                });
                            }),
                        )),
                );
            }
            None => {
                row = row.child(div().text_size(px(text::CAPTION)).text_color(ui.faint).child(SharedString::from(from.to_string())));
                if let Some(e) = e.cloned() {
                    row = row.child(
                        focus::show_when_focused(div().opacity(0.).group_hover(gid.clone(), |s| s.opacity(1.)), &actions_id).child(
                            icon_button(
                                eid("copy"),
                                Icon::Copy,
                                format!("Make my own version of {say}"),
                                ui,
                                self.act_window(cx, move |v, window, cx| v.open_expansion_form(None, Some(e.clone()), window, cx)),
                            ),
                        ),
                    );
                }
            }
        }
        row.into_any_element()
    }

    /// The editor, while it's open.
    pub(super) fn snippet_modal(&self, ui: Ui, cx: &Context<'_, Self>) -> Option<AnyElement> {
        let form = self.snippets.form.as_ref()?;
        let mono = kit::mono(cx);
        let (id, title, body, footer) = match form {
            Form::Snippet(f) => {
                let s = self.form_snippet(f, cx);
                let segs = s.preview(&Default::default(), None);
                let guess = snippets::guess_safety(&s.line);
                let safety_opts = [
                    (None, "Guess"),
                    (Some(Safety::Safe), "Only reads"),
                    (Some(Safety::Changes), "Makes changes"),
                    (Some(Safety::Destructive), "Deletes data"),
                ];
                let this = cx.entity().downgrade();
                let safety = segmented("snippet-safety", &safety_opts, &f.safety, ui, move |v, _, cx| {
                    let _ = this.update(cx, |view, cx| {
                        if let Some(Form::Snippet(f)) = &mut view.snippets.form {
                            f.safety = v;
                        }
                        cx.notify();
                    });
                });
                let rows: Vec<AnyElement> = f.rows.iter().enumerate().map(|(i, r)| self.input_row_view(ui, &mono, i, r, cx)).collect();
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(field(ui, "Title", f.title.clone()))
                    .child(field(ui, "Command", div().font_family(mono.clone()).child(f.line.clone())))
                    .child(kit::hint(ui, "Write {name} where a value goes: goatty asks for it and quotes it. {{ is a literal brace."))
                    .child(div().px(px(2.)).child(crate::command_palette::preview_line(&segs, ui, mono.clone())))
                    .children(rows)
                    .child(
                        div()
                            .flex()
                            .gap(px(12.))
                            .child(field(ui, "Description", f.description.clone()).flex_1())
                            .child(field(ui, "Tags", f.tags.clone()).w(px(170.))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child(SharedString::from(format!(
                                "What running it does (goatty's guess: {})",
                                guess.label().to_lowercase()
                            ))))
                            .child(safety),
                    )
                    .children(f.error.clone().map(|e| banner(ui, Tone::Danger, e, Vec::new())));
                let title = if f.editing.is_some() { "Edit snippet" } else { "New snippet" };
                let footer = div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(kit::hint(ui, "Applied with the other shell changes").mr(px(8.)))
                    .child(button("snippet-cancel", "Cancel", ButtonKind::Ghost, ui, self.act(cx, |v, cx| v.close_snippet_form(true, cx))))
                    .child(button(
                        "snippet-save",
                        if f.editing.is_some() { "Save" } else { "Add snippet" },
                        ButtonKind::Primary,
                        ui,
                        self.act(cx, |v, cx| v.save_snippet(cx)),
                    ));
                ("snippet-modal", title, body.into_any_element(), footer)
            }
            Form::Expansion(f) => {
                let e = self.form_expansion(f, cx);
                let example = e.say.replace("{n}", "20").replace("{count}", "20");
                let example = placeholders_in_say(&example).iter().fold(example.clone(), |acc, n| acc.replace(&format!("{{{n}}}"), "word"));
                let shown = snippets::compile(&e).ok().and_then(|c| c.apply(&format!("cat log.txt {example}")));
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(field(ui, "You type", div().font_family(mono.clone()).child(f.say.clone())))
                    .child(field(ui, "It becomes", div().font_family(mono.clone()).child(f.to.clone())))
                    .child(kit::hint(
                        ui,
                        "At least two words; {name} stands for one word you type, used again in what it becomes ({n} or {count}: a number).",
                    ))
                    .child(field(ui, "Description", f.description.clone()))
                    .when_some(shown, |d, s| {
                        d.child(
                            div()
                                .p(px(12.))
                                .rounded(px(radius::CONTROL))
                                .bg(ui.bg)
                                .border_1()
                                .border_color(ui.divider)
                                .font_family(mono.clone())
                                .text_size(px(text::LABEL))
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .child(div().text_color(ui.muted).child(SharedString::from(format!("cat log.txt {example}"))))
                                .child(div().text_color(ui.accent_text).child(SharedString::from(format!("→ {s}")))),
                        )
                    })
                    .children(f.error.clone().map(|e| banner(ui, Tone::Danger, e, Vec::new())));
                let title = if f.editing.is_some() { "Edit expansion" } else { "New expansion" };
                let footer = div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(kit::hint(ui, "Applied with the other shell changes").mr(px(8.)))
                    .child(button(
                        "expansion-cancel",
                        "Cancel",
                        ButtonKind::Ghost,
                        ui,
                        self.act(cx, |v, cx| v.close_snippet_form(true, cx)),
                    ))
                    .child(button(
                        "expansion-save",
                        if f.editing.is_some() { "Save" } else { "Add expansion" },
                        ButtonKind::Primary,
                        ui,
                        self.act(cx, |v, cx| v.save_expansion(cx)),
                    ));
                ("expansion-modal", title, body.into_any_element(), footer)
            }
        };
        Some(
            modal(ui, id, title, body, footer)
                .track_focus(&self.snippets.modal_focus)
                .on_mouse_down(MouseButton::Left, cx.listener(|v, _, _, cx| v.close_snippet_form(true, cx)))
                .into_any_element(),
        )
    }

    fn input_row_view(&self, ui: Ui, mono: &SharedString, i: usize, r: &InputRow, cx: &Context<'_, Self>) -> AnyElement {
        let kinds = vec![
            opt(InputKind::Text, "Text"),
            opt(InputKind::Path, "File or folder"),
            opt(InputKind::Folder, "Folder"),
            opt(InputKind::Integer, "Whole number"),
            opt(InputKind::Number, "Number"),
            opt(InputKind::Choice, "One of a list"),
            opt(InputKind::Flag, "Yes or no"),
        ];
        let this = cx.entity().downgrade();
        let kind = self.select(&format!("snippet-kind-{i}"), "Type", kinds, &r.kind, cx, move |k, _, cx| {
            let _ = this.update(cx, |v, cx| {
                if let Some(Form::Snippet(f)) = &mut v.snippets.form
                    && let Some(row) = f.rows.get_mut(i)
                {
                    row.kind = k;
                }
                cx.notify();
            });
        });
        let (extra_label, extra_hint) = match r.kind {
            InputKind::Choice => (Some("Choices"), "Separated by commas: small, medium, large"),
            InputKind::Flag => (Some("“Yes” adds"), "The text it inserts, like -r"),
            InputKind::Integer | InputKind::Number => (None, ""),
            _ => (Some("Suggestions from"), "Optional: a command whose output lines are the suggestions"),
        };
        let optional = r.optional;
        let this = cx.entity().downgrade();
        let opt_toggle = toggle(SharedString::from(format!("snippet-opt-{i}")), optional, ui, move |_, cx| {
            let _ = this.update(cx, |v, cx| {
                if let Some(Form::Snippet(f)) = &mut v.snippets.form
                    && let Some(row) = f.rows.get_mut(i)
                {
                    row.optional = !optional;
                }
                cx.notify();
            });
        })
        .aria_label(SharedString::from(format!("{{{}}} may be left empty", r.id)));
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(12.))
            .rounded(px(radius::CONTROL))
            .border_1()
            .border_color(ui.divider)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .px(px(6.))
                            .rounded(px(radius::SMALL))
                            .bg(ui.accent_soft)
                            .text_color(ui.accent_text)
                            .font_family(mono.clone())
                            .text_size(px(text::LABEL))
                            .child(SharedString::from(format!("{{{}}}", r.id))),
                    )
                    .child(div().flex_1().child(r.label.clone()))
                    .child(kind),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(10.))
                    .when(r.kind != InputKind::Flag, |d| d.child(field(ui, "Starts with", r.default.clone()).w(px(130.))))
                    .when_some(extra_label, |d, l| {
                        d.child(field(ui, l, div().font_family(mono.clone()).child(r.extra.clone())).flex_1().min_w(px(120.)))
                    })
                    .when(r.kind != InputKind::Flag, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .pb(px(8.))
                                .text_size(px(text::SMALL))
                                .text_color(ui.muted)
                                .child(opt_toggle)
                                .child("Optional"),
                        )
                    }),
            )
            .when(!extra_hint.is_empty(), |d| d.child(kit::hint(ui, extra_hint)))
            .into_any_element()
    }
}

/// The placeholders of an expansion's `say`.
fn placeholders_in_say(say: &str) -> Vec<String> {
    say.split_whitespace().filter_map(|w| w.strip_prefix('{').and_then(|w| w.strip_suffix('}'))).map(str::to_string).collect()
}

/// `typed` with the first rule that matches its end applied (what Space
/// at the end of it would do), or `None`.
fn expand_all(rules: &[snippets::Compiled], typed: &str) -> Option<String> {
    let t = typed.trim_end();
    rules.iter().find_map(|r| r.apply(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_come_off_command_lines() {
        assert_eq!(strip_prompt("~/src/goatty ❯ ls -l | wc"), "ls -l | wc");
        assert_eq!(strip_prompt("me@box:~$ du -sh *"), "du -sh *");
        assert_eq!(strip_prompt("git status"), "git status");
    }

    #[test]
    fn ids_from_titles() {
        let none = |_: &str| false;
        assert_eq!(slug("Find big files!", &none), "find-big-files");
        assert_eq!(slug("  ", &none), "my");
        assert_eq!(slug("7zip it", &none), "my-7zip-it");
        let taken = |s: &str| s == "deploy" || s == "deploy-2";
        assert_eq!(slug("Deploy", &taken), "deploy-3");
    }

    #[test]
    fn rows_follow_the_line() {
        assert_eq!(placeholders("tar -czf {archive} {dir} '{x}' {archive}"), None, "a quoted one is an error");
        assert_eq!(placeholders("tar -czf {archive} {dir} 'open"), None, "half-typed");
        assert_eq!(placeholders("tar -czf {archive} {dir} {archive}").unwrap(), ["archive", "dir"]);
        assert_eq!(label_for("max_depth"), "Max depth");
    }

    #[test]
    fn try_it_applies_the_first_matching_rule() {
        let e = Expansion {
            say: "| first {n} lines".into(),
            to: "| head -n {n}".into(),
            description: String::new(),
            inputs: Default::default(),
        };
        let rules = vec![snippets::compile(&e).unwrap()];
        assert_eq!(expand_all(&rules, "ls | first 3 lines ").as_deref(), Some("ls | head -n 3"));
        assert_eq!(expand_all(&rules, "ls"), None);
    }
}
