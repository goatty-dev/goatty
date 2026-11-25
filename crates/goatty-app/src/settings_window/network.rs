//! The Network page (#15, #301): goatty over the user's tailnet.
//!
//! Covers both directions clearly:
//! - **Connect to another computer** (client side): paste a one-time pairing
//!   code (`goatty-pair:…`) from a headless server (`goattyd net pair`), enter
//!   any Tailscale host or IP (optional `:port`), copy the exact headless setup
//!   command, browse discovered peers and paired hosts, or — when viewing a
//!   remote SSH host — pair it over Tailscale in one click.
//! - **Share this computer** (host side, through the active tab's `goattyd`):
//!   Tailscale Serve availability, port, incoming pairing window (with a
//!   copyable one-time pairing code and 6-digit code confirmations), and
//!   paired devices (revoke).

use std::time::Duration;

use goatty_net::api::{NetStatus, Request, RouteStatus};
use goatty_net::identity::short;
use goatty_net::pairing::spaced;
use goatty_net::tailscale::State;
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Subscription, Task, div,
    prelude::*, px,
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
    /// Input for pairing with another computer by `goatty-pair:…` code or Tailscale host[:port].
    pub pair_input: Entity<TextInput>,
    /// Refreshes the status while the pairing window is open.
    poll: Option<Task<()>>,
    /// Bumped when the window follows another host: replies to requests
    /// sent to the previous one are dropped.
    host_gen: u64,
    /// Numbers the requests, so a status read before a change isn't shown
    /// after it.
    seq: u64,
    /// The number of the latest change (enable, pairing, revoke, …).
    changed: u64,
    _subs: Vec<Subscription>,
}

impl NetPage {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> Self {
        let port = cx.new(|cx| TextInput::new("7422", "", cx).label("Port"));
        let pair_input = cx.new(|cx| TextInput::new("goatty-pair:… or vps.tailnet.ts.net", "", cx).label("Pairing code or Tailscale host"));
        let subs = vec![
            cx.subscribe(&port, |view, _, ev: &TextInputEvent, cx| {
                if let TextInputEvent::Submitted(_) = ev {
                    view.net_change_port(cx);
                }
            }),
            cx.subscribe(&pair_input, |view, _, ev: &TextInputEvent, cx| {
                if let TextInputEvent::Submitted(_) = ev {
                    view.net_pair_input(cx);
                }
            }),
        ];
        Self {
            status: None,
            error: None,
            loading: false,
            busy: false,
            port,
            pair_input,
            poll: None,
            host_gen: 0,
            seq: 0,
            changed: 0,
            _subs: subs,
        }
    }

    /// Forget what was shown (another host).
    pub fn reset(&mut self) {
        self.status = None;
        self.error = None;
        self.poll = None;
        self.loading = false;
        self.busy = false;
        self.host_gen += 1;
    }

    /// A number for the next request.
    fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

/// `4:05`.
fn mm_ss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// A monospace pill with a Copy button (for commands and one-time pairing codes).
fn copyable(
    id: impl Into<gpui::ElementId>,
    value: String,
    copied_msg: &'static str,
    ui: Ui,
    cx: &Context<'_, SettingsWindow>,
) -> gpui::Div {
    let shown = SharedString::from(value.clone());
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
                .child(shown),
        )
        .child(icon_button(id, Icon::Copy, "Copy", ui, move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
            toast::show(cx, Toast::success(copied_msg).key("settings.net.copy"));
        }))
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
        let (host_gen, seq) = (self.net.host_gen, self.net.next());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { link.net(&Request::Status) }).await;
            let _ = this.update(cx, |view, cx| {
                if view.net.host_gen != host_gen {
                    return; // asked of the host the window showed before
                }
                view.net.loading = false;
                if seq < view.net.changed {
                    // Read before a change that answered (or will answer) with a newer status.
                    return;
                }
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
        let (host_gen, seq) = (self.net.host_gen, self.net.next());
        self.net.changed = seq;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { link.net(&req) }).await;
            let _ = this.update(cx, |view, cx| {
                if view.net.host_gen != host_gen {
                    return;
                }
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

    fn net_pair_input(&mut self, cx: &mut Context<'_, Self>) {
        let input = self.net.pair_input.read(cx).text().trim().to_string();
        if input.is_empty() {
            toast::show(cx, Toast::warning("Enter a pairing code or Tailscale host").key("settings.net.pair"));
            return;
        }
        self.net.pair_input.update(cx, |i, cx| i.set_text("", cx));
        tailnet::pair_with_input(cx, &input);
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

    fn tailscale_section(&self, ui: Ui, s: &NetStatus, label: &str, local: bool) -> kit::Section {
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
                "With it, your computers reach each other's goatty directly without SSH".into(),
            ),
            other => (Icon::TriangleAlert, Tone::Warning, other.describe(), "goatty uses Tailscale as you set it up".into()),
        };
        let mut sec = section(ui, "Tailscale").child(row(ui, title).icon(icon).desc(desc).control(badge(
            if tone == Tone::Success { "Ready" } else { "Not ready" },
            tone,
            ui,
        )));
        if !local {
            sec = sec.desc(format!("Showing Tailscale status on {label} (the active tab's host)"));
        }
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

    /// When viewing a remote SSH host in Settings → Network, offer one-click
    /// Tailscale pairing right at the top.
    fn ssh_quick_pair_section(&self, ui: Ui, s: &NetStatus, label: &str, cx: &Context<'_, Self>) -> Option<kit::Section> {
        let conn = self.link.as_ref()?.ssh_connection()?;
        let running = s.tailscale.running();
        let known = cx.try_global::<TailnetList>().map(|l| l.known.clone()).unwrap_or_default();
        let paired_host = running
            .and_then(|r| {
                let id = r.me.host_id();
                known.iter().find(|h| h.dns == id && h.key == s.key)
            })
            .cloned();
        let sec = section(ui, "Pair over Tailscale");
        if let Some(h) = paired_host {
            let (dns, name) = (h.dns.clone(), h.name.clone());
            return Some(
                sec.child(
                    row(ui, format!("{} is paired with this computer", h.name))
                        .icon(Icon::CircleCheck)
                        .desc(format!("{} · port {} · works anywhere on your tailnet without SSH", h.dns, h.port))
                        .control(button_with(
                            "net-ssh-connect-tailnet",
                            Some(Icon::Terminal),
                            "Connect over Tailnet",
                            ButtonKind::Primary,
                            ui,
                            move |_, cx| tailnet::connect(cx, &dns, &name),
                        )),
                ),
            );
        }
        let can_pair = running.is_some() && !self.net.busy;
        let desc = if running.is_some() {
            "Enables Tailscale access there and pairs this computer in one step — faster than SSH and works off your local network"
        } else {
            "Install and log in to Tailscale on this host first"
        };
        Some(sec.child(row(ui, format!("Pair {label} with this computer")).icon(Icon::Globe).desc(desc).disabled(!can_pair).control(
            button_with("net-ssh-pair-now", Some(Icon::Globe), "Pair over Tailscale", ButtonKind::Primary, ui, move |_, cx| {
                tailnet::pair_ssh_connection(conn.clone(), cx)
            }),
        )))
    }

    fn route_problem(&self, ui: Ui, s: &NetStatus, cx: &Context<'_, Self>) -> Option<AnyElement> {
        match &s.route {
            RouteStatus::Permission(p) => Some(
                card(ui)
                    .p(px(16.))
                    .gap(px(10.))
                    .child(banner(
                        ui,
                        Tone::Warning,
                        "Tailscale doesn't let goatty add its route yet",
                        vec!["Run this once in a terminal (it needs sudo rights), then turn availability on again:".into()],
                    ))
                    .child(copyable("net-copy-operator", p.operator.clone(), "Copied", ui, cx))
                    .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child("Or add the route yourself:"))
                    .child(copyable("net-copy-direct", p.direct.clone(), "Copied", ui, cx))
                    .into_any_element(),
            ),
            RouteStatus::PortTaken { .. } | RouteStatus::Failed { .. } => {
                Some(banner(ui, Tone::Danger, s.route.describe(s.port), Vec::new()).into_any_element())
            }
            RouteStatus::Off | RouteStatus::Active | RouteStatus::NoTailscale => None,
        }
    }

    fn pairing_rows(&self, ui: Ui, s: &NetStatus, cx: &Context<'_, Self>) -> Vec<AnyElement> {
        let Some(w) = &s.pairing else { return Vec::new() };
        let mut out = Vec::new();
        if let Some(tok) = &w.token {
            out.push(
                row(ui, "One-time pairing code")
                    .icon(Icon::KeyRound)
                    .desc("Paste on the other computer in Settings → Network, or run `goatty net pair <code>`")
                    .below(copyable("net-copy-pair-token", tok.clone(), "Copied pairing code", ui, cx))
                    .into_any_element(),
            );
        }
        if w.pending.is_empty() {
            out.push(
                row(ui, "Waiting for a device")
                    .icon(Icon::Loader)
                    .desc("Paste the pairing code on the other computer, run `goatty net pair <code>`, or pick this host in Ctrl+Shift+H")
                    .into_any_element(),
            );
            return out;
        }
        out.extend(w.pending.iter().map(|p| {
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
                            div()
                                .text_size(px(text::SMALL))
                                .text_color(ui.muted)
                                .child(SharedString::from(format!("Confirm only if its screen shows this code · key {}", short(&p.key)))),
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
        }));
        out
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
        let pair_form = div().flex().items_center().gap(px(8.)).child(div().flex_1().min_w_0().child(self.net.pair_input.clone())).child(
            button_with("net-pair-submit", Some(Icon::Link), "Pair", ButtonKind::Primary, ui, self.act(cx, |v, cx| v.net_pair_input(cx))),
        );
        let headless_help = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(copyable("net-copy-headless-cmd", "goattyd net pair".into(), "Copied command", ui, cx))
            .child(
                div()
                    .text_size(px(text::SMALL))
                    .text_color(ui.muted)
                    .child("If the server is reachable over SSH right now, you can also run \"Tailnet: Pair an SSH host over Tailscale…\" in Ctrl+Shift+P."),
            );
        let mut sec = section(ui, "Your other computers")
            .desc("Connect to headless servers and other computers over Tailscale without SSH")
            .action(button_with("net-refresh-peers", Some(Icon::Refresh), "Refresh", ButtonKind::Normal, ui, |_, cx| {
                tailnet::refresh_now(cx);
            }))
            .child(
                row(ui, "Pair with a host or pairing code")
                    .icon(Icon::KeyRound)
                    .desc("Paste a `goatty-pair:…` code, or enter a Tailscale name or IP (optional :port)")
                    .below(pair_form),
            )
            .child(
                row(ui, "Pair a headless server")
                    .icon(Icon::Terminal)
                    .desc("No GUI needed on the server: run this in its terminal, then paste its pairing code above")
                    .below(headless_help),
            )
            .child(
                row(ui, "List computers on your tailnet")
                    .desc("In the host switcher and below: paired hosts, and ones running goatty you can pair with")
                    .control(toggle("net-discover", settings.discover, ui, |_, cx| {
                        AppSettings::update(cx, |s| s.tailnet.discover = !s.tailnet.discover);
                        tailnet::refresh_now(cx);
                    })),
            )
            .child(
                row(ui, "Include tagged and shared devices")
                    .desc("Also tagged servers (e.g. tag:server) and other people's devices on your tailnet")
                    .disabled(!settings.discover)
                    .control(toggle("net-all-devices", settings.all_devices, ui, |_, cx| {
                        AppSettings::update(cx, |s| s.tailnet.all_devices = !s.tailnet.all_devices);
                        tailnet::refresh_now(cx);
                    })),
            );

        let entries = tailnet::current(cx);
        for e in entries {
            if e.paired {
                let (dns_c, name_c, dns_f) = (e.dns.clone(), e.name.clone(), e.dns.clone());
                let controls = div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(button_with(
                        SharedString::from(format!("net-connect-{}", e.dns)),
                        Some(Icon::Terminal),
                        "Connect",
                        ButtonKind::Normal,
                        ui,
                        move |_, cx| tailnet::connect(cx, &dns_c, &name_c),
                    ))
                    .child(icon_button(
                        SharedString::from(format!("net-forget-{}", e.dns)),
                        Icon::Trash,
                        format!("Forget {}", e.name),
                        ui,
                        move |_, cx| tailnet::forget(cx, &dns_f),
                    ));
                sec = sec.child(row(ui, e.name).icon(Icon::Server).desc(format!("{} · {}", e.dns, e.detail)).control(controls));
            } else if let Some(found) = e.found {
                let ready = found.probe.pairing;
                let label = if found.key_changed { "Pair again" } else { "Pair" };
                let kind = if ready { ButtonKind::Primary } else { ButtonKind::Normal };
                let f = found.clone();
                sec = sec.child(row(ui, e.name).icon(Icon::Globe).desc(format!("{} · {}", e.dns, e.detail)).control(button_with(
                    SharedString::from(format!("net-pair-found-{}", e.dns)),
                    Some(Icon::Link),
                    label,
                    kind,
                    ui,
                    move |_, cx| tailnet::pair(cx, f.clone()),
                )));
            }
        }
        sec.footer(
            "Ctrl+Shift+H lists paired and discovered hosts. From a terminal: `goatty net pair <code|host>`, `goatty net pair --ssh HOST`, or `goatty --tailnet HOST`.",
        )
    }

    fn host_sections(&self, ui: Ui, s: &NetStatus, label: &str, local: bool, cx: &Context<'_, Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let running = s.tailscale.running().is_some();
        let what = if local { "Make this computer available".to_string() } else { format!("Make {label} available") };
        let heading = if local { "Share this computer".to_string() } else { format!("Share {label}") };
        let busy = self.net.busy;
        let available = s.available;
        let mut avail = section(ui, heading)
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
                        self.act(cx, move |v, cx| v.net_do(if available { Request::Disable } else { Request::Enable { port: None } }, cx)),
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
        out.push(avail.into_any_element());
        if let Some(p) = self.route_problem(ui, s, cx) {
            out.push(p);
        }
        if available {
            let open = s.pairing.as_ref().map(|w| w.secs_left);
            let mut pairing = section(ui, "Pairing").child(
                row(ui, "Allow a new device")
                    .desc(match open {
                        Some(left) => format!("Open for {} more", mm_ss(left)),
                        None => "Opens for 5 minutes with a one-time pairing code and 6-digit confirmation".into(),
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
            out.push(
                pairing
                    .footer(format!("This host's goatty key: {} · From a terminal: `goattyd net pair`", short(&s.key)))
                    .into_any_element(),
            );
        }
        if available || !s.devices.is_empty() {
            out.push(self.devices_section(ui, s, cx).into_any_element());
        }
        out
    }

    pub(super) fn network_page(&self, ui: Ui, cx: &Context<'_, Self>) -> AnyElement {
        let mut page = div().flex().flex_col().gap(px(28.));
        let label = self.link.as_ref().map(|l| l.label()).unwrap_or_else(|| "this computer".into());
        let local = self.link.as_ref().is_none_or(|l| l.is_local());
        match (&self.net.status, &self.net.error) {
            (_, Some(e)) => {
                page = page.child(banner(ui, Tone::Danger, "Couldn't ask the host", vec![e.clone()]));
                page = page.child(self.client_section(ui, cx));
            }
            (None, None) => {
                page = page.child(kit::empty(ui, Icon::Loader, "Checking Tailscale…", "Asking the host's goattyd."));
                page = page.child(self.client_section(ui, cx));
            }
            (Some(s), None) => {
                page = page.child(self.tailscale_section(ui, s, &label, local));
                if local {
                    // On this computer, connecting to headless/remote hosts comes first.
                    page = page.child(self.client_section(ui, cx));
                    page = page.children(self.host_sections(ui, s, &label, local, cx));
                } else {
                    // When viewing a remote SSH host, offer one-click pairing first,
                    // then that host's availability & paired devices.
                    if let Some(quick) = self.ssh_quick_pair_section(ui, s, &label, cx) {
                        page = page.child(quick);
                    }
                    page = page.children(self.host_sections(ui, s, &label, local, cx));
                    page = page.child(self.client_section(ui, cx));
                }
            }
        }
        page.into_any_element()
    }
}
