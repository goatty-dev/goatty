//! The settings window's Hosts page (#115): the hosts the command palette
//! offers to connect to. Hosts added here and the choices for hosts from
//! `~/.ssh/config` are client settings (`ssh` in `settings.json`), saved
//! right away.

use goatty_config::{Color, HostColor, Issue, LocalEcho, OpenIn, SshHost, ssh_host_problem};
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Subscription, div, prelude::*, px,
};

use super::SettingsWindow;
use super::kit::{self, card, divider, field, row, section};
use crate::hosts::{self, HostEntry, HostList};
use crate::settings::AppSettings;
use crate::ui::color_picker::{ColorPicker, ColorPickerEvent};
use crate::ui::controls::{ButtonKind, Tone, badge, banner, button_with, icon_button, segmented, toggle, tooltip};
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

    fn host_row(&self, ui: Ui, e: &HostEntry, cx: &Context<'_, Self>) -> AnyElement {
        let (host, discovered, plain) = (e.host.clone(), e.discovered, e.plain);
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

    /// The shared shell config (#313): every host goatty met or the shared
    /// config names, whether it follows it, how it stands; devices waiting
    /// to be trusted; changes made on a host without a GUI.
    fn shared_section(&self, ui: Ui, cx: &Context<'_, Self>) -> kit::Section {
        use crate::shared_sync::{self, Standing};
        let mut sec =
            section(ui, "Shared shell config").desc("One shell config on every host that follows it, with per-host overrides").action(
                button_with("shared-sync-now", Some(Icon::Refresh), "Sync now", ButtonKind::Normal, ui, |_, cx| shared_sync::sync_now(cx)),
            );
        let Some(state) = shared_sync::state(cx) else {
            return sec
                .child(kit::empty(ui, Icon::Loader, "Not synced yet", "goatty syncs the shared config with each host it connects to."))
                .footer(SHARED_FOOTER);
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for id in state.listed() {
            let known = state.hosts.get(&id);
            let standing = state.standing(&id);
            let member = state.member(&id) == Some(true);
            let mut detail = vec![standing.label().to_string()];
            if let Some(t) = known.and_then(|k| k.synced_unix) {
                detail.push(format!("last sync {}", ago(now.saturating_sub(t))));
            }
            if let Some(st) = known.and_then(|k| k.status.as_ref()) {
                let pending = state.doc.newer_than(&st.have).len();
                if pending > 0 {
                    detail.push(format!("{pending} change(s) waiting"));
                }
                if !st.needs_review.is_empty() {
                    let ids: Vec<&str> = st.needs_review.iter().map(|r| r.id.as_str()).collect();
                    detail.push(format!("needs review: {}", ids.join(", ")));
                }
                if let Some(a) = st.last_apply.as_ref().filter(|a| !a.ok) {
                    detail.push(format!("last apply failed: {}", a.lines.first().cloned().unwrap_or_default()));
                }
            }
            if let Some(e) = known.and_then(|k| k.error.as_ref()) {
                detail.push(format!("couldn't sync: {e}"));
            }
            let overrides = state.doc.overrides(&id, &state.trust).len();
            if overrides > 0 {
                detail.push(format!("{overrides} override(s)"));
            }
            let label = state.label(&id);
            let local = state.doc.local_changes(&id, &state.trust);
            let tone = match standing {
                Standing::Synced => Tone::Success,
                Standing::Waiting => Tone::Warning,
                Standing::NotSynced | Standing::Offline => Tone::Neutral,
            };
            let host = id.clone();
            let mut r = row(ui, label.clone())
                .icon(if state.me.as_deref() == Some(id.as_str()) { Icon::Monitor } else { Icon::Server })
                .desc(detail.join(" · "))
                .control(
                    div().flex().items_center().gap(px(10.)).child(badge(standing.label(), tone, ui)).child(
                        toggle(("shared-member", hash(&id)), member, ui, move |_, cx| shared_sync::set_member(host.clone(), !member, cx))
                            .aria_label(SharedString::from(format!("{label} follows the shared config"))),
                    ),
                );
            if !local.is_empty() {
                let paths: Vec<String> = local.iter().map(|p| goatty_config::shared::display_path(p)).collect();
                let (h1, h2) = (id.clone(), id.clone());
                r = r.below(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_size(px(text::SMALL))
                                .text_color(ui.muted)
                                .child(SharedString::from(format!("Changed on {label} itself: {}", paths.join(", ")))),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button_with(("shared-share", hash(&id)), None, "Share", ButtonKind::Normal, ui, move |_, cx| {
                                    shared_sync::adopt_local_changes(h1.clone(), true, cx)
                                }))
                                .child(button_with(
                                    ("shared-keep", hash(&id)),
                                    None,
                                    format!("Keep on {label} only"),
                                    ButtonKind::Normal,
                                    ui,
                                    move |_, cx| shared_sync::adopt_local_changes(h2.clone(), false, cx),
                                )),
                        ),
                );
            }
            sec = sec.child(r);
        }
        // Devices whose changes wait to be trusted (this computer's view).
        let me = state.me.clone().unwrap_or_default();
        let untrusted = state.hosts.get(&me).and_then(|k| k.status.as_ref()).map(|s| s.untrusted.clone()).unwrap_or_default();
        for u in untrusted {
            let name = u.name.clone().unwrap_or_else(|| goatty_config::shared::short_id(&u.device).to_owned());
            let (d1, d2) = (u.device.clone(), u.device.clone());
            sec = sec.child(
                row(ui, format!("Changes from {name}"))
                    .icon(Icon::TriangleAlert)
                    .desc(format!(
                        "{} change(s) signed by {} ({}) don't apply until you trust it",
                        u.entries,
                        name,
                        goatty_config::shared::short_id(&u.device)
                    ))
                    .control(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button_with(("shared-trust", hash(&u.device)), None, "Trust", ButtonKind::Primary, ui, move |_, cx| {
                                shared_sync::trust_device(d1.clone(), true, cx)
                            }))
                            .child(button_with(
                                ("shared-ignore", hash(&u.device)),
                                None,
                                "Ignore",
                                ButtonKind::Normal,
                                ui,
                                move |_, cx| shared_sync::ignore_device(d2.clone(), cx),
                            )),
                    ),
            );
        }
        sec.footer(SHARED_FOOTER)
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
        let (discover, open_in, local_echo) = (settings.discover, settings.open_in, settings.local_echo);
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

        let setup = self.shared_section(ui, cx);

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

const SHARED_FOOTER: &str = "Hosts that follow get your prompt, aliases, plugins and their settings, and apply changes \
     themselves; set a value for one host only from the scope picker on the Shell pages. Changes are signed by the goatty \
     computer that made them, and a host only accepts them from devices you trust, so a server can't change this computer's \
     shell.";

/// "5 min ago".
fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
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
