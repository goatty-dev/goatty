//! "Close this?": asked before closing a pane or tab whose shell is
//! running a program (#119), so a build, an editor with unsaved changes or
//! an SSH session doesn't end by accident.
//!
//! The [`Workspace`](crate::workspace::Workspace) decides when to ask (the
//! `panes.confirm_close` setting, and a foreground program that isn't a
//! shell) and shows a [`CloseConfirm`] over the window. It emits
//! [`CloseConfirmEvent::Done`]; "Don't ask again" turns the setting off.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, KeyDownEvent, Window, actions, div, prelude::*, px,
};

use crate::settings::AppSettings;
use crate::ui::controls::{ButtonKind, button, toggle};
use crate::ui::focus::Stops;
use crate::ui::icons::Icon;
use crate::ui::style::Ui;

pub const CONTEXT: &str = "CloseConfirm";

const WIDTH: f32 = 460.;

actions!(close_confirm, [Cancel, Detach, FocusNext, FocusPrev]);

pub fn bindings() -> Vec<KeyBinding> {
    let c = Some(CONTEXT);
    vec![
        KeyBinding::new("escape", Cancel, c),
        KeyBinding::new("d", Detach, c),
        KeyBinding::new("alt-d", Detach, c),
        KeyBinding::new("tab", FocusNext, c),
        KeyBinding::new("right", FocusNext, c),
        KeyBinding::new("down", FocusNext, c),
        KeyBinding::new("shift-tab", FocusPrev, c),
        KeyBinding::new("left", FocusPrev, c),
        KeyBinding::new("up", FocusPrev, c),
    ]
}

/// What is being closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closing {
    Pane,
    /// A tab with this many panes.
    Tab {
        panes: usize,
    },
    /// This many tabs ("Close the other tabs").
    Tabs {
        tabs: usize,
    },
}

/// What the user chose in the close confirmation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseChoice {
    Cancel,
    /// Detach without ending the running program(s).
    Detach,
    /// Close and end the running program(s).
    Close,
}

pub enum CloseConfirmEvent {
    Done(CloseChoice),
}

pub struct CloseConfirm {
    closing: Closing,
    /// The running programs (not shells), one per busy pane.
    programs: Vec<String>,
    /// Whether the target is backed by `goattyd` and can be detached instead.
    can_detach: bool,
    dont_ask: bool,
    focus: FocusHandle,
    stops: Stops,
}

impl EventEmitter<CloseConfirmEvent> for CloseConfirm {}

impl Focusable for CloseConfirm {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.stops.last().cloned().unwrap_or_else(|| self.focus.clone())
    }
}

/// The dialog's title and explanation.
pub fn describe(closing: Closing, programs: &[String]) -> (String, String) {
    let list = match programs {
        [] => "a program".to_string(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    };
    let title = match closing {
        Closing::Pane => "Close this pane?".to_string(),
        Closing::Tab { panes: 1 } => "Close this tab?".to_string(),
        Closing::Tab { panes } => format!("Close this tab and its {panes} panes?"),
        Closing::Tabs { tabs } => format!("Close {tabs} tabs?"),
    };
    let verb = if programs.len() > 1 { "are" } else { "is" };
    let body = format!("{list} {verb} still running. Closing ends it, and anything it hasn't saved is lost.");
    (title, body)
}

impl CloseConfirm {
    pub fn new(closing: Closing, programs: Vec<String>, can_detach: bool, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let focus = cx.focus_handle();
        let stops = Stops::new(3 + usize::from(can_detach), cx);
        stops.focus(stops.len() - 1, window, cx);
        Self { closing, programs, can_detach, dont_ask: false, focus, stops }
    }

    fn finish(&mut self, choice: CloseChoice, cx: &mut Context<'_, Self>) {
        if choice == CloseChoice::Close && self.dont_ask {
            AppSettings::update(cx, |s| s.panes.confirm_close = false);
        }
        cx.emit(CloseConfirmEvent::Done(choice));
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.finish(CloseChoice::Cancel, cx);
    }

    fn detach(&mut self, _: &Detach, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.can_detach {
            self.finish(CloseChoice::Detach, cx);
        }
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stops.step(1, window, cx);
    }

    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stops.step(-1, window, cx);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        // Focused controls handle Enter and Space through GPUI's on_click;
        // Enter here is only the fallback when focus sits on the dialog frame.
        if ev.keystroke.key == "enter" && !ev.keystroke.modifiers.modified() && !self.stops.any_focused(window) {
            cx.stop_propagation();
            self.finish(CloseChoice::Close, cx);
        }
    }

    fn done(&self, cx: &mut Context<'_, Self>, choice: CloseChoice) -> impl Fn(&mut Window, &mut App) + 'static {
        let this = cx.entity().downgrade();
        move |_, cx| {
            let _ = this.update(cx, |this, cx| this.finish(choice, cx));
        }
    }
}

impl Render for CloseConfirm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let ui_font = cx.global::<AppSettings>().ui_font.clone();
        let (title, mut body) = describe(self.closing, &self.programs);
        if self.can_detach {
            let pron = if self.programs.len() > 1 { "them" } else { "it" };
            body.push_str(&format!(" Detaching keeps {pron} running in the background."));
        }
        let label = match self.closing {
            Closing::Pane => "Close pane",
            Closing::Tab { .. } => "Close tab",
            Closing::Tabs { .. } => "Close tabs",
        };
        let detach_label = match self.closing {
            Closing::Pane => "Detach pane (D)",
            Closing::Tab { .. } => "Detach tab (D)",
            Closing::Tabs { .. } => "Detach tabs (D)",
        };
        let this = cx.entity().downgrade();
        let flip = move |_: &mut Window, cx: &mut App| {
            let _ = this.update(cx, |this, cx| {
                this.dont_ask = !this.dont_ask;
                cx.notify();
            });
        };

        let close_idx = self.stops.len().saturating_sub(1);

        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::detach))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.finish(CloseChoice::Cancel, cx)))
            .occlude()
            .w(px(WIDTH))
            .max_w_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .font_family(ui_font)
            .text_sm()
            .text_color(ui.text)
            .bg(ui.bg)
            .border_1()
            .border_color(ui.warning.opacity(0.6))
            .rounded_lg()
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::TriangleAlert.svg().size(px(18.)).text_color(ui.warning))
                    .child(div().text_base().font_weight(FontWeight::BOLD).child(title)),
            )
            .child(div().text_color(ui.muted).child(body))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.stops.wrap(0, toggle("close-dont-ask", self.dont_ask, ui, flip).aria_label("Don't ask again"), ui, ui.bg))
                    .child(div().text_xs().text_color(ui.muted).child("Don't ask again (Settings → Workspace)")),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(self.stops.wrap(
                        1,
                        button("close-cancel", "Cancel", ButtonKind::Normal, ui, self.done(cx, CloseChoice::Cancel)),
                        ui,
                        ui.bg,
                    ))
                    .when(self.can_detach, |d| {
                        d.child(self.stops.wrap(
                            2,
                            button("close-detach", detach_label, ButtonKind::Normal, ui, self.done(cx, CloseChoice::Detach)),
                            ui,
                            ui.bg,
                        ))
                    })
                    .child(self.stops.wrap(
                        close_idx,
                        button("close-ok", label, ButtonKind::Danger, ui, self.done(cx, CloseChoice::Close)),
                        ui,
                        ui.bg,
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_name_the_programs() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (title, body) = describe(Closing::Pane, &p(&["nvim"]));
        assert_eq!(title, "Close this pane?");
        assert!(body.starts_with("nvim is still running."), "{body}");
        let (title, body) = describe(Closing::Tab { panes: 3 }, &p(&["cargo", "ssh", "htop"]));
        assert_eq!(title, "Close this tab and its 3 panes?");
        assert!(body.starts_with("cargo, ssh and htop are still running."), "{body}");
        assert_eq!(describe(Closing::Tab { panes: 1 }, &p(&["make"])).0, "Close this tab?");
        assert_eq!(describe(Closing::Tabs { tabs: 4 }, &p(&["make"])).0, "Close 4 tabs?");
    }
}
