//! A right-click menu (#186): items with an icon, a label and the shortcut
//! that does the same thing, separators, group headings, one level of
//! submenus, and notes. It works like any desktop menu:
//!
//! * Up/Down, Home/End or an item's first letter pick an item, Enter or
//!   Space runs it, Right (or Enter) opens a submenu, Left closes it, Esc
//!   closes the submenu and then the menu.
//! * Hovering picks an item and opens its submenu. When the pointer leaves
//!   a submenu's row towards the submenu, the submenu stays open briefly.
//!   A click outside closes the menu (and does nothing else).
//! * Screen readers get a menu of menu items, with the picked item as the
//!   active descendant, its shortcut, and why it's unavailable.
//!
//! The menu runs nothing itself. It emits [`MenuEvent::Run`] with the item's
//! handler, and its owner closes the menu, gives focus back (so actions reach
//! the terminal), and then calls the handler.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, Anchor, AnyElement, App, Bounds, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Pixels, Point, Render, Role, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window,
    anchored, canvas, deferred, div, point, prelude::*, px,
};

use super::icons::Icon;
use super::style::{Ui, radius, text};
use crate::settings::AppSettings;

/// What an item does.
pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One line of a menu.
pub enum Entry {
    Item(Item),
    Separator,
    /// A small heading over the items after it ("Command line").
    Header(SharedString),
    /// A muted line of explanation, not selectable.
    Note(Icon, SharedString),
}

pub struct Item {
    pub label: SharedString,
    pub icon: Option<Icon>,
    /// The keys that do the same ("Ctrl+Shift+C").
    pub shortcut: Option<SharedString>,
    /// Shown but not usable, and why (read to screen readers).
    pub disabled: Option<SharedString>,
    /// Ends or removes something (drawn in the danger color).
    pub danger: bool,
    pub kind: Kind,
}

pub enum Kind {
    Run(Handler),
    Submenu(Vec<Entry>),
}

impl Item {
    /// An item that calls `f`.
    pub fn new(label: impl Into<SharedString>, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Self { label: label.into(), icon: None, shortcut: None, disabled: None, danger: false, kind: Kind::Run(Rc::new(f)) }
    }

    /// An item that dispatches the bindable action `name`
    /// (`goatty_config::keys`) and shows its shortcut.
    pub fn action(label: impl Into<SharedString>, name: &'static str, cx: &App) -> Self {
        let action: Box<dyn Action> = crate::actions::action_by_name(name).unwrap_or_else(|| panic!("unknown action {name}"));
        let mut item = Self::new(label, move |window, cx| window.dispatch_action(action.boxed_clone(), cx));
        item.shortcut = shortcut(name, cx);
        item
    }

    pub fn submenu(label: impl Into<SharedString>, entries: Vec<Entry>) -> Self {
        Self { label: label.into(), icon: None, shortcut: None, disabled: None, danger: false, kind: Kind::Submenu(tidy(entries)) }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Unavailable when `reason` is `Some`.
    pub fn disabled(mut self, reason: Option<impl Into<SharedString>>) -> Self {
        self.disabled = reason.map(Into::into);
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    fn enabled(&self) -> bool {
        self.disabled.is_none() && !matches!(&self.kind, Kind::Submenu(e) if !e.iter().any(|e| matches!(e, Entry::Item(_))))
    }
}

impl From<Item> for Entry {
    fn from(item: Item) -> Self {
        Entry::Item(item)
    }
}

/// The first key bound to the action `name`, as people write it.
pub fn shortcut(name: &str, cx: &App) -> Option<SharedString> {
    let overrides = &cx.global::<AppSettings>().client.keybindings;
    goatty_config::keys::keys_for(name, overrides).first().map(|k| crate::ui::controls::pretty_keys(k).into())
}

/// `entries` without separators at the ends or next to each other, and
/// without headings that head nothing (built lists add them freely).
pub fn tidy(entries: Vec<Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(entries.len());
    for e in entries {
        if matches!(e, Entry::Separator | Entry::Header(_)) {
            // A heading or separator followed by another one says nothing.
            while matches!(out.last(), Some(Entry::Header(_)))
                || (matches!(e, Entry::Separator) && matches!(out.last(), Some(Entry::Separator)))
            {
                out.pop();
            }
            if out.is_empty() && matches!(e, Entry::Separator) {
                continue;
            }
        }
        out.push(e);
    }
    while matches!(out.last(), Some(Entry::Separator | Entry::Header(_))) {
        out.pop();
    }
    out
}

pub enum MenuEvent {
    /// Close the menu, then call this.
    Run(Handler),
    /// Closed without running anything (Esc, a click outside, focus left).
    Dismissed,
}

/// The open submenu.
struct Sub {
    /// Its item in the menu.
    parent: usize,
    selected: Option<usize>,
}

pub struct ContextMenu {
    entries: Vec<Entry>,
    /// Where it opened, in window coordinates.
    at: Point<Pixels>,
    focus: FocusHandle,
    selected: Option<usize>,
    sub: Option<Sub>,
    /// Keys go to the submenu.
    in_sub: bool,
    /// Where the submenu rows were drawn (window coordinates), for placing
    /// their submenus.
    rows: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    /// Closes the submenu shortly after the pointer left its row (unless
    /// it reached the submenu by then).
    closing: Option<Task<()>>,
    _blur: Subscription,
}

impl EventEmitter<MenuEvent> for ContextMenu {}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// How long a submenu stays open after the pointer left its row.
const SUBMENU_GRACE: Duration = Duration::from_millis(300);

impl ContextMenu {
    /// A menu at `at` that takes the keyboard. `keyboard`: opened with a
    /// key, so the first item starts out picked.
    pub fn new(entries: Vec<Entry>, at: Point<Pixels>, keyboard: bool, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |_, _, cx| cx.emit(MenuEvent::Dismissed));
        focus.focus(window, cx);
        let entries = tidy(entries);
        let selected = if keyboard { next_enabled(&entries, None, 1) } else { None };
        Self { entries, at, focus, selected, sub: None, in_sub: false, rows: Rc::default(), closing: None, _blur: blur }
    }

    /// The entries of the level the keys go to, and its pick.
    fn level(&self) -> (&[Entry], Option<usize>) {
        match &self.sub {
            Some(sub) if self.in_sub => match &self.entries[sub.parent] {
                Entry::Item(Item { kind: Kind::Submenu(e), .. }) => (e, sub.selected),
                _ => (&[], None),
            },
            _ => (&self.entries, self.selected),
        }
    }

    fn select(&mut self, ix: Option<usize>) {
        match &mut self.sub {
            Some(sub) if self.in_sub => sub.selected = ix,
            _ => self.selected = ix,
        }
    }

    fn open_sub(&mut self, parent: usize, keys: bool) {
        let Some(Entry::Item(Item { kind: Kind::Submenu(entries), .. })) = self.entries.get(parent) else { return };
        let selected = if keys { next_enabled(entries, None, 1) } else { None };
        self.closing = None;
        self.selected = Some(parent);
        self.sub = Some(Sub { parent, selected });
        self.in_sub = keys;
    }

    fn close_sub(&mut self) {
        if let Some(sub) = self.sub.take() {
            self.selected = Some(sub.parent);
        }
        self.in_sub = false;
        self.closing = None;
    }

    /// Run or open the item at `ix` of the main menu (`sub`: of the submenu).
    fn activate(&mut self, ix: usize, in_sub: bool, cx: &mut Context<'_, Self>) {
        let item = if in_sub {
            match self.sub.as_ref().map(|s| &self.entries[s.parent]) {
                Some(Entry::Item(Item { kind: Kind::Submenu(e), .. })) => e.get(ix),
                _ => None,
            }
        } else {
            self.entries.get(ix)
        };
        let Some(Entry::Item(item)) = item else { return };
        if !item.enabled() {
            return;
        }
        match &item.kind {
            Kind::Run(h) => cx.emit(MenuEvent::Run(h.clone())),
            Kind::Submenu(_) if !in_sub => {
                self.open_sub(ix, true);
                cx.notify();
            }
            Kind::Submenu(_) => {}
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let (entries, pick) = self.level();
        let key = ev.keystroke.key.as_str();
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform {
            return;
        }
        match key {
            "escape" if self.in_sub => self.close_sub(),
            "escape" => {
                cx.emit(MenuEvent::Dismissed);
                return;
            }
            "down" => self.select(next_enabled(entries, pick, 1)),
            "tab" if !m.shift => self.select(next_enabled(entries, pick, 1)),
            "up" => self.select(next_enabled(entries, pick, -1)),
            "tab" => self.select(next_enabled(entries, pick, -1)),
            "home" | "pageup" => self.select(next_enabled(entries, None, 1)),
            "end" | "pagedown" => self.select(next_enabled(entries, None, -1)),
            "left" if self.in_sub => self.close_sub(),
            "right" if !self.in_sub => {
                if let Some(ix) = pick.filter(|&i| matches!(&entries[i], Entry::Item(Item { kind: Kind::Submenu(_), .. }))) {
                    self.open_sub(ix, true);
                }
            }
            "enter" | "space" => {
                if let Some(ix) = pick {
                    let in_sub = self.in_sub;
                    self.activate(ix, in_sub, cx);
                }
            }
            _ => {
                // The first letter of an item: the next item starting with it.
                let Some(ch) = ev.keystroke.key_char.as_deref().and_then(|s| s.chars().next()).filter(|c| c.is_alphanumeric()) else {
                    return;
                };
                let found = by_letter(entries, pick, ch);
                if found.is_none() {
                    return;
                }
                self.select(found);
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The pointer is over item `ix` of the main menu.
    fn hover_main(&mut self, ix: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.selected == Some(ix) && (self.sub.is_none() || self.sub.as_ref().is_some_and(|s| s.parent == ix)) {
            self.closing = None;
            return;
        }
        self.selected = Some(ix);
        let opens = matches!(&self.entries[ix], Entry::Item(i @ Item { kind: Kind::Submenu(_), .. }) if i.enabled());
        if self.sub.is_none() {
            if opens {
                self.open_sub(ix, false);
            }
        } else if self.closing.is_none() {
            // Leave the submenu open a moment: the pointer may be on its way there.
            self.closing = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(SUBMENU_GRACE).await;
                let _ = this.update(cx, |this, cx| {
                    this.closing = None;
                    let Some(now) = this.selected else { return };
                    if this.sub.as_ref().is_some_and(|s| s.parent == now) {
                        return;
                    }
                    this.sub = None;
                    this.in_sub = false;
                    if matches!(&this.entries[now], Entry::Item(i @ Item { kind: Kind::Submenu(_), .. }) if i.enabled()) {
                        this.open_sub(now, false);
                    }
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }

    /// The pointer is over item `ix` of the submenu.
    fn hover_sub(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        self.closing = None;
        if let Some(sub) = &mut self.sub {
            self.selected = Some(sub.parent);
            sub.selected = Some(ix);
            self.in_sub = true;
            cx.notify();
        }
    }

    fn render_list(
        &self,
        entries: &[Entry],
        level: usize,
        selected: Option<usize>,
        cx: &mut Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let ui = Ui::get(cx);
        let in_sub = level == 1;
        let items: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| matches!(e, Entry::Item(_))).map(|(i, _)| i).collect();
        let mut list = div()
            .id(("context-menu", level))
            .role(Role::Menu)
            .occlude()
            .min_w(px(220.))
            .max_w(px(420.))
            .p(px(4.))
            .flex()
            .flex_col()
            .bg(ui.menu)
            .border_1()
            .border_color(ui.border)
            .rounded(px(radius::CONTROL))
            .shadow_lg()
            .font_family(cx.global::<AppSettings>().ui_font.clone())
            .text_size(px(text::LABEL))
            .text_color(ui.text);
        for (ix, entry) in entries.iter().enumerate() {
            list = list.child(match entry {
                Entry::Separator => div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(ui.divider).into_any_element(),
                Entry::Header(t) => div()
                    .px(px(10.))
                    .pt(px(6.))
                    .pb(px(2.))
                    .text_size(px(text::CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(ui.muted)
                    .child(t.clone())
                    .into_any_element(),
                Entry::Note(icon, t) => div()
                    .flex()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(6.))
                    .max_w(px(320.))
                    .text_size(px(text::CAPTION))
                    .text_color(ui.muted)
                    .child(icon.svg().size(px(13.)).mt(px(1.)).text_color(ui.muted))
                    .child(div().flex_1().child(t.clone()))
                    .into_any_element(),
                Entry::Item(item) => {
                    let pos = items.iter().position(|&i| i == ix).unwrap_or(0);
                    self.render_item(item, ix, in_sub, selected == Some(ix), (pos, items.len()), cx)
                }
            });
        }
        list
    }

    fn render_item(
        &self,
        item: &Item,
        ix: usize,
        in_sub: bool,
        picked: bool,
        (pos, count): (usize, usize),
        cx: &mut Context<'_, Self>,
    ) -> AnyElement {
        let ui = Ui::get(cx);
        let enabled = item.enabled();
        let is_sub = matches!(item.kind, Kind::Submenu(_));
        let open = !in_sub && self.sub.as_ref().is_some_and(|s| s.parent == ix);
        let lit = picked && enabled;
        let fg = match (enabled, item.danger, lit) {
            (false, _, _) => ui.faint,
            (true, true, _) => ui.danger,
            (true, false, true) => ui.accent_text,
            (true, false, false) => ui.text,
        };
        let icon_color = match (enabled, item.danger, lit) {
            (false, _, _) => ui.faint,
            (true, true, _) => ui.danger,
            (true, false, true) => ui.accent_text,
            (true, false, false) => ui.muted,
        };
        let icon = match item.icon {
            Some(i) => i.svg().size(px(15.)).text_color(icon_color).into_any_element(),
            None => div().size(px(15.)).flex_none().into_any_element(),
        };
        let right = if is_sub {
            Some(Icon::ChevronRight.svg().size(px(13.)).text_color(if lit { ui.accent_text } else { ui.muted }).into_any_element())
        } else {
            item.shortcut.clone().map(|k| {
                div()
                    .flex_none()
                    .pl(px(16.))
                    .text_size(px(text::CAPTION))
                    .text_color(if lit { ui.accent_text } else { ui.muted })
                    .child(k)
                    .into_any_element()
            })
        };
        let mut row = div()
            .id(("context-menu-item", ix + if in_sub { 10_000 } else { 0 }))
            .role(Role::MenuItem)
            .aria_label(item.label.clone())
            .aria_position_in_set(pos + 1)
            .aria_size_of_set(count)
            .relative()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(30.))
            .px(px(10.))
            .rounded(px(radius::SMALL))
            .whitespace_nowrap()
            .text_color(fg)
            .when(lit || open, |d| d.bg(if item.danger { ui.danger.opacity(0.14) } else { ui.accent_soft }))
            .when(picked, |d| d.aria_active_descendant())
            .when_some(item.shortcut.clone(), |d, k| d.aria_keyshortcuts(k))
            .when_some(item.disabled.clone(), |d, why| d.aria_description(why))
            .when(is_sub, |d| d.aria_expanded(open))
            .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                if *hovered {
                    if in_sub {
                        this.hover_sub(ix, cx);
                    } else {
                        this.hover_main(ix, window, cx);
                    }
                }
            }))
            .child(icon)
            .child(div().flex_1().min_w_0().truncate().child(item.label.clone()))
            .children(right);
        if enabled {
            row = row.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.activate(ix, in_sub, cx)));
        }
        if is_sub && !in_sub {
            let rows = self.rows.clone();
            row = row.child(
                canvas(
                    move |bounds, _, _| {
                        rows.borrow_mut().insert(ix, bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            );
        }
        row.into_any_element()
    }
}

/// The next enabled item after `from` (`step` 1) or before it (-1),
/// wrapping around; from the start or end without `from`.
fn next_enabled(entries: &[Entry], from: Option<usize>, step: isize) -> Option<usize> {
    let n = entries.len() as isize;
    if n == 0 {
        return None;
    }
    let mut i = match from {
        Some(f) => f as isize,
        None if step > 0 => -1,
        None => n,
    };
    for _ in 0..n {
        i = (i + step).rem_euclid(n);
        if matches!(&entries[i as usize], Entry::Item(item) if item.enabled()) {
            return Some(i as usize);
        }
    }
    None
}

/// The next enabled item after `from` whose label starts with `ch`.
fn by_letter(entries: &[Entry], from: Option<usize>, ch: char) -> Option<usize> {
    let n = entries.len();
    let start = from.map_or(0, |f| f + 1);
    (0..n).map(|k| (start + k) % n).find(|&i| match &entries[i] {
        Entry::Item(item) => item.enabled() && item.label.chars().next().is_some_and(|c| c.to_lowercase().eq(ch.to_lowercase())),
        _ => false,
    })
}

impl Render for ContextMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let main = self.render_list(&self.entries, 0, self.selected, cx).track_focus(&self.focus).on_key_down(cx.listener(Self::on_key));
        let sub = self.sub.as_ref().and_then(|sub| {
            let Entry::Item(Item { kind: Kind::Submenu(entries), .. }) = &self.entries[sub.parent] else { return None };
            let row = self.rows.borrow().get(&sub.parent).copied();
            let Some(row) = row else {
                // Its row hasn't been drawn yet: next frame.
                cx.on_next_frame(window, |_, _, cx| cx.notify());
                return None;
            };
            let list = self.render_list(entries, 1, sub.selected, cx);
            Some(
                deferred(anchored().attach(row, Anchor::TopRight).anchor(Anchor::TopLeft).offset(point(px(2.), px(-5.))).child(list))
                    .with_priority(4),
            )
        });
        // A click anywhere else closes the menu and does nothing more.
        let dismiss = |_: &mut Self, _: &gpui::MouseDownEvent, _: &mut Window, cx: &mut Context<'_, Self>| cx.emit(MenuEvent::Dismissed);
        let backdrop = div()
            .id("context-menu-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(dismiss))
            .on_mouse_down(MouseButton::Right, cx.listener(dismiss))
            .on_mouse_down(MouseButton::Middle, cx.listener(dismiss));
        div()
            .absolute()
            .inset_0()
            .child(deferred(backdrop).with_priority(2))
            .child(deferred(anchored().position(self.at).snap_to_window_with_margin(px(8.)).child(main)).with_priority(3))
            .children(sub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str) -> Entry {
        Entry::Item(Item::new(label.to_string(), |_, _| {}))
    }

    fn shape(entries: &[Entry]) -> String {
        entries
            .iter()
            .map(|e| match e {
                Entry::Item(i) => i.label.to_string(),
                Entry::Separator => "-".into(),
                Entry::Header(h) => format!("#{h}"),
                Entry::Note(_, t) => format!("({t})"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn tidy_drops_empty_groups() {
        let e = vec![
            Entry::Separator,
            Entry::Header("Empty".into()),
            Entry::Separator,
            item("Copy"),
            Entry::Separator,
            Entry::Separator,
            Entry::Header("Gone".into()),
            Entry::Header("Kept".into()),
            item("Sudo"),
            Entry::Separator,
            Entry::Header("Tail".into()),
        ];
        assert_eq!(shape(&tidy(e)), "Copy - #Kept Sudo");
    }

    #[test]
    fn keys_skip_what_cant_be_picked() {
        let e = vec![
            Entry::Header("H".into()),
            item("Copy"),
            Entry::Item(Item::new("Paste", |_, _| {}).disabled(Some("Nothing to paste"))),
            Entry::Separator,
            item("Split"),
            Entry::Item(Item::submenu("Empty", Vec::new())),
        ];
        assert_eq!(next_enabled(&e, None, 1), Some(1));
        assert_eq!(next_enabled(&e, Some(1), 1), Some(4));
        assert_eq!(next_enabled(&e, Some(4), 1), Some(1), "wraps, past the empty submenu");
        assert_eq!(next_enabled(&e, None, -1), Some(4));
        assert_eq!(by_letter(&e, None, 's'), Some(4));
        assert_eq!(by_letter(&e, Some(1), 'c'), Some(1));
        assert_eq!(by_letter(&e, None, 'p'), None, "disabled");
    }
}
