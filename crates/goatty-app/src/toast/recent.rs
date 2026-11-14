//! "Notifications: Show recent" (#84): an overlay like the command palette
//! that lists the [`history`](super::history), newest first, to read a
//! toast again after it's gone and copy its text.
//!
//! Typing filters by words in the title or body. Up/Down (or Tab) choose
//! one, Enter copies it, Ctrl+Shift+C copies all listed, Ctrl+Shift+Backspace
//! clears the history, Esc closes; the shortcut that opened it closes it
//! too. The filter keeps the focus, so screen readers hear the chosen
//! notification and the copy results through live status nodes. It follows the history while open, so a toast shown meanwhile
//! appears at the top. The [`Workspace`](crate::workspace::Workspace) shows
//! it above its panes and gives the terminal its focus back when it emits
//! [`RecentEvent::Dismissed`].

use std::rc::Rc;
use std::time::SystemTime;

use gpui::accesskit::Live;
use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, Role, ScrollHandle, SharedString,
    Subscription, WeakEntity, Window, actions, div, prelude::*, px,
};

use super::ToastCenter;
use super::history::{self, Entry};
use super::layer::{level_color, level_icon};
use crate::settings::AppSettings;
use crate::ui::controls::{ButtonKind, button_with, icon_button};
use crate::ui::icons::Icon;
use crate::ui::style::Ui;
use crate::ui::text_input::{TextInput, TextInputEvent};

pub const CONTEXT: &str = "RecentNotifications";

const WIDTH: f32 = 620.;
const MAX_LIST_HEIGHT: f32 = 440.;

actions!(recent_notifications, [SelectNext, SelectPrevious, SelectFirst, SelectLast, Dismiss, CopyAll, ClearAll]);

/// Key bindings inside the overlay. `toggle_keys` are the keystrokes bound
/// to opening it, which close it again.
pub fn bindings(toggle_keys: &[String], cx: &App) -> Vec<KeyBinding> {
    let c = Some(CONTEXT);
    let mut v = vec![
        KeyBinding::new("down", SelectNext, c),
        KeyBinding::new("up", SelectPrevious, c),
        KeyBinding::new("ctrl-n", SelectNext, c),
        KeyBinding::new("ctrl-p", SelectPrevious, c),
        KeyBinding::new("tab", SelectNext, c),
        KeyBinding::new("shift-tab", SelectPrevious, c),
        KeyBinding::new("ctrl-home", SelectFirst, c),
        KeyBinding::new("ctrl-end", SelectLast, c),
        KeyBinding::new("escape", Dismiss, c),
        KeyBinding::new("ctrl-shift-c", CopyAll, c),
        KeyBinding::new("ctrl-shift-backspace", ClearAll, c),
    ];
    let ctx = gpui::KeyBindingContextPredicate::parse(CONTEXT).ok().map(Rc::new);
    for k in toggle_keys {
        match KeyBinding::load(k, Box::new(Dismiss), ctx.clone(), false, None, cx) {
            Ok(b) => v.push(b),
            Err(e) => log::debug!("recent notifications: can't bind {k} to close: {e}"),
        }
    }
    v
}

pub enum RecentEvent {
    Dismissed,
}

pub struct RecentNotifications {
    input: Entity<TextInput>,
    /// The history when last refreshed, newest first.
    entries: Vec<Entry>,
    version: u64,
    /// Indices into `entries` that match the query.
    shown: Vec<usize>,
    selected: usize,
    query: String,
    scroll: ScrollHandle,
    /// What the footer says after copying ("Copied …").
    status: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RecentEvent> for RecentNotifications {}

impl Focusable for RecentNotifications {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl RecentNotifications {
    pub fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Filter notifications…", "", cx).label("Filter notifications"));
        let typed = cx.subscribe_in(&input, window, |this, _, ev: &TextInputEvent, _, cx| match ev {
            TextInputEvent::Changed(q) => {
                this.query = q.trim().to_lowercase();
                this.filter(cx);
            }
            TextInputEvent::Submitted(_) => this.copy_selected(cx),
        });
        let history = cx.observe_global::<ToastCenter>(|this, cx| this.refresh(cx));
        input.focus_handle(cx).focus(window, cx);
        let mut this = Self {
            input,
            entries: Vec::new(),
            version: u64::MAX,
            shown: Vec::new(),
            selected: 0,
            query: String::new(),
            scroll: ScrollHandle::new(),
            status: None,
            _subscriptions: vec![typed, history],
        };
        this.refresh(cx);
        this
    }

    /// Take a new snapshot if the history changed, keeping the selection on
    /// the same notification.
    fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        let (entries, version) = super::recent(cx);
        if version == self.version {
            return;
        }
        let selected = self.selected_entry().map(|e| e.id);
        self.entries = entries;
        self.version = version;
        self.filter(cx);
        if let Some(i) = selected.and_then(|id| self.shown.iter().position(|&i| self.entries[i].id == id)) {
            self.select(i, cx);
        }
    }

    fn filter(&mut self, cx: &mut Context<'_, Self>) {
        self.shown = (0..self.entries.len()).filter(|&i| self.entries[i].matches(&self.query)).collect();
        self.select(0, cx);
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.shown.get(self.selected).map(|&i| &self.entries[i])
    }

    fn select(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        self.selected = ix.min(self.shown.len().saturating_sub(1));
        if !self.shown.is_empty() {
            self.scroll.scroll_to_item(self.selected);
        }
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<'_, Self>) {
        let n = self.shown.len().max(1);
        self.select((self.selected + 1) % n, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<'_, Self>) {
        let n = self.shown.len().max(1);
        self.select((self.selected + n - 1) % n, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.select(self.shown.len().saturating_sub(1), cx);
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<'_, Self>) {
        cx.emit(RecentEvent::Dismissed);
    }

    fn copy_selected(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(&i) = self.shown.get(self.selected) {
            self.copy(i, cx);
        }
    }

    /// Copy one notification. Said in the footer, not with a toast: that
    /// would add itself to the list being read.
    fn copy(&mut self, i: usize, cx: &mut Context<'_, Self>) {
        let Some(e) = self.entries.get(i) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(e.text()));
        self.status = Some(format!("Copied \u{201c}{}\u{201d}", e.title).into());
        cx.notify();
    }

    /// Copy every listed notification, newest first.
    fn copy_all(&mut self, cx: &mut Context<'_, Self>) {
        if self.shown.is_empty() {
            return;
        }
        let text = self.shown.iter().map(|&i| plain(&self.entries[i])).collect::<Vec<_>>().join("\n\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let n = self.shown.len();
        self.status = Some(if n == 1 { "Copied 1 notification".into() } else { format!("Copied {n} notifications").into() });
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<'_, Self>) {
        super::clear_history(cx);
        self.status = None;
        self.refresh(cx);
    }

    fn render_row(&self, ix: usize, now: SystemTime, ui: Ui, me: &WeakEntity<Self>, cx: &mut Context<'_, Self>) -> gpui::AnyElement {
        let i = self.shown[ix];
        let e = &self.entries[i];
        let selected = ix == self.selected;
        let color = level_color(e.level, ui);
        let group = SharedString::from(format!("recent-{ix}"));
        let copy = {
            let me = me.clone();
            icon_button(("recent-copy", ix), Icon::Copy, "Copy", ui, move |_, cx| {
                let _ = me.update(cx, |this, cx| this.copy(i, cx));
            })
            .size(px(26.))
            .when(!selected, |d| d.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
        };
        div()
            .id(("recent", ix))
            .role(Role::ListItem)
            .aria_label(SharedString::from(e.text()))
            .aria_selected(selected)
            .group(group)
            .w_full()
            .flex()
            .items_start()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |d| d.bg(ui.surface_hover))
            .when(!selected, |d| d.hover(|s| s.bg(ui.surface)))
            .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
            .child(
                div()
                    .size(px(22.))
                    .flex_none()
                    .rounded_full()
                    .bg(color.opacity(0.14))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(level_icon(e.level).svg().size(px(13.)).text_color(color)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_h(px(22.))
                            .child(div().flex_1().min_w_0().font_weight(FontWeight::MEDIUM).child(e.title.clone()))
                            .when(e.count > 1, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .px_1p5()
                                        .rounded_full()
                                        .bg(ui.surface_hover)
                                        .text_xs()
                                        .text_color(ui.muted)
                                        .child(format!("×{}", e.count)),
                                )
                            })
                            .child(div().flex_none().text_xs().text_color(ui.muted).child(history::ago(e.at, now))),
                    )
                    .when_some(e.body.clone(), |d, body| d.child(div().text_xs().text_color(ui.muted).child(body))),
            )
            .child(copy)
            .into_any_element()
    }
}

/// One notification as plain text, for "Copy all".
fn plain(e: &Entry) -> String {
    format!("[{}] {}", e.level, e.text())
}

impl Render for RecentNotifications {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let ui_font = cx.global::<AppSettings>().ui_font.clone();
        let now = SystemTime::now();
        let me = cx.entity().downgrade();
        let list = if self.shown.is_empty() {
            let empty = if self.entries.is_empty() { "No notifications yet" } else { "No matching notifications" };
            div().px_4().py_3().text_color(ui.muted).child(empty).into_any_element()
        } else {
            let rows: Vec<_> = (0..self.shown.len()).map(|ix| self.render_row(ix, now, ui, &me, cx)).collect();
            div()
                .id("recent-list")
                .role(Role::List)
                .aria_label("Recent notifications")
                .px_1()
                .max_h(px(MAX_LIST_HEIGHT))
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .children(rows)
                .into_any_element()
        };
        // The filter has the focus, so the chosen row is said through a live
        // node: "Couldn't save settings. Permission denied, 2 of 7".
        let selection = self.selected_entry().map(|e| {
            let label = format!("{}, {} of {}", e.text().replace('\n', ". "), self.selected + 1, self.shown.len());
            div()
                .id("recent-selection")
                .role(Role::Status)
                .aria_label(label)
                .a11y_synthetic_children(|b| b.parent_node().set_live(Live::Polite))
                .absolute()
                .size_0()
                .overflow_hidden()
        });
        let footer = {
            let (copy_all, clear) = (me.clone(), me);
            let hint = self.status.clone().unwrap_or_else(|| "Enter: copy · Ctrl+Shift+C: copy all · Esc: close".into());
            div()
                .id("recent-footer")
                .role(Role::Status)
                .aria_label(hint.clone())
                .a11y_synthetic_children(|b| b.parent_node().set_live(Live::Polite))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .pt_1()
                .border_t_1()
                .border_color(ui.border)
                .child(div().flex_1().min_w_0().text_xs().text_color(ui.muted).child(hint))
                .when(!self.shown.is_empty(), |d| {
                    d.child(
                        button_with("recent-copy-all", Some(Icon::Copy), "Copy all", ButtonKind::Ghost, ui, move |_, cx| {
                            let _ = copy_all.update(cx, |this, cx| this.copy_all(cx));
                        })
                        .aria_keyshortcuts("Control+Shift+C"),
                    )
                })
                .when(!self.entries.is_empty(), |d| {
                    d.child(
                        button_with("recent-clear", Some(Icon::Trash), "Clear", ButtonKind::Ghost, ui, move |_, cx| {
                            let _ = clear.update(cx, |this, cx| this.clear(cx));
                        })
                        .aria_keyshortcuts("Control+Shift+Backspace"),
                    )
                })
        };
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(|this, _: &CopyAll, _, cx| this.copy_all(cx)))
            .on_action(cx.listener(|this, _: &ClearAll, _, cx| this.clear(cx)))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(RecentEvent::Dismissed)))
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
            .child(div().p_2().child(self.input.clone()))
            .child(list)
            .child(footer)
            .children(selection)
    }
}
