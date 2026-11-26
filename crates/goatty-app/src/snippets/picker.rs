//! The snippet picker (Ctrl+Shift+S): search the active host's snippets by
//! what they do, narrow them down by topic, see the command before you
//! pick it. Enter starts filling it in (`super::ask_for`); a snippet
//! without blanks goes straight onto the prompt.
//!
//! Ranking: the palette's fuzzy matcher over the title (highlighted), with
//! the description, tags, keywords and the command itself as keywords.
//! Recently used ones come first when nothing is typed; snippets whose
//! program the host doesn't have go last, dimmed ("needs docker").

use std::path::PathBuf;
use std::rc::Rc;

use goatty_config::CommandHistory;
use goatty_plugin::api::{HostSnippet, SnippetList};
use goatty_plugin::snippets::Safety;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, KeyBinding, ScrollStrategy, SharedString, StyledText,
    Subscription, UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};

use super::{State, Store};
use crate::command_palette::{Entry, fuzzy, rank};
use crate::settings::AppSettings;
use crate::settings_window::HostLink;
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

pub const CONTEXT: &str = "SnippetPicker";

const WIDTH: f32 = 720.;
const ROW_HEIGHT: f32 = 48.;
const MAX_VISIBLE_ROWS: usize = 8;
/// Topic chips shown (the most used tags).
const MAX_TAGS: usize = 7;

actions!(snippet_picker, [Next, Previous, PageDown, PageUp, NextFilter, PreviousFilter, Close, First, Last]);

/// Keys inside the picker. `toggle_keys` (the shortcut that opens it) close it.
pub fn bindings(toggle_keys: &[String], cx: &App) -> Vec<KeyBinding> {
    let c = Some(CONTEXT);
    let mut v = vec![
        KeyBinding::new("down", Next, c),
        KeyBinding::new("up", Previous, c),
        KeyBinding::new("ctrl-n", Next, c),
        KeyBinding::new("ctrl-p", Previous, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("ctrl-home", First, c),
        KeyBinding::new("ctrl-end", Last, c),
        KeyBinding::new("tab", NextFilter, c),
        KeyBinding::new("shift-tab", PreviousFilter, c),
        KeyBinding::new("escape", Close, c),
    ];
    for k in toggle_keys {
        match KeyBinding::load(k, Box::new(Close), gpui::KeyBindingContextPredicate::parse(CONTEXT).ok().map(Rc::new), false, None, cx) {
            Ok(b) => v.push(b),
            Err(e) => log::debug!("snippet picker: can't bind {k} to close: {e}"),
        }
    }
    v
}

/// What the list shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    /// The user's own.
    Mine,
    Tag(String),
}

/// Where the picker was, to come back to it from the fill-in steps.
#[derive(Debug, Clone, Default)]
pub struct Memory {
    pub query: String,
    pub filter: Filter,
    /// The chosen snippet's history id (selected again).
    pub selected: Option<String>,
}

pub enum PickerEvent {
    Dismissed,
    /// Fill in this snippet (the picker's state to come back to).
    Fill(Box<HostSnippet>, Memory),
    /// Open Settings → Snippets.
    Manage,
}

pub struct SnippetPicker {
    link: HostLink,
    input: gpui::Entity<TextInput>,
    query: String,
    filter: Filter,
    list: Option<Rc<SnippetList>>,
    /// Snippets that pass the filter, in list order.
    shown: Vec<usize>,
    /// Ranked: (index into the list's snippets, matched title positions).
    hits: Vec<(usize, Vec<usize>)>,
    selected: usize,
    scroll: UniformListScrollHandle,
    history: CommandHistory,
    history_path: Option<PathBuf>,
    /// The topics to filter by, most used first: (tag, count).
    tags: Vec<(String, usize)>,
    want_selected: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for SnippetPicker {}

impl Focusable for SnippetPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl SnippetPicker {
    pub fn new(link: HostLink, memory: Memory, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let history_path = goatty_config::command_history::snippet_history_file();
        let history = history_path.as_deref().map(CommandHistory::load).unwrap_or_default();
        let input = cx.new(|cx| {
            let mut t = TextInput::new("What do you want to do? (find big files, extract, kill…)", memory.query.clone(), cx);
            t.set_label("Search snippets");
            t
        });
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &TextInputEvent, _, cx| match ev {
            TextInputEvent::Changed(q) => {
                this.query = q.clone();
                this.rank(cx);
            }
            TextInputEvent::Submitted(_) => this.choose(this.selected, cx),
        });
        let store = cx.observe_global::<Store>(|this, cx| this.reload(cx));
        input.focus_handle(cx).focus(window, cx);
        super::ensure(&link, cx);
        let mut this = Self {
            link,
            input,
            query: memory.query,
            filter: memory.filter,
            list: None,
            shown: Vec::new(),
            hits: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            history,
            history_path,
            tags: Vec::new(),
            want_selected: memory.selected,
            _subscriptions: vec![sub, store],
        };
        this.reload(cx);
        this
    }

    fn state(&self, cx: &App) -> State {
        let store = cx.global::<Store>();
        if store.link.as_ref().is_some_and(|l| l.same(&self.link)) { store.state.clone() } else { State::Loading }
    }

    /// The store changed: take its list.
    fn reload(&mut self, cx: &mut Context<'_, Self>) {
        self.list = match self.state(cx) {
            State::Ready(list) => Some(list),
            _ => None,
        };
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for s in self.list.iter().flat_map(|l| &l.snippets) {
            for t in &s.snippet.tags {
                *counts.entry(t.to_lowercase()).or_default() += 1;
            }
        }
        let mut tags: Vec<(String, usize)> = counts.into_iter().collect();
        tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        tags.truncate(MAX_TAGS);
        self.tags = tags;
        self.rank(cx);
    }

    fn filters(&self) -> Vec<Filter> {
        let mut v = vec![Filter::All];
        if self.list.iter().flat_map(|l| &l.snippets).any(|s| s.origin.mine()) {
            v.push(Filter::Mine);
        }
        v.extend(self.tags.iter().map(|(t, _)| Filter::Tag(t.clone())));
        v
    }

    fn passes(&self, s: &HostSnippet, f: &Filter) -> bool {
        match f {
            Filter::All => true,
            Filter::Mine => s.origin.mine(),
            Filter::Tag(t) => s.snippet.tags.iter().any(|x| x.eq_ignore_ascii_case(t)),
        }
    }

    fn rank(&mut self, cx: &mut Context<'_, Self>) {
        let Some(list) = self.list.clone() else {
            self.hits.clear();
            cx.notify();
            return;
        };
        self.shown = (0..list.snippets.len()).filter(|&i| self.passes(&list.snippets[i], &self.filter)).collect();
        let keywords: Vec<Vec<SharedString>> = self
            .shown
            .iter()
            .map(|&i| {
                let s = &list.snippets[i];
                let mut k: Vec<SharedString> =
                    vec![s.snippet.description.clone().into(), s.snippet.line.clone().into(), s.origin.name.clone().into()];
                k.extend(s.snippet.tags.iter().chain(&s.snippet.keywords).map(|t| SharedString::from(t.clone())));
                k
            })
            .collect();
        let ids: Vec<String> = self.shown.iter().map(|&i| super::history_id(&self.link, &list.snippets[i])).collect();
        let entries: Vec<Entry<'_>> = self
            .shown
            .iter()
            .zip(&keywords)
            .zip(&ids)
            .map(|((&i, k), id)| Entry {
                label: &list.snippets[i].snippet.title,
                title_start: 0,
                keywords: k,
                recent_rank: self.history.rank(id),
                only_when_searched: false,
            })
            .collect();
        let mut hits: Vec<(usize, Vec<usize>)> =
            rank(&entries, &self.query).into_iter().map(|h| (self.shown[h.index], h.positions)).collect();
        // Snippets this host can't run go last (the order among them kept).
        hits.sort_by_key(|(i, _)| !list.snippets[*i].missing.is_empty());
        self.hits = hits;
        let want = self.want_selected.take();
        let ix = want.and_then(|w| self.hits.iter().position(|(i, _)| super::history_id(&self.link, &list.snippets[*i]) == w)).unwrap_or(0);
        self.select(ix, cx);
    }

    fn select(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        self.selected = ix.min(self.hits.len().saturating_sub(1));
        if !self.hits.is_empty() {
            self.scroll.scroll_to_item(self.selected, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn set_filter(&mut self, f: Filter, cx: &mut Context<'_, Self>) {
        self.filter = f;
        self.rank(cx);
    }

    fn step_filter(&mut self, by: isize, cx: &mut Context<'_, Self>) {
        let all = self.filters();
        let at = all.iter().position(|f| *f == self.filter).unwrap_or(0) as isize;
        let next = (at + by).rem_euclid(all.len() as isize) as usize;
        self.set_filter(all[next].clone(), cx);
    }

    fn selected_snippet(&self) -> Option<&HostSnippet> {
        let (i, _) = self.hits.get(self.selected)?;
        self.list.as_ref()?.snippets.get(*i)
    }

    fn choose(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        self.select(ix, cx);
        let Some(s) = self.selected_snippet().cloned() else { return };
        let id = super::history_id(&self.link, &s);
        if self.history.push(&id)
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
        let memory = Memory { query: self.query.clone(), filter: self.filter.clone(), selected: Some(id) };
        cx.emit(PickerEvent::Fill(Box::new(s), memory));
    }

    fn render_row(&self, ix: usize, ui: Ui, mono: &SharedString, cx: &mut Context<'_, Self>) -> Option<gpui::AnyElement> {
        let (i, positions) = self.hits.get(ix)?;
        let s = &self.list.as_ref()?.snippets[*i];
        let selected = ix == self.selected;
        let title: SharedString = s.snippet.title.clone().into();
        let matched = HighlightStyle { color: Some(ui.accent), font_weight: Some(FontWeight::BOLD), ..Default::default() };
        let ranges = fuzzy::byte_ranges(&title, positions);
        let title = StyledText::new(title).with_highlights(ranges.into_iter().map(|r| (r, matched)));
        let missing = !s.missing.is_empty();
        let second: gpui::AnyElement = if missing {
            div().text_color(ui.warning).child(format!("Needs {}, which this host doesn't have", s.missing.join(", "))).into_any_element()
        } else if !s.snippet.description.is_empty() {
            div().child(s.snippet.description.clone()).into_any_element()
        } else {
            // No description: the command itself, with its blanks named.
            let line: String = s.snippet.preview(&Default::default(), None).iter().map(|seg| seg.text()).collect();
            div().font_family(mono.clone()).child(line).into_any_element()
        };
        let destructive = s.snippet.safety() == Safety::Destructive;
        // Where it comes from, unless it's one of the built-in basics.
        let source = if s.origin.mine() {
            Some(SharedString::from("Yours"))
        } else if s.origin.owner != "basics" {
            Some(s.origin.name.clone().into())
        } else {
            None
        };
        Some(
            div()
                .id(("snippet", ix))
                .w_full()
                .h(px(ROW_HEIGHT))
                .px_3()
                .flex()
                .items_center()
                .gap_3()
                .rounded(px(radius::CONTROL))
                .cursor_pointer()
                .when(selected, |d| d.bg(ui.surface_hover))
                .when(!selected, |d| d.hover(|st| st.bg(ui.surface)))
                .when(missing, |d| d.opacity(0.6))
                .on_click(cx.listener(move |this, _, _, cx| this.choose(ix, cx)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .min_w_0()
                                .line_height(px(19.))
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(ui.text)
                                        .child(title),
                                )
                                .when(destructive, |d| d.child(deletes_icon(("deletes", ix), ui))),
                        )
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .line_height(px(17.))
                                .text_size(px(text::SMALL))
                                .text_color(ui.muted)
                                .child(second),
                        ),
                )
                .when_some(source, |d, src| {
                    d.child(
                        div()
                            .flex_none()
                            .max_w(px(140.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(text::CAPTION))
                            .text_color(ui.faint)
                            .child(src),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_preview(&self, ui: Ui, mono: SharedString) -> gpui::AnyElement {
        let Some(s) = self.selected_snippet() else { return div().into_any_element() };
        // With the defaults filled in: the likely command.
        let values: goatty_plugin::snippets::Answers =
            s.snippet.inputs.iter().filter_map(|i| i.default.clone().map(|d| (i.id.clone(), d))).collect();
        let segs = s.snippet.preview(&values, None);
        let destructive = s.snippet.safety() == Safety::Destructive;
        let what = match s.snippet.inputs.len() {
            0 => "Goes straight onto your prompt".to_string(),
            1 => "1 blank to fill in".to_string(),
            n => format!("{n} blanks to fill in"),
        };
        let source = if s.origin.mine() { "Your snippet".to_string() } else { format!("From {}", s.origin.name) };
        div()
            .px_3()
            .pt_2()
            .pb_2p5()
            .flex()
            .flex_col()
            .gap_1p5()
            .border_t_1()
            .border_color(ui.border)
            .child(crate::command_palette::preview_line(&segs, ui, mono))
            .child(
                div()
                    .px_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(px(text::CAPTION))
                    .text_color(ui.muted)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .when(destructive, |d| {
                                d.text_color(ui.danger)
                                    .child(Icon::Trash.svg().flex_none().size(px(12.)).text_color(ui.danger))
                                    .child(div().flex_none().child("Deletes data, there's no undo"))
                                    .child(div().flex_none().text_color(ui.faint).child("·"))
                            })
                            .child(
                                div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(ui.muted).child(source),
                            ),
                    )
                    .child(div().flex_none().child(what)),
            )
            .into_any_element()
    }
}

/// The quiet marker on a snippet that deletes or overwrites something.
fn deletes_icon(id: impl Into<gpui::ElementId>, ui: Ui) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .tooltip(crate::ui::controls::tooltip(format!("{}: {}", Safety::Destructive.label(), Safety::Destructive.meaning())))
        .child(Icon::Trash.svg().size(px(12.)).text_color(ui.danger))
}

/// A key and what it does, for the hint line at the bottom.
fn hint(key: &'static str, does: &'static str, ui: Ui) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(div().px_1().min_w(px(16.)).flex().justify_center().rounded(px(4.)).bg(ui.field).text_color(ui.text).child(key))
        .child(does)
}

impl Render for SnippetPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let settings = cx.global::<AppSettings>();
        let ui_font = settings.ui_font.clone();
        let mono: SharedString = settings.client.font.family.clone().unwrap_or_else(|| "monospace".into()).into();
        let state = self.state(cx);
        let total = self.list.as_ref().map_or(0, |l| l.snippets.len());
        let mine = self.list.iter().flat_map(|l| &l.snippets).filter(|s| s.origin.mine()).count();

        let chip = |id: SharedString, label: String, count: usize, on: bool, f: Filter, cx: &mut Context<'_, Self>| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(24.))
                .px_2p5()
                .rounded_full()
                .text_size(px(text::CAPTION))
                .cursor_pointer()
                .when(on, |d| d.bg(ui.accent_soft).text_color(ui.accent_text))
                .when(!on, |d| d.bg(ui.field).text_color(ui.muted).hover(|s| s.bg(ui.field_hover).text_color(ui.text)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_filter(f.clone(), cx)))
                .child(label)
                .child(div().text_color(if on { ui.accent_text } else { ui.faint }).child(count.to_string()))
        };
        let mut chips: Vec<gpui::AnyElement> =
            vec![chip("f-all".into(), "All".into(), total, self.filter == Filter::All, Filter::All, cx).into_any_element()];
        if mine > 0 {
            chips.push(chip("f-mine".into(), "Yours".into(), mine, self.filter == Filter::Mine, Filter::Mine, cx).into_any_element());
        }
        for (t, n) in self.tags.clone() {
            let f = Filter::Tag(t.clone());
            let on = self.filter == f;
            chips.push(chip(format!("f-tag-{t}").into(), t, n, on, f, cx).into_any_element());
        }

        let rows = self.hits.len().min(MAX_VISIBLE_ROWS);
        let body: gpui::AnyElement = match (&state, self.hits.is_empty()) {
            (State::Loading | State::Idle, _) if self.list.is_none() => div()
                .px_4()
                .py_6()
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(ui.muted)
                .child(Icon::Loader.svg().size(px(14.)).text_color(ui.accent))
                .child(format!("Loading the snippets on {}…", self.link.label()))
                .into_any_element(),
            (State::Failed(msg), _) => div().px_4().py_4().text_color(ui.warning).child(msg.clone()).into_any_element(),
            (_, true) => {
                let msg = if self.query.trim().is_empty() {
                    "No snippets here yet.".to_string()
                } else {
                    format!("No snippet for “{}”.", self.query.trim())
                };
                div()
                    .px_4()
                    .py_5()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .child(div().text_color(ui.text).child(msg))
                    .child(
                        div()
                            .id("manage-empty")
                            .text_size(px(text::SMALL))
                            .text_color(ui.accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PickerEvent::Manage)))
                            .child("Add your own in Settings → Snippets"),
                    )
                    .into_any_element()
            }
            _ => {
                let mono = mono.clone();
                let list = uniform_list(
                    "snippets",
                    self.hits.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        range.filter_map(|ix| this.render_row(ix, ui, &mono, cx)).collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .w_full()
                .h(px(ROW_HEIGHT * rows as f32));
                div().px_1().child(list).into_any_element()
            }
        };

        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &Next, _, cx| {
                let n = this.hits.len().max(1);
                this.select((this.selected + 1) % n, cx)
            }))
            .on_action(cx.listener(|this, _: &Previous, _, cx| {
                let n = this.hits.len().max(1);
                this.select((this.selected + n - 1) % n, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.select(this.selected + MAX_VISIBLE_ROWS - 1, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.select(this.selected.saturating_sub(MAX_VISIBLE_ROWS - 1), cx)))
            .on_action(cx.listener(|this, _: &First, _, cx| this.select(0, cx)))
            .on_action(cx.listener(|this, _: &Last, _, cx| this.select(this.hits.len().saturating_sub(1), cx)))
            .on_action(cx.listener(|this, _: &NextFilter, _, cx| this.step_filter(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousFilter, _, cx| this.step_filter(-1, cx)))
            .on_action(cx.listener(|_, _: &Close, _, cx| cx.emit(PickerEvent::Dismissed)))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(PickerEvent::Dismissed)))
            .occlude()
            .w(px(WIDTH))
            .max_w_full()
            .flex()
            .flex_col()
            .font_family(ui_font)
            .text_size(px(text::BODY))
            .text_color(ui.text)
            .bg(ui.bg)
            .border_1()
            .border_color(ui.border)
            .rounded(px(radius::CARD))
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::Code.svg().size(px(16.)).text_color(ui.accent))
                    .child(div().flex_1().child(self.input.clone()))
                    .child(
                        div()
                            .id("manage")
                            .flex_none()
                            .p_1()
                            .rounded(px(radius::SMALL))
                            .cursor_pointer()
                            .hover(|s| s.bg(ui.field))
                            .tooltip(crate::ui::controls::tooltip("Manage snippets and expansions"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PickerEvent::Manage)))
                            .child(Icon::Sliders.svg().size(px(15.)).text_color(ui.muted)),
                    ),
            )
            .when(self.list.is_some(), |d| d.child(div().px_3().pb_2().flex().gap_1().overflow_hidden().children(chips)))
            .child(body)
            .when(!self.hits.is_empty(), |d| d.child(self.render_preview(ui, mono.clone())))
            .child(
                div()
                    .px_4()
                    .py_1p5()
                    .border_t_1()
                    .border_color(ui.border)
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(px(text::CAPTION))
                    .text_color(ui.muted)
                    .child(hint("↑↓", "pick", ui))
                    .child(hint("Enter", "fill in", ui))
                    .child(hint("Tab", "topic", ui))
                    .child(hint("Esc", "close", ui)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_cycle_through_topics() {
        let all = [Filter::All, Filter::Mine, Filter::Tag("git".into())];
        let at = |f: &Filter| all.iter().position(|x| x == f).unwrap() as isize;
        assert_eq!((at(&Filter::All) - 1).rem_euclid(3), 2, "Shift+Tab from All wraps to the last topic");
    }
}
