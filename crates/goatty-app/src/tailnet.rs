//! Computers on the user's tailnet that run goatty (#15), on the client
//! side: the paired ones and the ones discovery found, as host switcher
//! and palette entries, and pairing with a new one.
//!
//! Discovery runs in the background when the switcher, the palette or the
//! Network page opens (`hosts::refresh`): one `tailscale status`, then a
//! probe of goatty's port on each online peer of the same user. Nothing
//! polls. Paired hosts connect through `Endpoint::Tailnet` like any other
//! host (sessions, tabs, reconnect, shell setup sync).
//!
//! Pairing ([`pair`]): the host's user opens its pairing window ("Allow a
//! new device", or `goattyd net pair`); here a toast shows the code, the
//! user compares it with the host's screen and confirms on both. The host
//! is pinned only when both confirmed.

use std::sync::Arc;

use goatty_client::tailnet::{self, Discovery, Found, Pairing};
use goatty_net::Paths;
use goatty_net::gateway::{GatewayError, Refusal};
use goatty_net::pairing::{KnownHost, spaced};
use goatty_net::tailscale::Tailscale;
use gpui::{App, Global};
use parking_lot::Mutex;

use crate::command_palette::Command;
use crate::hosts::{self, HostRef, OpenRequest};
use crate::settings::AppSettings;
use crate::toast::{self, Toast};

/// goatty's tailnet files on this computer (device key, pinned hosts).
/// `None` without `$HOME`: never a shared temporary directory, where
/// another user could plant a key.
pub fn paths() -> Option<Paths> {
    goatty_shell::Dirs::from_env().map(|d| Paths::new(d.tailnet_dir()))
}

/// The hosts this computer paired with.
fn known() -> Vec<KnownHost> {
    paths().map(|p| tailnet::known_hosts(&p)).unwrap_or_default()
}

/// What the switcher knows about the tailnet: the paired hosts and the
/// last discovery.
#[derive(Default)]
pub struct TailnetList {
    pub known: Arc<Vec<KnownHost>>,
    /// `None` until discovery ran (or with discovery off).
    pub discovery: Option<Arc<Discovery>>,
    reading: bool,
    /// A refresh was asked for while one ran: run another when it ends
    /// (its results may predate a pairing or a forget).
    again: bool,
    /// When the last discovery finished: the palette opens often, and each
    /// discovery probes every online peer.
    finished: Option<std::time::Instant>,
}

/// Discovery runs at most this often.
const REDISCOVER_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

impl Global for TailnetList {}

/// Re-read the paired hosts and discover again now (after pairing,
/// forgetting, or a change of the discovery settings).
pub fn refresh_now(cx: &mut App) {
    cx.default_global::<TailnetList>().finished = None;
    refresh(cx);
}

/// Re-read the paired hosts and, with `tailnet.discover`, discover peers
/// in the background (at most every few seconds); observers of
/// [`TailnetList`] hear about changes.
pub fn refresh(cx: &mut App) {
    let list = cx.default_global::<TailnetList>();
    if list.reading {
        list.again |= list.finished.is_none();
        return;
    }
    if list.finished.is_some_and(|t| t.elapsed() < REDISCOVER_AFTER) {
        return;
    }
    list.reading = true;
    let settings = cx.global::<AppSettings>().client.tailnet.clone();
    cx.spawn(async move |cx| {
        let (known, discovery) = cx
            .background_executor()
            .spawn(async move {
                let Some(paths) = paths() else { return (Vec::new(), None) };
                let known = tailnet::known_hosts(&paths);
                let discovery = settings
                    .discover
                    .then(|| tailnet::discover(&Tailscale::system(), &paths, goatty_net::DEFAULT_PORT, settings.all_devices));
                (known, discovery)
            })
            .await;
        cx.update(|cx| {
            let l = cx.global::<TailnetList>();
            let same = *l.known == known && l.discovery.as_deref() == discovery.as_ref();
            let finished = Some(std::time::Instant::now());
            let again = cx.global::<TailnetList>().again;
            if same {
                let l = cx.global_mut::<TailnetList>();
                l.reading = false;
                l.again = false;
                l.finished = finished;
            } else {
                let (known, discovery) = (Arc::new(known), discovery.map(Arc::new));
                cx.set_global(TailnetList { known, discovery, reading: false, again: false, finished });
            }
            if again {
                refresh_now(cx);
            }
        });
    })
    .detach();
}

/// A tailnet computer to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub dns: String,
    pub name: String,
    pub paired: bool,
    /// One line: path, offline, pairing open, ….
    pub detail: String,
    /// What discovery found (to pair with it).
    pub found: Option<Found>,
}

/// The paired hosts (with what discovery saw of them), then the unpaired
/// computers discovery found.
pub fn entries(known: &[KnownHost], discovery: Option<&Discovery>) -> Vec<Entry> {
    let status = discovery.and_then(|d| d.tailscale.running());
    let found = |dns: &str| discovery.and_then(|d| d.found.iter().find(|f| f.node.dns == dns));
    let mut out: Vec<Entry> = known
        .iter()
        .map(|h| {
            let f = found(&h.dns);
            let peer = status.and_then(|s| s.peer(&h.dns));
            let detail = match (f, peer) {
                (Some(f), _) if f.key_changed => "its goatty key changed: pair again".to_string(),
                (Some(f), _) => path(&f.node),
                (None, Some(p)) if !p.online => "offline".into(),
                (None, Some(_)) => "not available right now".into(),
                (None, None) if status.is_some() => "not on your tailnet any more".into(),
                (None, None) => "tailnet".into(),
            };
            Entry { dns: h.dns.clone(), name: h.name.clone(), paired: f.is_none_or(|f| !f.key_changed), detail, found: f.cloned() }
        })
        .collect();
    if let Some(d) = discovery {
        for f in d.found.iter().filter(|f| !known.iter().any(|h| h.dns == f.node.dns)) {
            let detail = if f.probe.pairing { "on your tailnet · ready to pair".into() } else { "on your tailnet · not paired".into() };
            out.push(Entry { dns: f.node.dns.clone(), name: f.node.name.clone(), paired: false, detail, found: Some(f.clone()) });
        }
    }
    out
}

/// `tailnet · direct` or `tailnet · relay fra`.
fn path(node: &goatty_net::tailscale::Node) -> String {
    if node.direct {
        "tailnet · direct".into()
    } else if node.relay.is_empty() {
        "tailnet · relayed".into()
    } else {
        format!("tailnet · relay {}", node.relay)
    }
}

/// [`entries`] from the app's last read.
pub fn current(cx: &App) -> Vec<Entry> {
    match cx.try_global::<TailnetList>() {
        Some(l) => entries(&l.known, l.discovery.as_deref()),
        None => entries(&known(), None),
    }
}

/// The name a paired host is shown with (its Tailscale host name).
pub fn name_of(dns: &str) -> String {
    known().into_iter().find(|h| h.dns == dns).map_or_else(|| dns.split('.').next().unwrap_or(dns).to_string(), |h| h.name)
}

/// The palette's command for `e`: connect to a paired host (its id makes
/// it a host for the switcher's session list), or pair with a found one.
pub fn command(e: &Entry) -> Command {
    let mut c = if e.paired {
        let host = HostRef::Tailnet { dns: e.dns.clone(), name: e.name.clone() };
        let id = format!("tailnet.host:{}", e.dns);
        let mut c = Command::handler(&id, "Tailnet", &format!("Connect to {}", e.name), move |_, cx| {
            let place = cx.global::<AppSettings>().client.ssh.open_in;
            hosts::open(cx, OpenRequest { host: host.clone(), attach: hosts::default_attach(cx), conn: None, place });
        });
        let live = hosts::live_detail(&HostRef::Tailnet { dns: e.dns.clone(), name: e.name.clone() });
        c.detail = Some(live.map_or_else(|| e.detail.clone(), |l| format!("{l} · tailnet")).into());
        c
    } else {
        let found = e.found.clone();
        let again = e.found.as_ref().is_some_and(|f| f.key_changed);
        let title = if again { format!("Pair with {} again", e.name) } else { format!("Pair with {}", e.name) };
        let mut c = Command::handler(&format!("tailnet.pair:{}", e.dns), "Tailnet", &title, move |_, cx| {
            if let Some(f) = &found {
                pair(cx, f.clone());
            }
        });
        c.detail = Some(e.detail.clone().into());
        c
    };
    let mut words = vec!["tailscale", "tailnet", "remote", "pair", e.dns.as_str()];
    if let Some(f) = &e.found {
        words.push(f.node.os.as_str());
    }
    c = c.keywords(&words);
    c.only_when_searched = true;
    c
}

/// What the user has done so far in a pairing.
#[derive(Default)]
struct PairState {
    /// The pairing's toast, kept through every stage so it stays in the
    /// window it started in.
    toast: Option<toast::ToastId>,
    confirmed_here: bool,
    cancelled: bool,
    /// The host's answer: `Ok` when its user confirmed.
    host: Option<Result<(), String>>,
    pairing: Option<Pairing>,
}

/// Pair with `found`: ask it, show the code in a toast, and pin it once
/// the users of both computers confirmed the same code.
pub fn pair(cx: &mut App, found: Found) {
    let name = found.node.name.clone();
    let key = format!("tailnet.pair.{}", found.node.dns);
    let state = Arc::new(Mutex::new(PairState::default()));
    stage(cx, None, &state, Toast::loading(format!("Pairing with {name}…")).body("Asking it for a code").key(key.clone()).sticky());
    cx.spawn(async move |cx| {
        let node = found.node.clone();
        let started = cx
            .background_executor()
            .spawn(async move {
                let paths = paths().ok_or_else(|| anyhow::anyhow!("no $HOME: goatty keeps this computer's tailnet key there"))?;
                let ts = Tailscale::system();
                let status = ts.detect();
                let status = status.running().ok_or_else(|| anyhow::anyhow!("{}", status.describe()))?;
                let port = tailnet::port_of(&paths, &node.dns);
                Pairing::start(&ts, &paths, status, &node, port, &tailnet::my_name())
            })
            .await;
        let pairing = match started {
            Ok(p) => p,
            Err(e) => {
                let t = match GatewayError::refusal(&e) {
                    Some(Refusal::PairingClosed) => Toast::warning(format!("{name} isn't ready to pair")).body(format!(
                        "On {name}, open Settings → Network → Allow a new device (or run `goattyd net pair` there), then try again."
                    )),
                    _ => Toast::error(format!("Couldn't pair with {name}")).body(format!("{e:#}")),
                };
                cx.update(|cx| {
                    let t = t.key(key.clone()).action("Try again", move |_, cx| pair(cx, found.clone()));
                    stage(cx, None, &state, t);
                });
                return;
            }
        };
        let code = spaced(&pairing.code);
        let cancel = pairing.canceller();
        let cancel: Arc<dyn Fn() + Send + Sync> = Arc::new(cancel);
        cx.update(|cx| ask_code(cx, &name, &code, &key, &state, &cancel, &found));
        // The host's answer, in the background.
        let mut pairing = pairing;
        let (pairing, answer) = cx
            .background_executor()
            .spawn(async move {
                let r = pairing.wait().map_err(|e| format!("{e:#}"));
                (pairing, r)
            })
            .await;
        {
            let mut s = state.lock();
            s.host = Some(answer);
            s.pairing = Some(pairing);
        }
        cx.update(|cx| finish(cx, None, &name, &key, &state, &found));
    })
    .detach();
}

/// Show the pairing's next stage: in place of its toast in the window it
/// is in. From a toast's button (`window`), as a new toast there with the
/// same key (the button dismisses the old one); otherwise as an update.
fn stage(cx: &mut App, window: Option<&gpui::Window>, state: &Arc<Mutex<PairState>>, t: Toast) {
    let id = state.lock().toast;
    let id = match (window, id) {
        (Some(w), _) => toast::show_in(w, cx, t),
        (None, Some(id)) => {
            toast::update(cx, id, t);
            id
        }
        (None, None) => toast::show(cx, t),
    };
    state.lock().toast = Some(id);
}

/// The toast with the code: "Codes match" or "Cancel".
fn ask_code(
    cx: &mut App,
    name: &str,
    code: &str,
    key: &str,
    state: &Arc<Mutex<PairState>>,
    cancel: &Arc<dyn Fn() + Send + Sync>,
    found: &Found,
) {
    let (s1, s2, c2) = (state.clone(), state.clone(), cancel.clone());
    let (n, k, f) = (name.to_string(), key.to_string(), found.clone());
    let t = Toast::info(format!("Does {name} show {code}?"))
        .body(format!("Confirm the code on {name} too. Only confirm here if both screens show the same code."))
        .key(key.to_string())
        .sticky()
        .action("Codes match", move |window, cx| {
            s1.lock().confirmed_here = true;
            finish(cx, Some(window), &n, &k, &s1, &f);
        })
        .action("Cancel", move |_, _| {
            s2.lock().cancelled = true;
            c2();
        });
    stage(cx, None, state, t);
}

/// Called when this side confirmed and when the host answered: pin once
/// both happened, report what went wrong otherwise.
fn finish(cx: &mut App, window: Option<&gpui::Window>, name: &str, key: &str, state: &Arc<Mutex<PairState>>, found: &Found) {
    let mut s = state.lock();
    if s.cancelled {
        return;
    }
    match (&s.host, s.confirmed_here) {
        (Some(Err(e)), _) => {
            let rejected = e.contains("wasn't confirmed");
            let t = if rejected {
                Toast::warning(format!("{name} didn't confirm the pairing"))
                    .body("It was rejected there, or its pairing window closed.".to_string())
            } else {
                Toast::error(format!("Pairing with {name} failed")).body(e.clone())
            };
            let f = found.clone();
            s.cancelled = true;
            drop(s);
            stage(cx, window, state, t.key(key.to_string()).action("Try again", move |_, cx| pair(cx, f.clone())));
        }
        (None, true) => {
            drop(s);
            let t =
                Toast::loading(format!("Waiting for {name}…")).body(format!("Confirm the code on {name}.")).key(key.to_string()).sticky();
            stage(cx, window, state, t);
        }
        (Some(Ok(())), true) => {
            let pinned = s.pairing.as_ref().map(|p| p.pin());
            s.cancelled = true;
            drop(s);
            match pinned {
                Some(Ok(h)) => {
                    let host = HostRef::Tailnet { dns: h.dns.clone(), name: h.name.clone() };
                    let t = Toast::success(format!("Paired with {name}"))
                        .body("It's in the host switcher (Ctrl+Shift+H) now.")
                        .key(key.to_string())
                        .action("Connect", move |_, cx| {
                            let place = cx.global::<AppSettings>().client.ssh.open_in;
                            hosts::open(cx, OpenRequest { host: host.clone(), attach: hosts::default_attach(cx), conn: None, place });
                        });
                    stage(cx, window, state, t);
                    refresh_now(cx);
                }
                Some(Err(e)) => {
                    let t = Toast::error(format!("Couldn't save the pairing with {name}")).body(format!("{e:#}")).key(key.to_string());
                    stage(cx, window, state, t);
                }
                None => {}
            }
        }
        // The host confirmed first: the toast with the code still waits for this side.
        (Some(Ok(())), false) | (None, false) => {}
    }
}

/// Forget a paired host on this computer.
pub fn forget(cx: &mut App, dns: &str) {
    let name = name_of(dns);
    let forgot = paths().ok_or_else(|| anyhow::anyhow!("no $HOME")).and_then(|p| tailnet::forget(&p, dns));
    match forgot {
        Ok(_) => {
            toast::show(
                cx,
                Toast::success(format!("Forgot {name}")).body(format!(
                    "This computer won't connect to it any more. To revoke this computer there too: Settings → Network on {name}."
                )),
            );
            refresh_now(cx);
        }
        Err(e) => {
            toast::show(cx, Toast::error(format!("Couldn't forget {name}")).body(format!("{e:#}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use goatty_net::gateway::{Greeting, Probe};
    use goatty_net::tailscale::{Node, State, Status};

    use super::*;

    fn node(name: &str, online: bool, direct: bool) -> Node {
        Node {
            name: name.into(),
            dns: format!("{name}.ts.net"),
            addrs: vec!["100.64.0.9".into()],
            os: "linux".into(),
            user_id: 1,
            user: None,
            online,
            tagged: false,
            direct,
            relay: "fra".into(),
        }
    }

    fn found(n: Node, key: &str, paired: bool) -> Found {
        let greeting = Greeting {
            gateway: 1,
            goatty: "0.1.0".into(),
            name: n.name.clone(),
            key: key.into(),
            nonce: String::new(),
            commit: String::new(),
            protocol: (7, 12),
        };
        Found { node: n, probe: Probe { greeting, pairing: false }, paired, key_changed: false }
    }

    fn known(name: &str) -> KnownHost {
        KnownHost { dns: format!("{name}.ts.net"), name: name.into(), addr: String::new(), port: 7422, key: "k".into(), paired_unix: 0 }
    }

    #[test]
    fn paired_hosts_come_first_with_their_state() {
        let me = node("me", true, true);
        let status = Status {
            version: "1.104.0".into(),
            tun: true,
            me,
            peers: vec![node("desk", true, true), node("nas", false, false), node("new", true, false)],
        };
        let mut changed = found(node("box", true, true), "other", false);
        changed.key_changed = true;
        let d = Discovery {
            tailscale: State::Running(status),
            found: vec![found(node("desk", true, true), "k", true), found(node("new", true, false), "n", false), changed],
            owner_known: true,
        };
        let e = entries(&[known("desk"), known("nas"), known("gone"), known("box")], Some(&d));
        let summary: Vec<_> = e.iter().map(|e| (e.name.as_str(), e.paired, e.detail.as_str())).collect();
        assert_eq!(
            summary,
            [
                ("desk", true, "tailnet · direct"),
                ("nas", true, "offline"),
                ("gone", true, "not on your tailnet any more"),
                ("box", false, "its goatty key changed: pair again"),
                ("new", false, "on your tailnet · not paired"),
            ]
        );
        assert_eq!(command(&e[0]).id.as_ref(), "tailnet.host:desk.ts.net");
        assert_eq!(command(&e[4]).id.as_ref(), "tailnet.pair:new.ts.net");
        assert_eq!(command(&e[3]).title.as_ref(), "Pair with box again");
        // Without discovery the paired hosts are still listed.
        let e = entries(&[known("desk")], None);
        assert_eq!((e[0].paired, e[0].detail.as_str()), (true, "tailnet"));
    }
}
