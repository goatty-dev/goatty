//! "Use goatty's shell in every terminal" on the Shell page (#283): one
//! line in the host's `.zshenv` makes every interactive zsh there run
//! goatty's shell (`goatty_shell::everywhere`). The switch shows what the
//! files have (the user may add or remove the line by hand); turning it on
//! opens a dialog with the exact lines, the files and what changes, and only
//! the dialog's button writes them. Turning it off removes the lines right
//! away (goatty keeps a backup).

use goatty_shell::everywhere::{Action, State, Status};
use gpui::{
    AnyElement, ClipboardItem, Context, FocusHandle, FontWeight, IntoElement, MouseButton, ParentElement, SharedString, Styled, Window,
    div, prelude::*, px,
};

use super::kit::{self, Row, modal, row};
use super::{Page, SettingsWindow};
use crate::toast::{self, Toast};
use crate::ui::controls::{ButtonKind, Tone, banner, button, button_with, icon_button, toggle};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};

/// The row's label (also its search entry and the dialog's title).
pub const LABEL: &str = "Use goatty's shell in every terminal";

pub struct EverywhereState {
    pub status: Option<Status>,
    pub loading: bool,
    /// Turning it on or off right now.
    pub busy: bool,
    /// Asking the host failed.
    pub error: Option<String>,
    /// Turning it on or off failed (shown in the dialog, or under the row).
    pub action_error: Option<String>,
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
            action_error: None,
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
        self.action_error = None;
        self.loading = false;
        self.busy = false;
        self.dialog = false;
        self.request += 1;
    }
}

/// goatty's lines in the terminal's font, with a Copy button: what the user
/// agrees to, or adds by hand when goatty can't. `copied`: show a check
/// instead (the caller's `on_copy` set it, or showed a toast).
pub fn code_lines(
    ui: Ui,
    id: &str,
    mono: SharedString,
    lines: &[String],
    copied: bool,
    on_copy: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let text = lines.join("\n");
    div()
        .flex()
        .items_start()
        .gap(px(8.))
        .p(px(12.))
        .rounded(px(radius::CONTROL))
        .bg(ui.bg)
        .border_1()
        .border_color(ui.divider)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.))
                .font_family(mono)
                .text_size(px(text::CAPTION))
                .text_color(ui.text)
                .children(lines.iter().map(|l| div().min_w_0().child(SharedString::from(l.clone())))),
        )
        .child(icon_button(
            SharedString::from(format!("{id}-copy")),
            if copied { Icon::Check } else { Icon::Copy },
            if copied { "Copied" } else { "Copy the lines" },
            ui,
            move |window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                on_copy(window, cx);
            },
        ))
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

/// A warning line under a row.
pub fn warning_line(ui: Ui, t: String) -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap(px(6.))
        .text_size(px(text::SMALL))
        .text_color(ui.warning)
        .child(Icon::TriangleAlert.svg().flex_shrink_0().size(px(13.)).mt(px(2.)).text_color(ui.warning))
        .child(div().min_w_0().child(SharedString::from(t)))
}

/// "starts" or "start", for one file or more.
pub fn starts(s: &Status) -> &'static str {
    if s.files.len() == 1 { "starts" } else { "start" }
}

/// What happens to the user's `~/.zshrc` in those shells, as (title, text),
/// from their settings: run guarded or not, imported or not.
pub fn zshrc_note(user_rc: bool, import: bool) -> (&'static str, &'static str) {
    match (user_rc, import) {
        (true, _) => (
            "Your ~/.zshrc runs guarded there",
            "As in goatty's tabs: its functions and key bindings run; prompt and plugin frameworks that would fight goatty's are skipped.",
        ),
        (false, true) => (
            "Your ~/.zshrc stops running there",
            "As in goatty's tabs. The PATH folders and variables it sets up still come along. Put functions and key bindings in ~/.config/goatty/zshrc, or run ~/.zshrc guarded under Settings → Dotfiles.",
        ),
        (false, false) => (
            "Your ~/.zshrc stops running there",
            "As in goatty's tabs. Settings → Dotfiles can bring the PATH folders and variables it sets up, or run it guarded; your own additions go in ~/.config/goatty/zshrc.",
        ),
    }
}

/// An error's lines without goatty's two lines (shown above it already).
pub fn error_lines(err: &str, s: &Status) -> Vec<String> {
    err.lines().filter(|l| !s.lines.iter().any(|g| g == l)).map(str::to_owned).collect()
}

impl SettingsWindow {
    /// Ask the host whether its `.zshenv` files have goatty's line.
    pub fn load_everywhere(&mut self, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        if self.everywhere.loading || self.everywhere.busy {
            return;
        }
        self.everywhere.loading = true;
        self.everywhere.request += 1;
        let request = self.everywhere.request;
        cx.notify();
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

    /// The host's settings changed (applied, rolled back): whether the line
    /// does anything there may have changed too.
    pub(super) fn everywhere_host_changed(&mut self, cx: &mut Context<'_, Self>) {
        if self.page == Page::Shell {
            self.load_everywhere(cx);
        } else {
            self.everywhere.status = None;
        }
    }

    /// Turn it on or off on the host, then say how it went.
    fn set_everywhere(&mut self, action: Action, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        if self.everywhere.busy {
            return;
        }
        self.everywhere.busy = true;
        self.everywhere.action_error = None;
        self.everywhere.request += 1;
        let request = self.everywhere.request;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let label = link.label();
            let result = cx.background_executor().spawn(async move { link.everywhere(action) }).await;
            let _ = this.update(cx, |v, cx| {
                if v.everywhere.request != request {
                    return;
                }
                v.everywhere.busy = false;
                match result {
                    Ok(s) => {
                        let t = match (action, s.state) {
                            (Action::On, State::On) => Toast::success(format!("goatty's shell is now on in every terminal on {label}"))
                                .body(format!("Added the lines to {}. New terminals use it; open ones don't change.", s.files_text())),
                            (_, State::Off) => Toast::success(format!("Back to plain zsh outside goatty on {label}")).body(format!(
                                "Removed goatty's lines from {}. Open shells keep goatty's until they exit.",
                                s.files_text()
                            )),
                            _ => Toast::info(format!("{} didn't change", s.files_text())),
                        };
                        toast::show(cx, t.key("shell.everywhere"));
                        if action == Action::On {
                            v.close_everywhere_dialog(cx);
                        }
                        v.everywhere.status = Some(s);
                        v.everywhere.error = None;
                    }
                    // In the dialog (or under the row): it may hold the lines to add by hand.
                    Err(err) => {
                        v.everywhere.action_error = Some(too_old(&format!("{err:#}"), &label));
                        // The file may have changed under us (edited by hand): show what it has.
                        v.load_everywhere(cx);
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
        self.everywhere.action_error = None;
        window.focus(&self.everywhere.modal_focus, cx);
        self.everywhere.dialog = true;
        cx.notify();
    }

    pub(super) fn close_everywhere_dialog(&mut self, cx: &mut Context<'_, Self>) {
        if !self.everywhere.dialog {
            return;
        }
        self.everywhere.dialog = false;
        self.everywhere.action_error = None;
        self.everywhere.restore = Some(self.everywhere.opener.take().unwrap_or_else(|| self.focus.clone()));
        cx.notify();
    }

    /// The switch on the Shell page. `zsh_managed`: goatty's managed zsh is
    /// on (in the draft) and the shell is zsh.
    pub(super) fn everywhere_row(&self, ui: Ui, zsh_managed: bool, cx: &Context<'_, Self>) -> Row {
        let e = &self.everywhere;
        let r = row(ui, LABEL).icon(Icon::SquareTerminal);
        let check_again = || {
            button_with(
                "everywhere-check",
                Some(Icon::Refresh),
                "Check again",
                ButtonKind::Ghost,
                ui,
                self.act(cx, |v, cx| v.load_everywhere(cx)),
            )
        };
        let Some(s) = &e.status else {
            return match &e.error {
                Some(err) => r.desc(format!("Couldn't check: {err}")).control(check_again()),
                None => r
                    .desc("IDE terminals, other terminals, tmux and SSH logins too · checking…")
                    .disabled(true)
                    .control(toggle("everywhere", false, ui, |_, _| {}).aria_label(LABEL)),
            };
        };
        if s.state == State::Changed {
            let at = s.changed.first().map_or_else(|| s.files_text(), ToString::to_string);
            let r = r
                .desc(format!("A goatty line was edited by hand ({at}). Remove or fix it in the file, then check again"))
                .control(check_again());
            let more: Vec<String> =
                s.changed.iter().skip(1).map(|c| format!("Also edited by hand: {c}")).chain(s.warnings.iter().cloned()).collect();
            return if more.is_empty() {
                r
            } else {
                r.below(div().flex().flex_col().gap(px(6.)).children(more.into_iter().map(|w| warning_line(ui, w))))
            };
        }
        let (desc, can_toggle) = match s.state {
            State::On if s.active && zsh_managed => {
                (format!("{} {} goatty's shell in every new interactive zsh", s.files_text(), starts(s)), true)
            }
            State::On => {
                let why = s.unavailable.clone().unwrap_or_else(|| "goatty's managed zsh is off".to_owned());
                (format!("Inactive ({why}) · switch this off to remove the lines from {}", s.files_text()), true)
            }
            _ => match &s.unavailable {
                Some(why) => (why.clone(), false),
                None if !zsh_managed => ("Needs goatty's managed zsh".to_owned(), false),
                None => (
                    format!("Use this shell in IDE terminals, other terminals, tmux and SSH logins · adds 2 lines to {}", s.files_text()),
                    true,
                ),
            },
        };
        let on = s.state == State::On;
        let r = r.desc(desc).disabled(!can_toggle || e.busy).control(
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
        let mut notes: Vec<String> = s.warnings.clone();
        if on {
            notes.extend(s.changed.iter().map(|c| format!("Also a goatty line edited by hand: {c}")));
        }
        let error = e.action_error.clone().filter(|_| !e.dialog);
        if notes.is_empty() && error.is_none() {
            return r;
        }
        r.below(div().flex().flex_col().gap(px(6.)).children(notes.into_iter().map(|w| warning_line(ui, w))).when_some(error, |d, err| {
            d.child(banner(ui, Tone::Danger, "Couldn't remove the lines", err.lines().map(str::to_owned).collect()))
        }))
    }

    /// The consent dialog: the exact lines and files, what changes, and the
    /// button that writes them.
    pub(super) fn everywhere_modal(&self, ui: Ui, cx: &Context<'_, Self>) -> Option<AnyElement> {
        if !self.everywhere.dialog {
            return None;
        }
        let s = self.everywhere.status.as_ref()?;
        let h = self.host_value()?;
        let host = self.host_label();
        let mono = kit::mono(cx);
        let mut points = div().flex().flex_col().gap(px(14.)).child(point(
            ui,
            Icon::FileText,
            zshrc_note(h.shell.zsh.user_rc, h.shell.import.enabled).0,
            zshrc_note(h.shell.zsh.user_rc, h.shell.import.enabled).1,
        ));
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
            "Turn this off and goatty removes exactly these lines, keeping a backup. Plain zsh for one shell: GOATTY_SHELL_MANAGED=0 zsh",
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
                    .child(div().text_size(px(text::LABEL)).font_weight(FontWeight::MEDIUM).text_color(ui.text).child(SharedString::from(
                        format!("goatty adds these lines to the end of {}", s.files_text()),
                    )))
                    .child(code_lines(ui, "everywhere-lines", mono, &s.lines, false, |_, cx| {
                        toast::show(cx, Toast::success("Copied the lines").key("shell.everywhere.copy"));
                    })),
            )
            .child(points)
            .when_some(self.everywhere.action_error.as_deref(), |d, err| {
                d.child(banner(ui, Tone::Danger, "Couldn't add the lines", error_lines(err, s)))
            });
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
