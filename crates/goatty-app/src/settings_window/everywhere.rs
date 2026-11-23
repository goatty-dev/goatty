//! "Use goatty's shell in every terminal" on the Shell page (#283): one
//! line in the host's `.zshenv` makes every interactive zsh there run
//! goatty's shell (`goatty_shell::everywhere`). The switch shows what the
//! file has (the user may add or remove the line by hand); turning it on
//! opens a dialog with the exact lines, the file and what changes, and only
//! the dialog's button writes the file. Turning it off removes the line
//! right away (goatty keeps a backup).

use goatty_shell::everywhere::{Action, State, Status};
use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, IntoElement, MouseButton, ParentElement, SharedString, Styled, Window, div, prelude::*,
    px,
};

use super::SettingsWindow;
use super::kit::{self, Row, modal, row};
use crate::toast::{self, Toast};
use crate::ui::controls::{ButtonKind, button, button_with, toggle};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};

/// The row's label (also its search entry).
pub const LABEL: &str = "Use goatty's shell in every terminal";

pub struct EverywhereState {
    pub status: Option<Status>,
    pub loading: bool,
    /// Turning it on or off right now.
    pub busy: bool,
    pub error: Option<String>,
    /// The consent dialog is open.
    pub dialog: bool,
    pub(super) modal_focus: FocusHandle,
    /// Where focus goes back to when the dialog closes.
    opener: Option<FocusHandle>,
    pub(super) restore: Option<FocusHandle>,
    /// Numbers requests: a reply for another host (or an older request) is dropped.
    request: u64,
}

impl EverywhereState {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> Self {
        Self {
            status: None,
            loading: false,
            busy: false,
            error: None,
            dialog: false,
            modal_focus: cx.focus_handle(),
            opener: None,
            restore: None,
            request: 0,
        }
    }

    /// Forget what the host said (another host).
    pub fn reset(&mut self) {
        self.status = None;
        self.error = None;
        self.loading = false;
        self.busy = false;
        self.dialog = false;
        self.request += 1;
    }
}

/// goatty's lines, in the terminal's font: what the user agrees to.
pub fn code_lines(ui: Ui, mono: SharedString, lines: &[String]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .p(px(12.))
        .rounded(px(radius::CONTROL))
        .bg(ui.bg)
        .border_1()
        .border_color(ui.divider)
        .font_family(mono)
        .text_size(px(text::CAPTION))
        .text_color(ui.text)
        .children(lines.iter().map(|l| div().min_w_0().child(SharedString::from(l.clone()))))
}

/// One point of what changes, with its icon.
pub fn point(ui: Ui, icon: Icon, title: impl Into<SharedString>, body: impl Into<SharedString>) -> impl IntoElement {
    div().flex().items_start().gap(px(10.)).child(icon.svg().flex_shrink_0().size(px(16.)).mt(px(1.)).text_color(ui.accent_text)).child(
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .min_w_0()
            .child(div().text_size(px(text::LABEL)).font_weight(FontWeight::MEDIUM).text_color(ui.text).child(title.into()))
            .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child(body.into())),
    )
}

impl SettingsWindow {
    /// Ask the host whether its `.zshenv` has goatty's line.
    pub fn load_everywhere(&mut self, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        if self.everywhere.loading || self.everywhere.busy {
            return;
        }
        self.everywhere.loading = true;
        self.everywhere.request += 1;
        let request = self.everywhere.request;
        cx.spawn(async move |this, cx| {
            let label = link.label();
            let result = cx.background_executor().spawn(async move { link.everywhere(Action::Status) }).await;
            let _ = this.update(cx, |v, cx| {
                let e = &mut v.everywhere;
                if e.request != request {
                    return;
                }
                e.loading = false;
                match result {
                    Ok(s) => {
                        e.status = Some(s);
                        e.error = None;
                    }
                    Err(err) => e.error = Some(too_old(&format!("{err:#}"), &label)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Turn it on or off on the host, then say how it went.
    fn set_everywhere(&mut self, action: Action, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        if self.everywhere.busy {
            return;
        }
        self.everywhere.busy = true;
        self.everywhere.request += 1;
        let request = self.everywhere.request;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let label = link.label();
            let result = cx.background_executor().spawn(async move { link.everywhere(action) }).await;
            let _ = this.update(cx, |v, cx| {
                let e = &mut v.everywhere;
                if e.request != request {
                    return;
                }
                e.busy = false;
                match result {
                    Ok(s) => {
                        let t = match (action, s.state) {
                            (Action::On, State::On) => Toast::success(format!("goatty's shell is on in every terminal of {label}"))
                                .body(format!("Added the line to {}. New terminals use it; open ones don't change.", s.file)),
                            (_, State::Off) => Toast::success(format!("Back to plain zsh outside goatty on {label}"))
                                .body(format!("Removed goatty's line from {}. Open shells keep goatty's until they exit.", s.file)),
                            _ => Toast::info(format!("{} didn't change", s.file)),
                        };
                        toast::show(cx, t.key("shell.everywhere"));
                        if action == Action::On {
                            v.close_everywhere_dialog(cx);
                        }
                        let e = &mut v.everywhere;
                        e.status = Some(s);
                        e.error = None;
                    }
                    Err(err) => {
                        let what = if action == Action::On { "Couldn't add the line" } else { "Couldn't remove the line" };
                        toast::show(cx, Toast::error(what).body(too_old(&format!("{err:#}"), &label)).key("shell.everywhere"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn everywhere_modal_open(&self) -> bool {
        self.everywhere.dialog
    }

    fn open_everywhere_dialog(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.everywhere.opener = window.focused(cx);
        window.focus(&self.everywhere.modal_focus, cx);
        self.everywhere.dialog = true;
        cx.notify();
    }

    pub(super) fn close_everywhere_dialog(&mut self, cx: &mut Context<'_, Self>) {
        if !self.everywhere.dialog {
            return;
        }
        self.everywhere.dialog = false;
        self.everywhere.restore = Some(self.everywhere.opener.take().unwrap_or_else(|| self.focus.clone()));
        cx.notify();
    }

    /// The switch on the Shell page. `zsh_managed`: goatty's managed zsh is
    /// on (in the draft) and the shell is zsh.
    pub(super) fn everywhere_row(&self, ui: Ui, zsh_managed: bool, cx: &Context<'_, Self>) -> Row {
        let e = &self.everywhere;
        let r = row(ui, LABEL).icon(Icon::SquareTerminal);
        let Some(s) = &e.status else {
            let desc = match &e.error {
                Some(err) => format!("Couldn't check: {err}"),
                None => "IDE terminals, other terminals, tmux and SSH logins too · checking…".to_owned(),
            };
            return r.desc(desc).disabled(true).control(toggle("everywhere", false, ui, |_, _| {}).aria_label(LABEL));
        };
        let line_of = |n: usize, l: &str| format!("line {n}: {l}");
        let (desc, can_toggle) = match s.state {
            State::On if s.active && zsh_managed => (format!("On · {} starts goatty's shell in every new zsh", s.file), true),
            State::On => (format!("Does nothing while goatty's managed zsh is off · turn it off to remove the line from {}", s.file), true),
            State::Changed => {
                let (n, l) = &s.changed[0];
                (format!("{} has a goatty line goatty didn't write ({}): edit it there", s.file, line_of(*n, l)), false)
            }
            State::Off => match &s.unavailable {
                Some(why) => (why.clone(), false),
                None if !zsh_managed => ("Needs goatty's managed zsh".to_owned(), false),
                None => {
                    (format!("IDE terminals, other terminals, tmux and SSH logins get the same shell · adds one line to {}", s.file), true)
                }
            },
        };
        let on = s.state == State::On;
        let mut r = r.desc(desc).disabled(!can_toggle || e.busy).control(
            toggle(
                "everywhere",
                on,
                ui,
                self.act_window(cx, move |v, window, cx| {
                    if on {
                        v.set_everywhere(Action::Off, cx);
                    } else {
                        v.open_everywhere_dialog(window, cx);
                    }
                }),
            )
            .aria_label(LABEL),
        );
        if on && !s.warnings.is_empty() {
            r = r.below(div().flex().flex_col().gap(px(4.)).children(s.warnings.iter().map(|w| {
                div()
                    .flex()
                    .items_start()
                    .gap(px(6.))
                    .text_size(px(text::SMALL))
                    .text_color(ui.warning)
                    .child(Icon::TriangleAlert.svg().flex_shrink_0().size(px(13.)).mt(px(2.)).text_color(ui.warning))
                    .child(div().min_w_0().child(SharedString::from(w.clone())))
            })));
        }
        r
    }

    /// The consent dialog: the exact lines and file, what changes, and the
    /// button that writes them.
    pub(super) fn everywhere_modal(&self, ui: Ui, cx: &Context<'_, Self>) -> Option<AnyElement> {
        if !self.everywhere.dialog {
            return None;
        }
        let s = self.everywhere.status.as_ref()?;
        let h = self.host_value()?;
        let host = self.host_label();
        let mono = kit::mono(cx);
        let user_rc = h.shell.zsh.user_rc;
        let mut points = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(point(
                ui,
                Icon::FileText,
                "Your ~/.zshrc stops running there",
                "As in goatty's tabs today. The PATH folders and variables it sets up still come along. Put functions and key bindings in ~/.config/goatty/zshrc, or run it guarded:",
            ))
            .child(
                row(ui, "Also run my ~/.zshrc, guarded")
                    .desc("Keeps its functions and key bindings; frameworks that would fight goatty's plugins stay out. Applied with your other shell changes")
                    .control(toggle("everywhere-user-rc", user_rc, ui, self.host(cx, |h| h.shell.zsh.user_rc = !h.shell.zsh.user_rc)).aria_label("Also run my ~/.zshrc, guarded")),
            );
        if h.prompt.plain_elsewhere() {
            points = points.child(point(
                ui,
                Icon::Palette,
                "Prompt icons stay in goatty",
                "They need a Nerd Font, so other terminals show text instead. Settings → Prompt can keep them everywhere.",
            ));
        }
        points = points.child(point(
            ui,
            Icon::RotateCcw,
            "Easy to undo",
            "Turn this off and goatty removes exactly these lines (keeping a backup). Plain zsh for one shell: GOATTY_SHELL_MANAGED=0 zsh",
        ));
        let body = div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(div().text_size(px(text::BODY)).text_color(ui.text).child(SharedString::from(format!(
                "Every new interactive zsh on {host} runs goatty's shell, with your prompt, plugins and history: VS Code and other editors, other terminals, tmux, SSH logins. Scripts and zsh -c stay plain, and your login shell doesn't change."
            ))))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().text_size(px(text::LABEL)).font_weight(FontWeight::MEDIUM).text_color(ui.text).child(SharedString::from(format!(
                        "goatty adds these lines to the end of {}",
                        s.file
                    ))))
                    .child(code_lines(ui, mono, &s.lines)),
            )
            .child(points);
        let busy = self.everywhere.busy;
        let footer = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(kit::hint(ui, "Open shells don't change").mr(px(8.)))
            .child(button("everywhere-cancel", "Cancel", ButtonKind::Normal, ui, self.act(cx, |v, cx| v.close_everywhere_dialog(cx))))
            .child(if busy {
                button_with("everywhere-add", Some(Icon::Loader), "Adding…", ButtonKind::Primary, ui, |_, _| {}).opacity(0.7)
            } else {
                button("everywhere-add", "Add the lines", ButtonKind::Primary, ui, self.act(cx, |v, cx| v.set_everywhere(Action::On, cx)))
            });
        Some(
            modal(ui, "everywhere-modal", LABEL, body, footer)
                .track_focus(&self.everywhere.modal_focus)
                .on_mouse_down(MouseButton::Left, cx.listener(|v, _, _, cx| v.close_everywhere_dialog(cx)))
                .into_any_element(),
        )
    }
}

/// An error from a goattyd that doesn't know the request: say to update it.
fn too_old(err: &str, host: &str) -> String {
    if err.contains("unknown variant") {
        format!("goattyd on {host} is older than this goatty. Update it to use this.")
    } else {
        err.to_owned()
    }
}
