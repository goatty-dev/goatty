//! The settings window's Hosts page (#115): the hosts the command palette
//! offers to connect to. Hosts added here and the choices for hosts from
//! `~/.ssh/config` are client settings (`ssh` in `settings.json`), saved
//! right away.

use goatty_config::{Color, HostColor, Issue, LocalEcho, OpenIn, SshHost, SyncMode, ssh_host_problem};
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Subscription, div, prelude::*, px,
};

use super::SettingsWindow;
use super::kit::{self, card, divider, field, opt, row, section};
use crate::hosts::{self, HostEntry, HostList};
use crate::settings::AppSettings;
use crate::ui::color_picker::{ColorPicker, ColorPickerEvent};
use crate::ui::controls::{ButtonKind, Tone, banner, button_with, icon_button, segmented, toggle, tooltip};
use crate::ui::focus::{self, Ring, Scroller};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

pub struct HostsPage {
    host: Entity<TextInput>,
    name: Entity<TextInput>,
    /// Why the last Add didn't go through.
    error: Option<String>,
    /// Picks a custom color for the host in `editing` (#257).
    picker: Entity<ColorPicker>,
    /// The host whose color is being edited, under its row.
    editing: Option<String>,
    /// A custom color shown but not saved yet (the picker is being dragged).
    pending: Option<(String, Color)>,
    _subs: Vec<Subscription>,
}

impl HostsPage {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> Self {
        let host = cx.new(|cx| TextInput::new("user@host, or a name from ~/.ssh/config", "", cx).label("Host"));
        let name = cx.new(|cx| TextInput::new("Optional", "", cx).label("Name"));
        let mut subs = vec![cx.observe_global::<HostList>(|_, cx| cx.notify())];
        for input in [&host, &name] {
            subs.push(cx.subscribe(input, |view, _, ev: &TextInputEvent, cx| match ev {
                TextInputEvent::Changed(_) => {
                    if view.hosts.error.take().is_some() {
                        cx.notify();
                    }
                }
                TextInputEvent::Submitted(_) => view.add_host(cx),
            }));
        }
        let picker = cx.new(|cx| ColorPicker::new(Color(0), cx));
        subs.push(cx.subscribe(&picker, |view, _, ev: &ColorPickerEvent, cx| match *ev {
            ColorPickerEvent::Changed(c) => view.host_color_changed(c, cx),
            // The user settled on it: it becomes the host's own color, also
            // when it equals the one shown (an automatic or theme color).
            ColorPickerEvent::Committed(c) => {
                if let Some(host) = view.hosts.editing.clone() {
                    view.hosts.pending = None;
                    AppSettings::update(cx, |s| s.tabs.set_host_color(&host, Some(HostColor::Custom(c))));
                }
            }
        }));
        // Closing the window right after a change must not lose it.
        subs.push(cx.on_release(|view, cx| view.flush_host_color(cx)));
        Self { host, name, error: None, picker, editing: None, pending: None, _subs: subs }
    }
}

/// Check a host before adding it: `Err` says why not.
pub fn check_new_host(existing: &[HostEntry], host: &str) -> Result<(), String> {
    if let Some(why) = ssh_host_problem(host) {
        return Err(format!("Host: {why}."));
    }
    if existing.iter().any(|e| e.host == host) {
        return Err(format!("{host} is in the list already."));
    }
    Ok(())
}

impl SettingsWindow {
    fn add_host(&mut self, cx: &mut Context<'_, Self>) {
        let text = |e: &Entity<TextInput>, cx: &Context<'_, Self>| e.read(cx).text().trim().to_owned();
        let (host, name) = (text(&self.hosts.host, cx), text(&self.hosts.name, cx));
        if let Err(e) = check_new_host(&hosts::current(cx), &host) {
            self.hosts.error = Some(e);
            cx.notify();
            return;
        }
        let name = (!name.is_empty() && name != host).then_some(name);
        AppSettings::update(cx, |s| s.ssh.hosts.push(SshHost { host, name, ..Default::default() }));
        for input in [&self.hosts.host, &self.hosts.name] {
            input.update(cx, |i, cx| i.set_text("", cx));
        }
        self.hosts.error = None;
        cx.notify();
    }

    /// The color the tab group of `host` has now.
    fn host_rgb(host: &str, cx: &App) -> Color {
        crate::workspace::groups::host_rgb(host, cx.global::<AppSettings>())
    }

    /// Show or hide the color editor under `host`'s row.
    fn toggle_host_color(&mut self, host: &str, cx: &mut Context<'_, Self>) {
        self.flush_host_color(cx);
        if self.hosts.editing.as_deref() == Some(host) {
            self.hosts.editing = None;
        } else {
            self.edit_host_color(host, cx);
        }
        cx.notify();
    }

    /// Open the color editor under `host`'s row, at its current color.
    pub(super) fn edit_host_color(&mut self, host: &str, cx: &mut Context<'_, Self>) {
        self.flush_host_color(cx);
        let color = Self::host_rgb(host, cx);
        self.hosts.editing = Some(host.to_string());
        self.hosts.picker.update(cx, |p, cx| p.open(color, cx));
        cx.notify();
    }

    /// A palette color for `host`, or `None`: automatic again.
    fn set_host_color(&mut self, host: &str, color: Option<HostColor>, cx: &mut Context<'_, Self>) {
        self.hosts.pending = None;
        AppSettings::update(cx, |s| s.tabs.set_host_color(host, color));
        let shown = Self::host_rgb(host, cx);
        self.hosts.picker.update(cx, |p, cx| p.open(shown, cx));
        cx.notify();
    }

    /// The picker moved to `color`: show it live, and save it once it settles.
    fn host_color_changed(&mut self, color: Color, cx: &mut Context<'_, Self>) {
        let Some(host) = self.hosts.editing.clone() else { return };
        AppSettings::preview(cx, |s| s.tabs.set_host_color(&host, Some(HostColor::Custom(color))));
        self.hosts.pending = Some((host, color));
        cx.notify();
    }

    /// Write a live color change to settings.json.
    fn flush_host_color(&mut self, cx: &mut App) {
        if let Some((host, color)) = self.hosts.pending.take() {
            // Something else (the group's menu) chose a color since: that one stays.
            let shown = cx.global::<AppSettings>().client.tabs.host_color(&host);
            if shown == Some(HostColor::Custom(color)) {
                AppSettings::update(cx, |s| s.tabs.set_host_color(&host, Some(HostColor::Custom(color))));
            }
        }
    }

    /// The round button that shows a host's color and opens its editor.
    fn host_color_button(&self, ui: Ui, e: &HostEntry, cx: &Context<'_, Self>) -> AnyElement {
        let host = e.host.clone();
        let open = self.hosts.editing.as_deref() == Some(e.host.as_str());
        let color = crate::workspace::groups::host_color(&e.host, cx.global::<AppSettings>());
        let id: gpui::ElementId = SharedString::from(format!("host-color-{}", hash(&e.host))).into();
        let el = div()
            .id(id.clone())
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!("Color of {}'s tabs", e.label)))
            .flex_shrink_0()
            .size(px(22.))
            .rounded_full()
            .border_2()
            .border_color(if open { ui.accent } else { ui.divider })
            .bg(color)
            .cursor_pointer()
            .tooltip(tooltip(format!("Color of {}'s tabs", e.label)))
            .on_click(cx.listener(move |v, _, _, cx| v.toggle_host_color(&host, cx)));
        focus::tab(el, &id, ui, Ring::Outer, Scroller::Page).into_any_element()
    }

    /// Under a host's row while its color is edited: the theme's colors, back
    /// to automatic, and the picker for any other color.
    fn host_color_editor(&self, ui: Ui, host: &str, cx: &Context<'_, Self>) -> AnyElement {
        let app = cx.global::<AppSettings>();
        let chosen = app.client.tabs.host_color(host);
        let key = hash(host);
        let swatch = |i: usize, name: &'static str, ansi: u8| {
            let (h, on) = (host.to_string(), chosen == Some(HostColor::Ansi(ansi)));
            let id: gpui::ElementId = SharedString::from(format!("host-swatch-{key}-{i}")).into();
            let el = div()
                .id(id.clone())
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(format!("Use {name}")))
                .size(px(26.))
                .rounded_full()
                .border_2()
                .border_color(if on { ui.accent } else { ui.divider })
                .bg(gpui::rgb(app.colors.ansi[usize::from(ansi) % 16].0))
                .cursor_pointer()
                .tooltip(tooltip(name))
                .on_click(cx.listener(move |v, _, _, cx| v.set_host_color(&h, Some(HostColor::Ansi(ansi)), cx)));
            focus::tab(el, &id, ui, Ring::Outer, Scroller::Page)
        };
        let automatic = {
            let h = host.to_string();
            button_with(
                ("host-color-auto", key),
                None,
                "Automatic",
                if chosen.is_none() { ButtonKind::Primary } else { ButtonKind::Normal },
                ui,
                self.act(cx, move |v, cx| v.set_host_color(&h, None, cx)),
            )
        };
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .px(px(16.))
            .pb(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_wrap()
                    .children(HostColor::PALETTE.iter().enumerate().map(|(i, &(name, ansi))| swatch(i, name, ansi)))
                    .child(automatic),
            )
            .child(self.hosts.picker.clone())
            .into_any_element()
    }

    /// The choices for bringing the shell setup to hosts; `default` adds
    /// "As set above" for a host's own choice.
    fn sync_options(default: Option<SyncMode>) -> Vec<kit::Opt<Option<SyncMode>>> {
        let name = |m: SyncMode| match m {
            SyncMode::Ask => "Ask",
            SyncMode::Always => "Always",
            SyncMode::Never => "Never",
        };
        let mut out = Vec::new();
        if let Some(d) = default {
            out.push(opt(None, format!("Default ({})", name(d))).desc("Whatever “Bring your shell setup” above says"));
        }
        out.push(opt(Some(SyncMode::Ask), "Ask").desc("When it differs from this computer's, once for each change made here"));
        out.push(opt(Some(SyncMode::Always), "Always").desc("Copy it without asking whenever the setup here changed"));
        out.push(opt(Some(SyncMode::Never), "Never").desc("Leave the host's shell setup alone; only offer missing tools"));
        out
    }

    fn host_row(&self, ui: Ui, e: &HostEntry, cx: &Context<'_, Self>) -> AnyElement {
        let (host, discovered, plain) = (e.host.clone(), e.discovered, e.plain);
        let ssh = &cx.global::<AppSettings>().file.value.ssh;
        let own_sync = ssh.get(&e.host).and_then(|h| h.sync);
        let sync_select = (!plain).then(|| {
            let host = host.clone();
            div().w(px(150.)).min_w(px(110.)).flex_shrink(1.).child(self.select(
                &format!("host-sync-{}", hash(&e.host)),
                &format!("Bring the shell setup to {}", e.label),
                Self::sync_options(Some(ssh.sync)),
                &own_sync,
                cx,
                move |v, _, cx| AppSettings::update(cx, |s| s.ssh.edit(&host, !discovered, |h| h.sync = v)),
            ))
        });
        let gid = SharedString::from(format!("host-row-{}", hash(&e.host)));
        let mut detail: Vec<String> = Vec::new();
        if e.label != e.host {
            detail.push(e.host.clone());
        }
        detail.extend(e.detail.clone());
        if plain {
            detail.push("plain SSH".into());
        }
        let plain_toggle = {
            let host = host.clone();
            div()
                .id(("host-plain-wrap", hash(&e.host)))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child("Plain SSH"))
                .child(
                    toggle(("host-plain", hash(&e.host)), plain, ui, move |_, cx| {
                        AppSettings::update(cx, |s| s.ssh.edit(&host, !discovered, |h| h.plain = !plain));
                    })
                    .aria_label(SharedString::from(format!("Plain SSH for {}", e.label))),
                )
                .tooltip(tooltip("Run ssh in a local session instead of goattyd on the host: nothing gets installed there"))
        };
        let connect = {
            let host = host.clone();
            button_with(("host-connect", hash(&e.host)), Some(Icon::Terminal), "Connect", ButtonKind::Normal, ui, move |_, cx| {
                hosts::connect(cx, &host)
            })
        };
        let color_button = self.host_color_button(ui, e, cx);
        let row = div()
            .id(gid.clone())
            .group(gid.clone())
            .flex()
            .items_center()
            .gap(px(14.))
            .min_h(px(60.))
            .px(px(16.))
            .py(px(10.))
            .child(
                div()
                    .flex_shrink_0()
                    .size(px(36.))
                    .rounded(px(10.))
                    .bg(ui.field)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::Server.svg().size(px(17.)).text_color(ui.muted)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(text::BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(SharedString::from(e.label.clone())),
                    )
                    .when(!detail.is_empty(), |d| {
                        d.child(
                            div().text_size(px(text::SMALL)).text_color(ui.muted).truncate().child(SharedString::from(detail.join(" · "))),
                        )
                    }),
            )
            .child(color_button)
            .children(sync_select)
            .child(plain_toggle)
            .child(connect)
            .when(!discovered, |d| {
                d.child(icon_button(
                    ("host-remove", hash(&e.host)),
                    Icon::Trash,
                    "Remove from the list",
                    ui,
                    self.act(cx, move |v, cx| {
                        if v.hosts.editing.as_deref() == Some(host.as_str()) {
                            v.hosts.editing = None;
                        }
                        v.hosts.pending = v.hosts.pending.take().filter(|(h, _)| *h != host);
                        AppSettings::update(cx, |s| {
                            s.ssh.hosts.retain(|h| h.host != host);
                            s.tabs.set_host_color(&host, None);
                        });
                    }),
                ))
            });
        let editor = (self.hosts.editing.as_deref() == Some(e.host.as_str())).then(|| self.host_color_editor(ui, &e.host, cx));
        div().flex().flex_col().child(row).children(editor).into_any_element()
    }

    fn host_list(&self, ui: Ui, entries: &[&HostEntry], cx: &Context<'_, Self>) -> gpui::Div {
        let mut c = card(ui);
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                c = c.child(divider(ui));
            }
            c = c.child(self.host_row(ui, e, cx));
        }
        c
    }

    /// The banner for [`skipped_hosts`], with a button that saves the list
    /// as it's used, which drops those entries from `settings.json`.
    fn skipped_hosts_banner(&self, ui: Ui, cx: &Context<'_, Self>) -> Option<gpui::Div> {
        let lines = skipped_hosts(&cx.global::<AppSettings>().issues());
        let title = match lines.len() {
            0 => return None,
            1 => "An entry in settings.json was skipped".to_string(),
            n => format!("{n} entries in settings.json were skipped"),
        };
        let remove = button_with(
            "hosts-drop-skipped",
            Some(Icon::Trash),
            "Remove from the file",
            ButtonKind::Normal,
            ui,
            self.act_window(cx, |v, window, cx| {
                // The banner is going: focus waits on the Add button.
                focus::hand_over(&v.focus, focus::Handover::Control("host-add".into()), window, cx);
                // The loaded list already lacks them: saving it writes them out.
                AppSettings::update(cx, |_| {});
            }),
        )
        .aria_label(SharedString::from("Remove the skipped hosts from settings.json"));
        Some(banner(ui, Tone::Warning, title, lines).items_center().child(remove))
    }

    pub(super) fn hosts_page(&self, ui: Ui, cx: &Context<'_, Self>) -> AnyElement {
        let settings = &cx.global::<AppSettings>().file.value.ssh;
        let (discover, open_in, sync, local_echo) = (settings.discover, settings.open_in, settings.sync, settings.local_echo);
        let entries = hosts::current(cx);
        let (added, found): (Vec<&HostEntry>, Vec<&HostEntry>) = entries.iter().partition(|e| !e.discovered);

        let connecting = section(ui, "Connecting")
            .child(row(ui, "Open hosts in").desc("Shift+Enter in the session list opens it the other way").control(segmented(
                "ssh-open-in",
                &[(OpenIn::Window, "New window"), (OpenIn::Tab, "New tab")],
                &open_in,
                ui,
                |v, _, cx| AppSettings::update(cx, |s| s.ssh.open_in = v),
            )))
            .child(
                row(ui, "Show typing before the host echoes it")
                    .desc("Underlined until the host confirms it; Auto: above 30 ms round trip")
                    .control(segmented(
                        "ssh-local-echo",
                        &[(LocalEcho::Auto, "Auto"), (LocalEcho::Always, "Always"), (LocalEcho::Never, "Never")],
                        &local_echo,
                        ui,
                        |v, _, cx| AppSettings::update(cx, |s| s.ssh.local_echo = v),
                    )),
            )
            .footer(
                "Ctrl+Shift+H lists your hosts and their sessions. goatty runs its session daemon on a host so shells survive \
                 disconnects; it asks before copying it there (to ~/.local/share/goatty, no sudo).",
            );

        let setup = section(ui, "Shell setup")
            .child(row(ui, "Bring your shell setup").desc("To hosts goatty connects to, unless a host below says otherwise").control(
                self.select("ssh-sync", "Bring your shell setup", Self::sync_options(None), &Some(sync), cx, |v, _, cx| {
                    AppSettings::update(cx, |s| s.ssh.sync = v.unwrap_or_default())
                }),
            ))
            .child(row(ui, "Sync the active tab's host now").desc("Compare it with this computer and copy what differs").control(
                button_with("ssh-sync-now", Some(Icon::Refresh), "Sync now", ButtonKind::Normal, ui, |_, cx| {
                    crate::shell_sync::sync_now(cx)
                }),
            ))
            .footer(
                "This computer's prompt, aliases, plugins with their settings, and goatty's tools go to the host, so its shell \
                 feels like this one. Plugins come from the same signed registry at the same version (never downgraded); \
                 nothing is uninstalled there, and the host keeps its own shell program.",
            );

        let form = card(ui)
            .p(px(16.))
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(10.))
                    .child(field(ui, "Host", self.hosts.host.clone()).flex_1())
                    .child(field(ui, "Name to show", self.hosts.name.clone()).w(px(180.)))
                    .child(
                        button_with("host-add", Some(Icon::Plus), "Add", ButtonKind::Primary, ui, self.act(cx, |v, cx| v.add_host(cx)))
                            .h(px(36.)),
                    ),
            )
            .children(self.hosts.error.clone().map(|e| banner(ui, Tone::Danger, e, Vec::new())));
        // "Show" on a problem with `ssh.hosts` lands on the banner (or the
        // form, once it's gone).
        let mut yours = section(ui, "Your hosts").plain();
        yours = match self.skipped_hosts_banner(ui, cx) {
            Some(b) => yours.child(kit::focus_target(ui, "Your hosts", b)).child(form),
            None => yours.child(kit::focus_target(ui, "Your hosts", form)),
        };
        if !added.is_empty() {
            yours = yours.child(self.host_list(ui, &added, cx));
        }

        let mut config = section(ui, "From ~/.ssh/config").child(
            row(ui, "List these hosts too").desc("Every Host entry without wildcards, with the files it includes").control(toggle(
                "ssh-discover",
                discover,
                ui,
                |_, cx| AppSettings::update(cx, |s| s.ssh.discover = !s.ssh.discover),
            )),
        );
        if discover {
            config = if found.is_empty() {
                config.child(kit::empty(ui, Icon::Server, "No hosts found", "~/.ssh/config has no Host entries without wildcards."))
            } else {
                config.children(found.iter().map(|e| self.host_row(ui, e, cx)))
            };
        }
        div().flex().flex_col().gap(px(28.)).child(connecting).child(setup).child(yours).child(config).into_any_element()
    }
}

/// Problems with `ssh.hosts` entries, which loading dropped (a host ssh
/// can't take, one listed twice): the list has no row for them, so they
/// show over it (#149).
fn skipped_hosts(issues: &[Issue]) -> Vec<String> {
    issues.iter().filter(|i| i.concerns("ssh.hosts")).map(|i| i.message.clone()).collect()
}

/// A stable element id for a host.
fn hash(s: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_hosts_are_checked() {
        let existing = [HostEntry { host: "box".into(), label: "box".into(), detail: None, plain: false, discovered: true }];
        assert!(check_new_host(&existing, "pi@10.0.0.5").is_ok());
        assert!(check_new_host(&existing, "box").unwrap_err().contains("already"));
        assert!(check_new_host(&existing, "").is_err());
        assert!(check_new_host(&existing, "a b").is_err());
    }

    #[test]
    fn skipped_hosts_come_from_ssh_hosts_problems() {
        let issues = [
            Issue::new("ssh.hosts.1", "box is listed twice; the first entry counts"),
            Issue::new("ssh.hosts_extra", "not about the list"),
            Issue::new("font.size", "400 is outside 6–48; using 48"),
        ];
        assert_eq!(skipped_hosts(&issues), ["box is listed twice; the first entry counts"]);
        assert!(skipped_hosts(&issues[2..]).is_empty());
    }
}
