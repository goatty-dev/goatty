//! The Terminal page (links, selection, mouse, pasting) and the Workspace page
//! (startup, tabs, panes, notifications).

use goatty_config::{
    CopyOnSelect, CtrlBackspace, CtrlDelete, LinkClick, LinkSettings, MiddleClick, NotifyWhen, OnLaunch, PaneSettings, PasteConfirm,
    RemoteFileOpen, RightClick, TabActivity, TabBarShow, TabSettings, TabStyle,
};
use gpui::{AnyElement, Context, FontWeight, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};

use super::appearance::opts;
use super::kit::{choice_card, row, section};
use super::{Page, SettingsWindow, client};
use crate::settings::AppSettings;
use crate::ui::controls::{segmented, slider, stepper, toggle};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, text};

/// Linux and FreeBSD have a primary selection (select to copy, middle
/// click to paste); elsewhere those options collapse.
const PRIMARY_SELECTION: bool = cfg!(any(target_os = "linux", target_os = "freebsd"));

impl SettingsWindow {
    pub(super) fn terminal(&self, ui: Ui, cx: &Context<'_, Self>) -> AnyElement {
        let s = &cx.global::<AppSettings>().file.value;
        let ctrl = if cfg!(target_os = "macos") { "Cmd+click" } else { "Ctrl+click" };
        let opener_off = matches!(s.links.remote_files, RemoteFileOpen::Fetch | RemoteFileOpen::Copy);
        let max_mb = s.links.remote_max_size_mb.clamp(LinkSettings::MIN_REMOTE_MAX_SIZE_MB, LinkSettings::MAX_REMOTE_MAX_SIZE_MB);
        let links = section(ui, "Links")
            .child(row(ui, "Open links").desc("Web addresses, file paths and links programs mark up").control(self.select(
                "links-open-on",
                "Open links",
                opts(&[
                    (LinkClick::CtrlClick, ctrl, "Hold the key to see where a link goes"),
                    (LinkClick::Click, "Click", "Any click on a link opens it"),
                    (LinkClick::Off, "Never", "Links are plain text"),
                ]),
                &s.links.open_on,
                cx,
                |v, _, cx| AppSettings::update(cx, |s| s.links.open_on = v),
            )))
            .child(
                row(ui, "Open files with")
                    .desc("A command, or empty for the desktop's default app. Enter saves; {path}, {line} and {column} are filled in")
                    .below(self.file_opener.clone()),
            )
            .child(row(ui, "Remote files").desc("How links to files and folders on remote hosts open").control(self.select(
                "links-remote-files",
                "Remote files",
                opts(&[
                    (RemoteFileOpen::Auto, "Automatic", "Use remote editor if set for SSH, else open read-only copy"),
                    (RemoteFileOpen::Fetch, "Open read-only copy", "Fetch over the session link and open locally"),
                    (RemoteFileOpen::RemoteOpener, "Open in remote editor", "Run the command below for SSH hosts (else fetch)"),
                    (RemoteFileOpen::Copy, "Copy path", "Copy the remote path to the clipboard"),
                ]),
                &s.links.remote_files,
                cx,
                |v, _, cx| AppSettings::update(cx, |s| s.links.remote_files = v),
            )))
            .child(
                row(ui, "Open remote files with")
                    .desc(if opener_off {
                        "Set Remote files to Automatic or Open in remote editor to use this command"
                    } else {
                        "For SSH sessions. Enter saves; {host}, {user}, {hostname}, {path}, {line} and {column} are filled in"
                    })
                    .disabled(opener_off)
                    .below(self.remote_file_opener.clone()),
            )
            .child(
                row(ui, "Max remote file download")
                    .desc(if s.links.remote_files == RemoteFileOpen::Copy {
                        "Fetching is off while Remote files copies paths"
                    } else {
                        "Larger files ask first before fetching over the session link"
                    })
                    .disabled(s.links.remote_files == RemoteFileOpen::Copy)
                    .control(stepper(
                        "links-remote-max-size",
                        format!("{max_mb} MiB"),
                        ui,
                        client(|s| {
                            s.links.remote_max_size_mb = s
                                .links
                                .remote_max_size_mb
                                .saturating_sub(1)
                                .clamp(LinkSettings::MIN_REMOTE_MAX_SIZE_MB, LinkSettings::MAX_REMOTE_MAX_SIZE_MB);
                        }),
                        client(|s| {
                            s.links.remote_max_size_mb = s
                                .links
                                .remote_max_size_mb
                                .saturating_add(1)
                                .clamp(LinkSettings::MIN_REMOTE_MAX_SIZE_MB, LinkSettings::MAX_REMOTE_MAX_SIZE_MB);
                        }),
                    )),
            )
            .footer("Paths count when the file or folder exists on that host. Right-click a link for all options.");

        let copy_opts = if PRIMARY_SELECTION {
            opts(&[
                (CopyOnSelect::Primary, "Primary selection", "Middle click pastes it, in any app"),
                (CopyOnSelect::Clipboard, "Primary and clipboard", "Ctrl+V pastes it too"),
                (CopyOnSelect::Off, "Off", "Copy with Ctrl+Shift+C"),
            ])
        } else {
            // Without a primary selection, `primary` copies nowhere.
            opts(&[(CopyOnSelect::Clipboard, "Clipboard", ""), (CopyOnSelect::Primary, "Off", "")])
        };
        let selection = section(ui, "Selection").child(row(ui, "Copy selected text").desc("When you select with the mouse").control(
            self.select("copy-on-select", "Copy selected text", copy_opts, &s.selection.copy_on_select, cx, |v, _, cx| {
                AppSettings::update(cx, |s| s.selection.copy_on_select = v)
            }),
        ));

        let mouse = section(ui, "Mouse").child(row(ui, "Right click").desc("Shift+right-click does the other thing").control(self.select(
            "right-click",
            "Right click",
            opts(&[
                (RightClick::Menu, "Shows goatty's menu", "Programs that use the mouse (vim, tmux) get the click"),
                (RightClick::Always, "Always shows goatty's menu", "In vim and tmux too; Shift+right-click reaches them"),
                (RightClick::Paste, "Copies or pastes", "Copies the selection, else pastes (like Windows Terminal)"),
                (RightClick::Off, "Does nothing", "Programs that use the mouse still get the click"),
            ]),
            &s.mouse.right_click,
            cx,
            |v, _, cx| AppSettings::update(cx, |s| s.mouse.right_click = v),
        )));

        let middle_opts = if PRIMARY_SELECTION {
            opts(&[
                (MiddleClick::Primary, "Primary selection", "What you last selected anywhere"),
                (MiddleClick::Clipboard, "Clipboard", "What you last copied"),
                (MiddleClick::Off, "Nothing", ""),
            ])
        } else {
            // Without a primary selection, `primary` pastes the clipboard.
            opts(&[(MiddleClick::Primary, "Clipboard", ""), (MiddleClick::Off, "Nothing", "")])
        };
        let pasting = section(ui, "Pasting")
            .child(row(ui, "Middle click pastes").desc("Hold Shift in programs that use the mouse").control(self.select(
                "middle-click",
                "Middle click pastes",
                middle_opts,
                &s.paste.middle_click,
                cx,
                |v, _, cx| AppSettings::update(cx, |s| s.paste.middle_click = v),
            )))
            .child(row(ui, "Ask before pasting").desc("Shows what's in it, cleans it up or makes it one line").control(self.select(
                "paste-confirm",
                "Ask before pasting",
                opts(&[
                    (PasteConfirm::Suspicious, "When it looks risky", "Would run at once, hidden characters, curl | sh, over 256 KiB"),
                    (PasteConfirm::Multiline, "Risky or several lines", "Also any paste with line breaks"),
                    (PasteConfirm::Never, "Never", "Paste everything as is"),
                ]),
                &s.paste.confirm,
                cx,
                |v, _, cx| AppSettings::update(cx, |s| s.paste.confirm = v),
            )));
        let keyboard = section(ui, "Keyboard")
            .child(
                row(ui, "Ctrl+Backspace")
                    .desc("What Ctrl+Backspace sends to the shell or program")
                    .control(self.select(
                        "keyboard-ctrl-backspace",
                        "Ctrl+Backspace",
                        opts(&[
                            (CtrlBackspace::EscBackspace, "Delete word (Esc+Backspace)", "Works in bash, zsh, fish and over SSH"),
                            (CtrlBackspace::CtrlW, "Delete word (Ctrl+W)", "Deletes to the previous space in shells, vim and raw input"),
                            (CtrlBackspace::CtrlH, "Standard (^H)", "xterm default; deletes a word in goatty's zsh, one character in bash"),
                        ]),
                        &s.keyboard.ctrl_backspace,
                        cx,
                        |v, _, cx| AppSettings::update(cx, |s| s.keyboard.ctrl_backspace = v),
                    )),
            )
            .child(
                row(ui, "Ctrl+Delete")
                    .desc("What Ctrl+Delete sends to the shell or program")
                    .control(self.select(
                        "keyboard-ctrl-delete",
                        "Ctrl+Delete",
                        opts(&[
                            (CtrlDelete::Standard, "Standard (CSI 3;5~)", "xterm default; works in goatty's zsh, fish and most TUIs"),
                            (CtrlDelete::AltD, "Delete word (Alt+D)", "Works in bash and plain zsh without extra config"),
                        ]),
                        &s.keyboard.ctrl_delete,
                        cx,
                        |v, _, cx| AppSettings::update(cx, |s| s.keyboard.ctrl_delete = v),
                    )),
            )
            .footer("Applies to local and SSH sessions alike. Programs that enable the kitty keyboard protocol still receive Ctrl+Backspace and Ctrl+Delete directly.");

        div().flex().flex_col().gap(px(28.)).child(links).child(selection).child(mouse).child(pasting).child(keyboard).into_any_element()
    }

    fn tab_style_cards(&self, ui: Ui, cx: &Context<'_, Self>) -> impl IntoElement {
        use crate::ui::style::{h, mix};
        let s = &cx.global::<AppSettings>().file.value;
        let t = cx.global::<AppSettings>().colors;
        let (bg, fg, bar) = (h(t.background), h(t.foreground), h(mix(t.background, t.foreground, 0.08)));
        let mono = super::kit::mono(cx);
        // The terminal under the tab bar: one prompt line.
        let screen = || {
            div()
                .flex_1()
                .bg(bg)
                .px(px(10.))
                .pt(px(6.))
                .flex()
                .gap(px(5.))
                .font_family(mono.clone())
                .text_size(px(10.))
                .child(div().text_color(h(t.ansi[4])).child("~"))
                .child(div().text_color(h(t.ansi[2])).child("❯"))
                .child(div().text_color(fg).child("nvim"))
        };
        let modern_bar = div().h(px(30.)).bg(bar).flex().items_end().px(px(8.)).gap(px(2.)).children([("zsh", true), ("nvim", false)].map(
            |(name, on)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(24.))
                    .px(px(9.))
                    .rounded_t(px(7.))
                    .when(on, |d| d.bg(bg))
                    .text_size(px(11.))
                    .text_color(if on { fg } else { fg.opacity(0.6) })
                    .child(Icon::Terminal.svg().size(px(11.)).text_color(if on { ui.accent } else { fg.opacity(0.5) }))
                    .child(name)
                    .child(Icon::Close.svg().size(px(9.)).text_color(fg.opacity(0.5)))
            },
        ));
        let compact_bar = div().h(px(18.)).bg(bar).flex().items_center().gap(px(10.)).px(px(10.)).text_size(px(10.)).children(
            [("1 zsh", true), ("2 nvim", false)].map(|(name, on)| {
                div().text_color(if on { fg } else { fg.opacity(0.55) }).when(on, |d| d.font_weight(FontWeight::BOLD)).child(name)
            }),
        );
        let modern = div().h(px(60.)).flex().flex_col().child(modern_bar).child(screen());
        let compact = div().h(px(60.)).flex().flex_col().child(compact_bar).child(screen());
        // A sidebar of rows, the active one raised, beside a rounded card.
        let side_row = |name: &'static str, on: bool| {
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .h(px(15.))
                .px(px(4.))
                .rounded(px(4.))
                .when(on, |d| d.bg(bg))
                .text_size(px(9.))
                .text_color(if on { fg } else { fg.opacity(0.6) })
                .child(Icon::Terminal.svg().size(px(8.)).text_color(if on { ui.accent } else { fg.opacity(0.5) }))
                .child(name)
        };
        let vertical = div()
            .h(px(60.))
            .flex()
            .bg(bar)
            .child(
                div()
                    .w(px(56.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(4.))
                    .child(side_row("zsh", true))
                    .child(side_row("nvim", false)),
            )
            .child(div().flex_1().flex().py(px(4.)).pr(px(4.)).child(screen().rounded(px(5.)).pt(px(5.))));
        div()
            .flex()
            .gap(px(10.))
            .child(
                choice_card(ui, "tabs-modern", "Modern", s.tabs.style == TabStyle::Modern, modern)
                    .on_click(|_, _, cx| AppSettings::update(cx, |s| s.tabs.style = TabStyle::Modern)),
            )
            .child(
                choice_card(ui, "tabs-compact", "Compact", s.tabs.style == TabStyle::Compact, compact)
                    .on_click(|_, _, cx| AppSettings::update(cx, |s| s.tabs.style = TabStyle::Compact)),
            )
            .child(
                choice_card(ui, "tabs-vertical", "Vertical", s.tabs.style == TabStyle::Vertical, vertical)
                    .on_click(|_, _, cx| AppSettings::update(cx, |s| s.tabs.style = TabStyle::Vertical)),
            )
    }

    pub(super) fn workspace(&self, ui: Ui, cx: &Context<'_, Self>) -> AnyElement {
        let s = &cx.global::<AppSettings>().file.value;
        let dim = s.panes.dim_inactive;
        let vertical = s.tabs.style == TabStyle::Vertical;
        let (width, min_w, max_w) = (s.tabs.sidebar_width, TabSettings::MIN_SIDEBAR_WIDTH, TabSettings::MAX_SIDEBAR_WIDTH);
        let startup = section(ui, "Startup")
            .child(row(ui, "When goatty starts").desc("Sessions keep running when you close a window").control(self.select(
                "on-launch",
                "When goatty starts",
                opts(&[
                    (OnLaunch::Reattach, "Restore every window", "Their tabs and splits on every host, with their sessions"),
                    (OnLaunch::ReattachLocal, "Restore this computer's tabs", "Other hosts' tabs stay detached until you connect"),
                    (OnLaunch::New, "Start a new session", "An empty window with a fresh shell"),
                ]),
                &s.workspace.on_launch,
                cx,
                |v, _, cx| AppSettings::update(cx, |s| s.workspace.on_launch = v),
            )))
            .footer("Reconnecting to other hosts may ask for SSH passwords. The --new, --attach and --ssh command-line flags still win.");
        let tabs = section(ui, "Tabs")
            .child(row(ui, "Style").below(self.tab_style_cards(ui, cx)))
            .child(row(ui, "Show the tab bar").control(segmented(
                "tabs-show",
                &[(TabBarShow::Always, "Always"), (TabBarShow::Multiple, "With 2+ tabs")],
                &s.tabs.show,
                ui,
                |v, _, cx| AppSettings::update(cx, |s| s.tabs.show = v),
            )))
            .child(
                row(ui, "Sidebar width")
                    .desc(if vertical { "Or drag the sidebar's edge" } else { "Only with vertical tabs" })
                    .disabled(!vertical)
                    .control(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(slider("tabs-sidebar-width", (width - min_w) / (max_w - min_w), ui, move |t, done, _, cx| {
                                let v = (min_w + t * (max_w - min_w)).round().clamp(min_w, max_w);
                                if done {
                                    AppSettings::update(cx, |s| s.tabs.sidebar_width = v);
                                } else if cx.global::<AppSettings>().file.value.tabs.sidebar_width != v {
                                    AppSettings::preview(cx, |s| s.tabs.sidebar_width = v);
                                }
                            }))
                            .child(
                                div()
                                    .w(px(48.))
                                    .text_size(px(text::LABEL))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(SharedString::from(format!("{width:.0} px"))),
                            ),
                    ),
            )
            .child(
                row(ui, "Fold the sidebar to icons")
                    .desc(if vertical { "Ctrl+Shift+B folds and unfolds it" } else { "Only with vertical tabs" })
                    .disabled(!vertical)
                    .control(toggle(
                        "tabs-sidebar-collapsed",
                        s.tabs.sidebar_collapsed,
                        ui,
                        client(|s| s.tabs.sidebar_collapsed = !s.tabs.sidebar_collapsed),
                    )),
            )
            .child(row(ui, "Switch tabs with the mouse wheel").desc("Scroll over the tabs to go to the next or previous one").control(
                toggle(
                    "tabs-scroll-to-switch",
                    s.tabs.scroll_to_switch,
                    ui,
                    client(|s| s.tabs.scroll_to_switch = !s.tabs.scroll_to_switch),
                ),
            ))
            .child(row(ui, "Program icons").desc("nvim, git, docker… and coding agents' logos in each tab").control(toggle(
                "tabs-icons",
                s.tabs.icons,
                ui,
                client(|s| s.tabs.icons = !s.tabs.icons),
            )))
            .child(row(ui, "Mark tabs that ring the bell").desc("A bell on a background tab whose program rang it").control(toggle(
                "tabs-mark-bell",
                s.tabs.mark_bell,
                ui,
                client(|s| s.tabs.mark_bell = !s.tabs.mark_bell),
            )))
            .child(row(ui, "Mark tabs with activity").desc("A dot on a background tab until you look at it").control(self.select(
                "tabs-mark-activity",
                "Mark tabs with activity",
                opts(&[
                    (TabActivity::Finished, "When a command finishes", "Red when it failed. Needs shell integration"),
                    (TabActivity::Output, "On any output", "Also finished commands; progress bars and clocks count too"),
                    (TabActivity::Off, "Never", "Only the bell, if that's on"),
                ]),
                &s.tabs.mark_activity,
                cx,
                |v, _, cx| AppSettings::update(cx, |s| s.tabs.mark_activity = v),
            )))
            .footer("Double-click a tab to rename it, middle-click to close it, right-click for more, drag to move it.");
        let max = PaneSettings::MAX_DIM;
        let panes = section(ui, "Panes")
            .child(
                row(ui, "Fade inactive panes").desc("How much the panes you're not typing in fade").control(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(slider("panes-dim", dim / max, ui, move |t, done, _, cx| {
                            let v = ((t * max * 20.0).round() / 20.0).clamp(0.0, max);
                            if done {
                                AppSettings::update(cx, |s| s.panes.dim_inactive = v);
                            } else if cx.global::<AppSettings>().file.value.panes.dim_inactive != v {
                                AppSettings::preview(cx, |s| s.panes.dim_inactive = v);
                            }
                        }))
                        .child(
                            div()
                                .w(px(40.))
                                .text_size(px(text::LABEL))
                                .font_weight(FontWeight::MEDIUM)
                                .child(SharedString::from(format!("{:.0} %", dim * 100.0))),
                        ),
                ),
            )
            .child(
                row(ui, "Ask before closing a running program")
                    .desc("When a pane still runs something besides its shell: a build, an editor, ssh")
                    .control(toggle(
                        "panes-confirm-close",
                        s.panes.confirm_close,
                        ui,
                        client(|s| s.panes.confirm_close = !s.panes.confirm_close),
                    )),
            )
            .child(
                row(ui, "Split and move between panes")
                    .desc("Ctrl+Shift+E splits right, Ctrl+Shift+O down, Alt+arrows move")
                    .icon(Icon::Keyboard)
                    .on_click("panes-shortcuts", self.listener_go(cx, Page::Shortcuts)),
            );
        let notifications = self.notifications_section(ui, cx);
        div().flex().flex_col().gap(px(28.)).child(startup).child(tabs).child(panes).child(notifications).into_any_element()
    }

    /// When goatty tells about what happens in panes (#205): coding agents
    /// waiting or done, programs' own notifications; in the window and on
    /// the desktop, set apart.
    fn notifications_section(&self, ui: Ui, cx: &Context<'_, Self>) -> impl IntoElement {
        let n = &cx.global::<AppSettings>().file.value.notifications;
        let when = || {
            opts(&[
                (NotifyWhen::Background, "When you're not looking", "The pane is in another tab, or goatty isn't focused"),
                (NotifyWhen::Unfocused, "When goatty isn't focused", "Only while you're in another app"),
                (NotifyWhen::Always, "Always", "Also for the pane you're looking at"),
                (NotifyWhen::Never, "Never", "Tabs still show it"),
            ])
        };
        let off = n.in_app == NotifyWhen::Never && n.system == NotifyWhen::Never;
        section(ui, "Notifications")
            .desc("When a coding agent needs you or finishes, or a program asks to notify you")
            .child(row(ui, "Notifications in goatty").desc("A message in the window's corner, with a button to go there").control(
                self.select("notify-in-app", "Notifications in goatty", when(), &n.in_app, cx, |v, _, cx| {
                    AppSettings::update(cx, |s| s.notifications.in_app = v)
                }),
            ))
            .child(row(ui, "Desktop notifications").desc("Your system's notifications; clicking one shows the pane").control(self.select(
                "notify-system",
                "Desktop notifications",
                when(),
                &n.system,
                cx,
                |v, _, cx| AppSettings::update(cx, |s| s.notifications.system = v),
            )))
            .child(
                row(ui, "A coding agent waits for you")
                    .desc(if off { "Notifications are off" } else { "It asks for permission, an answer or a sign-in" })
                    .disabled(off)
                    .control(toggle(
                        "notify-agent-waiting",
                        n.agent_waiting,
                        ui,
                        client(|s| s.notifications.agent_waiting = !s.notifications.agent_waiting),
                    )),
            )
            .child(
                row(ui, "A coding agent finishes")
                    .desc(if off { "Notifications are off" } else { "Its task is done, or it failed" })
                    .disabled(off)
                    .control(toggle(
                        "notify-agent-done",
                        n.agent_done,
                        ui,
                        client(|s| s.notifications.agent_done = !s.notifications.agent_done),
                    )),
            )
            .child(
                row(ui, "A program asks to notify you")
                    .desc(if off { "Notifications are off" } else { "Programs that send notifications (OSC 9, OSC 777)" })
                    .disabled(off)
                    .control(toggle("notify-programs", n.programs, ui, client(|s| s.notifications.programs = !s.notifications.programs))),
            )
            .footer(
                "Agents that report their state show it on their tab: a spinner while they work, a badge while they wait for you. \
                 Claude Code, Codex, OpenCode and Antigravity do it on their own.",
            )
    }
}
