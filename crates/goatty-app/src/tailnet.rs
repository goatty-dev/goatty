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

use goatty_client::tailnet::{self, DirectPairOutcome, Discovery, Found, Pairing};
use goatty_client::{Connection, Endpoint, RemoteProblem};
use goatty_net::Paths;
use goatty_net::gateway::{GatewayError, Refusal};
use goatty_net::pairing::{KnownHost, PairToken, spaced};
use goatty_net::tailscale::Tailscale;
use gpui::{App, ClipboardItem, Global, Window};
use parking_lot::Mutex;

use crate::command_palette::input::Suggestion;
use crate::command_palette::{Ask, Command, Field};
use crate::hosts::{self, HostRef, OpenRequest};
use crate::settings::AppSettings;
use crate::settings_window::{HostLink, HostLinkGlobal};
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

/// Open a paired tailnet host in a new tab or window (`ssh.open_in`).
pub fn connect(cx: &mut App, dns: &str, name: &str) {
    let place = cx.global::<AppSettings>().client.ssh.open_in;
    hosts::open(
        cx,
        OpenRequest {
            host: HostRef::Tailnet { dns: dns.to_string(), name: name.to_string() },
            attach: hosts::default_attach(cx),
            conn: None,
            place,
        },
    );
}

/// Pair with a user-provided string: either a one-time `goatty-pair:…` code
/// from `goattyd net pair` or a Tailscale hostname / MagicDNS / IP (optional
/// `:port`).
pub fn pair_with_input(cx: &mut App, input: &str) {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        toast::show(cx, Toast::warning("Enter a pairing code or Tailscale host").key("tailnet.pair.input"));
        return;
    }
    if PairToken::looks_like(trimmed) {
        pair_token(cx, trimmed.to_string());
    } else {
        pair_address(cx, trimmed.to_string());
    }
}

/// Pair in one step using a one-time `goatty-pair:…` code printed by a host.
pub fn pair_token(cx: &mut App, token_str: String) {
    let parsed = match PairToken::decode(&token_str) {
        Ok(t) => t,
        Err(e) => {
            toast::show(cx, Toast::error("Invalid pairing code").body(format!("{e:#}")).key("tailnet.pair.token"));
            return;
        }
    };
    let name = parsed.display_name().to_string();
    let key = format!("tailnet.pair.{}", parsed.host_id());
    let id = toast::show(
        cx,
        Toast::loading(format!("Pairing with {name}…")).body("Connecting over Tailscale with your one-time code").key(key.clone()).sticky(),
    );
    cx.spawn(async move |cx| {
        let token_clone = token_str.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                let paths = paths().ok_or_else(|| anyhow::anyhow!("no $HOME: goatty keeps this computer's tailnet key there"))?;
                tailnet::pair_with_token(&Tailscale::system(), &paths, &token_clone, &tailnet::my_name())
            })
            .await;
        cx.update(|cx| match result {
            Ok(h) => {
                let (dns, host_name) = (h.dns.clone(), h.name.clone());
                let t = Toast::success(format!("Paired with {}", h.name))
                    .body("It's in the host switcher (Ctrl+Shift+H) now.")
                    .key(key)
                    .action("Connect", move |_, cx| connect(cx, &dns, &host_name));
                toast::update(cx, id, t);
                refresh_now(cx);
            }
            Err(e) => {
                let body = match GatewayError::refusal(&e) {
                    Some(Refusal::PairingClosed) => {
                        format!("The pairing code expired or was already used. Run `goattyd net pair` on {name} for a fresh code.")
                    }
                    _ => format!("{e:#}"),
                };
                let retry = token_str.clone();
                let t = Toast::error(format!("Couldn't pair with {name}"))
                    .body(body)
                    .key(key)
                    .action("Try again", move |_, cx| pair_token(cx, retry.clone()));
                toast::update(cx, id, t);
            }
        });
    })
    .detach();
}

/// Pair with a Tailscale hostname, MagicDNS name, or IP (with optional `:port`)
/// using the interactive 6-digit code handshake. Works even for tagged servers
/// or custom ports that discovery didn't list.
pub fn pair_address(cx: &mut App, target: String) {
    let key = format!("tailnet.pair.{target}");
    let state = Arc::new(Mutex::new(PairState::default()));
    stage(cx, None, &state, Toast::loading(format!("Pairing with {target}…")).body("Asking it for a code").key(key.clone()).sticky());
    cx.spawn(async move |cx| {
        let target_clone = target.clone();
        let started = cx
            .background_executor()
            .spawn(async move {
                let paths = paths().ok_or_else(|| anyhow::anyhow!("no $HOME: goatty keeps this computer's tailnet key there"))?;
                Pairing::start_target(&Tailscale::system(), &paths, &target_clone, &tailnet::my_name())
            })
            .await;
        let pairing = match started {
            Ok(p) => p,
            Err(e) => {
                let t = match GatewayError::refusal(&e) {
                    Some(Refusal::PairingClosed) => Toast::warning(format!("{target} isn't ready to pair")).body(format!(
                        "On {target}, run `goattyd net pair` (or open Settings → Network → Allow a new device), then try again."
                    )),
                    _ => Toast::error(format!("Couldn't pair with {target}")).body(format!("{e:#}")),
                };
                let retry = target.clone();
                cx.update(|cx| {
                    let t = t.key(key.clone()).action("Try again", move |_, cx| pair_address(cx, retry.clone()));
                    stage(cx, None, &state, t);
                });
                return;
            }
        };
        let host_name = if pairing.host_name.is_empty() { target.clone() } else { pairing.host_name.clone() };
        let retry_target = target.clone();
        run_interactive_pairing(cx, pairing, host_name, key, state, Arc::new(move |cx| pair_address(cx, retry_target.clone()))).await;
    })
    .detach();
}

/// Command Palette: "Tailnet: Pair with a host or code…".
pub fn ask_pair_host(window: &mut Window, cx: &mut App) {
    refresh(cx);
    let suggestions: Vec<Suggestion> = current(cx)
        .into_iter()
        .filter(|e| !e.paired)
        .map(|e| Suggestion { value: e.dns.clone(), label: e.name.clone(), detail: Some(format!("{} · {}", e.dns, e.detail)) })
        .collect();
    let mut field = Field::text("target", "Pairing code or Tailscale host")
        .placeholder("goatty-pair:… or vps.tailnet.ts.net (optional :port)")
        .help("On a headless host, run `goattyd net pair` and paste its code here — or type a Tailscale name or IP");
    field.trim = true;
    field.suggestions = suggestions;
    let ask = Ask::new("Pair with a Tailscale host", vec![field], |answers, _, cx| pair_with_input(cx, &answers["target"]));
    crate::command_palette::ask(window, cx, ask);
}

/// Command Palette: "Tailnet: Pair this SSH host over Tailscale".
/// Pairs the active tab's SSH host in one step; if the active tab isn't an SSH
/// host, opens the SSH host picker instead.
pub fn pair_current_ssh_host(window: &mut Window, cx: &mut App) {
    let link = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone());
    match link {
        Some(HostLink::Daemon(conn)) if matches!(conn.endpoint(), Endpoint::Ssh(_)) => pair_ssh_connection(conn, cx),
        Some(HostLink::Daemon(conn)) if matches!(conn.endpoint(), Endpoint::Tailnet(_)) => {
            toast::show(
                cx,
                Toast::info(format!("Already connected to {} over Tailscale", conn.endpoint().label())).key("tailnet.pair.ssh"),
            );
        }
        _ => ask_pair_ssh(window, cx),
    }
}

/// Command Palette: "Tailnet: Pair an SSH host over Tailscale…".
pub fn ask_pair_ssh(window: &mut Window, cx: &mut App) {
    let known: Vec<(String, String)> = hosts::current(cx).into_iter().map(|e| (e.host, e.label)).collect();
    let field = Field::host("host", "SSH host", known)
        .placeholder("user@example.com, or an alias from ~/.ssh/config")
        .help("Connects over SSH, enables Tailscale Serve there, and pairs this computer in one step");
    let ask = Ask::new("Pair an SSH host over Tailscale", vec![field], |answers, _, cx| {
        pair_ssh_host(cx, answers["host"].clone());
    });
    crate::command_palette::ask(window, cx, ask);
}

/// Connect to `ssh_host` over SSH (reusing an open pooled connection if one
/// exists) and pair it over Tailscale in one step.
pub fn pair_ssh_host(cx: &mut App, ssh_host: String) {
    let endpoint = HostRef::Ssh(ssh_host.clone()).endpoint();
    if let Some(conn) = hosts::pool::live(&endpoint) {
        pair_ssh_connection(conn, cx);
        return;
    }
    let key = format!("tailnet.pair.ssh.{ssh_host}");
    let id = toast::show(
        cx,
        Toast::loading(format!("Connecting to {ssh_host} over SSH…"))
            .body("Enabling Tailscale access and exchanging device keys")
            .key(key.clone())
            .sticky(),
    );
    cx.spawn(async move |cx| {
        let ep = endpoint.clone();
        let opened = cx.background_executor().spawn(async move { hosts::pool::open(ep) }).await;
        match opened {
            Ok(conn) => cx.update(|cx| pair_ssh_connection_in(conn, Some(id), cx)),
            Err(e) => {
                let needs_install = matches!(
                    e.downcast_ref::<RemoteProblem>(),
                    Some(RemoteProblem::NotInstalled { .. } | RemoteProblem::Incompatible { .. })
                );
                cx.update(|cx| {
                    let h = ssh_host.clone();
                    let t = if needs_install {
                        Toast::warning(format!("{ssh_host} needs goattyd first"))
                            .body("Open an SSH tab to install or update goattyd there, then pair over Tailscale.")
                            .key(key)
                            .action("Connect over SSH", move |_, cx| hosts::connect(cx, &h))
                    } else {
                        let retry = ssh_host.clone();
                        Toast::error(format!("Couldn't connect to {ssh_host} over SSH"))
                            .body(format!("{e:#}"))
                            .key(key)
                            .action("Try again", move |_, cx| pair_ssh_host(cx, retry.clone()))
                    };
                    toast::update(cx, id, t);
                });
            }
        }
    })
    .detach();
}

/// Enable Tailscale Serve on the remote host behind `conn`, authorize this
/// computer's device key there, and pin the host locally — all in one step.
pub fn pair_ssh_connection(conn: Arc<Connection>, cx: &mut App) {
    pair_ssh_connection_in(conn, None, cx);
}

fn pair_ssh_connection_in(conn: Arc<Connection>, existing_toast: Option<toast::ToastId>, cx: &mut App) {
    let label = conn.endpoint().label().to_string();
    let key = format!("tailnet.pair.ssh.{label}");
    let loading = Toast::loading(format!("Pairing {label} over Tailscale…"))
        .body("Enabling Tailscale access and exchanging device keys")
        .key(key.clone())
        .sticky();
    let id = match existing_toast {
        Some(id) => {
            toast::update(cx, id, loading);
            id
        }
        None => toast::show(cx, loading),
    };
    cx.spawn(async move |cx| {
        let c = conn.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                let paths = paths().ok_or_else(|| anyhow::anyhow!("no $HOME: goatty keeps this computer's tailnet key there"))?;
                tailnet::pair_via_remote(
                    |req| {
                        let reply = c.net(serde_json::to_string(req)?)?;
                        serde_json::from_str::<goatty_net::api::Reply>(&reply)?.into_result()
                    },
                    &Tailscale::system(),
                    &paths,
                    &tailnet::my_name(),
                )
            })
            .await;
        cx.update(|cx| match result {
            Ok(DirectPairOutcome::Paired { host, reachability }) => {
                let (dns, name) = (host.dns.clone(), host.name.clone());
                let body = match reachability {
                    Ok(via) => format!("Reachable over Tailscale ({via}). You can connect without SSH from Ctrl+Shift+H."),
                    Err(note) => note,
                };
                let t = Toast::success(format!("Paired {} over Tailscale", host.name))
                    .body(body)
                    .key(key)
                    .action("Connect over Tailnet", move |_, cx| connect(cx, &dns, &name));
                toast::update(cx, id, t);
                refresh_now(cx);
            }
            Ok(DirectPairOutcome::NeedsPermission { host_name, permission }) => {
                let cmd = permission.operator.clone();
                let retry_conn = conn.clone();
                let t = Toast::warning(format!("Tailscale on {host_name} needs permission"))
                    .body(format!("Run `{}` once on {host_name}, then try again.", permission.operator))
                    .key(key)
                    .sticky()
                    .action("Copy command", move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(cmd.clone()));
                        toast::show(cx, Toast::success("Copied command").key("tailnet.pair.copy"));
                    })
                    .action("Try again", move |_, cx| pair_ssh_connection(retry_conn.clone(), cx));
                toast::update(cx, id, t);
            }
            Err(e) => {
                let retry_conn = conn.clone();
                let t = Toast::error(format!("Couldn't pair {label} over Tailscale"))
                    .body(format!("{e:#}"))
                    .key(key)
                    .action("Try again", move |_, cx| pair_ssh_connection(retry_conn.clone(), cx));
                toast::update(cx, id, t);
            }
        });
    })
    .detach();
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
                        "On {name}, run `goattyd net pair` (or open Settings → Network → Allow a new device), then try again."
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
        let retry_found = found.clone();
        run_interactive_pairing(cx, pairing, name, key, state, Arc::new(move |cx| pair(cx, retry_found.clone()))).await;
    })
    .detach();
}

async fn run_interactive_pairing(
    cx: &mut gpui::AsyncApp,
    mut pairing: Pairing,
    name: String,
    key: String,
    state: Arc<Mutex<PairState>>,
    retry: Arc<dyn Fn(&mut App) + Send + Sync>,
) {
    let code = spaced(&pairing.code);
    let cancel = pairing.canceller();
    let cancel: Arc<dyn Fn() + Send + Sync> = Arc::new(cancel);
    cx.update(|cx| ask_code(cx, &name, &code, &key, &state, &cancel, &retry));
    // The host's answer, in the background.
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
    cx.update(|cx| finish(cx, None, &name, &key, &state, &retry));
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
    retry: &Arc<dyn Fn(&mut App) + Send + Sync>,
) {
    let (s1, s2, c2) = (state.clone(), state.clone(), cancel.clone());
    let (n, k, r) = (name.to_string(), key.to_string(), retry.clone());
    let t = Toast::info(format!("Does {name} show {code}?"))
        .body(format!("Confirm the code on {name} too. Only confirm here if both screens show the same code."))
        .key(key.to_string())
        .sticky()
        .action("Codes match", move |window, cx| {
            s1.lock().confirmed_here = true;
            finish(cx, Some(window), &n, &k, &s1, &r);
        })
        .action("Cancel", move |_, _| {
            s2.lock().cancelled = true;
            c2();
        });
    stage(cx, None, state, t);
}

/// Called when this side confirmed and when the host answered: pin once
/// both happened, report what went wrong otherwise.
fn finish(
    cx: &mut App,
    window: Option<&gpui::Window>,
    name: &str,
    key: &str,
    state: &Arc<Mutex<PairState>>,
    retry: &Arc<dyn Fn(&mut App) + Send + Sync>,
) {
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
            let retry = retry.clone();
            s.cancelled = true;
            drop(s);
            stage(cx, window, state, t.key(key.to_string()).action("Try again", move |_, cx| retry(cx)));
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
                    let (dns, host_name) = (h.dns.clone(), h.name.clone());
                    let t = Toast::success(format!("Paired with {name}"))
                        .body("It's in the host switcher (Ctrl+Shift+H) now.")
                        .key(key.to_string())
                        .action("Connect", move |_, cx| connect(cx, &dns, &host_name));
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
