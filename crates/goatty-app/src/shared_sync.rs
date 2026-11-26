//! One shared shell config on every host, from the GUI's side (#313).
//!
//! Every connection to a goattyd that takes part (protocol v18) runs an
//! exchange (`goatty_shell::shared::exchange`) between this computer's
//! goattyd, that host and every other host this process is connected to:
//! each gets what it lacks and applies it itself. That is how a change made
//! on the laptop reaches the desktop through any server both connect to.
//!
//! What only the user can decide happens here:
//!
//! * **Membership.** The first time goatty meets a host that never decided,
//!   it asks once: follow the shared config (keeping the host's differences
//!   as its overrides, or replacing them) or keep it separate. A host whose
//!   goattyd goatty just installed joins without a question. Choices carried
//!   over from the old push sync (`ssh.sync`) answer it for the hosts they
//!   named ([`ClientState`]).
//! * **Trust.** Changes signed by a device this computer doesn't trust yet
//!   wait until the user trusts it (once per device) or ignores it.
//! * **This computer** seeds the shared config from its own settings the
//!   first time (it is a member like any other).
//!
//! Older daemons keep the push sync (`shell_sync`). Edits made in the
//! settings window go through [`publish`].

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use goatty_config::shared::{self, ClientState, Entry, Hlc, SharedDoc, Signer, Trust};
use goatty_config::{HostSettings, store};
use goatty_shell::shared::api::Status;
use goatty_shell::shared::{self as host_shared, Device, Peer};
use gpui::{App, Global};

use crate::settings_window::{self, HostLink, Page};
use crate::toast::{self, Toast};

/// What this process knows about the shared config.
#[derive(Default)]
pub struct Shared {
    /// Everything the last exchange saw, merged.
    pub doc: SharedDoc,
    /// The devices this computer trusts.
    pub trust: Trust,
    /// This device (and this computer's host id: the same key).
    pub me: Option<String>,
    /// Every host met or named in the shared config, by id.
    pub hosts: BTreeMap<String, Known>,
    /// Devices the user chose not to be asked about again.
    pub ignored: BTreeSet<String>,
    /// The newest exchange started, and the newest one recorded: an older
    /// result finishing later doesn't replace a newer one.
    started: u64,
    recorded: u64,
}

impl Global for Shared {}

/// A host as the scope picker and the Hosts page show it.
#[derive(Clone)]
pub struct Known {
    pub label: String,
    /// The connection the last exchange used (`None`: not connected).
    pub link: Option<HostLink>,
    pub status: Option<Status>,
    /// When it was last exchanged with (unix seconds).
    pub synced_unix: Option<u64>,
    pub error: Option<String>,
}

/// How a host stands, for the dot next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Follows the shared config and has everything this computer has.
    Synced,
    /// Follows it, but has changes waiting (offline, or the last exchange failed).
    Waiting,
    /// Keeps its own settings (or never decided).
    NotSynced,
    /// Not connected, and doesn't follow.
    Offline,
}

impl Standing {
    pub fn label(self) -> &'static str {
        match self {
            Standing::Synced => "synced",
            Standing::Waiting => "waiting to sync",
            Standing::NotSynced => "not synced",
            Standing::Offline => "offline",
        }
    }
}

impl Shared {
    /// What `id` is called.
    pub fn label(&self, id: &str) -> String {
        if self.me.as_deref() == Some(id) {
            return "This computer".into();
        }
        self.hosts
            .get(id)
            .map(|k| k.label.clone())
            .or_else(|| self.doc.name(id, &self.trust))
            .unwrap_or_else(|| shared::short_id(id).to_owned())
    }

    /// Whether `id` follows the shared config.
    pub fn member(&self, id: &str) -> Option<bool> {
        self.hosts.get(id).and_then(|k| k.status.as_ref()).and_then(|s| s.member).or_else(|| self.doc.member(id, &self.trust))
    }

    pub fn standing(&self, id: &str) -> Standing {
        let k = self.hosts.get(id);
        let online = k.and_then(|k| k.link.as_ref()).is_some_and(HostLink::is_connected);
        match self.member(id) {
            Some(true) if online && k.is_some_and(|k| k.error.is_none()) => {
                let behind = k.and_then(|k| k.status.as_ref()).is_some_and(|s| !self.doc.newer_than(&s.have).is_empty());
                if behind { Standing::Waiting } else { Standing::Synced }
            }
            Some(true) => Standing::Waiting,
            _ if online => Standing::NotSynced,
            _ => Standing::Offline,
        }
    }

    /// Every host to list: this computer first, then the others by name.
    pub fn listed(&self) -> Vec<String> {
        let mut ids: BTreeSet<String> = self.hosts.keys().cloned().collect();
        ids.extend(self.doc.members(&self.trust).into_keys());
        let mut out: Vec<String> = ids.into_iter().collect();
        out.sort_by_key(|id| (self.me.as_deref() != Some(id.as_str()), self.label(id).to_lowercase()));
        out
    }

    /// The host behind `link`, if an exchange met it.
    pub fn id_of(&self, link: &HostLink) -> Option<String> {
        if link.is_local() {
            return self.me.clone();
        }
        self.hosts.iter().find(|(_, k)| k.link.as_ref().is_some_and(|l| l.same(link))).map(|(id, _)| id.clone())
    }
}

/// The shared state (empty before the first exchange).
pub fn state(cx: &App) -> Option<&Shared> {
    cx.try_global::<Shared>()
}

/// This device's key.
fn device() -> Result<Device> {
    let dirs = goatty_shell::Dirs::from_env().context("no $HOME: goatty has nowhere to keep its device key")?;
    Device::load(&dirs)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The hosts an exchange reaches: this computer's goattyd (or this process
/// with `--local`), `hint`, and every other connection that takes part.
fn peers(hint: Option<HostLink>) -> Vec<HostLink> {
    let mut out: Vec<HostLink> = Vec::new();
    let push = |out: &mut Vec<HostLink>, l: HostLink| {
        if l.shares_config() && !out.iter().any(|o| o.same(&l)) {
            out.push(l);
        }
    };
    match &hint {
        Some(l @ HostLink::Local(_)) => push(&mut out, l.clone()),
        _ => match crate::hosts::pool::open(goatty_client::Endpoint::Local) {
            Ok(c) => push(&mut out, HostLink::Daemon(c)),
            Err(e) => log::info!("shared config: this computer's goattyd isn't reachable: {e:#}"),
        },
    }
    if let Some(l) = hint {
        push(&mut out, l);
    }
    for c in crate::hosts::pool::all() {
        if c.is_connected() {
            push(&mut out, HostLink::Daemon(c));
        }
    }
    out
}

/// What an exchange came back with.
pub struct Outcome {
    pub x: host_shared::Exchange,
    pub links: Vec<HostLink>,
    pub me: String,
}

impl Outcome {
    /// At least one goattyd stored what this exchange pushed (or there was
    /// nothing to push).
    pub fn saved_anywhere(&self) -> bool {
        let ok = |i: &usize| self.x.statuses[*i].is_some() && !self.x.errors.iter().any(|(j, _)| j == i);
        let pushed: Vec<usize> = (0..self.links.len()).filter(|i| self.x.pushed[*i] > 0).collect();
        if pushed.is_empty() { (0..self.links.len()).any(|i| ok(&i)) } else { pushed.iter().any(ok) }
    }

    fn index_of(&self, link: &HostLink) -> Option<usize> {
        self.links.iter().position(|l| l.same(link))
    }

    fn local_index(&self) -> Option<usize> {
        self.links.iter().position(HostLink::is_local)
    }

    fn status_of(&self, link: &HostLink) -> Option<&Status> {
        self.index_of(link).and_then(|i| self.x.statuses[i].as_ref())
    }

    /// This computer's trust (from its goattyd's roots; just this device without one).
    fn trust(&self) -> Trust {
        match self.local_index() {
            Some(i) => self.x.trust_of(i),
            None => self.x.doc.trust(&[self.me.clone()].into()),
        }
    }
}

/// Exchange the shared config with every host this process reaches (plus
/// `extra`, entries this device just signed), then `done`. Off the UI
/// thread; the [`Shared`] state is updated before `done` runs.
pub fn exchange(extra: Vec<Entry>, hint: Option<HostLink>, cx: &mut App, done: impl FnOnce(Result<Arc<Outcome>>, &mut App) + 'static) {
    let seq = {
        let s = cx.default_global::<Shared>();
        s.started += 1;
        s.started
    };
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let me = device()?;
                let links = peers(hint);
                anyhow::ensure!(!links.is_empty(), "no goatty host to exchange the shared config with");
                let refs: Vec<&dyn Peer> = links.iter().map(|l| l as &dyn Peer).collect();
                let x = host_shared::exchange(&refs, &me, extra);
                for (i, e) in &x.errors {
                    log::info!("shared config: exchange with {} failed: {e}", links[*i].label());
                }
                Ok(Arc::new(Outcome { x, links, me: me.device() }))
            })
            .await;
        cx.update(|cx| {
            if let Ok(o) = &result {
                record(o, seq, cx);
                prompt_trust(o, cx);
                report_reviews(o, cx);
            }
            done(result, cx);
        });
    })
    .detach();
}

/// Keep what an exchange saw.
fn record(o: &Outcome, seq: u64, cx: &mut App) {
    let trust = o.trust();
    let now = now_unix();
    let ignored = ClientState::file().map(|f| ClientState::load(&f).ignored).unwrap_or_default();
    let state = cx.default_global::<Shared>();
    if seq < state.recorded {
        return;
    }
    state.recorded = seq;
    state.ignored = ignored;
    state.doc = o.x.doc.clone();
    state.trust = trust;
    state.me = Some(o.me.clone());
    for (i, link) in o.links.iter().enumerate() {
        let error = o.x.errors.iter().find(|(j, _)| *j == i).map(|(_, e)| e.clone());
        let Some(status) = o.x.statuses[i].clone() else { continue };
        let label = if link.is_local() { "This computer".into() } else { remote_label(link) };
        state
            .hosts
            .insert(status.host.clone(), Known { label, link: Some(link.clone()), status: Some(status), synced_unix: Some(now), error });
    }
    // Hosts no longer connected stay listed, without a link.
    for k in state.hosts.values_mut() {
        if k.link.as_ref().is_some_and(|l| !l.is_connected()) {
            k.link = None;
        }
    }
}

/// The name goatty reaches a host by (`pi`, `me@server`).
fn remote_label(link: &HostLink) -> String {
    link.remote_key().unwrap_or_else(|| link.label())
}

/// A tab connected to `link`'s host. A goattyd from before the shared
/// config gets the push sync; otherwise exchange, then whatever the user
/// still has to decide about that host. `after_install`: goatty just
/// installed goattyd there (it joins without a question).
pub fn on_connect(link: HostLink, after_install: bool, cx: &mut App) {
    if !link.shares_config() {
        return crate::shell_sync::on_connect(link, after_install, cx);
    }
    let l = link.clone();
    exchange(Vec::new(), Some(link), cx, move |result, cx| {
        let o = match result {
            Ok(o) => o,
            Err(e) => {
                log::info!("shared config: not exchanged with {}: {e:#}", l.label());
                return crate::shell_setup::offer(l, cx);
            }
        };
        let Some(status) = o.status_of(&l).cloned() else { return crate::shell_setup::offer(l, cx) };
        // This computer seeds the shared config first; a remote host is
        // decided about against the seeded document (connect again after).
        let local = o.local_index().and_then(|i| o.x.statuses[i].clone().map(|s| (o.links[i].clone(), s)));
        if let Some((local_link, local_status)) = local
            && local_status.member.is_none()
        {
            let again = (!l.is_local()).then(|| l.clone());
            seed(local_link, o.clone(), cx, move |cx| {
                if let Some(remote) = again {
                    on_connect(remote, after_install, cx);
                }
            });
            if l.is_local() {
                crate::shell_setup::offer(l, cx);
            }
            return;
        }
        if l.is_local() {
            return crate::shell_setup::offer(l, cx);
        }
        match status.member.or_else(|| o.x.doc.member(&status.host, &o.trust())) {
            Some(_) => crate::shell_setup::offer(l, cx),
            None if after_install => join(l, &status, false, cx),
            None => {
                let intent = ClientState::file().map(|f| ClientState::load(&f)).unwrap_or_default().intent(&remote_label(&l));
                match intent {
                    Some(true) => join(l, &status, true, cx),
                    Some(false) => leave(l, &status, cx),
                    None => ask(l, status, o, cx),
                }
            }
        }
    });
}

/// A connection came back after a drop: exchange again (once per
/// connection, however many panes it has).
pub fn on_reconnect(link: HostLink, cx: &mut App) {
    #[derive(Default)]
    struct Recent(Vec<(usize, std::time::Instant)>);
    impl Global for Recent {}
    let HostLink::Daemon(conn) = &link else { return };
    let key = conn.key();
    let recent = &mut cx.default_global::<Recent>().0;
    // Every pane of the connection reports the same reconnect within moments.
    recent.retain(|(_, at)| at.elapsed() < std::time::Duration::from_secs(2));
    if recent.iter().any(|(k, _)| *k == key) {
        return;
    }
    recent.push((key, std::time::Instant::now()));
    on_connect(link, false, cx);
}

/// The first run with the shared config on this computer: carry over the
/// old push sync's choices (#159) from the raw settings file, once. Runs
/// at startup before anything saves `settings.json` (a save drops the old
/// keys) and before any host is met (it reads the choices). Small file
/// reads only.
pub fn migrate_once() {
    let Some(file) = ClientState::file() else { return };
    let mut state = ClientState::load(&file);
    if state.migrated {
        return;
    }
    let raw = goatty_config::client_file().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let synced: Vec<String> = goatty_config::sync_state::state_file()
        .map(|f| goatty_config::sync_state::SyncState::load(&f))
        .unwrap_or_default()
        .hosts
        .into_iter()
        .filter(|(_, h)| h.synced)
        .map(|(host, _)| host)
        .collect();
    state.migrate(&raw, &synced);
    if let Err(e) = state.save(&file) {
        log::warn!("couldn't save {}: {e}", file.display());
    }
}

/// This computer never decided: seed the shared config from its settings
/// and plugins (signed as of when its settings file last changed).
fn seed(link: HostLink, o: Arc<Outcome>, cx: &mut App, then: impl FnOnce(&mut App) + 'static) {
    let me = o.me.clone();
    cx.spawn(async move |cx| {
        let l = link.clone();
        let entries = cx.background_executor().spawn(async move { seed_entries_for(&l, &device()?, &me) }).await;
        cx.update(|cx| match entries {
            Ok(entries) => {
                log::info!("shared config: seeded from this computer's settings ({} entries)", entries.len());
                exchange(entries, Some(link), cx, |r, cx| match r {
                    Ok(o) if o.local_index().and_then(|i| o.x.statuses[i].as_ref()).is_some_and(|s| s.member == Some(true)) => then(cx),
                    _ => log::warn!("shared config: this computer's goattyd didn't take the seed"),
                });
            }
            Err(e) => log::warn!("shared config: couldn't seed it from this computer: {e:#}"),
        });
    })
    .detach();
}

/// The entries that seed the shared config from this computer (`link` is
/// its goattyd, `host` its id). Blocks.
pub fn seed_entries_for(link: &HostLink, me: &Device, host: &str) -> Result<Vec<Entry>> {
    let own = store::parse::<HostSettings>(&link.get()?.json).value;
    let pins = match link.plugin(goatty_plugin::api::Request::Catalog { refresh: false }) {
        Ok(goatty_plugin::api::Reply::Catalog(c)) => host_shared::pins_from_catalog(&c),
        _ => BTreeMap::new(),
    };
    // As of when those settings were made.
    let made = goatty_config::host_file()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(Hlc::now_ms, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    Ok(host_shared::seed_entries(&own, &pins, host, me, Hlc { ms: made, n: 0 }))
}

/// This device's key (for the command line).
pub fn this_device() -> Result<Device> {
    device()
}

fn toast_key(host: &str) -> String {
    format!("shared.join:{host}")
}

/// `link`'s host follows the shared config from now on; with `keep`, its
/// own differences stay as its overrides.
pub fn join(link: HostLink, status: &Status, keep: bool, cx: &mut App) {
    let (host, label) = (status.host.clone(), remote_label(&link));
    let id = toast::show(cx, Toast::loading(format!("Bringing your shell config to {label}…")).key(toast_key(&host)));
    cx.spawn(async move |cx| {
        let (l, h, n) = (link.clone(), host.clone(), label.clone());
        let entries = cx
            .background_executor()
            .spawn(async move {
                let me = device()?;
                let own = if keep { Some(store::parse::<HostSettings>(&l.get()?.json).value) } else { None };
                let (doc, trust) = local_view(&l)?;
                anyhow::Ok(host_shared::join_entries(&doc, &trust, &h, &n, own.as_ref(), &me))
            })
            .await;
        cx.update(|cx| match entries {
            Ok(entries) => exchange(entries, Some(link.clone()), cx, move |r, cx| {
                let failed = |why: String| Toast::error(format!("Couldn't bring your shell config to {label}")).body(why);
                let t = match &r {
                    Err(e) => failed(format!("{e:#}")),
                    Ok(o) => match (o.index_of(&link), o.status_of(&link)) {
                        (Some(i), _) if o.x.errors.iter().any(|(j, _)| *j == i) => {
                            failed(o.x.errors.iter().filter(|(j, _)| *j == i).map(|(_, e)| e.clone()).collect::<Vec<_>>().join("\n"))
                        }
                        (_, Some(s)) if s.member == Some(true) => match &s.last_apply {
                            Some(a) if !a.ok => Toast::error(format!("{label} couldn't apply your shell config")).body(a.lines.join("\n")),
                            _ => Toast::success(format!("{label} follows your shell config")).body(if keep {
                                "Its own differences stay as overrides (Settings → Shell, scope picker)."
                            } else {
                                "Shells there reload at their next idle prompt."
                            }),
                        },
                        _ => failed("The host didn't confirm it follows the shared config.".into()),
                    },
                };
                toast::update(cx, id, t.key(toast_key(&host)));
                crate::shell_setup::offer(link, cx);
            }),
            Err(e) => toast::update(
                cx,
                id,
                Toast::error(format!("Couldn't bring your shell config to {label}")).body(format!("{e:#}")).key(toast_key(&host)),
            ),
        });
    })
    .detach();
}

/// `link`'s host keeps its own settings.
pub fn leave(link: HostLink, status: &Status, cx: &mut App) {
    let (host, label) = (status.host.clone(), remote_label(&link));
    cx.spawn(async move |cx| {
        let l = link.clone();
        let entries = cx
            .background_executor()
            .spawn(async move {
                let me = device()?;
                let (doc, _) = local_view(&l)?;
                anyhow::Ok(host_shared::leave_entries(&doc, &host, &label, &me))
            })
            .await;
        cx.update(|cx| match entries {
            Ok(entries) => exchange(entries, Some(link.clone()), cx, move |_, cx| crate::shell_setup::offer(link, cx)),
            Err(e) => log::warn!("shared config: couldn't record the choice: {e:#}"),
        });
    })
    .detach();
}

/// The document and trust as this computer sees them now (blocks).
fn local_view(hint: &HostLink) -> Result<(SharedDoc, Trust)> {
    let links = peers(Some(hint.clone()));
    let local = links.iter().find(|l| l.is_local()).context("this computer's goattyd isn't reachable")?;
    let st = host_shared::status(local)?;
    let mut doc = SharedDoc::default();
    // Page by page, asking each time for what isn't merged yet.
    for _ in 0..10_000 {
        match local.shared(host_shared::api::Request::Pull { have: doc.version() })? {
            host_shared::api::Reply::Entries { entries, more, .. } => {
                let added = doc.merge(entries);
                if !more || added == 0 {
                    break;
                }
            }
            other => anyhow::bail!("unexpected reply {other:?}"),
        }
    }
    let trust = doc.trust(&st.roots.iter().cloned().collect());
    Ok((doc, trust))
}

/// Ask once: "Use your goatty setup on pi?", with its differences.
fn ask(link: HostLink, status: Status, o: Arc<Outcome>, cx: &mut App) {
    let label = remote_label(&link);
    cx.spawn(async move |cx| {
        let l = link.clone();
        let trust = o.trust();
        let doc = o.x.doc.clone();
        let diff = cx
            .background_executor()
            .spawn(async move {
                let own = store::parse::<HostSettings>(&l.get()?.json).value;
                anyhow::Ok(host_shared::join_diff(&doc, &trust, &own))
            })
            .await;
        cx.update(|cx| {
            let key = toast_key(&status.host);
            let diff = match diff {
                Ok(d) => d,
                Err(e) => {
                    // Without its settings there's no telling what it would lose: ask again later.
                    let retry = link.clone();
                    toast::show(
                        cx,
                        Toast::warning(format!("Couldn't compare {label}'s shell config with yours"))
                            .body(format!("{e:#}"))
                            .key(key)
                            .action("Retry", move |_, cx| on_connect(retry.clone(), false, cx)),
                    );
                    return;
                }
            };
            let mut t = Toast::info(format!("Use your goatty setup on {label}?")).key(key).sticky();
            let (l1, s1) = (link.clone(), status.clone());
            let (l2, s2) = (link.clone(), status.clone());
            let (l3, s3) = (link.clone(), status.clone());
            if diff.is_empty() {
                t = t
                    .body("Your prompt, aliases, plugins and the rest of your shell config, kept in sync with your other hosts.")
                    .action("Follow shared config", move |_, cx| join(l1.clone(), &s1, false, cx))
                    .action("Keep separate", move |_, cx| leave(l3.clone(), &s3, cx));
            } else {
                let mut lines: Vec<String> = diff.iter().take(4).map(|c| c.text()).collect();
                if diff.len() > 4 {
                    lines.push(format!("and {} more", diff.len() - 4));
                }
                t = t
                    .body(format!("{label} has its own changes:\n{}", lines.join("\n")))
                    .action(format!("Keep {label}'s differences as overrides"), move |_, cx| join(l1.clone(), &s1, true, cx))
                    .action("Replace with shared", move |_, cx| join(l2.clone(), &s2, false, cx))
                    .action("Keep separate", move |_, cx| leave(l3.clone(), &s3, cx));
            }
            toast::show(cx, t);
        });
    })
    .detach();
}

/// Devices already asked about in this run.
#[derive(Default)]
struct AskedTrust(HashSet<String>);
impl Global for AskedTrust {}

/// Changes from a device this computer doesn't trust: ask once per device
/// whether it is one of the user's computers.
fn prompt_trust(o: &Outcome, cx: &mut App) {
    let Some(local) = o.local_index().and_then(|i| o.x.statuses[i].as_ref()) else { return };
    let ignored = ClientState::file().map(|f| ClientState::load(&f).ignored).unwrap_or_default();
    for u in &local.untrusted {
        if ignored.contains(&u.device) || !cx.default_global::<AskedTrust>().0.insert(u.device.clone()) {
            continue;
        }
        let name = u.name.clone().unwrap_or_else(|| format!("another device ({})", shared::short_id(&u.device)));
        let (d1, d2) = (u.device.clone(), u.device.clone());
        toast::show(
            cx,
            Toast::info(format!("Shell config changes from {name}"))
                .body(format!(
                    "{} change(s) signed by goatty on {name} ({}). Trust it if it's one of your computers; until then they don't apply here.",
                    u.entries,
                    shared::short_id(&u.device)
                ))
                .key(format!("shared.trust:{}", u.device))
                .sticky()
                .action("Trust", move |_, cx| trust_device(d1.clone(), true, cx))
                .action("Ignore", move |_, cx| ignore_device(d2.clone(), cx)),
        );
    }
}

/// Trust `device` (or stop trusting it) from this device.
pub fn trust_device(device: String, trusted: bool, cx: &mut App) {
    let doc = state(cx).map(|s| s.doc.clone()).unwrap_or_default();
    match device_entries(|me| host_shared::trust_entries(&doc, &device, trusted, me)) {
        Ok(entries) => publish(entries, cx, |_, _| {}),
        Err(e) => {
            toast::show(cx, Toast::error("Couldn't change the trusted devices").body(format!("{e:#}")));
        }
    }
}

/// Don't ask about `device` again.
pub fn ignore_device(device: String, cx: &mut App) {
    cx.default_global::<Shared>().ignored.insert(device.clone());
    cx.refresh_windows();
    toast::show(
        cx,
        Toast::info("Ignored that device's changes")
            .body("goatty won't ask about them again. You can still trust it on the Hosts page.")
            .key(format!("shared.trust:{device}")),
    );
    cx.background_executor()
        .spawn(async move {
            let Some(file) = ClientState::file() else { return };
            let mut s = ClientState::load(&file);
            s.ignored.insert(device);
            if let Err(e) = s.save(&file) {
                log::warn!("couldn't save {}: {e}", file.display());
            }
        })
        .detach();
}

fn device_entries(f: impl FnOnce(&Device) -> Vec<Entry>) -> Result<Vec<Entry>> {
    Ok(f(&device()?))
}

/// Shared plugins a host couldn't install without another review: say so
/// once per run and host.
fn report_reviews(o: &Outcome, cx: &mut App) {
    #[derive(Default)]
    struct Reported(HashSet<String>);
    impl Global for Reported {}
    for (i, st) in o.x.statuses.iter().enumerate() {
        let Some(st) = st else { continue };
        if st.needs_review.is_empty() {
            continue;
        }
        let ids: Vec<&str> = st.needs_review.iter().map(|r| r.id.as_str()).collect();
        let key = format!("{}:{}", st.host, ids.join(","));
        if !cx.default_global::<Reported>().0.insert(key) {
            continue;
        }
        let label = if o.links[i].is_local() { "This computer".to_string() } else { remote_label(&o.links[i]) };
        let lines: Vec<String> = st.needs_review.iter().map(|r| format!("{}: {}", r.id, r.reason)).collect();
        let host = st.host.clone();
        toast::show(
            cx,
            Toast::warning(format!("{label} needs a review for {}", crate::shell_setup::names(&ids)))
                .body(lines.join("\n"))
                .key(format!("shared.review:{}", st.host))
                .action("Plugins", move |_, cx| settings_window::open_for_host(cx, Some(Page::Plugins), host.clone())),
        );
    }
}

/// Sign nothing, just spread: entries already signed (an edit in the
/// settings window) go to every host this process reaches.
pub fn publish(entries: Vec<Entry>, cx: &mut App, done: impl FnOnce(Result<Arc<Outcome>>, &mut App) + 'static) {
    let hint = cx.try_global::<settings_window::HostLinkGlobal>().and_then(|g| g.0.clone());
    exchange(entries, hint, cx, done);
}

/// Plugins `ids` were installed (`installed`) or removed on `link`'s host
/// with the scope on All synced hosts: pin them in the shared config (or
/// remove them there), so every host that follows installs or removes them.
pub fn share_plugins(link: HostLink, ids: Vec<String>, installed: bool, cx: &mut App) {
    let doc = state(cx).map(|s| s.doc.clone()).unwrap_or_default();
    cx.spawn(async move |cx| {
        let entries = cx
            .background_executor()
            .spawn(async move {
                let me = device()?;
                let pins = if installed {
                    match link.plugin(goatty_plugin::api::Request::Catalog { refresh: false })? {
                        goatty_plugin::api::Reply::Catalog(c) => host_shared::pins_from_catalog(&c),
                        _ => BTreeMap::new(),
                    }
                } else {
                    BTreeMap::new()
                };
                let mut out = Vec::new();
                for id in ids {
                    // Built-in plugins and ones from a folder aren't pinned.
                    if goatty_shell::builtins::get(&id).is_some() {
                        continue;
                    }
                    match (installed, pins.get(&id)) {
                        (true, Some(pin)) => out.extend(host_shared::plugin_entries(&doc, &id, Some(pin.clone()), &me)),
                        (true, None) => {}
                        (false, _) => out.extend(host_shared::plugin_entries(&doc, &id, None, &me)),
                    }
                }
                anyhow::Ok(out)
            })
            .await;
        let failed = |cx: &mut App, why: String| {
            toast::show(
                cx,
                Toast::warning("The plugin change wasn't shared with your other hosts")
                    .body(format!("{why}\nIt applies to this host only for now; Sync now on the Hosts page tries again."))
                    .key("shared.plugins"),
            );
        };
        cx.update(|cx| match entries {
            Ok(e) if e.is_empty() => {}
            Ok(e) => publish(e, cx, move |r, cx| match r {
                Ok(o) if o.saved_anywhere() => {}
                Ok(o) => failed(cx, o.x.errors.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>().join("\n")),
                Err(e) => failed(cx, format!("{e:#}")),
            }),
            Err(e) => failed(cx, format!("{e:#}")),
        });
    })
    .detach();
}

/// Share (`share`) or keep on that host the changes made on `host` without a GUI.
pub fn adopt_local_changes(host: String, share: bool, cx: &mut App) {
    let Some(s) = state(cx) else { return };
    let (doc, trust) = (s.doc.clone(), s.trust.clone());
    match device_entries(|me| host_shared::adopt_local_changes(&doc, &trust, &host, share, me)) {
        Ok(entries) => publish(entries, cx, |_, _| {}),
        Err(e) => {
            toast::show(cx, Toast::error("Couldn't change the shared config").body(format!("{e:#}")));
        }
    }
}

/// The Hosts page's switch: `host` follows the shared config (`on`) or not.
/// A connected host that joins keeps its differences as overrides.
pub fn set_member(host: String, on: bool, cx: &mut App) {
    let Some(s) = state(cx) else { return };
    let known = s.hosts.get(&host).cloned();
    if let (Some(link), Some(status)) = (known.as_ref().and_then(|k| k.link.clone()), known.as_ref().and_then(|k| k.status.clone())) {
        return if on { join(link, &status, true, cx) } else { leave(link, &status, cx) };
    }
    let (doc, trust, label) = (s.doc.clone(), s.trust.clone(), s.label(&host));
    let entries = device_entries(|me| {
        if on {
            host_shared::join_entries(&doc, &trust, &host, &label, None, me)
        } else {
            host_shared::leave_entries(&doc, &host, &label, me)
        }
    });
    match entries {
        Ok(entries) => publish(entries, cx, |_, _| {}),
        Err(e) => {
            toast::show(cx, Toast::error("Couldn't change the shared config").body(format!("{e:#}")));
        }
    }
}

/// The palette's "Sync the shared shell config now": exchange with every
/// host this process reaches (and push-sync the active one if it's older).
pub fn sync_now(cx: &mut App) {
    let mut link = cx.try_global::<settings_window::HostLinkGlobal>().and_then(|g| g.0.clone());
    if let Some(l) = &link
        && !l.shares_config()
    {
        // The active host only knows the push sync; the others still exchange.
        crate::shell_sync::sync_now(cx);
        link = None;
    }
    let id = toast::show(cx, Toast::loading("Syncing your shell config…").key("shared.sync"));
    exchange(Vec::new(), link, cx, move |r, cx| {
        let t = match r {
            Ok(o) => {
                let reached = o.x.statuses.iter().filter(|s| s.is_some()).count();
                let pushed: usize = o.x.pushed.iter().sum();
                let mut t = Toast::success(format!("Shell config synced with {reached} host(s)")).body(if pushed == 0 {
                    "Everything was up to date.".to_string()
                } else {
                    format!("{pushed} change(s) passed on.")
                });
                if !o.x.errors.is_empty() {
                    let lines: Vec<String> = o.x.errors.iter().map(|(i, e)| format!("{}: {e}", o.links[*i].label())).collect();
                    t = Toast::warning("Synced part of your shell config").body(lines.join("\n"));
                }
                t
            }
            Err(e) => Toast::error("Couldn't sync your shell config").body(format!("{e:#}")),
        };
        toast::update(cx, id, t.key("shared.sync"));
    });
}

/// The entries that make the shared settings (`All synced hosts`) go from
/// `before` to `after`.
pub fn shared_edit(before: &HostSettings, after: &HostSettings, cx: &App) -> Result<Vec<Entry>> {
    let hlc = state(cx).map(|s| s.doc.next_hlc()).unwrap_or_else(|| Hlc::next(Hlc::default(), Hlc::now_ms()));
    device_entries(|me| shared::shared_entries(before, after, hlc, me))
}

/// The entries that make host `host` (shown as `before`) use `after`.
pub fn override_edit(host: &str, before: &HostSettings, after: &HostSettings, cx: &App) -> Result<Vec<Entry>> {
    let s = state(cx).context("the shared config isn't loaded yet")?;
    let (doc, trust) = (&s.doc, &s.trust);
    device_entries(|me| shared::override_entries(doc, trust, host, before, after, doc.next_hlc(), me))
}

/// "Use shared value": drop `host`'s overrides of `paths` (and below).
pub fn reset_overrides(host: &str, keys: &[&str], cx: &App) -> Result<Vec<Entry>> {
    let s = state(cx).context("the shared config isn't loaded yet")?;
    let overrides = s.doc.overrides(host, &s.trust);
    let paths: Vec<String> = overrides
        .keys()
        .filter(|p| {
            let shown = shared::display_path(p);
            keys.iter().any(|k| shown == *k || shown.starts_with(&format!("{k}.")))
        })
        .cloned()
        .collect();
    let mut t = s.doc.next_hlc();
    device_entries(|me| {
        paths
            .into_iter()
            .map(|p| {
                let e = Entry::new(&shared::Key::Override { host: host.to_owned(), path: p }, None, t, me);
                t = shared::bump(t, 1);
                e
            })
            .collect()
    })
}

/// The hosts whose overrides touch one of `keys` (for "Overridden on …").
pub fn overridden_on(keys: &[&str], cx: &App) -> Vec<String> {
    let Some(s) = state(cx) else { return Vec::new() };
    let mut out = Vec::new();
    for (key, _) in s.doc.entries().map(|e| (e.key.clone(), ())) {
        let Some(shared::Key::Override { host, path }) = shared::Key::parse(&key) else { continue };
        let shown = shared::display_path(&path);
        if keys.iter().any(|k| shown == *k || shown.starts_with(&format!("{k}.")))
            && s.doc.winner(&key, &s.trust).is_some_and(|e| !e.deleted)
            && !out.contains(&host)
        {
            out.push(host);
        }
    }
    out
}

/// Whether `host` overrides one of `keys`.
pub fn overrides_any(host: &str, keys: &[&str], cx: &App) -> bool {
    let Some(s) = state(cx) else { return false };
    s.doc.overrides(host, &s.trust).keys().any(|p| {
        let shown = shared::display_path(p);
        keys.iter().any(|k| shown == *k || shown.starts_with(&format!("{k}.")))
    })
}

/// The settings `host` uses (`None`: the shared settings), with the
/// machine-only values of `machine`.
pub fn resolve(host: Option<&str>, machine: &HostSettings, cx: &App) -> Option<HostSettings> {
    let s = state(cx)?;
    Some(s.doc.resolve(host, &s.trust, machine).value)
}
