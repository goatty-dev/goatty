//! The Shortcuts page: every bindable action in a table, grouped or
//! sorted, searchable by name or by keys (typed or pressed), with filters
//! for changed and unbound actions.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

use goatty_config::keys::{self, ACTIONS, GROUPS};
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, KeyDownEvent, ParentElement, SharedString, Styled, Subscription, Window,
    div, prelude::*, px,
};

use super::SettingsWindow;
use super::kit::{self, card, chip, divider, search_field};
use crate::command_palette::registry::{CommandRegistry, Source};
use crate::settings::AppSettings;
use crate::ui::controls::{ButtonKind, Tone, badge, banner, button_with, icon_button, icon_button_tinted, keycaps, pretty_keys, tooltip};
use crate::ui::focus::{self, Ring, Scroller};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

const PLUGINS_GROUP: &str = "Plugins";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Changed,
    Unbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// By group, in [`GROUPS`] order (collapsible).
    Group,
    Name,
    Keys,
}

pub struct ShortcutsPage {
    query: Entity<TextInput>,
    filter: Filter,
    sort: Sort,
    collapsed: HashSet<&'static str>,
    /// The next keystroke becomes the search (instead of binding anything).
    pub(super) listening: bool,
    /// A pressed keystroke to search for (canonical).
    pressed: Option<String>,
    /// "Reset all" was clicked once; the second click resets.
    confirm_reset: bool,
    _subs: Vec<Subscription>,
}

impl ShortcutsPage {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> Self {
        let query = cx.new(|cx| TextInput::new("Search by name or keys, like ctrl+t", "", cx).bare());
        let subs = vec![cx.subscribe(&query, |v, _, _: &TextInputEvent, cx| {
            v.shortcuts.pressed = None;
            cx.notify();
        })];
        Self {
            query,
            filter: Filter::All,
            sort: Sort::Group,
            collapsed: HashSet::new(),
            listening: false,
            pressed: None,
            confirm_reset: false,
            _subs: subs,
        }
    }
}

/// One action as the table shows it.
struct Line {
    name: Cow<'static, str>,
    label: Cow<'static, str>,
    group: &'static str,
    keys: Vec<String>,
    changed: bool,
    available: bool,
}

/// `query` typed as keys: `Ctrl+Shift+T`, `ctrl t` → `ctrl-shift-t`, `ctrl-t`.
fn as_keys(query: &str) -> String {
    query.trim().to_lowercase().replace(['+', ' '], "-")
}

/// Whether `line` matches the search: every word in the name, or the keys.
fn matches(query: &str, line: &Line) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let label = format!("{} {} {} {}", line.label, line.name, line.name.replace(['_', '.'], " "), line.group).to_lowercase();
    if q.split_whitespace().all(|w| label.contains(w)) {
        return true;
    }
    let k = as_keys(&q);
    line.keys.iter().any(|key| key.contains(&k) || pretty_keys(key).to_lowercase().contains(&q))
}

pub(super) fn has_row(action: &str, cx: &App) -> bool {
    let overrides = &cx.global::<AppSettings>().file.value.keybindings;
    has_row_in(action, overrides, cx.try_global::<CommandRegistry>())
}

fn has_row_in(action: &str, overrides: &BTreeMap<String, Option<String>>, registry: Option<&CommandRegistry>) -> bool {
    if keys::find_action(action).is_some() {
        return true;
    }
    if registry.is_some_and(|reg| reg.get(action).is_some_and(|c| matches!(c.source, Source::Plugin(_)) && c.key_action() == Some(action)))
    {
        return true;
    }
    keys::parse_plugin_action(action).is_some() && overrides.values().flatten().any(|a| a == action)
}

fn action_is_changed(name: &str, overrides: &BTreeMap<String, Option<String>>) -> bool {
    overrides
        .iter()
        .any(|(k, v)| v.as_deref() == Some(name) || (v.is_none() && keys::DEFAULT_BINDINGS.iter().any(|(dk, da)| dk == k && *da == name)))
}

fn lines(overrides: &BTreeMap<String, Option<String>>, registry: Option<&CommandRegistry>) -> Vec<Line> {
    let mut out: Vec<Line> = ACTIONS
        .iter()
        .map(|a| Line {
            name: Cow::Borrowed(a.name),
            label: Cow::Borrowed(a.label),
            group: a.group,
            keys: keys::keys_for(a.name, overrides),
            changed: action_is_changed(a.name, overrides),
            available: true,
        })
        .collect();
    let mut seen = HashSet::new();
    if let Some(reg) = registry {
        for c in reg.commands() {
            if !matches!(c.source, Source::Plugin(_)) {
                continue;
            }
            let Some(action) = c.key_action() else { continue };
            if !seen.insert(action.to_string()) {
                continue;
            }
            let keys = keys::keys_for(action, overrides);
            out.push(Line {
                name: Cow::Owned(action.to_string()),
                label: Cow::Owned(c.label()),
                group: PLUGINS_GROUP,
                changed: !keys.is_empty(),
                keys,
                available: true,
            });
        }
    }
    let mut extra: Vec<String> =
        overrides.values().flatten().filter(|a| keys::parse_plugin_action(a).is_some() && seen.insert((*a).clone())).cloned().collect();
    extra.sort();
    for action in extra {
        let Some((plugin, cmd)) = keys::parse_plugin_action(&action) else { continue };
        let keys = keys::keys_for(&action, overrides);
        let changed = !keys.is_empty();
        out.push(Line {
            label: Cow::Owned(format!("{plugin}: {cmd}")),
            name: Cow::Owned(action),
            group: PLUGINS_GROUP,
            keys,
            changed,
            available: false,
        });
    }
    out
}

fn action_label(name: &str, cx: &App) -> String {
    if let Some(a) = keys::find_action(name) {
        return a.label.to_string();
    }
    if let Some(c) = cx.try_global::<CommandRegistry>().and_then(|r| r.get(name)) {
        return c.label();
    }
    if let Some((plugin, cmd)) = keys::parse_plugin_action(name) {
        return format!("{plugin}: {cmd}");
    }
    name.to_string()
}

impl SettingsWindow {
    /// Show the actions matching `text` (from a settings search result).
    pub(super) fn shortcuts_search(&mut self, text: &str, cx: &mut Context<'_, Self>) {
        self.shortcuts.filter = Filter::All;
        self.shortcuts.pressed = None;
        self.shortcuts.query.update(cx, |i, cx| i.set_text(text.to_owned(), cx));
    }

    /// Take a keystroke for the shortcut being recorded, or as the key
    /// search. Returns whether it was used.
    pub(super) fn record_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<'_, Self>) -> bool {
        let listening = self.shortcuts.listening;
        if self.recording.is_none() && !listening {
            return false;
        }
        let k = &ev.keystroke;
        if k.key == "escape" && !k.modifiers.modified() {
            self.recording = None;
            self.shortcuts.listening = false;
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if matches!(k.key.as_str(), "shift" | "control" | "alt" | "platform" | "function") {
            return true; // wait for the actual key
        }
        let mut parts = Vec::new();
        for (on, name) in [
            (k.modifiers.control, "ctrl"),
            (k.modifiers.alt, "alt"),
            (k.modifiers.shift, "shift"),
            (k.modifiers.platform, "super"),
            (k.modifiers.function, "fn"),
        ] {
            if on {
                parts.push(name);
            }
        }
        parts.push(k.key.as_str());
        let keystroke = keys::normalize_keystroke(&parts.join("-"));
        cx.stop_propagation();
        if listening {
            self.shortcuts.listening = false;
            self.shortcuts.query.update(cx, |i, cx| i.set_text(pretty_keys(&keystroke), cx));
            self.shortcuts.pressed = Some(keystroke);
            cx.notify();
            return true;
        }
        let Some(action) = self.recording.take() else { return true };
        let overrides = &cx.global::<AppSettings>().file.value.keybindings;
        // One keystroke runs one action: say which one this took it from.
        self.shortcut_note = keys::action_for(&keystroke, overrides).filter(|a| a != &action).map(|prev| {
            let label = action_label(&prev, cx);
            (action.clone(), format!("{} was bound to \"{label}\"; it does this now.", pretty_keys(&keystroke)))
        });
        AppSettings::update(cx, |s| keys::set_binding(&mut s.keybindings, &keystroke, Some(&action)));
        cx.notify();
        true
    }

    fn shortcut_row(&self, ui: Ui, line: &Line, query: &str, fallback_focus: &gpui::ElementId, cx: &Context<'_, Self>) -> AnyElement {
        let app = cx.global::<AppSettings>();
        let overrides = &app.file.value.keybindings;
        let filter = self.shortcuts.filter;
        let pressed = self.shortcuts.pressed.clone();
        let name = line.name.as_ref().to_owned();
        let label_text: SharedString = line.label.as_ref().to_owned().into();
        let recording = self.recording.as_deref() == Some(name.as_str());
        // Problems per keystroke: unreadable ones, and keys the user added
        // that the shell would otherwise get (the defaults are curated).
        let mut warnings = Vec::new();
        let problem: Vec<bool> = line
            .keys
            .iter()
            .map(|k| {
                let gpui_error = format!("keybindings.{k}: ");
                let broken = match keys::canonical_keystroke(k) {
                    Err(e) => Some(e),
                    Ok(_) => app.key_errors.iter().find_map(|e| e.strip_prefix(&gpui_error)).map(str::to_owned),
                };
                if let Some(e) = broken {
                    warnings.push(format!("{} doesn't work: {e}", pretty_keys(k)));
                    return true;
                }
                if !keys::is_default(k, &name)
                    && let Some(c) = keys::shell_conflict(k)
                {
                    warnings.push(format!("{}: {c}. Bound here, the shell no longer gets it.", pretty_keys(k)));
                    return true;
                }
                false
            })
            .collect();
        if let Some((for_action, msg)) = &self.shortcut_note
            && for_action == &name
        {
            warnings.insert(0, msg.clone());
        }
        let group_id = SharedString::from(format!("sc-{name}"));
        // A button that removes itself hands focus back to the row it was in,
        // or to a surviving neighbor when clearing the binding removes the row.
        let row_elem = row_elem_id(&name);
        let page_focus = self.focus.clone();
        let chips = div().flex().flex_wrap().items_center().gap(px(8.)).children(line.keys.iter().enumerate().map(|(ki, k)| {
            let key = k.clone();
            let (row_elem, page_focus, fallback_focus) = (row_elem.clone(), page_focus.clone(), fallback_focus.clone());
            let stays_after_remove = row_stays_after_remove(line, overrides, filter, query, pressed.as_deref(), k);
            let gid = SharedString::from(format!("sc-key-{name}-{ki}"));
            let rm_id: gpui::ElementId = SharedString::from(format!("sc-rm-{name}-{ki}")).into();
            let rm_focus = rm_id.clone();
            // Hidden until the key is hovered or focused, but a tab stop all along (an
            // `invisible()` one isn't), so any binding can be removed from the keyboard.
            let remove = div()
                .id(rm_id.clone())
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(format!("Remove {} from {}", pretty_keys(k), line.label)))
                .size(px(18.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .opacity(0.)
                .group_hover(gid.clone(), |s| s.opacity(1.))
                .in_focus(|s| s.opacity(1.))
                .cursor_pointer()
                .hover(|s| s.bg(ui.danger.opacity(0.15)))
                .child(Icon::Close.svg().size(px(12.)).text_color(ui.muted))
                .tooltip(tooltip("Remove this shortcut"))
                .on_click(move |_, window, cx| {
                    if focus::is_focused(&rm_focus, window) {
                        let target = if stays_after_remove { row_elem.clone() } else { fallback_focus.clone() };
                        focus::hand_over(&page_focus, focus::Handover::Control(target), window, cx);
                    }
                    AppSettings::update(cx, |s| keys::set_binding(&mut s.keybindings, &key, None));
                });
            div()
                .id(gid.clone())
                .group(gid)
                .relative()
                .flex()
                .items_center()
                .gap(px(4.))
                .when(problem[ki], |d| d.p(px(2.)).rounded(px(8.)).border_1().border_color(ui.warning))
                .child(keycaps(k, ui))
                .child(focus::tab(remove, &rm_id, ui, Ring::Outer, Scroller::Page))
        }));
        let cell = if recording {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(28.))
                .px(px(12.))
                .rounded_full()
                .border_1()
                .border_color(ui.accent)
                .bg(ui.accent_soft)
                .text_size(px(text::SMALL))
                .text_color(ui.accent_text)
                .child(Icon::Keyboard.svg().size(px(14.)).text_color(ui.accent_text))
                .child("Press the new keys · Esc cancels")
                .into_any_element()
        } else if line.keys.is_empty() {
            div().text_size(px(text::SMALL)).text_color(ui.faint).child("Not set").into_any_element()
        } else {
            chips.into_any_element()
        };
        let this = cx.entity().downgrade();
        let stays_after_reset = row_stays_after_reset(line, overrides, filter, query, pressed.as_deref());
        let actions = div()
            .w(px(72.))
            .flex_shrink_0()
            .flex()
            .justify_end()
            .gap(px(2.))
            .when(line.changed, |d| {
                let (row_elem, page_focus, reset_name, fallback_focus) =
                    (row_elem.clone(), page_focus.clone(), name.clone(), fallback_focus.clone());
                let reset_id = SharedString::from(format!("sc-reset-{name}"));
                let reset_focus: gpui::ElementId = reset_id.clone().into();
                d.child(icon_button(reset_id, Icon::RotateCcw, "Reset to the default", ui, move |window, cx| {
                    // The button goes away once the row is back to its default.
                    if focus::is_focused(&reset_focus, window) {
                        let target = if stays_after_reset { row_elem.clone() } else { fallback_focus.clone() };
                        focus::hand_over(&page_focus, focus::Handover::Control(target), window, cx);
                    }
                    AppSettings::update(cx, |s| keys::reset_action(&mut s.keybindings, &reset_name));
                }))
            })
            .child(
                icon_button(SharedString::from(format!("sc-add-{name}")), Icon::Plus, "Add a shortcut", ui, {
                    let rec_name = name.clone();
                    move |window, cx| {
                        let _ = this.update(cx, |v, cx| {
                            v.recording = Some(rec_name.clone());
                            v.shortcuts.listening = false;
                            v.shortcut_note = None;
                            window.focus(&v.focus, cx);
                            cx.notify();
                        });
                    }
                })
                .when(!line.keys.is_empty() && !line.changed, |d| {
                    // Enter on the row does the same: hidden until hover or focus, still a tab stop.
                    d.opacity(0.).group_hover(group_id.clone(), |s| s.opacity(1.)).in_focus(|s| s.opacity(1.))
                }),
            );
        // The row is a tab stop: Enter records a new shortcut, Delete removes the last one.
        let row_id = row_elem;
        let last_key = line.keys.last().cloned();
        let stays_after_delete =
            last_key.as_deref().is_none_or(|k| row_stays_after_remove(line, overrides, filter, query, pressed.as_deref(), k));
        let fallback_focus = fallback_focus.clone();
        let row = div()
            .id(row_id.clone())
            .role(gpui::Role::Row)
            .aria_label(label_text.clone())
            .aria_description("Enter adds a shortcut, Delete removes the last one")
            .on_key_down({
                let (row_id, this, rec_name, page_focus) = (row_id.clone(), cx.entity().downgrade(), name, page_focus);
                move |ev, window, cx| {
                    // Only the row itself: a button inside it has its own Enter.
                    if ev.keystroke.modifiers.modified() || !focus::is_focused(&row_id, window) {
                        return;
                    }
                    match ev.keystroke.key.as_str() {
                        "enter" => {
                            cx.stop_propagation();
                            let _ = this.update(cx, |v, cx| {
                                v.recording = Some(rec_name.clone());
                                v.shortcuts.listening = false;
                                v.shortcut_note = None;
                                window.focus(&v.focus, cx);
                                cx.notify();
                            });
                        }
                        "delete" | "backspace" => {
                            if let Some(key) = &last_key {
                                cx.stop_propagation();
                                if !stays_after_delete {
                                    focus::hand_over(&page_focus, focus::Handover::Control(fallback_focus.clone()), window, cx);
                                }
                                AppSettings::update(cx, |s| keys::set_binding(&mut s.keybindings, key, None));
                            }
                        }
                        _ => {}
                    }
                }
            })
            .group(group_id.clone())
            .flex()
            .flex_col()
            .px(px(16.))
            .py(px(8.))
            .min_h(px(48.))
            .justify_center()
            .when(recording, |d| d.bg(ui.accent_soft.opacity(0.5)))
            .hover(|s| s.bg(ui.surface_hover))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .flex_1()
                            .min_w(px(140.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().min_w_0().text_size(px(text::BODY)).text_color(ui.text).child(label_text))
                                    .when(line.changed, |d| d.child(badge("Changed", Tone::Accent, ui)))
                                    .when(!line.available, |d| d.child(badge("Unavailable", Tone::Neutral, ui))),
                            )
                            .when(!line.available, |d| {
                                d.child(
                                    div()
                                        .text_size(px(text::SMALL))
                                        .text_color(ui.muted)
                                        .child("Not active on this host (plugin not installed or enabled, a setting is off, or its program is missing)."),
                                )
                            }),
                    )
                    .child(div().w(px(240.)).flex_shrink(1.).min_w(px(150.)).child(cell))
                    .child(actions),
            )
            .children((!warnings.is_empty()).then(|| {
                div().flex().flex_col().gap(px(2.)).pt(px(6.)).children(warnings.into_iter().map(|w| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(text::SMALL))
                        .text_color(ui.warning)
                        .child(Icon::TriangleAlert.svg().size(px(13.)).text_color(ui.warning))
                        .child(SharedString::from(w))
                }))
            }));
        focus::tab(row, &row_id, ui, Ring::Inner, Scroller::Page).into_any_element()
    }

    fn column_header(&self, ui: Ui, cx: &Context<'_, Self>) -> impl IntoElement {
        let sort = self.shortcuts.sort;
        let head = |id: &'static str, label: &'static str, mine: Sort| {
            let on = sort == mine;
            let id: gpui::ElementId = id.into();
            let el = div()
                .id(id.clone())
                .role(gpui::Role::Button)
                .aria_label(label)
                .flex()
                .items_center()
                .gap(px(4.))
                .cursor_pointer()
                .text_color(if on { ui.accent_text } else { ui.muted })
                .hover(|s| s.text_color(ui.text))
                .child(label)
                .child(Icon::ArrowUpDown.svg().size(px(12.)).text_color(if on { ui.accent_text } else { ui.faint }))
                .tooltip(tooltip(if on { "Click again to group by area" } else { "Sort by this column" }))
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.shortcuts.sort = if v.shortcuts.sort == mine { Sort::Group } else { mine };
                    cx.notify();
                }));
            focus::tab(el, &id, ui, Ring::Outer, Scroller::Page)
        };
        div()
            .flex()
            .items_center()
            .gap(px(16.))
            .h(px(38.))
            .px(px(16.))
            .text_size(px(text::CAPTION))
            .font_weight(FontWeight::SEMIBOLD)
            .child(div().flex_1().min_w(px(140.)).child(head("sort-name", "ACTION", Sort::Name)))
            .child(div().w(px(240.)).flex_shrink(1.).min_w(px(150.)).child(head("sort-keys", "SHORTCUT", Sort::Keys)))
            .child(div().w(px(72.)))
    }

    pub(super) fn shortcuts_page(&self, ui: Ui, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        let app = cx.global::<AppSettings>();
        let all = lines(&app.file.value.keybindings, cx.try_global::<CommandRegistry>());
        let st = &self.shortcuts;
        let query = st.query.read(cx).text().trim().to_string();
        let changed_n = all.iter().filter(|l| l.changed).count();
        let unbound_n = all.iter().filter(|l| l.keys.is_empty()).count();
        let mut shown: Vec<&Line> = all.iter().filter(|l| line_matches_view(l, st.filter, &query, st.pressed.as_deref())).collect();
        match st.sort {
            Sort::Group => {}
            Sort::Name => shown.sort_by_key(|l| l.label.to_lowercase()),
            Sort::Keys => shown.sort_by_key(|l| (l.keys.is_empty(), l.keys.first().map(|k| pretty_keys(k).to_lowercase()))),
        }

        let listen = icon_button_tinted(
            "sc-listen",
            Icon::Keyboard,
            if st.listening { "Listening: press the keys to find · Esc stops" } else { "Search by pressing keys" },
            st.listening.then_some(ui.on_accent),
            ui,
            self.act(cx, |v, cx| {
                v.shortcuts.listening = !v.shortcuts.listening;
                v.recording = None;
                cx.notify();
            }),
        )
        .size(px(38.))
        .bg(ui.field)
        .when(st.listening, |d| d.bg(ui.accent).hover(|s| s.bg(ui.accent)));
        let set_filter = |f: Filter| {
            self.act(cx, move |v, cx| {
                v.shortcuts.filter = f;
                cx.notify();
            })
        };
        let toolbar = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().child(if st.listening {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(38.))
                            .px(px(14.))
                            .rounded_full()
                            .bg(ui.accent_soft)
                            .border_1()
                            .border_color(ui.accent)
                            .text_size(px(text::BODY))
                            .text_color(ui.accent_text)
                            .child(Icon::Keyboard.svg().size(px(15.)).text_color(ui.accent_text))
                            .child("Press the shortcut you're looking for…")
                            .into_any_element()
                    } else {
                        search_field(ui, "sc-search", &st.query, window, cx).into_any_element()
                    }))
                    .child(listen),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(chip(ui, "sc-all", "All", Some(all.len()), st.filter == Filter::All, set_filter(Filter::All)))
                    .child(chip(ui, "sc-changed", "Changed", Some(changed_n), st.filter == Filter::Changed, set_filter(Filter::Changed)))
                    .child(chip(ui, "sc-unbound", "Unbound", Some(unbound_n), st.filter == Filter::Unbound, set_filter(Filter::Unbound)))
                    .child(div().flex_1())
                    .when(changed_n > 0, |d| {
                        let confirming = st.confirm_reset;
                        d.child(button_with(
                            "sc-reset-all",
                            Some(Icon::RotateCcw),
                            if confirming { "Reset every shortcut?" } else { "Reset all" },
                            if confirming { ButtonKind::Danger } else { ButtonKind::Ghost },
                            ui,
                            self.act(cx, |v, cx| {
                                if v.shortcuts.confirm_reset {
                                    AppSettings::update(cx, |s| s.keybindings.clear());
                                }
                                v.shortcuts.confirm_reset = !v.shortcuts.confirm_reset;
                                cx.notify();
                            }),
                        ))
                    }),
            );

        let mut table = card(ui).overflow_hidden().child(self.column_header(ui, cx)).child(div().h(px(1.)).bg(ui.divider));
        if shown.is_empty() {
            let (title, desc) = match (&st.pressed, st.filter) {
                (Some(k), _) => {
                    (format!("Nothing uses {}", pretty_keys(k)), "That key goes to the shell. Add it to an action with +.".to_string())
                }
                (None, Filter::Unbound) if query.is_empty() => ("Every action has a shortcut".into(), "Remove one to see it here.".into()),
                (None, Filter::Changed) if query.is_empty() => ("No changes yet".into(), "Every shortcut is goatty's default.".into()),
                _ => ("No matching shortcuts".into(), "Try another name or key, or clear the filter.".into()),
            };
            table = table.child(kit::empty(ui, Icon::Keyboard, title, desc));
        } else if st.sort == Sort::Group {
            let searching = !query.is_empty() || st.pressed.is_some();
            let groups: Vec<(&'static str, Vec<&Line>, bool)> = GROUPS
                .iter()
                .copied()
                .chain(std::iter::once(PLUGINS_GROUP))
                .filter_map(|g| {
                    let rows: Vec<&Line> = shown.iter().filter(|l| l.group == g).copied().collect();
                    (!rows.is_empty()).then_some((g, rows, !searching && st.collapsed.contains(g)))
                })
                .collect();
            for (gi, (g, rows, folded)) in groups.iter().enumerate() {
                if gi > 0 {
                    table = table.child(div().h(px(1.)).bg(ui.divider));
                }
                let g = *g;
                let head_id = group_elem_id(g);
                let head = div()
                    .id(head_id.clone())
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!("{g} shortcuts")))
                    .aria_expanded(!folded)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(38.))
                    .px(px(14.))
                    .bg(ui.field.opacity(0.5))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.field))
                    .child((if *folded { Icon::ChevronRight } else { Icon::ChevronDown }).svg().size(px(14.)).text_color(ui.muted))
                    .child(div().text_size(px(text::LABEL)).font_weight(FontWeight::SEMIBOLD).child(g))
                    .child(div().text_size(px(text::CAPTION)).text_color(ui.faint).child(rows.len().to_string()))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        if !v.shortcuts.collapsed.remove(g) {
                            v.shortcuts.collapsed.insert(g);
                        }
                        cx.notify();
                    }));
                table = table.child(focus::tab(head, &head_id, ui, Ring::Inner, Scroller::Page));
                if !folded {
                    for (i, l) in rows.iter().enumerate() {
                        if i > 0 {
                            table = table.child(divider(ui));
                        }
                        let fallback = grouped_removal_fallback(&groups, gi, i);
                        table = table.child(self.shortcut_row(ui, l, &query, &fallback, cx));
                    }
                }
            }
        } else {
            for (i, l) in shown.iter().enumerate() {
                if i > 0 {
                    table = table.child(divider(ui));
                }
                let fallback = flat_removal_fallback(&shown, i);
                table = table.child(self.shortcut_row(ui, l, &query, &fallback, cx));
            }
        }

        let mut page = div().flex().flex_col().gap(px(16.)).child(toolbar);
        if !app.key_errors.is_empty() {
            page = page.child(banner(ui, Tone::Warning, "Some shortcuts don't work", app.key_errors.clone()));
        }
        page.child(table)
            .child(kit::hint(ui, "Shortcuts work in terminal panes; keys no action uses go to the shell. Symbol keys show the character they type: Ctrl+Shift with the + key on a German layout is Ctrl+*.").px(px(4.)))
            .into_any_element()
    }
}

fn row_elem_id(name: &str) -> gpui::ElementId {
    SharedString::from(format!("sc-{name}")).into()
}

fn group_elem_id(group: &str) -> gpui::ElementId {
    SharedString::from(format!("sc-group-{group}")).into()
}

fn line_matches_view(line: &Line, filter: Filter, query: &str, pressed: Option<&str>) -> bool {
    let passes_filter = match filter {
        Filter::All => true,
        Filter::Changed => line.changed,
        Filter::Unbound => line.keys.is_empty(),
    };
    let passes_search = match pressed {
        Some(k) => line.keys.iter().any(|key| key == k),
        None => matches(query, line),
    };
    passes_filter && passes_search
}

fn row_stays_after_overrides(
    line: &Line,
    after: &BTreeMap<String, Option<String>>,
    filter: Filter,
    query: &str,
    pressed: Option<&str>,
) -> bool {
    let after_keys = keys::keys_for(&line.name, after);
    let is_builtin = keys::find_action(&line.name).is_some();
    if !is_builtin && !line.available && after_keys.is_empty() {
        return false;
    }
    let after_changed = if is_builtin { action_is_changed(&line.name, after) } else { !after_keys.is_empty() };
    let after_line = Line {
        name: line.name.clone(),
        label: line.label.clone(),
        group: line.group,
        keys: after_keys,
        changed: after_changed,
        available: line.available,
    };
    line_matches_view(&after_line, filter, query, pressed)
}

fn row_stays_after_remove(
    line: &Line,
    overrides: &BTreeMap<String, Option<String>>,
    filter: Filter,
    query: &str,
    pressed: Option<&str>,
    key: &str,
) -> bool {
    let mut after = overrides.clone();
    keys::set_binding(&mut after, key, None);
    row_stays_after_overrides(line, &after, filter, query, pressed)
}

fn row_stays_after_reset(
    line: &Line,
    overrides: &BTreeMap<String, Option<String>>,
    filter: Filter,
    query: &str,
    pressed: Option<&str>,
) -> bool {
    let mut after = overrides.clone();
    keys::reset_action(&mut after, &line.name);
    row_stays_after_overrides(line, &after, filter, query, pressed)
}

fn grouped_removal_fallback(groups: &[(&'static str, Vec<&Line>, bool)], gi: usize, ri: usize) -> gpui::ElementId {
    let rows = &groups[gi].1;
    if let Some(next) = rows.get(ri + 1) {
        return row_elem_id(&next.name);
    }
    if let Some(prev) = ri.checked_sub(1).and_then(|p| rows.get(p)) {
        return row_elem_id(&prev.name);
    }
    if let Some((next_g, _, _)) = groups.get(gi + 1) {
        return group_elem_id(next_g);
    }
    if let Some((prev_g, prev_rows, prev_folded)) = gi.checked_sub(1).and_then(|pg| groups.get(pg)) {
        if !prev_folded && let Some(last) = prev_rows.last() {
            return row_elem_id(&last.name);
        }
        return group_elem_id(prev_g);
    }
    SharedString::from("sc-all").into()
}

fn flat_removal_fallback(shown: &[&Line], i: usize) -> gpui::ElementId {
    if let Some(next) = shown.get(i + 1) {
        return row_elem_id(&next.name);
    }
    if let Some(prev) = i.checked_sub(1).and_then(|p| shown.get(p)) {
        return row_elem_id(&prev.name);
    }
    SharedString::from("sc-all").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_palette::Command;

    #[test]
    fn search_by_name_or_keys() {
        let a = keys::find_action("new_tab").unwrap();
        let l = Line {
            name: Cow::Borrowed(a.name),
            label: Cow::Borrowed(a.label),
            group: a.group,
            keys: vec!["ctrl-shift-t".into()],
            changed: false,
            available: true,
        };
        for q in ["", "new tab", "tab new", "TAB", "ctrl+shift+t", "Ctrl+Shift+T", "shift-t", "tabs"] {
            assert!(matches(q, &l), "{q:?}");
        }
        assert!(!matches("paste", &l));
        assert!(!matches("ctrl+alt", &l));
    }

    #[test]
    fn plugin_commands_and_inactive_bound_overrides_appear_under_plugins() {
        let mut reg = CommandRegistry::default();
        let mut active = Command::handler("plugin.docker.exec", "Docker", "Open a shell in a container", |_, _| {})
            .with_key_action("plugin.docker.exec");
        active.source = Source::Plugin("docker".into());
        reg.register(active);

        let overrides = BTreeMap::from([
            ("ctrl-alt-e".to_string(), Some("plugin.docker.exec".to_string())),
            ("ctrl-alt-g".to_string(), Some("plugin.git.status".to_string())),
        ]);
        let all = lines(&overrides, Some(&reg));
        let plugin_lines: Vec<&Line> = all.iter().filter(|l| l.group == PLUGINS_GROUP).collect();
        assert_eq!(plugin_lines.len(), 2);

        let docker = plugin_lines.iter().find(|l| l.name == "plugin.docker.exec").unwrap();
        assert_eq!(docker.label, "Docker: Open a shell in a container");
        assert_eq!(docker.keys, ["ctrl-alt-e"]);
        assert!(docker.changed);
        assert!(docker.available);
        assert!(matches("docker exec", docker));
        assert!(matches("plugin.docker.exec", docker));
        assert!(matches("plugins", docker));

        let git = plugin_lines.iter().find(|l| l.name == "plugin.git.status").unwrap();
        assert_eq!(git.label, "git: status");
        assert_eq!(git.keys, ["ctrl-alt-g"]);
        assert!(git.changed);
        assert!(!git.available);
        assert!(matches("plugin.git.status", git));

        assert!(has_row_in("new_tab", &overrides, Some(&reg)));
        assert!(has_row_in("plugin.docker.exec", &BTreeMap::new(), Some(&reg)));
        assert!(has_row_in("plugin.git.status", &overrides, Some(&reg)));
        assert!(!has_row_in("plugin.git.status", &BTreeMap::new(), Some(&reg)));
        assert!(!has_row_in("plugin.missing.cmd", &overrides, Some(&reg)));
    }

    #[test]
    fn removal_focus_target_picks_surviving_neighbor() {
        let tabs_line = Line {
            name: Cow::Borrowed("new_tab"),
            label: Cow::Borrowed("New tab"),
            group: "Tabs",
            keys: vec!["ctrl-shift-t".into(), "ctrl-alt-n".into()],
            changed: true,
            available: true,
        };
        let plugin_avail = Line {
            name: Cow::Borrowed("plugin.docker.exec"),
            label: Cow::Borrowed("Docker: Exec"),
            group: PLUGINS_GROUP,
            keys: vec!["ctrl-alt-e".into()],
            changed: true,
            available: true,
        };
        let plugin_a = Line {
            name: Cow::Borrowed("plugin.alpha.one"),
            label: Cow::Borrowed("alpha: one"),
            group: PLUGINS_GROUP,
            keys: vec!["ctrl-alt-1".into()],
            changed: true,
            available: false,
        };
        let plugin_b = Line {
            name: Cow::Borrowed("plugin.beta.two"),
            label: Cow::Borrowed("beta: two"),
            group: PLUGINS_GROUP,
            keys: vec!["ctrl-alt-2".into(), "ctrl-alt-3".into()],
            changed: true,
            available: false,
        };
        let overrides = BTreeMap::from([
            ("ctrl-alt-n".to_string(), Some("new_tab".to_string())),
            ("ctrl-alt-e".to_string(), Some("plugin.docker.exec".to_string())),
            ("ctrl-alt-1".to_string(), Some("plugin.alpha.one".to_string())),
            ("ctrl-alt-2".to_string(), Some("plugin.beta.two".to_string())),
            ("ctrl-alt-3".to_string(), Some("plugin.beta.two".to_string())),
        ]);

        assert!(!row_stays_after_remove(&plugin_a, &overrides, Filter::All, "", None, "ctrl-alt-1"));
        assert!(row_stays_after_remove(&plugin_b, &overrides, Filter::All, "", None, "ctrl-alt-2"));
        assert!(!row_stays_after_remove(&plugin_a, &overrides, Filter::Changed, "", None, "ctrl-alt-1"));

        // Text search matching only the removed/reset binding hides the row; label match keeps it.
        assert!(!row_stays_after_remove(&plugin_avail, &overrides, Filter::All, "ctrl-alt-e", None, "ctrl-alt-e"));
        assert!(!row_stays_after_reset(&plugin_avail, &overrides, Filter::All, "ctrl-alt-e", None));
        assert!(row_stays_after_remove(&plugin_avail, &overrides, Filter::All, "docker", None, "ctrl-alt-e"));
        assert!(row_stays_after_reset(&plugin_avail, &overrides, Filter::All, "docker", None));

        // Removing a single custom binding on `new_tab` leaves its default key, so Changed drops it.
        assert!(!row_stays_after_remove(&tabs_line, &overrides, Filter::Changed, "", None, "ctrl-alt-n"));
        // Adding a second custom binding on `new_tab` keeps it in Changed when only one is removed.
        let mut two_custom = overrides.clone();
        two_custom.insert("ctrl-alt-m".to_string(), Some("new_tab".to_string()));
        assert!(row_stays_after_remove(&tabs_line, &two_custom, Filter::Changed, "", None, "ctrl-alt-n"));

        let two_plugins = vec![("Tabs", vec![&tabs_line], false), (PLUGINS_GROUP, vec![&plugin_a, &plugin_b], false)];
        assert_eq!(grouped_removal_fallback(&two_plugins, 1, 0), row_elem_id("plugin.beta.two"));
        assert_eq!(grouped_removal_fallback(&two_plugins, 1, 1), row_elem_id("plugin.alpha.one"));

        // When the only plugin row disappears, `Plugins` header also disappears, so fall back to the preceding surviving row.
        let one_plugin = vec![("Tabs", vec![&tabs_line], false), (PLUGINS_GROUP, vec![&plugin_a], false)];
        assert_eq!(grouped_removal_fallback(&one_plugin, 1, 0), row_elem_id("new_tab"));

        // If no rows remain anywhere on the page, fall back to the All filter chip.
        let only_plugin = vec![(PLUGINS_GROUP, vec![&plugin_a], false)];
        assert_eq!(grouped_removal_fallback(&only_plugin, 0, 0), gpui::ElementId::from(SharedString::from("sc-all")));
        assert_eq!(flat_removal_fallback(&[&plugin_a], 0), gpui::ElementId::from(SharedString::from("sc-all")));
    }
}
