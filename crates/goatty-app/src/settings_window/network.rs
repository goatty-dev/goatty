//! The Network page (#15): goatty over the user's tailnet.
//!
//! The top half is about the active tab's host (through its goattyd, the
//! `Net` request): whether Tailscale is there, making the host available
//! to the user's other devices (one `tailscale serve` route), the port,
//! the pairing window with the requests waiting for a confirmation, and
//! the paired devices. The bottom half is this computer as a client:
//! discovery in the host switcher and the hosts it paired with.

use std::time::Duration;

use goatty_net::api::{NetStatus, Request, RouteStatus};
use goatty_net::identity::short;
use goatty_net::pairing::spaced;
use goatty_net::tailscale::State;
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Task, div, prelude::*, px,
};

use super::kit::{self, card, row, section};
use super::{Page, SettingsWindow};
use crate::settings::AppSettings;
use crate::tailnet::{self, TailnetList};
use crate::toast::{self, Toast};
use crate::ui::controls::{ButtonKind, Tone, badge, banner, button_with, icon_button, toggle};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

pub struct NetPage {
    pub status: Option<NetStatus>,
    pub error: Option<String>,
    pub loading: bool,
    /// A change (enable, pairing, revoke) is running.
    pub busy: bool,
    pub port: Entity<TextInput>,
    /// Refreshes the status while the pairing window is open.
    poll: Option<Task<()>>,
}

impl NetPage {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> (Self, gpui::Subscription) {
        let port = cx.new(|cx| TextInput::new("7422", "", cx).label("Port"));
        let sub = cx.subscribe(&port, |view, _, ev: &TextInputEvent, cx| {
            if let TextInputEvent::Submitted(_) = ev {
                view.net_change_port(cx);
            }
        });
        (Self { status: None, error: None, loading: false, busy: false, port, poll: None }, sub)
    }

    /// Forget what was shown (another host).
    pub fn reset(&mut self) {
        self.status = None;
        self.error = None;
        self.poll = None;
    }
}

/// `4:05`.
fn mm_ss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

impl SettingsWindow {
    pub fn load_net(&mut self, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else {
            self.net.error = Some("This window isn't connected to a goatty host.".into());
            return;
        };
        if self.net.loading {
            return;
        }
        self.net.loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { link.net(&Request::Status) }).await;
            let _ = this.update(cx, |view, cx| {
                view.net.loading = false;
                view.net_show(result, cx);
            });
        })
        .detach();
    }

    fn net_show(&mut self, result: anyhow::Result<NetStatus>, cx: &mut Context<'_, Self>) {
        match result {
            Ok(s) => {
                if self.net.status.as_ref().is_none_or(|old| old.port != s.port) {
                    let port = s.port.to_string();
                    self.net.port.update(cx, |i, cx| i.set_text(&port, cx));
                }
                self.net.status = Some(s);
                self.net.error = None;
            }
            Err(e) => self.net.error = Some(format!("{e:#}")),
        }
        self.net_poll(cx);
        cx.notify();
    }

    /// Send `req` to the host; show the status after it, or say what went wrong.
    fn net_do(&mut self, req: Request, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        if self.net.busy {
            return;
        }
        self.net.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { link.net(&req) }).await;
            let _ = this.update(cx, |view, cx| {
                view.net.busy = false;
                match result {
                    Ok(s) => view.net_show(Ok(s), cx),
                    Err(e) => {
                        toast::show(cx, Toast::error("Tailnet").body(format!("{e:#}")).key("settings.net"));
                        // What went wrong shows on the page too (the route's state).
                        view.load_net(cx);
                    }
                }
            });
        })
        .detach();
    }

    fn net_change_port(&mut self, cx: &mut Context<'_, Self>) {
        let text = self.net.port.read(cx).text().trim().to_string();
        match text.parse::<u16>() {
            Ok(port) if port > 0 => self.net_do(Request::Enable { port: Some(port) }, cx),
            _ => {
                toast::show(cx, Toast::error("That isn't a port").body("A number from 1 to 65535.").key("settings.net"));
            }
        }
    }

    /// While the pairing window is open and this page shows, refresh every
    /// second (requests come in, the time runs down).
    fn net_poll(&mut self, cx: &mut Context<'_, Self>) {
        let open = self.net.status.as_ref().is_some_and(|s| s.pairing.is_some());
        if !open || self.page != Page::Network {
            self.net.poll = None;
            return;
        }
        if self.net.poll.is_some() {
            return;
        }
        self.net.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let go_on = this.update(cx, |view, cx| {
                    let open = view.page == Page::Network && view.net.status.as_ref().is_some_and(|s| s.pairing.is_some());
                    if open && !view.net.busy {
                        view.load_net(cx);
                    }
                    open
                });
                if !go_on.unwrap_or(false) {
                    break;
                }
            }
            let _ = this.update(cx, |view, _| view.net.poll = None);
        }));
    }

    fn tailscale_section(&self, ui: Ui, s: &NetStatus) -> kit::Section {
        let (icon, tone, title, desc) = match &s.tailscale {
            State::Running(r) => (
                Icon::CircleCheck,
                Tone::Success,
                format!("Connected as {}", r.me.name),
                format!("Tailscale {}{}", r.version, if r.tun { "" } else { " · userspace networking (goatty uses tailscale nc)" }),
            ),
            State::NotInstalled => (
                Icon::Info,
                Tone::Neutral,
                "Tailscale isn't installed".into(),
                "With it, your computers reach each other's goatty without SSH".into(),
            ),
            other => (Icon::TriangleAlert, Tone::Warning, other.describe(), "goatty uses Tailscale as you set it up".into()),
        };
        let mut sec = section(ui, "Tailscale").child(row(ui, title).icon(icon).desc(desc).control(badge(
            if tone == Tone::Success { "Ready" } else { "Not ready" },
            tone,
            ui,
        )));
        if matches!(s.tailscale, State::NotInstalled) {
            sec = sec.child(
                row(ui, "Get Tailscale")
                    .icon(Icon::ExternalLink)
                    .desc("tailscale.com: install it and log in; goatty never does that for you")
                    .on_click("net-get-tailscale", |_, _, cx| cx.open_url("https://tailscale.com/download")),
            );
        }
        sec.footer("goatty only uses the Tailscale you installed: no second login, no extra device. SSH keeps working as before.")
    }

    fn route_problem(&self, ui: Ui, s: &NetStatus, cx: &Context<'_, Self>) -> Option<AnyElement> {
        match &s.route {
            RouteStatus::Permission(p) => {
                let copy = |id: &'static str, cmd: String| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px(px(10.))
                                .py(px(6.))
                                .rounded(px(8.))
                                .bg(ui.field)
                                .font_family(kit::mono(cx))
                                .text_size(px(text::SMALL))
                                .truncate()
                                .child(SharedString::from(cmd.clone())),
                        )
                        .child(icon_button(id, Icon::Copy, "Copy", ui, move |_, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(cmd.clone()));
                            toast::show(cx, Toast::success("Copied").key("settings.net.copy"));
                        }))
                };
                Some(
                    card(ui)
                        .p(px(16.))
                        .gap(px(10.))
                        .child(banner(
                            ui,
                            Tone::Warning,
                            "Tailscale doesn't let goatty add its route yet",
                            vec!["Run this once in a terminal (it needs sudo rights), then turn availability on again:".into()],
                        ))
                        .child(copy("net-copy-operator", p.operator.clone()))
                        .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child("Or add the route yourself:"))
                        .child(copy("net-copy-direct", p.direct.clone()))
                        .into_any_element(),
                )
            }
            RouteStatus::PortTaken { .. } | RouteStatus::Failed { .. } => {
                Some(banner(ui, Tone::Danger, s.route.describe(s.port), Vec::new()).into_any_element())
            }
            RouteStatus::Off | RouteStatus::Active | RouteStatus::NoTailscale => None,
        }
    }

    fn pairing_rows(&self, ui: Ui, s: &NetStatus, cx: &Context<'_, Self>) -> Vec<AnyElement> {
        let Some(w) = &s.pairing else { return Vec::new() };
        if w.pending.is_empty() {
            return vec![
                row(ui, "Waiting for a device")
                    .icon(Icon::Loader)
                    .desc("On the other computer: Ctrl+Shift+H, pick this one under the tailnet's computers, Pair")
                    .into_any_element(),
            ];
        }
        w.pending
            .iter()
            .map(|p| {
                let (yes, no) = (p.id, p.id);
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .px(px(16.))
                    .py(px(12.))
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
                                    .child(SharedString::from(format!("{} wants to pair", p.name))),
                            )
                            .child(
                                div().text_size(px(text::SMALL)).text_color(ui.muted).child(SharedString::from(format!(
                                    "Confirm only if its screen shows this code · key {}",
                                    short(&p.key)
                                ))),
                            ),
                    )
                    .child(
                        div()
                            .font_family(kit::mono(cx))
                            .text_size(px(text::TITLE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(spaced(&p.code))),
                    )
                    .child(button_with(
                        ("net-confirm", p.id as usize),
                        Some(Icon::Check),
                        "Confirm",
                        ButtonKind::Primary,
                        ui,
                        self.act(cx, move |v, cx| v.net_do(Request::Decide { id: yes, accept: true }, cx)),
                    ))
                    .child(button_with(
                        ("net-reject", p.id as usize),
                        None,
                        "Reject",
                        ButtonKind::Normal,
                        ui,
                        self.act(cx, move |v, cx| v.net_do(Request::Decide { id: no, accept: false }, cx)),
                    ))
                    .into_any_element()
            })
            .collect()
    }

    fn devices_section(&self, ui: Ui, s: &NetStatus, cx: &Context<'_, Self>) -> kit::Section {
        let mut sec = section(ui, "Paired devices");
        if s.devices.is_empty() {
            return sec.child(kit::empty(ui, Icon::Shield, "No paired devices", "Allow a new device above to pair one."));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for d in &s.devices {
            let key = d.key.clone();
            let ago = match now.saturating_sub(d.paired_unix) / 86_400 {
                0 => "today".to_string(),
                1 => "yesterday".to_string(),
                n => format!("{n} days ago"),
            };
            sec = sec.child(row(ui, d.name.clone()).icon(Icon::ShieldCheck).desc(format!("Key {} · paired {ago}", short(&d.key))).control(
                icon_button(
                    SharedString::from(format!("net-revoke-{}", &d.key[..16.min(d.key.len())])),
                    Icon::Trash,
                    format!("Revoke {}", d.name),
                    ui,
                    self.act(cx, move |v, cx| v.net_do(Request::Revoke { key: key.clone() }, cx)),
                ),
            ));
        }
        sec.footer("A revoked device can't connect any more; it can pair again while you allow a new device.")
    }

    fn client_section(&self, ui: Ui, cx: &Context<'_, Self>) -> kit::Section {
        let settings = cx.global::<AppSettings>().file.value.tailnet.clone();
        let mut sec = section(ui, "Your other computers")
            .child(
                row(ui, "List computers on your tailnet")
                    .desc("In the host switcher: the paired ones, and ones running goatty you can pair with")
                    .control(toggle("net-discover", settings.discover, ui, |_, cx| {
                        AppSettings::update(cx, |s| s.tailnet.discover = !s.tailnet.discover);
                        tailnet::refresh_now(cx);
                    })),
            )
            .child(
                row(ui, "Include everyone's devices")
                    .desc("Also other people's and tagged devices on your tailnet, not only yours")
                    .disabled(!settings.discover)
                    .control(toggle("net-all-devices", settings.all_devices, ui, |_, cx| {
                        AppSettings::update(cx, |s| s.tailnet.all_devices = !s.tailnet.all_devices);
                        tailnet::refresh_now(cx);
                    })),
            );
        let known = cx.try_global::<TailnetList>().map(|l| l.known.clone()).unwrap_or_default();
        for h in known.iter() {
            let dns = h.dns.clone();
            sec = sec.child(row(ui, h.name.clone()).icon(Icon::Server).desc(format!("{} · paired", h.dns)).control(icon_button(
                SharedString::from(format!("net-forget-{}", h.dns)),
                Icon::Trash,
                format!("Forget {}", h.name),
                ui,
                move |_, cx| tailnet::forget(cx, &dns),
            )));
        }
        sec.footer("Ctrl+Shift+H lists them. Pairing compares a 6-digit code on both screens; goatty then checks both computers' keys on every connection.")
    }

    pub(super) fn network_page(&self, ui: Ui, cx: &Context<'_, Self>) -> AnyElement {
        let mut page = div().flex().flex_col().gap(px(28.));
        let label = self.link.as_ref().map(|l| l.label()).unwrap_or_else(|| "this computer".into());
        let local = self.link.as_ref().is_none_or(|l| l.is_local());
        match (&self.net.status, &self.net.error) {
            (_, Some(e)) => {
                page = page.child(banner(ui, Tone::Danger, "Couldn't ask the host", vec![e.clone()]));
            }
            (None, None) => {
                page = page.child(kit::empty(ui, Icon::Loader, "Checking Tailscale…", "Asking the host's goattyd."));
            }
            (Some(s), None) => {
                let running = s.tailscale.running().is_some();
                page = page.child(self.tailscale_section(ui, s));
                let what = if local { "Make this computer available".to_string() } else { format!("Make {label} available") };
                let busy = self.net.busy;
                let available = s.available;
                let mut avail = section(ui, "Availability")
                    .child(
                        row(ui, what)
                            .desc(if running {
                                "Your other devices on the tailnet can open sessions here once paired"
                            } else {
                                "Needs Tailscale running and logged in"
                            })
                            .disabled(!running && !available || busy)
                            .control(toggle(
                                "net-available",
                                available,
                                ui,
                                self.act(cx, move |v, cx| {
                                    v.net_do(if available { Request::Disable } else { Request::Enable { port: None } }, cx)
                                }),
                            )),
                    )
                    .child(
                        row(ui, "Port")
                            .desc("goatty's port on the tailnet; type another and press Enter to move there")
                            .disabled(!available || busy)
                            .control(div().w(px(110.)).child(self.net.port.clone())),
                    );
                if available && s.route == RouteStatus::Active {
                    avail = avail.footer(format!(
                        "One `tailscale serve` TCP route on port {} forwards into goattyd's private socket; goatty never touches your other routes.",
                        s.port
                    ));
                }
                page = page.child(avail);
                if let Some(p) = self.route_problem(ui, s, cx) {
                    page = page.child(p);
                }
                if available {
                    let open = s.pairing.as_ref().map(|w| w.secs_left);
                    let mut pairing = section(ui, "Pairing").child(
                        row(ui, "Allow a new device")
                            .desc(match open {
                                Some(left) => format!("Open for {} more", mm_ss(left)),
                                None => "Opens for 5 minutes; you confirm the code the device shows".into(),
                            })
                            .disabled(busy)
                            .control(match open {
                                Some(_) => button_with(
                                    "net-pair-stop",
                                    None,
                                    "Stop",
                                    ButtonKind::Normal,
                                    ui,
                                    self.act(cx, |v, cx| v.net_do(Request::ClosePairing, cx)),
                                ),
                                None => button_with(
                                    "net-pair-open",
                                    Some(Icon::Plus),
                                    "Allow",
                                    ButtonKind::Primary,
                                    ui,
                                    self.act(cx, |v, cx| v.net_do(Request::OpenPairing { secs: None }, cx)),
                                ),
                            }),
                    );
                    for r in self.pairing_rows(ui, s, cx) {
                        pairing = pairing.child(r);
                    }
                    page = page.child(pairing.footer(format!("This host's goatty key: {}", short(&s.key))));
                }
                if available || !s.devices.is_empty() {
                    page = page.child(self.devices_section(ui, s, cx));
                }
            }
        }
        page = page.child(self.client_section(ui, cx));
        page.into_any_element()
    }
}
