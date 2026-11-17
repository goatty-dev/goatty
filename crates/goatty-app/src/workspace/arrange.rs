//! Rearranging panes and tabs (#124): swapping and rotating the panes of a
//! tab, moving a pane to a new tab or into another tab, bringing a whole
//! tab into another one, evening out one split, and the sizes in cells
//! that keep a pane usable: the resize actions move a divider by a few
//! cells, and no pane gets smaller than [`MIN_COLS`] × [`MIN_ROWS`] by
//! resizing or splitting.
//!
//! Panes only move between tabs on the same host ([`Spawner::same_host`]):
//! a tab's new panes start where its others are, and each goattyd keeps
//! the layout of its own tabs.

use gpui::{Context, Window};

use super::layout::{Axis, Direction, MinSize, Node, PaneId};
use super::{Tab, Workspace};
use crate::command_palette::{Ask, Field};
use crate::session::Spawner;
use crate::toast::{self, Toast};

/// The smallest pane resizing and splitting leave, in cells.
const MIN_COLS: f32 = 12.0;
const MIN_ROWS: f32 = 3.0;
/// How far the resize actions move a divider, in cells.
const RESIZE_COLS: f32 = 2.0;
const RESIZE_ROWS: f32 = 1.0;
/// The step (a share of the tab) before the first paint tells the cell size.
const RESIZE_FALLBACK: f32 = 0.05;

impl Workspace {
    // ---- sizes in cells ---------------------------------------------------------

    /// The cell size and padding of the focused pane (every pane of a window
    /// has the same font), and the panes' area, when they're known.
    fn cell_metrics(&self, cx: &gpui::App) -> Option<(gpui::Size<gpui::Pixels>, gpui::Edges<gpui::Pixels>, gpui::Size<gpui::Pixels>)> {
        let view = self.focused_view()?.read(cx);
        let area = self.panes_bounds.get().size;
        (area.width > gpui::px(0.) && area.height > gpui::px(0.)).then_some((view.cell_size()?, view.padding(), area))
    }

    /// The smallest a pane may be, as a share of the panes' area.
    pub(super) fn min_pane_size(&self, cx: &gpui::App) -> MinSize {
        let Some((cell, pad, area)) = self.cell_metrics(cx) else { return MinSize::NONE };
        let divider = super::DIVIDER;
        let w = MIN_COLS * f32::from(cell.width) + f32::from(pad.left + pad.right) + divider;
        let h = MIN_ROWS * f32::from(cell.height) + f32::from(pad.top + pad.bottom) + divider;
        MinSize { w: w / f32::from(area.width), h: h / f32::from(area.height) }
    }

    /// How far a resize action moves a divider towards `dir`, as a share of
    /// the panes' area.
    pub(super) fn resize_step(&self, dir: Direction, cx: &gpui::App) -> f32 {
        let Some((cell, _, area)) = self.cell_metrics(cx) else { return RESIZE_FALLBACK };
        match dir.axis() {
            Axis::Horizontal => RESIZE_COLS * f32::from(cell.width) / f32::from(area.width),
            Axis::Vertical => RESIZE_ROWS * f32::from(cell.height) / f32::from(area.height),
        }
    }

    /// Double-clicking a divider: the parts of its split get the same size.
    pub(super) fn equalize_split(&mut self, tab: u64, path: &[usize], cx: &mut Context<'_, Self>) {
        let Some(i) = self.tab_index(tab) else { return };
        self.tabs[i].tree.equalize_split(path);
        self.save_layout(cx);
        cx.notify();
    }

    // ---- within a tab -----------------------------------------------------------

    /// Let the focused pane trade places with its neighbour on the `dir` side.
    pub(super) fn swap_pane(&mut self, dir: Direction, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab_mut() else { return };
        let focused = tab.focused;
        let Some(other) = (!tab.zoomed).then(|| tab.tree.neighbor(focused, dir)).flatten() else {
            cx.propagate();
            return;
        };
        tab.tree.swap(focused, other);
        self.save_layout(cx);
        cx.notify();
    }

    /// Move every pane of the active tab one place on (or back); focus
    /// stays with the same pane.
    pub(super) fn rotate_panes(&mut self, forward: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab_mut() else { return };
        if tab.zoomed {
            tab.zoomed = false;
        }
        if !tab.tree.rotate(forward) {
            let toast = Toast::info("Nothing to rotate").body("Rotating moves the panes of a split round their places.");
            toast::show_in(window, cx, toast.key("pane.rotate"));
            return;
        }
        self.save_layout(cx);
        cx.notify();
    }

    // ---- between tabs -----------------------------------------------------------

    /// Why the panes of tab `from` can't go into tab `to` (by index), or
    /// `None` when they can.
    pub(super) fn cannot_join(&self, from: usize, to: usize) -> Option<&'static str> {
        let (a, b) = (&self.tabs[from], &self.tabs[to]);
        if a.connecting.is_some() || b.connecting.is_some() {
            return Some("Still connecting");
        }
        match (&a.host, &b.host) {
            (Some(x), Some(y)) if x.same_host(y) => None,
            _ => Some("On another host"),
        }
    }

    /// What a tab is called in a list of tabs: "Tab 2: vim".
    pub(super) fn tab_choice_label(&self, index: usize, cx: &gpui::App) -> String {
        format!("Tab {}: {}", index + 1, self.tab_label(&self.tabs[index], cx))
    }

    /// Take the focused pane out of its split into a tab of its own, after
    /// the current one.
    pub(super) fn move_pane_to_new_tab(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(id) = self.active_tab().map(|t| t.focused) {
            self.move_pane(id, None, window, cx);
        }
    }

    /// Move pane `id` into tab `to` (by id), next to its focused pane, or
    /// into a new tab after its own (`None`). A tab left without panes goes
    /// away; the shell keeps running either way.
    pub(super) fn move_pane(&mut self, id: PaneId, to: Option<u64>, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some((from, _)) = self.find_pane(id) else { return };
        let only = self.tabs[from].panes.len() == 1;
        let target = match to {
            None if only => {
                let toast = Toast::info("This pane is the tab's only one").body("Moving it to a new tab works for a pane of a split.");
                toast::show_in(window, cx, toast.key("pane.move"));
                return;
            }
            None => None,
            Some(t) => {
                let Some(i) = self.tab_index(t).filter(|&i| i != from) else { return };
                if let Some(why) = self.cannot_join(from, i) {
                    toast::show_in(window, cx, Toast::info("Can't move the pane there").body(why).key("pane.move"));
                    return;
                }
                Some(t)
            }
        };
        let min = self.min_pane_size(cx);

        let source = &mut self.tabs[from];
        let source_id = source.id;
        let Some(at) = source.panes.iter().position(|p| p.id == id) else { return };
        let pane = source.panes.remove(at);
        if !only {
            let pos = source.tree.leaves().iter().position(|&p| p == id).unwrap_or(0);
            source.tree.remove(id);
            if source.focused == id {
                let left = source.tree.leaves();
                source.focused = left[pos.saturating_sub(1).min(left.len() - 1)];
            }
            source.zoomed = false;
        }
        let host = source.host.clone();

        let dest = match target {
            None => {
                let tab = self.next_id();
                self.tabs.insert(from + 1, Tab::single(tab, id, pane, host));
                tab
            }
            Some(t) => {
                let i = self.tab_index(t).expect("checked above");
                let dest = &mut self.tabs[i];
                // Beside the focused pane when there's room, else below it.
                let dir = [Direction::Right, Direction::Down]
                    .into_iter()
                    .find(|&d| dest.tree.can_split(dest.focused, d, min))
                    .unwrap_or(Direction::Right);
                dest.tree.split(dest.focused, id, dir);
                dest.panes.push(pane);
                dest.focused = id;
                dest.zoomed = false;
                t
            }
        };
        if only && let Some(i) = self.tab_index(source_id) {
            self.remove_tab(i, window, cx);
        }
        if let Some(i) = self.tab_index(dest) {
            self.activate(i, window, cx);
        }
    }

    /// Bring every pane of tab `from` into tab `into` (by ids), to the right
    /// of its panes; tab `from` goes away.
    pub(super) fn merge_tab(&mut self, from: u64, into: u64, window: &mut Window, cx: &mut Context<'_, Self>) {
        let (Some(a), Some(b)) = (self.tab_index(from), self.tab_index(into)) else { return };
        if a == b {
            return;
        }
        if let Some(why) = self.cannot_join(a, b) {
            toast::show_in(window, cx, Toast::info("Can't bring that tab here").body(why).key("tab.merge"));
            return;
        }
        let source = &mut self.tabs[a];
        let panes = std::mem::take(&mut source.panes);
        let tree = std::mem::replace(&mut source.tree, Node::Leaf(0));
        let focused = source.focused;
        let dest = &mut self.tabs[b];
        dest.tree.join(tree, Direction::Right);
        dest.panes.extend(panes);
        dest.focused = focused;
        dest.zoomed = false;
        self.remove_tab(a, window, cx);
        if let Some(i) = self.tab_index(into) {
            self.activate(i, window, cx);
        }
    }

    // ---- asking where -------------------------------------------------------------

    /// "Move the pane to another tab…": the tabs it can go to (and a new
    /// one) in the palette.
    pub(super) fn ask_move_pane(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(tab) = self.active_tab() else { return };
        let (pane, only) = (tab.focused, tab.panes.len() == 1);
        let mut choices = Vec::new();
        if !only {
            choices.push(("new".to_string(), "A new tab".to_string(), "The pane gets a tab of its own".to_string()));
        }
        for i in (0..self.tabs.len()).filter(|&i| i != self.active && self.cannot_join(self.active, i).is_none()) {
            let panes = self.tabs[i].panes.len();
            let help = if panes == 1 { "Beside its pane".to_string() } else { format!("Beside its {panes} panes") };
            choices.push((self.tabs[i].id.to_string(), self.tab_choice_label(i, cx), help));
        }
        if choices.is_empty() {
            let toast = Toast::info("No other tab to move it to")
                .body("Panes can join a tab on the same host. Open another tab, or split this one first.");
            toast::show_in(window, cx, toast.key("pane.move"));
            return;
        }
        let me = cx.entity().downgrade();
        let ask = Ask::new("Move the pane to", vec![Field::choice("tab", "Which tab?", choices)], move |answers, window, cx| {
            let to = answers.get("tab").and_then(|v| v.parse::<u64>().ok());
            let _ = me.update(cx, |this, cx| this.move_pane(pane, to, window, cx));
        });
        self.open_ask(ask, window, cx);
    }

    /// "Bring another tab into this one…": which tab joins the active one.
    pub(super) fn ask_merge_tab(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(into) = self.active_tab().map(|t| t.id) else { return };
        let choices: Vec<_> = (0..self.tabs.len())
            .filter(|&i| i != self.active && self.cannot_join(i, self.active).is_none())
            .map(|i| {
                let panes = self.tabs[i].panes.len();
                let help = if panes == 1 { "Its pane goes to the right".to_string() } else { format!("Its {panes} panes go to the right") };
                (self.tabs[i].id.to_string(), self.tab_choice_label(i, cx), help)
            })
            .collect();
        if choices.is_empty() {
            let toast = Toast::info("No other tab to bring here").body("Tabs on the same host can be put side by side in one tab.");
            toast::show_in(window, cx, toast.key("tab.merge"));
            return;
        }
        let me = cx.entity().downgrade();
        let ask = Ask::new("Bring a tab into this one", vec![Field::choice("tab", "Which tab?", choices)], move |answers, window, cx| {
            let Some(from) = answers.get("tab").and_then(|v| v.parse::<u64>().ok()) else { return };
            let _ = me.update(cx, |this, cx| this.merge_tab(from, into, window, cx));
        });
        self.open_ask(ask, window, cx);
    }
}

impl Tab {
    /// A tab holding just `pane` (with id `pane_id`), on `host`.
    fn single(id: u64, pane_id: PaneId, pane: super::Pane, host: Option<Spawner>) -> Self {
        Tab {
            id,
            tree: Node::Leaf(pane_id),
            panes: vec![pane],
            focused: pane_id,
            zoomed: false,
            name: None,
            host,
            connecting: None,
            activity: None,
        }
    }
}
