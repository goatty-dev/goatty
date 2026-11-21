//! Tabs grouped by host.
//!
//! A window can hold tabs on several hosts, so the tab bar and the sidebar
//! group them by host the way browsers group tabs. Each host's tabs stay
//! next to each other, after a label with the host's name and color, and a
//! group can be folded down to its label. Groups appear only once the
//! window has a tab that isn't on this computer: a window with only local
//! tabs looks as it always did.
//!
//! - **Order**: the groups are in the order of their first tab
//!   ([`group_order`]). A new tab joins its host's group, and a tab that
//!   connects moves to its host's group. Dragging a tab in front of
//!   another group's first tab moves its whole group there.
//! - **Folding**: click the label. The active tab's group can't stay
//!   folded: folding it switches to a tab of the nearest other group, and
//!   going to a tab in a folded group (a click, Alt+number, the palette)
//!   unfolds it. The next/previous tab keys and the mouse wheel skip
//!   folded tabs. "Show only this host" folds every other group.
//! - **Colors**: a host's color is the one chosen for it (`tabs.host_colors`,
//!   from the Hosts page or the label's menu, #257), else one of the theme's
//!   colors picked from its name, so it's the same in every window and every
//!   run ([`host_color`]). This computer has no color.
//! - **Switching hosts**: the next/previous host actions go to the tab
//!   that was last active in that host's group.

use goatty_config::HostColor;
use gpui::{Context, Hsla, SharedString, Window};

use super::tab_bar::TabView;
use super::{Tab, Workspace, marks};
use crate::session::Spawner;
use crate::settings::AppSettings;
use crate::terminal::Activity;
use crate::toast::{self, Toast};

/// Which host a tab's sessions are on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HostKey {
    /// A window without goattyd (`--local`).
    Local,
    /// A goattyd connection, by identity (the pool shares one per host).
    Daemon(usize),
    /// Plain SSH to the host, through `base`.
    Plain(String, Box<HostKey>),
    /// A tab still connecting (by tab id): a group of its own until then.
    Connecting(u64),
}

impl HostKey {
    pub(super) fn of(spawner: &Spawner) -> Self {
        match spawner {
            Spawner::Local(_) => HostKey::Local,
            Spawner::Daemon(conn) => HostKey::Daemon(std::sync::Arc::as_ptr(conn) as usize),
            Spawner::Plain { host, base } => HostKey::Plain(host.clone(), Box::new(HostKey::of(base))),
        }
    }
}

/// A host's name for messages: "this computer", the SSH host, the tailnet name.
pub(super) fn host_label(spawner: &Spawner) -> String {
    match spawner {
        Spawner::Local(_) => "this computer".into(),
        Spawner::Daemon(conn) if conn.endpoint().is_local() => "this computer".into(),
        Spawner::Daemon(conn) => conn.endpoint().label().to_string(),
        Spawner::Plain { host, .. } => host.clone(),
    }
}

/// One host's tabs, as drawn.
pub(super) struct Group {
    pub key: HostKey,
    /// The host as the user named it ("prod", "This computer").
    pub label: SharedString,
    /// More about it: `user@hostname`, "plain SSH", "Connecting…".
    pub detail: Option<SharedString>,
    pub local: bool,
    pub color: Hsla,
    /// Its tabs' indices and ids, in order.
    pub tabs: Vec<usize>,
    pub ids: Vec<u64>,
    pub folded: bool,
    /// Holds the active tab.
    pub active: bool,
    /// The strongest activity mark among its tabs (shown when folded).
    pub activity: Option<Activity>,
    /// A coding agent in one of its tabs waits for the user (shown when
    /// folded, before the activity).
    pub waiting: bool,
}

/// What the tab strip (or sidebar) lists, in order.
pub(super) enum StripItem {
    Group(Group),
    Tab(TabView),
}

/// The color of the tab group of the host named `name`: its chosen color,
/// else the automatic one.
pub(crate) fn host_rgb(name: &str, settings: &AppSettings) -> goatty_config::Color {
    match settings.client.tabs.host_color(name).unwrap_or_else(|| HostColor::automatic(name)) {
        HostColor::Ansi(i) => settings.colors.ansi[usize::from(i) % 16],
        HostColor::Custom(c) => c,
    }
}

/// [`host_rgb`] as the tab strip draws it.
pub(crate) fn host_color(name: &str, settings: &AppSettings) -> Hsla {
    gpui::rgb(host_rgb(name, settings).0).into()
}

impl Workspace {
    pub(super) fn host_key(&self, tab: &Tab) -> HostKey {
        match &tab.host {
            Some(spawner) => HostKey::of(spawner),
            None => HostKey::Connecting(tab.id),
        }
    }

    /// Is tab `tab` on this computer (or connecting to it)?
    fn is_local(tab: &Tab, cx: &gpui::App) -> bool {
        match (&tab.host, &tab.connecting) {
            (Some(Spawner::Local(_)), _) => true,
            (Some(Spawner::Daemon(conn)), _) => conn.endpoint().is_local(),
            (Some(Spawner::Plain { .. }), _) => false,
            (None, Some((view, _))) => view.read(cx).is_local(),
            (None, None) => true,
        }
    }

    /// Whether the tabs show as groups: some tab is on another host.
    pub(super) fn groups_shown(&self, cx: &gpui::App) -> bool {
        self.tabs.iter().any(|t| !Self::is_local(t, cx))
    }

    /// The host's name, more about it, and whether it's this computer.
    fn host_names(tab: &Tab, cx: &gpui::App) -> (String, Option<String>, bool) {
        let local = Self::is_local(tab, cx);
        if local && !matches!(tab.host, Some(Spawner::Plain { .. })) {
            return ("This computer".into(), None, true);
        }
        match (&tab.host, &tab.connecting) {
            (Some(Spawner::Daemon(conn)), _) => {
                let h = conn.host();
                let detail = (!h.hostname.is_empty()).then(|| format!("{}@{}", h.user, h.hostname));
                (conn.endpoint().label().to_string(), detail, false)
            }
            (Some(Spawner::Plain { host, .. }), _) => (host.clone(), Some("Plain SSH".into()), false),
            (None, Some((view, _))) => (view.read(cx).host_label().to_string(), Some("Connecting…".into()), false),
            _ => ("This computer".into(), None, true),
        }
    }

    /// The groups, in order (the tabs are grouped already: see [`Self::regroup`]).
    pub(super) fn groups(&self, cx: &gpui::App) -> Vec<Group> {
        let settings = cx.global::<AppSettings>();
        let mut out: Vec<Group> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            let key = self.host_key(tab);
            let activity = tab.activity.filter(|&a| i != self.active && marks(&settings.client.tabs, a));
            let waiting = tab
                .panes
                .iter()
                .any(|p| p.view.read(cx).agent_status(p.seen).is_some_and(|s| s.state == goatty_term::StatusState::Blocked));
            if let Some(g) = out.last_mut().filter(|g| g.key == key) {
                g.tabs.push(i);
                g.ids.push(tab.id);
                g.active |= i == self.active;
                g.activity = g.activity.max(activity);
                g.waiting |= waiting;
                continue;
            }
            let (label, detail, local) = Self::host_names(tab, cx);
            let color = if local {
                // Quiet, like its chip.
                crate::ui::style::Ui::get(cx).muted.opacity(0.5)
            } else {
                host_color(&label, settings)
            };
            out.push(Group {
                folded: self.folded.contains(&key),
                key,
                label: label.into(),
                detail: detail.map(Into::into),
                local,
                color,
                tabs: vec![i],
                ids: vec![tab.id],
                active: i == self.active,
                activity,
                waiting,
            });
        }
        out
    }

    /// The strip's items: each group's label and its tabs unless folded,
    /// or only the tabs when groups don't show.
    pub(super) fn strip_items(&self, cx: &gpui::App) -> Vec<StripItem> {
        let mut views: Vec<Option<TabView>> = self.tab_views(cx).into_iter().map(Some).collect();
        if !self.groups_shown(cx) {
            return views.into_iter().flatten().map(StripItem::Tab).collect();
        }
        let mut out = Vec::new();
        for g in self.groups(cx) {
            let (color, folded, tabs) = (g.color, g.folded, g.tabs.clone());
            out.push(StripItem::Group(g));
            if folded {
                continue;
            }
            let last = tabs.last().copied();
            for i in tabs {
                if let Some(mut v) = views[i].take() {
                    v.group = Some(color);
                    v.group_end = Some(i) == last;
                    out.push(StripItem::Tab(v));
                }
            }
        }
        out
    }

    /// Where tab `index` is among the strip's children (for scrolling it
    /// into view).
    pub(super) fn strip_index(&self, index: usize, cx: &gpui::App) -> usize {
        if !self.groups_shown(cx) {
            return index;
        }
        let mut n = 0;
        for g in self.groups(cx) {
            n += 1;
            if g.folded {
                continue;
            }
            if let Some(at) = g.tabs.iter().position(|&i| i == index) {
                return n + at;
            }
            n += g.tabs.len();
        }
        n
    }

    /// Is tab `index` folded away?
    pub(super) fn hidden(&self, index: usize, cx: &gpui::App) -> bool {
        index != self.active && self.groups_shown(cx) && self.folded.contains(&self.host_key(&self.tabs[index]))
    }

    /// Keep each host's tabs together, in the order of the groups' first
    /// tabs, and forget folds of hosts that are gone. The active tab stays
    /// active.
    pub(super) fn regroup(&mut self) {
        let keys: Vec<HostKey> = self.tabs.iter().map(|t| self.host_key(t)).collect();
        self.folded.retain(|k| keys.contains(k));
        self.group_focus.retain(|(k, _)| keys.contains(k));
        let order = group_order(&keys);
        if order.iter().enumerate().all(|(i, &o)| i == o) {
            return;
        }
        let active = self.tabs.get(self.active).map(|t| t.id);
        let mut old: Vec<Option<Tab>> = std::mem::take(&mut self.tabs).into_iter().map(Some).collect();
        self.tabs = order.into_iter().filter_map(|i| old[i].take()).collect();
        self.active = active.and_then(|id| self.tab_index(id)).unwrap_or(0);
    }

    /// Tab `index` became the active one: its group unfolds and remembers it.
    pub(super) fn group_activated(&mut self, index: usize) {
        let key = self.host_key(&self.tabs[index]);
        let id = self.tabs[index].id;
        self.folded.retain(|k| *k != key);
        match self.group_focus.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = id,
            None => self.group_focus.push((key, id)),
        }
    }

    /// The tab to show for group `g`: the one last active there, else its first.
    fn group_tab(&self, g: &Group) -> usize {
        let remembered = self.group_focus.iter().find(|(k, _)| *k == g.key).map(|(_, id)| *id);
        remembered.and_then(|id| g.ids.iter().position(|&t| t == id)).map_or(g.tabs[0], |at| g.tabs[at])
    }

    /// Fold (or unfold) the group of `key`. Folding the active tab's group
    /// first switches to the nearest other group.
    pub(super) fn fold_group(&mut self, key: &HostKey, fold: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !fold {
            self.folded.retain(|k| k != key);
            cx.notify();
            return;
        }
        let groups = self.groups(cx);
        let Some(at) = groups.iter().position(|g| g.key == *key) else { return };
        if groups[at].active {
            // The next group, else the one before; an unfolded one first.
            let others: Vec<usize> = (at + 1..groups.len()).chain((0..at).rev()).collect();
            let Some(&to) = others.iter().find(|&&o| !groups[o].folded).or(others.first()) else {
                let toast = Toast::info("Nothing else to show").body("Folding hides a host's tabs while you work in another host's.");
                toast::show_in(window, cx, toast.key("tab.fold"));
                return;
            };
            let index = self.group_tab(&groups[to]);
            self.activate(index, window, cx);
        }
        if !self.folded.contains(key) {
            self.folded.push(key.clone());
        }
        cx.notify();
    }

    /// Fold every group but `key`'s, going to one of its tabs.
    pub(super) fn show_only_group(&mut self, key: &HostKey, window: &mut Window, cx: &mut Context<'_, Self>) {
        let groups = self.groups(cx);
        let Some(g) = groups.iter().find(|g| g.key == *key) else { return };
        if !g.active {
            let index = self.group_tab(g);
            self.activate(index, window, cx);
        }
        self.folded = groups.into_iter().map(|g| g.key).filter(|k| k != key).collect();
        cx.notify();
    }

    pub(super) fn show_all_groups(&mut self, cx: &mut Context<'_, Self>) {
        self.folded.clear();
        cx.notify();
    }

    /// The active tab's host, if groups show.
    pub(super) fn active_group_key(&self, cx: &gpui::App) -> Option<HostKey> {
        (self.groups_shown(cx)).then(|| self.active_tab().map(|t| self.host_key(t))).flatten()
    }

    /// Go `steps` groups on (negative: back), to the tab last active there.
    pub(super) fn step_group(&mut self, steps: isize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let groups = self.groups(cx);
        if groups.len() < 2 {
            cx.propagate();
            return;
        }
        let Some(at) = groups.iter().position(|g| g.active) else { return };
        let to = (at as isize + steps).clamp(0, groups.len() as isize - 1) as usize;
        if to != at {
            let index = self.group_tab(&groups[to]);
            self.activate(index, window, cx);
        }
    }

    /// A new tab on the host of group `key`, at the end of its group.
    pub(super) fn new_tab_in_group(&mut self, key: &HostKey, window: &mut Window, cx: &mut Context<'_, Self>) {
        // A connecting tab has no host yet: the new tab would open elsewhere.
        if matches!(key, HostKey::Connecting(_)) {
            return;
        }
        let Some(last) = self.tabs.iter().rposition(|t| self.host_key(t) == *key) else { return };
        self.activate(last, window, cx);
        self.spawn(super::Placement::NewTab, window, cx);
    }
}

/// The order that keeps equal keys together, each run where its first
/// member was (a stable sort by the first appearance of the key): indices
/// into `keys`.
pub(super) fn group_order<K: PartialEq>(keys: &[K]) -> Vec<usize> {
    let mut firsts: Vec<&K> = Vec::new();
    for k in keys {
        if !firsts.contains(&k) {
            firsts.push(k);
        }
    }
    let mut order = Vec::with_capacity(keys.len());
    for f in firsts {
        order.extend(keys.iter().enumerate().filter(|(_, k)| *k == f).map(|(i, _)| i));
    }
    order
}

/// The next (`forward`) or previous tab after `from` that isn't `hidden`,
/// coming round at the ends; `None` when there is no other one.
pub(super) fn cycle_visible(from: usize, forward: bool, hidden: &[bool]) -> Option<usize> {
    let n = hidden.len();
    (1..n).map(|k| if forward { (from + k) % n } else { (from + n - k) % n }).find(|&i| !hidden[i])
}

/// From tab `from`, `steps` tabs on (negative: back) counting only the
/// tabs that aren't `hidden`, stopping at the ends.
pub(super) fn step_visible(from: usize, steps: isize, hidden: &[bool]) -> usize {
    let mut at = from;
    let dir = steps.signum();
    for _ in 0..steps.unsigned_abs() {
        let mut next = at as isize + dir;
        while next >= 0 && (next as usize) < hidden.len() && hidden[next as usize] {
            next += dir;
        }
        if next < 0 || next as usize >= hidden.len() {
            break;
        }
        at = next as usize;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_keep_their_first_tabs_place() {
        assert_eq!(group_order(&["a", "b", "a", "c", "b"]), [0, 2, 1, 4, 3]);
        assert_eq!(group_order(&["a", "a", "b"]), [0, 1, 2]);
        assert_eq!(group_order::<&str>(&[]), Vec::<usize>::new());
        // A tab of b dragged in front of a's first tab: b's group goes first.
        assert_eq!(group_order(&["b", "a", "a", "b"]), [0, 3, 1, 2]);
    }

    #[test]
    fn stepping_skips_folded_tabs() {
        let hidden = [false, true, true, false, false];
        assert_eq!(step_visible(0, 1, &hidden), 3);
        assert_eq!(step_visible(3, -1, &hidden), 0);
        assert_eq!(step_visible(0, 2, &hidden), 4);
        assert_eq!(step_visible(4, 5, &hidden), 4, "stops at the end");
        assert_eq!(step_visible(0, -1, &hidden), 0);
        assert_eq!(step_visible(3, 0, &hidden), 3);
        assert_eq!(step_visible(0, 1, &[false, true]), 0, "nothing visible after");
    }

    #[test]
    fn cycling_skips_hidden_tabs_and_comes_round() {
        // A1 A2 | B1 folded.
        let hidden = [false, false, true];
        assert_eq!(cycle_visible(1, true, &hidden), Some(0));
        assert_eq!(cycle_visible(0, false, &hidden), Some(1));
        assert_eq!(cycle_visible(0, true, &[false, true]), None);
        assert_eq!(cycle_visible(0, true, &[]), None);
    }
}
