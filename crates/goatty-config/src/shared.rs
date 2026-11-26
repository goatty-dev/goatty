//! One shared shell config on every host, with per-host overrides (#313).
//!
//! Every host setting resolves as **defaults ← shared ← this host's
//! override**, plus the values only this machine has (`shell.program`, the
//! dotfile import: [`HostSettings::MACHINE`]), which never leave its
//! `host.json`.
//!
//! The shared document ([`SharedDoc`]) is a map from keys to signed entries
//! ([`Entry`]). A key names one setting (`set/prompt.style`, a map entry
//! such as `set/aliases.gs`), one host's override of it
//! (`host/<id>/prompt.style`), or a fact about the setup: which hosts follow
//! it (`member/<id>`), which devices are trusted (`trust/<device>`), which
//! registry plugins it has (`plugin/<id>`), and what a device or host is
//! called (`name/<id>`). See [`Key`].
//!
//! # Merge
//!
//! For each key the document keeps the newest entry *per device*. The value
//! of a key is the newest of those (hybrid logical clock [`Hlc`], then the
//! device id) among the devices this machine trusts ([`Trust`]). That makes
//! merging a join of per-(key, device) maxima: commutative, associative and
//! idempotent, so the order and repetition of exchanges don't matter and
//! there is no conflict UI. An entry from a device this machine doesn't trust
//! is stored and passed on (another machine may trust it) but never wins,
//! so it can't push a trusted value out either: the worst a host that
//! relays the document can do is withhold entries.
//!
//! Resetting a setting stores a deletion marker. An override set to `null`
//! removes the setting on that host (its default applies there).
//!
//! # Signatures and trust
//!
//! Every entry is signed with the ed25519 key of the device that made it
//! (the device key goatty-net already uses for pairing). [`SharedDoc::insert`]
//! drops entries whose signature doesn't verify. Which signatures count is
//! up to each machine: its *roots* (the GUI computers that connected to it,
//! kept outside the document) plus every device a trusted device vouched for
//! with a `trust/<device>` entry ([`SharedDoc::trust`]). A host's own key
//! may set that host's overrides (`goattyd settings set` on a server without
//! a GUI) but nothing that applies elsewhere.
//!
//! # Exchanges
//!
//! [`SharedDoc::version`] summarises what a document holds as the newest
//! clock per device; [`SharedDoc::newer_than`] returns what a peer with a
//! given summary lacks. Because a device's older entry for a key is only
//! ever replaced by its own newer one, "everything newer than the peer's
//! clock for that device" is exactly what the peer is missing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::host::HostSettings;
use crate::store::{self, Document, Loaded};

/// The format of `shared.json` and of signed messages.
pub const DOC_VERSION: u32 = 1;

/// The most entries a document takes (a misbehaving peer can't make every
/// host store an unbounded amount).
pub const MAX_ENTRIES: usize = 50_000;

/// The largest value one entry may carry, as JSON.
pub const MAX_VALUE_BYTES: usize = 64 << 10;

/// Maps whose entries are one value each (an alias is replaced as a whole,
/// not field by field).
const ATOMIC_MAPS: &[&str] = &["aliases", "snippets", "expansions"];

// ---- Clocks ----------------------------------------------------------------------------

/// A hybrid logical clock: wall time in milliseconds, plus a counter for
/// changes within the same millisecond or behind a clock that ran ahead.
/// Ordered by `ms`, then `n`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hlc {
    pub ms: u64,
    pub n: u32,
}

impl Hlc {
    /// The next time after `last` (the newest time this device has seen),
    /// at wall time `now_ms`.
    pub fn next(last: Hlc, now_ms: u64) -> Hlc {
        if now_ms > last.ms { Hlc { ms: now_ms, n: 0 } } else { last.plus(1) }
    }

    /// `k` steps later: the counter carries into the milliseconds instead of
    /// stopping at its maximum, so a later clock is always later.
    pub fn plus(self, k: u64) -> Hlc {
        let total = u64::from(self.n) + k;
        let span = u64::from(u32::MAX) + 1;
        Hlc { ms: self.ms.saturating_add(total / span), n: u32::try_from(total % span).unwrap_or(0) }
    }

    /// The wall time now, in milliseconds.
    pub fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// The newest clock per device a document holds.
pub type VersionVector = BTreeMap<String, Hlc>;

// ---- Keys ------------------------------------------------------------------------------

/// What an entry is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// A shared setting, by its path ([`join_path`]).
    Setting(String),
    /// Host `host`'s override of a setting.
    Override { host: String, path: String },
    /// Whether host `0` follows the shared config (`true`/`false`).
    Member(String),
    /// Whether device `0` is trusted to change the shared config.
    Trust(String),
    /// What a device or host is called (a string, for lists).
    Name(String),
    /// A registry plugin of the shared config ([`PluginPin`]).
    Plugin(String),
}

impl Key {
    pub fn parse(s: &str) -> Option<Key> {
        let (kind, rest) = s.split_once('/')?;
        if rest.is_empty() {
            return None;
        }
        Some(match kind {
            "set" => Key::Setting(rest.to_owned()),
            "host" => {
                let (host, path) = rest.split_once('/')?;
                if !is_device_id(host) || path.is_empty() {
                    return None;
                }
                Key::Override { host: host.to_owned(), path: path.to_owned() }
            }
            "member" if is_device_id(rest) => Key::Member(rest.to_owned()),
            "trust" if is_device_id(rest) => Key::Trust(rest.to_owned()),
            "name" if is_device_id(rest) => Key::Name(rest.to_owned()),
            "plugin" if !rest.contains('/') => Key::Plugin(rest.to_owned()),
            _ => return None,
        })
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Setting(p) => write!(f, "set/{p}"),
            Key::Override { host, path } => write!(f, "host/{host}/{path}"),
            Key::Member(h) => write!(f, "member/{h}"),
            Key::Trust(d) => write!(f, "trust/{d}"),
            Key::Name(d) => write!(f, "name/{d}"),
            Key::Plugin(id) => write!(f, "plugin/{id}"),
        }
    }
}

/// A device's (and a host's) id: its ed25519 public key, 64 lowercase hex digits.
pub fn is_device_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The first 8 hex digits, for logs and lists.
pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// A setting's path from its JSON segments: dotted, with `~` and `.`
/// inside a segment written `~0` and `~1` (an alias may be called `g.s`).
pub fn join_path(segments: &[&str]) -> String {
    segments.iter().map(|s| s.replace('~', "~0").replace('.', "~1")).collect::<Vec<_>>().join(".")
}

/// The JSON segments of a path made by [`join_path`].
pub fn split_path(path: &str) -> Vec<String> {
    path.split('.').map(|s| s.replace("~1", ".").replace("~0", "~")).collect()
}

/// A path as people read it (`aliases.g.s`): for issues and lists.
pub fn display_path(path: &str) -> String {
    split_path(path).join(".")
}

// ---- Entries and signatures ------------------------------------------------------------

/// Signs entries as one device.
pub trait Signer {
    /// The device's id (public key, hex).
    fn device(&self) -> String;
    /// The signature over `msg`, hex.
    fn sign(&self, msg: &[u8]) -> String;
}

/// A [`Signer`] holding the key itself (tests, and code that has the seed).
pub struct KeySigner(SigningKey);

impl KeySigner {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }
}

impl Signer for KeySigner {
    fn device(&self) -> String {
        hex(self.0.verifying_key().as_bytes())
    }

    fn sign(&self, msg: &[u8]) -> String {
        hex(&self.0.sign(msg).to_bytes())
    }
}

/// One signed change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// A [`Key`] as text.
    pub key: String,
    /// A deletion marker (the setting was reset, the override removed).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub value: Value,
    pub hlc: Hlc,
    /// The device that made it ([`is_device_id`]).
    pub device: String,
    /// ed25519 over [`Entry::message`], hex.
    pub sig: String,
}

impl Entry {
    /// What the signature covers: everything but the signature, canonical.
    fn message(key: &str, deleted: bool, value: &Value, hlc: Hlc, device: &str) -> Vec<u8> {
        let doc = Value::Array(vec![
            Value::from("goatty-shared/1"),
            Value::from(key),
            Value::from(deleted),
            value.clone(),
            Value::from(hlc.ms),
            Value::from(hlc.n),
            Value::from(device),
        ]);
        let mut out = String::new();
        canonical(&doc, &mut out);
        out.into_bytes()
    }

    /// A new entry for `key`: `Some(value)` sets it, `None` deletes it.
    pub fn new(key: &Key, value: Option<Value>, hlc: Hlc, signer: &dyn Signer) -> Entry {
        let key = key.to_string();
        let device = signer.device();
        let deleted = value.is_none();
        let value = value.unwrap_or(Value::Null);
        let sig = signer.sign(&Self::message(&key, deleted, &value, hlc, &device));
        Entry { key, deleted, value, hlc, device, sig }
    }

    /// The value it sets (`None`: a deletion marker).
    pub fn value(&self) -> Option<&Value> {
        (!self.deleted).then_some(&self.value)
    }

    pub fn parsed_key(&self) -> Option<Key> {
        Key::parse(&self.key)
    }

    /// Well formed and signed by the device it names.
    pub fn verify(&self) -> bool {
        if !is_device_id(&self.device) || Key::parse(&self.key).is_none() || (self.deleted && !self.value.is_null()) {
            return false;
        }
        if serde_json::to_string(&self.value).map_or(true, |s| s.len() > MAX_VALUE_BYTES) {
            return false;
        }
        let (Some(pk), Some(sig)) = (unhex::<32>(&self.device), unhex::<64>(&self.sig)) else { return false };
        let Ok(pk) = VerifyingKey::from_bytes(&pk) else { return false };
        let sig = Signature::from_bytes(&sig);
        pk.verify(&Self::message(&self.key, self.deleted, &self.value, self.hlc, &self.device), &sig).is_ok()
    }

    /// Ordering between devices' entries for one key: newest clock, then
    /// the larger device id.
    fn rank(&self) -> (Hlc, &str) {
        (self.hlc, &self.device)
    }
}

/// The first `budget` bytes' worth of `entries` (at least one), and whether
/// any are left.
pub fn page(mut entries: Vec<Entry>, budget: usize) -> (Vec<Entry>, bool) {
    let mut used = 0;
    let mut cut = entries.len();
    for (i, e) in entries.iter().enumerate() {
        used += serde_json::to_string(e).map_or(0, |s| s.len()) + 1;
        if used > budget && i > 0 {
            cut = i;
            break;
        }
    }
    let more = cut < entries.len();
    entries.truncate(cut);
    (entries, more)
}

/// Split `entries` into pages of about `budget` bytes of JSON each.
pub fn pages(entries: Vec<Entry>, budget: usize) -> Vec<Vec<Entry>> {
    let mut out = Vec::new();
    let mut rest = entries;
    while !rest.is_empty() {
        let (first, more) = page(rest.clone(), budget);
        rest = if more { rest.split_off(first.len()) } else { Vec::new() };
        out.push(first);
    }
    out
}

/// JSON with object keys sorted at every level, so a signature doesn't
/// depend on how a build orders maps.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            out.push('{');
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("string"));
                out.push(':');
                canonical(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).expect("json")),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// ---- Trust ------------------------------------------------------------------------------

/// The devices whose entries count on this machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trust {
    pub devices: BTreeSet<String>,
}

impl Trust {
    pub fn contains(&self, device: &str) -> bool {
        self.devices.contains(device)
    }

    /// Whether `entry` may decide its key here.
    pub fn accepts(&self, entry: &Entry) -> bool {
        match entry.parsed_key() {
            // A host may set its own overrides; nobody else's.
            Some(Key::Override { host, .. }) => self.contains(&entry.device) || entry.device == host,
            // Names are only labels: a device may name itself.
            Some(Key::Name(id)) => self.contains(&entry.device) || entry.device == id,
            Some(_) => self.contains(&entry.device),
            None => false,
        }
    }
}

// ---- Plugins -----------------------------------------------------------------------------

/// A registry plugin in the shared config: which version, from where, and
/// the registry keys the device that added it trusted (a host installs it
/// without another review only when its copy of the registry is signed by
/// one of them, or both use goatty's built-in registry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPin {
    pub version: String,
    pub registry: String,
    #[serde(default)]
    pub keys: Vec<String>,
    /// The registry is goatty's built-in one.
    #[serde(default)]
    pub builtin: bool,
}

// ---- The document ----------------------------------------------------------------------

/// The shared config as one machine holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedDoc {
    /// key → device → that device's newest entry for it.
    entries: BTreeMap<String, BTreeMap<String, Entry>>,
    len: usize,
}

/// `shared.json` on disk.
#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    entries: Vec<Value>,
}

impl SharedDoc {
    /// Take `entry` if it is signed correctly and newer than what its
    /// device said about that key before. Returns whether anything changed.
    pub fn insert(&mut self, entry: Entry) -> bool {
        // A device never reuses a clock; if one did, the larger signature
        // decides, so every machine keeps the same entry.
        if let Some(old) = self.entries.get(&entry.key).and_then(|m| m.get(&entry.device))
            && (old.hlc, &old.sig) >= (entry.hlc, &entry.sig)
        {
            return false;
        }
        if !entry.verify() {
            return false;
        }
        // A safety valve against a misbehaving peer, far above any real
        // setup: past it, new key/device pairs are refused (and the
        // document stops converging for them), updates still apply.
        let known = self.entries.get(&entry.key).is_some_and(|m| m.contains_key(&entry.device));
        if !known {
            if self.len >= MAX_ENTRIES {
                return false;
            }
            self.len += 1;
        }
        self.entries.entry(entry.key.clone()).or_default().insert(entry.device.clone(), entry);
        true
    }

    /// [`insert`](Self::insert) each; returns how many changed something.
    pub fn merge(&mut self, entries: impl IntoIterator<Item = Entry>) -> usize {
        entries.into_iter().filter(|e| self.insert(e.clone())).count()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().flat_map(BTreeMap::values)
    }

    /// The newest clock per device.
    pub fn version(&self) -> VersionVector {
        let mut vv = VersionVector::new();
        for e in self.entries() {
            let t = vv.entry(e.device.clone()).or_default();
            *t = (*t).max(e.hlc);
        }
        vv
    }

    /// The entries a peer whose document is summarised by `vv` lacks.
    pub fn newer_than(&self, vv: &VersionVector) -> Vec<Entry> {
        self.entries().filter(|e| vv.get(&e.device).is_none_or(|t| e.hlc > *t)).cloned().collect()
    }

    /// [`newer_than`](Self::newer_than) in pages of about `budget` bytes of
    /// JSON (a reply must fit in one protocol frame), oldest first per
    /// device: after merging a page, the receiver's version vector still
    /// asks for the rest. Returns the page and whether more follow.
    pub fn newer_than_page(&self, vv: &VersionVector, budget: usize) -> (Vec<Entry>, bool) {
        let mut all = self.newer_than(vv);
        all.sort_by(|a, b| (&a.device, a.hlc).cmp(&(&b.device, b.hlc)));
        page(all, budget)
    }

    /// The newest clock in the document: a new change must come after it.
    pub fn max_hlc(&self) -> Hlc {
        self.entries().map(|e| e.hlc).max().unwrap_or_default()
    }

    /// The clock for a change made now.
    pub fn next_hlc(&self) -> Hlc {
        Hlc::next(self.max_hlc(), Hlc::now_ms())
    }

    /// The deciding entry for `key`: the newest one `trust` accepts.
    pub fn winner(&self, key: &str, trust: &Trust) -> Option<&Entry> {
        self.entries.get(key)?.values().filter(|e| trust.accepts(e)).max_by(|a, b| a.rank().cmp(&b.rank()))
    }

    /// Every device's entry for `key` (for "changed on two machines").
    pub fn all_for(&self, key: &str) -> impl Iterator<Item = &Entry> {
        self.entries.get(key).into_iter().flat_map(BTreeMap::values)
    }

    /// The devices trusted here: `roots` (kept by this machine), then every
    /// device whose newest `trust/<device>` entry among the devices trusted
    /// so far says `true`, until nothing is added. A device's entries about
    /// itself don't count, and a device once added stays for this
    /// computation, so it always ends and every machine with the same
    /// document and roots gets the same set. A root can't be distrusted by
    /// an entry; a revocation counts when its signer was trusted first
    /// (a device can't be cut off by one it vouched for).
    pub fn trust(&self, roots: &BTreeSet<String>) -> Trust {
        let mut trust = Trust { devices: roots.clone() };
        loop {
            let mut added = Vec::new();
            for (key, per_device) in self.entries.range("trust/".to_owned()..) {
                let Some(device) = key.strip_prefix("trust/") else { break };
                if trust.contains(device) {
                    continue;
                }
                let newest =
                    per_device.values().filter(|e| e.device != device && trust.contains(&e.device)).max_by(|a, b| a.rank().cmp(&b.rank()));
                if newest.and_then(Entry::value).and_then(Value::as_bool) == Some(true) {
                    added.push(device.to_owned());
                }
            }
            if added.is_empty() {
                return trust;
            }
            trust.devices.extend(added);
        }
    }

    /// Winners of every key with `prefix`, as (rest of the key, value).
    fn winners<'a>(&'a self, prefix: &'a str, trust: &'a Trust) -> impl Iterator<Item = (&'a str, &'a Entry)> + 'a {
        self.entries
            .range(prefix.to_owned()..)
            .take_while(move |(k, _)| k.starts_with(prefix))
            .filter_map(move |(k, _)| Some((&k[prefix.len()..], self.winner(k, trust)?)))
    }

    /// The shared settings (not deleted), by path.
    pub fn settings(&self, trust: &Trust) -> BTreeMap<String, Value> {
        self.winners("set/", trust).filter_map(|(p, e)| Some((p.to_owned(), e.value()?.clone()))).collect()
    }

    /// Host `host`'s overrides, by path: `Some(value)` sets it there,
    /// `None` (an override to `null`) removes the shared value there.
    pub fn overrides(&self, host: &str, trust: &Trust) -> BTreeMap<String, Option<Value>> {
        let prefix = format!("host/{host}/");
        self.winners(&prefix, trust)
            .filter_map(|(p, e)| Some((p.to_owned(), e.value().map(|v| (!v.is_null()).then(|| v.clone()))?)))
            .collect()
    }

    /// The overrides of `host` that only the host itself signed (changes
    /// made there without a GUI, waiting for "Share" or "Keep on this host").
    pub fn local_changes(&self, host: &str, trust: &Trust) -> Vec<String> {
        let prefix = format!("host/{host}/");
        self.winners(&prefix, trust).filter(|(_, e)| e.device == host && !trust.contains(&e.device)).map(|(p, _)| p.to_owned()).collect()
    }

    /// Every host with an override of `path` (for "Overridden on …").
    pub fn overridden_on(&self, path: &str, trust: &Trust) -> Vec<String> {
        let mut out = Vec::new();
        for (key, _) in self.entries.range("host/".to_owned()..).take_while(|(k, _)| k.starts_with("host/")) {
            let Some(Key::Override { host, path: p }) = Key::parse(key) else { continue };
            if p == path && self.winner(key, trust).is_some_and(|e| !e.deleted) && !out.contains(&host) {
                out.push(host);
            }
        }
        out
    }

    /// Whether `host` follows the shared config (`None`: never decided).
    pub fn member(&self, host: &str, trust: &Trust) -> Option<bool> {
        self.winner(&Key::Member(host.to_owned()).to_string(), trust).and_then(Entry::value).and_then(Value::as_bool)
    }

    /// Every host anyone decided about, and whether it follows.
    pub fn members(&self, trust: &Trust) -> BTreeMap<String, bool> {
        self.winners("member/", trust).filter_map(|(h, e)| Some((h.to_owned(), e.value()?.as_bool()?))).collect()
    }

    /// What `id` (a device or host) is called.
    pub fn name(&self, id: &str, trust: &Trust) -> Option<String> {
        self.winner(&Key::Name(id.to_owned()).to_string(), trust).and_then(Entry::value).and_then(Value::as_str).map(str::to_owned)
    }

    /// The registry plugins of the shared config.
    pub fn plugins(&self, trust: &Trust) -> BTreeMap<String, PluginPin> {
        self.winners("plugin/", trust)
            .filter_map(|(id, e)| Some((id.to_owned(), serde_json::from_value(e.value()?.clone()).ok()?)))
            .collect()
    }

    /// Plugins the shared config removed (deletion markers).
    pub fn removed_plugins(&self, trust: &Trust) -> Vec<String> {
        self.winners("plugin/", trust).filter(|(_, e)| e.deleted).map(|(id, _)| id.to_owned()).collect()
    }

    /// Devices whose entries don't count here, with how many entries of
    /// theirs would change something if they did (a host's changes to its
    /// own overrides don't count: those apply already).
    pub fn untrusted(&self, trust: &Trust) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for e in self.entries() {
            if trust.accepts(e) {
                continue;
            }
            if matches!(e.parsed_key(), Some(Key::Name(_))) {
                continue;
            }
            *out.entry(e.device.clone()).or_default() += 1;
        }
        out
    }

    /// The settings `host` uses: defaults ← shared ← its overrides, with
    /// the machine-only values from `machine` (its `host.json`). `None` for
    /// `host` gives the shared settings themselves.
    pub fn resolve(&self, host: Option<&str>, trust: &Trust, machine: &HostSettings) -> Loaded<HostSettings> {
        let mut flat = self.settings(trust);
        if let Some(h) = host {
            for (path, v) in self.overrides(h, trust) {
                match v {
                    Some(v) => {
                        flat.insert(path, v);
                    }
                    None => remove_under(&mut flat, &path),
                }
            }
        }
        flat.retain(|p, _| HostSettings::scope(p) == Scope::Shared);
        let mut doc = unflatten(&flat);
        // A value can hold a machine-only setting inside it (an entry for
        // all of `shell`): those never come from the shared config.
        for m in HostSettings::MACHINE {
            remove_json_path(&mut doc, &split_path(m));
        }
        let machine_values: BTreeMap<String, Value> =
            flatten(&sparse(machine)).into_iter().filter(|(p, _)| HostSettings::scope(p) == Scope::Machine).collect();
        merge_json(&mut doc, unflatten(&machine_values));
        if let Value::Object(m) = &mut doc {
            m.insert("version".into(), Value::from(HostSettings::VERSION));
        }
        store::parse::<HostSettings>(&doc.to_string())
    }

    // ---- Files ----

    /// Read `shared.json`. Never fails: a missing or damaged file is an empty
    /// document, and entries that don't verify are dropped.
    pub fn load(path: &Path) -> SharedDoc {
        let mut doc = SharedDoc::default();
        let Ok(text) = std::fs::read_to_string(path) else { return doc };
        let Ok(file) = serde_json::from_str::<File>(&text) else { return doc };
        for v in file.entries {
            if let Ok(e) = serde_json::from_value::<Entry>(v) {
                doc.insert(e);
            }
        }
        doc
    }

    /// The document as `shared.json` has it.
    pub fn to_json(&self) -> String {
        let entries = self.entries().map(|e| serde_json::to_value(e).expect("entry")).collect();
        let mut text = serde_json::to_string_pretty(&File { version: DOC_VERSION, entries }).expect("json");
        text.push('\n');
        text
    }

    /// Write `shared.json` atomically (unless unchanged).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        store::write_text(path, &self.to_json()).map(drop)
    }
}

// ---- Scope ------------------------------------------------------------------------------

/// Where a host setting lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// In the shared config (with per-host overrides).
    Shared,
    /// Only in the machine's own `host.json`, never synced or exported.
    Machine,
}

impl HostSettings {
    /// Settings only one machine can use (`scope = machine`): its shell's
    /// path and what it imports from the user's own dotfiles there. Every
    /// other setting is shared. Exports, syncs and the shared config leave
    /// these out.
    pub const MACHINE: &'static [&'static str] = &["shell.program", "shell.import"];

    /// The scope of `path` (dotted, [`join_path`]): machine when it is one
    /// of [`HostSettings::MACHINE`] or inside one.
    pub fn scope(path: &str) -> Scope {
        let machine = Self::MACHINE.iter().any(|m| path == *m || path.strip_prefix(m).is_some_and(|r| r.starts_with('.')));
        if machine { Scope::Machine } else { Scope::Shared }
    }
}

// ---- Flattening ---------------------------------------------------------------------------

/// `settings` as stored: non-default values only, without `version`.
pub fn sparse(settings: &HostSettings) -> Value {
    let mut v: Value = serde_json::from_str(&store::to_json(settings, &[])).expect("our own JSON");
    if let Value::Object(m) = &mut v {
        m.remove("version");
    }
    v
}

/// A settings document as one value per path. Objects are walked into,
/// except entries of [`ATOMIC_MAPS`] (an alias is one value); lists and
/// empty objects are values.
pub fn flatten(doc: &Value) -> BTreeMap<String, Value> {
    fn walk(v: &Value, path: &mut Vec<String>, out: &mut BTreeMap<String, Value>) {
        let atomic = path.len() == 2 && ATOMIC_MAPS.contains(&path[0].as_str());
        match v {
            Value::Object(m) if !m.is_empty() && !atomic => {
                for (k, child) in m {
                    if path.is_empty() && k == "version" {
                        continue;
                    }
                    path.push(k.clone());
                    walk(child, path, out);
                    path.pop();
                }
            }
            _ if path.is_empty() => {}
            other => {
                let segs: Vec<&str> = path.iter().map(String::as_str).collect();
                out.insert(join_path(&segs), other.clone());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(doc, &mut Vec::new(), &mut out);
    out
}

/// The document a [`flatten`]ed map came from.
pub fn unflatten(flat: &BTreeMap<String, Value>) -> Value {
    let mut root = Map::new();
    for (path, v) in flat {
        let segs = split_path(path);
        let Some((last, parents)) = segs.split_last() else { continue };
        let mut cur = &mut root;
        for s in parents {
            let slot = cur.entry(s.clone()).or_insert_with(|| Value::Object(Map::new()));
            if !slot.is_object() {
                *slot = Value::Object(Map::new());
            }
            cur = slot.as_object_mut().expect("object");
        }
        match (cur.get_mut(last), v) {
            // An empty object marks presence; don't drop what's inside.
            (Some(Value::Object(existing)), Value::Object(new)) if new.is_empty() => {
                let _ = existing;
            }
            _ => {
                cur.insert(last.clone(), v.clone());
            }
        }
    }
    Value::Object(root)
}

/// Remove the value at `segs` from a JSON document.
fn remove_json_path(doc: &mut Value, segs: &[String]) {
    let Some((last, parents)) = segs.split_last() else { return };
    let mut cur = doc;
    for s in parents {
        match cur.get_mut(s) {
            Some(next) => cur = next,
            None => return,
        }
    }
    if let Value::Object(m) = cur {
        m.remove(last);
    }
}

/// Put `add`'s values into `doc` (objects merged, everything else replaced).
fn merge_json(doc: &mut Value, add: Value) {
    match (doc, add) {
        (Value::Object(d), Value::Object(a)) => {
            for (k, v) in a {
                match d.get_mut(&k) {
                    Some(existing) => merge_json(existing, v),
                    None => {
                        d.insert(k, v);
                    }
                }
            }
        }
        (d, a) => *d = a,
    }
}

/// Remove `path` and everything under it.
fn remove_under(flat: &mut BTreeMap<String, Value>, path: &str) {
    let inside = format!("{path}.");
    flat.retain(|p, _| p != path && !p.starts_with(&inside));
}

/// The default value at `path` (`None`: the default has nothing there,
/// e.g. an alias).
pub fn default_at(path: &str) -> Option<Value> {
    let mut cur = serde_json::to_value(HostSettings::default()).expect("defaults serialize");
    for seg in split_path(path) {
        cur = cur.get_mut(&seg)?.take();
    }
    Some(cur)
}

/// How `after` differs from `before` in shared settings, by path:
/// `Some(value)` for a new or changed value, `None` for one that went back
/// to its default (or was removed). Machine-only paths are left out.
pub fn changes(before: &HostSettings, after: &HostSettings) -> Vec<(String, Option<Value>)> {
    let (a, b) = (flatten(&sparse(before)), flatten(&sparse(after)));
    let mut out = Vec::new();
    for (p, v) in &b {
        if a.get(p) != Some(v) {
            out.push((p.clone(), Some(v.clone())));
        }
    }
    for p in a.keys() {
        if !b.contains_key(p) {
            out.push((p.clone(), None));
        }
    }
    out.retain(|(p, _)| HostSettings::scope(p) == Scope::Shared);
    out.sort_by(|x, y| x.0.cmp(&y.0));
    out
}

/// The entries that make the shared settings go from `before` to `after`.
pub fn shared_entries(before: &HostSettings, after: &HostSettings, hlc: Hlc, signer: &dyn Signer) -> Vec<Entry> {
    let mut t = hlc;
    changes(before, after)
        .into_iter()
        .map(|(p, v)| {
            let e = Entry::new(&Key::Setting(p), v, t, signer);
            t = t.plus(1);
            e
        })
        .collect()
}

/// The override entries that make host `host` (whose settings were
/// `before`) use `after`: a value equal to the shared one drops the
/// override, any other one sets it; a value reset to its default while the
/// shared config sets it overrides it with the default.
pub fn override_entries(
    doc: &SharedDoc,
    trust: &Trust,
    host: &str,
    before: &HostSettings,
    after: &HostSettings,
    hlc: Hlc,
    signer: &dyn Signer,
) -> Vec<Entry> {
    let shared = doc.settings(trust);
    let mut t = hlc;
    let mut out = Vec::new();
    for (path, v) in changes(before, after) {
        let key = Key::Override { host: host.to_owned(), path: path.clone() };
        let value = match v {
            Some(v) if shared.get(&path) == Some(&v) => None,
            Some(v) => Some(v),
            None if shared.contains_key(&path) => Some(default_at(&path).unwrap_or(Value::Null)),
            None => None,
        };
        out.push(Entry::new(&key, value, t, signer));
        t = t.plus(1);
    }
    out
}

/// The overrides that keep `host`'s own settings when it joins the shared
/// config ("Keep pi's differences as overrides"): one per path where its
/// settings differ from the shared ones.
///
/// Overrides the host had from before (it left and joins again) are
/// replaced: one that isn't among the differences now is removed, so the
/// host ends up with exactly its own settings.
pub fn keep_as_overrides(doc: &SharedDoc, trust: &Trust, host: &str, own: &HostSettings, hlc: Hlc, signer: &dyn Signer) -> Vec<Entry> {
    let shared = doc.resolve(None, trust, own).value;
    let mut out = override_entries(doc, trust, host, &shared, own, hlc, signer);
    let set: BTreeSet<String> = out.iter().filter_map(|e| e.key.strip_prefix(&format!("host/{host}/")).map(str::to_owned)).collect();
    let mut t = bump(hlc, out.len());
    for path in doc.overrides(host, trust).into_keys() {
        if !set.contains(&path) {
            out.push(Entry::new(&Key::Override { host: host.to_owned(), path }, None, t, signer));
            t = t.plus(1);
        }
    }
    out
}

/// The entries that seed an empty shared config from one machine's
/// settings (the first run with #313): every shared value it sets.
pub fn seed_entries(settings: &HostSettings, hlc: Hlc, signer: &dyn Signer) -> Vec<Entry> {
    shared_entries(&HostSettings::default(), settings, hlc, signer)
}

/// Clocks `n` changes apart from `hlc` (for a batch made at once).
pub fn bump(hlc: Hlc, n: usize) -> Hlc {
    hlc.plus(u64::try_from(n).unwrap_or(u64::MAX))
}

// ---- Trust roots (this machine's) ------------------------------------------------------

/// The devices this machine trusts on its own authority (`shared-trust.json`,
/// next to `shared.json`, never synced): the GUI computers that connected
/// to its goattyd. Anyone who can connect there can already run commands
/// as this user, so connecting is what makes a device a root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roots {
    #[serde(default)]
    pub roots: BTreeMap<String, Root>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    #[serde(default)]
    pub name: String,
    /// When it was added (unix seconds).
    #[serde(default)]
    pub added: u64,
}

impl Roots {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        text.push('\n');
        store::write_text(path, &text).map(drop)
    }

    pub fn devices(&self) -> BTreeSet<String> {
        self.roots.keys().cloned().collect()
    }

    /// Add `device`; returns whether it is new.
    pub fn add(&mut self, device: &str, name: &str) -> bool {
        if !is_device_id(device) {
            return false;
        }
        let added = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        match self.roots.get_mut(device) {
            Some(r) => {
                if !name.is_empty() && r.name != name {
                    r.name = name.to_owned();
                }
                false
            }
            None => {
                self.roots.insert(device.to_owned(), Root { name: name.to_owned(), added });
                true
            }
        }
    }
}

// ---- A GUI computer's own state ---------------------------------------------------------

/// What a GUI computer remembers about the shared config that isn't a
/// setting (`$XDG_STATE_HOME/goatty/shared-client.json`): the choices
/// carried over from the old push sync (#159) for hosts not met yet, and
/// the devices the user chose to ignore.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientState {
    /// The old `ssh.sync` / `ssh.hosts[].sync` and `shell-sync.json` were
    /// read (once).
    #[serde(default)]
    pub migrated: bool,
    /// What to do with a host (by the name goatty reaches it with) the
    /// first time it is met: `true` follow and keep its differences as
    /// overrides, `false` keep it separate.
    #[serde(default)]
    pub intents: BTreeMap<String, bool>,
    /// The default for hosts without an intent (`ssh.sync`): `None` asks.
    #[serde(default)]
    pub default_intent: Option<bool>,
    /// Hosts set to ask on their own (`"sync": "ask"`), whatever the default says.
    #[serde(default)]
    pub ask: BTreeSet<String>,
    /// Devices whose changes the user chose not to trust (don't ask again).
    #[serde(default)]
    pub ignored: BTreeSet<String>,
}

impl ClientState {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        text.push('\n');
        store::write_text(path, &text).map(drop)
    }

    /// The file for this user. `None` without `$HOME`.
    pub fn file() -> Option<std::path::PathBuf> {
        crate::color_history::state_dir_from(|k| std::env::var(k).ok()).map(|d| d.join("shared-client.json"))
    }

    /// Carry over the old push sync's choices (#159): `settings_json` is the
    /// raw `settings.json` (its `ssh.sync` and `ssh.hosts[].sync`), `synced`
    /// the hosts `shell-sync.json` says were synced. Hosts set to always
    /// sync, or synced before, follow and keep their differences; hosts set
    /// to never sync stay separate.
    pub fn migrate(&mut self, settings_json: &str, synced: &[String]) {
        if self.migrated {
            return;
        }
        self.migrated = true;
        let v: Value = serde_json::from_str(settings_json).unwrap_or(Value::Null);
        let intent = |s: Option<&str>| match s {
            Some("always") => Some(true),
            Some("never") => Some(false),
            _ => None,
        };
        self.default_intent = intent(v.pointer("/ssh/sync").and_then(Value::as_str));
        for host in synced {
            self.intents.entry(host.clone()).or_insert(true);
        }
        for h in v.pointer("/ssh/hosts").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = h.get("host").and_then(Value::as_str) else { continue };
            match h.get("sync").and_then(Value::as_str) {
                Some("ask") if !self.intents.contains_key(name) => {
                    self.ask.insert(name.to_owned());
                }
                other => {
                    if let Some(i) = intent(other) {
                        self.intents.insert(name.to_owned(), i);
                    }
                }
            }
        }
    }

    /// What to do with `host` when it is first met (`None`: ask).
    pub fn intent(&self, host: &str) -> Option<bool> {
        match self.intents.get(host) {
            Some(i) => Some(*i),
            None if self.ask.contains(host) => None,
            None => self.default_intent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer(n: u8) -> KeySigner {
        KeySigner::from_seed([n; 32])
    }

    fn trust_of(signers: &[&KeySigner]) -> Trust {
        Trust { devices: signers.iter().map(|s| s.device()).collect() }
    }

    fn set(path: &str, v: Value, ms: u64, s: &KeySigner) -> Entry {
        Entry::new(&Key::Setting(path.into()), Some(v), Hlc { ms, n: 0 }, s)
    }

    /// A small deterministic random generator (xorshift), so the property
    /// tests need no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_entries(rng: &mut Rng, signers: &[KeySigner], n: usize) -> Vec<Entry> {
        let keys = ["prompt.style", "history.size", "aliases.gs", "aliases.ll"];
        let hosts: Vec<String> = signers.iter().map(Signer::device).collect();
        (0..n)
            .map(|_| {
                let s = &signers[rng.below(signers.len() as u64) as usize];
                let path = keys[rng.below(keys.len() as u64) as usize].to_owned();
                let key = match rng.below(4) {
                    0 => Key::Override { host: hosts[rng.below(hosts.len() as u64) as usize].clone(), path },
                    1 => Key::Member(hosts[rng.below(hosts.len() as u64) as usize].clone()),
                    _ => Key::Setting(path),
                };
                let value = match rng.below(5) {
                    0 => None,
                    1 => Some(Value::Bool(rng.below(2) == 0)),
                    _ => Some(Value::from(rng.below(1000))),
                };
                // Few distinct times: ties between devices must be decided too.
                Entry::new(&key, value, Hlc { ms: rng.below(8), n: rng.below(2) as u32 }, s)
            })
            .collect()
    }

    fn doc_of(entries: &[Entry]) -> SharedDoc {
        let mut d = SharedDoc::default();
        d.merge(entries.iter().cloned());
        d
    }

    #[test]
    fn merge_is_commutative_associative_and_idempotent() {
        let signers = [signer(1), signer(2), signer(3)];
        let trust = trust_of(&[&signers[0], &signers[1]]);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..60 {
            let a = random_entries(&mut rng, &signers, 12);
            let b = random_entries(&mut rng, &signers, 12);
            let c = random_entries(&mut rng, &signers, 12);
            let merged = |parts: &[&[Entry]]| {
                let mut d = SharedDoc::default();
                for p in parts {
                    let other = doc_of(p);
                    d.merge(other.entries().cloned());
                }
                d
            };
            let ab = merged(&[&a, &b]);
            assert_eq!(ab, merged(&[&b, &a]), "commutative");
            let ab_c = {
                let mut d = ab.clone();
                d.merge(c.iter().cloned());
                d
            };
            let a_bc = {
                let mut d = doc_of(&a);
                d.merge(merged(&[&b, &c]).entries().cloned());
                d
            };
            assert_eq!(ab_c, a_bc, "associative");
            let mut again = ab_c.clone();
            assert_eq!(again.merge(ab_c.entries().cloned()), 0);
            again.merge(a.iter().cloned());
            assert_eq!(again, ab_c, "idempotent");
            // Shuffled one by one: the same document and the same settings.
            let mut all: Vec<Entry> = a.iter().chain(&b).chain(&c).cloned().collect();
            for i in (1..all.len()).rev() {
                all.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let shuffled = doc_of(&all);
            assert_eq!(shuffled, ab_c);
            assert_eq!(shuffled.settings(&trust), ab_c.settings(&trust));
        }
    }

    #[test]
    fn bad_signatures_and_malformed_entries_are_rejected() {
        let s = signer(1);
        let good = set("history.size", Value::from(5000), 1, &s);
        let mut d = SharedDoc::default();

        let mut forged = good.clone();
        forged.value = Value::from(10);
        assert!(!d.insert(forged), "changed value");

        let mut other = good.clone();
        other.device = signer(2).device();
        assert!(!d.insert(other), "claims another device");

        let mut later = good.clone();
        later.hlc.ms = 99;
        assert!(!d.insert(later), "changed clock");

        let mut bad_key = Entry::new(&Key::Setting("x".into()), Some(Value::Bool(true)), Hlc::default(), &s);
        bad_key.key = "nonsense".into();
        assert!(!bad_key.verify());

        assert!(d.insert(good.clone()));
        assert!(!d.insert(good), "same entry twice");
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn untrusted_devices_never_win() {
        let (me, stranger) = (signer(1), signer(2));
        let trust = trust_of(&[&me]);
        let mut d = SharedDoc::default();
        d.insert(set("prompt.style", "lean".into(), 1, &me));
        d.insert(set("prompt.style", "classic".into(), 9, &stranger));
        assert_eq!(d.settings(&trust)["prompt.style"], "lean");
        assert_eq!(d.untrusted(&trust).get(&stranger.device()), Some(&1));
        // Once vouched for, the newer value wins.
        d.insert(Entry::new(&Key::Trust(stranger.device()), Some(true.into()), Hlc { ms: 10, n: 0 }, &me));
        let trust = d.trust(&trust.devices);
        assert!(trust.contains(&stranger.device()));
        assert_eq!(d.settings(&trust)["prompt.style"], "classic");
        // And distrusting it brings the trusted value back.
        d.insert(Entry::new(&Key::Trust(stranger.device()), Some(false.into()), Hlc { ms: 11, n: 0 }, &me));
        let trust = d.trust(&[me.device()].into());
        assert_eq!(d.settings(&trust)["prompt.style"], "lean");
    }

    #[test]
    fn trust_ends_when_a_device_revokes_itself_or_its_voucher() {
        let (a, b, c) = (signer(1), signer(2), signer(3));
        let mut d = SharedDoc::default();
        let roots: BTreeSet<String> = [a.device()].into();
        d.insert(Entry::new(&Key::Trust(b.device()), Some(true.into()), Hlc { ms: 1, n: 0 }, &a));
        // b distrusts itself: that doesn't count.
        d.insert(Entry::new(&Key::Trust(b.device()), Some(false.into()), Hlc { ms: 2, n: 0 }, &b));
        d.insert(Entry::new(&Key::Trust(c.device()), Some(true.into()), Hlc { ms: 3, n: 0 }, &b));
        // c (trusted through b) distrusts b: b stays.
        d.insert(Entry::new(&Key::Trust(b.device()), Some(false.into()), Hlc { ms: 4, n: 0 }, &c));
        let t = d.trust(&roots);
        assert!(t.contains(&b.device()) && t.contains(&c.device()));
        // The root's own newer revocation does count.
        d.insert(Entry::new(&Key::Trust(b.device()), Some(false.into()), Hlc { ms: 5, n: 0 }, &a));
        let t = d.trust(&roots);
        assert!(!t.contains(&b.device()) && !t.contains(&c.device()));
    }

    #[test]
    fn clocks_carry_and_machine_values_never_come_from_shared_entries() {
        let top = Hlc { ms: 7, n: u32::MAX };
        assert!(Hlc::next(top, 0) > top && top.plus(1) == Hlc { ms: 8, n: 0 });
        let gui = signer(1);
        let trust = trust_of(&[&gui]);
        let mut d = SharedDoc::default();
        d.insert(set("shell", serde_json::json!({"program": "/evil", "path": ["~/bin"]}), 1, &gui));
        let r = d.resolve(None, &trust, &HostSettings::default()).value;
        assert_eq!(r.shell.program, None);
        assert_eq!(r.shell.path, ["~/bin"]);
    }

    #[test]
    fn pages_cover_everything_in_order() {
        let s = signer(1);
        let mut d = SharedDoc::default();
        for i in 0..20u64 {
            d.insert(set(&format!("aliases.a{i}"), serde_json::json!({"command": "x".repeat(100)}), i + 1, &s));
        }
        let mut got = SharedDoc::default();
        let mut rounds = 0;
        loop {
            let (page, more) = d.newer_than_page(&got.version(), 600);
            got.merge(page);
            rounds += 1;
            if !more {
                break;
            }
        }
        assert_eq!(got, d);
        assert!(rounds > 2);
        assert_eq!(pages(d.newer_than(&VersionVector::new()), 600).concat().len(), 20);
    }

    #[test]
    fn rejoining_with_keep_drops_stale_overrides() {
        let gui = signer(1);
        let pi = signer(2).device();
        let trust = trust_of(&[&gui]);
        let mut d = SharedDoc::default();
        d.insert(set("history.size", Value::from(5000), 1, &gui));
        d.insert(Entry::new(
            &Key::Override { host: pi.clone(), path: "history.size".into() },
            Some(Value::from(300)),
            Hlc { ms: 2, n: 0 },
            &gui,
        ));
        let mut own = HostSettings::default();
        own.history.size = 5000;
        d.merge(keep_as_overrides(&d.clone(), &trust, &pi, &own, Hlc { ms: 3, n: 0 }, &gui));
        assert_eq!(d.resolve(Some(&pi), &trust, &HostSettings::default()).value.history.size, 5000);
    }

    #[test]
    fn trust_is_transitive_from_the_roots_only() {
        let (a, b, c) = (signer(1), signer(2), signer(3));
        let mut d = SharedDoc::default();
        // c vouches for itself and b vouches for c, but nobody trusts b yet.
        d.insert(Entry::new(&Key::Trust(c.device()), Some(true.into()), Hlc { ms: 1, n: 0 }, &c));
        d.insert(Entry::new(&Key::Trust(c.device()), Some(true.into()), Hlc { ms: 1, n: 0 }, &b));
        let roots: BTreeSet<String> = [a.device()].into();
        assert_eq!(d.trust(&roots).devices, roots);
        d.insert(Entry::new(&Key::Trust(b.device()), Some(true.into()), Hlc { ms: 2, n: 0 }, &a));
        let t = d.trust(&roots);
        assert!(t.contains(&b.device()) && t.contains(&c.device()));
    }

    #[test]
    fn a_host_may_only_set_its_own_overrides() {
        let (gui, pi, server) = (signer(1), signer(2), signer(3));
        let trust = trust_of(&[&gui]);
        let mut d = SharedDoc::default();
        let ov = |host: &KeySigner, s: &KeySigner, v: &str| {
            Entry::new(&Key::Override { host: host.device(), path: "prompt.style".into() }, Some(v.into()), Hlc { ms: 1, n: 0 }, s)
        };
        d.insert(ov(&pi, &pi, "lean"));
        d.insert(ov(&pi, &server, "classic"));
        d.insert(Entry::new(&Key::Setting("prompt.style".into()), Some("classic".into()), Hlc { ms: 1, n: 0 }, &server));
        assert_eq!(d.overrides(&pi.device(), &trust)["prompt.style"], Some("lean".into()));
        assert!(d.settings(&trust).is_empty(), "a host can't change the shared config");
        assert_eq!(d.local_changes(&pi.device(), &trust), ["prompt.style"]);
    }

    #[test]
    fn resolves_defaults_shared_and_overrides() {
        let gui = signer(1);
        let pi = signer(2).device();
        let trust = trust_of(&[&gui]);
        let mut d = SharedDoc::default();
        d.insert(set("history.size", Value::from(5000), 1, &gui));
        d.insert(set("aliases.gs", serde_json::json!({"command": "git status"}), 1, &gui));
        d.insert(set("shell.program", Value::from("/evil"), 1, &gui));
        d.insert(Entry::new(
            &Key::Override { host: pi.clone(), path: "history.size".into() },
            Some(Value::from(300)),
            Hlc { ms: 2, n: 0 },
            &gui,
        ));
        d.insert(Entry::new(&Key::Override { host: pi.clone(), path: "aliases.gs".into() }, Some(Value::Null), Hlc { ms: 2, n: 0 }, &gui));
        let mut machine = HostSettings::default();
        machine.shell.program = Some("/usr/bin/zsh".into());
        machine.history.size = 7;

        let shared = d.resolve(None, &trust, &machine).value;
        assert_eq!(shared.history.size, 5000);
        assert_eq!(shared.aliases["gs"].command, "git status");
        assert_eq!(shared.shell.program.as_deref(), Some("/usr/bin/zsh"), "machine-only values stay the machine's");

        let on_pi = d.resolve(Some(&pi), &trust, &machine).value;
        assert_eq!(on_pi.history.size, 300);
        assert!(on_pi.aliases.is_empty(), "an override to null removes the shared value");

        // A reset (deletion marker) brings the default back.
        d.insert(Entry::new(&Key::Setting("history.size".into()), None, Hlc { ms: 3, n: 0 }, &gui));
        assert_eq!(d.resolve(None, &trust, &machine).value.history.size, HistorySettings::default().size);
    }

    use crate::host::HistorySettings;

    #[test]
    fn edits_become_shared_entries_and_overrides() {
        let gui = signer(1);
        let pi = signer(2).device();
        let trust = trust_of(&[&gui]);
        let mut d = SharedDoc::default();
        let mut mine = HostSettings::default();
        mine.history.size = 5000;
        mine.aliases.insert("gs".into(), crate::host::UserAlias { command: "git status".into(), ..Default::default() });
        d.merge(seed_entries(&mine, Hlc { ms: 1, n: 0 }, &gui));
        assert_eq!(d.resolve(None, &trust, &HostSettings::default()).value, mine);

        // pi's own settings kept as overrides: it resolves to what it had.
        let mut pis = HostSettings::default();
        pis.history.size = 200;
        d.merge(keep_as_overrides(&d.clone(), &trust, &pi, &pis, Hlc { ms: 2, n: 0 }, &gui));
        assert_eq!(d.resolve(Some(&pi), &trust, &HostSettings::default()).value, pis);

        // Editing pi back to the shared value drops its override.
        let before = d.resolve(Some(&pi), &trust, &HostSettings::default()).value;
        let mut after = before.clone();
        after.history.size = 5000;
        d.merge(override_entries(&d.clone(), &trust, &pi, &before, &after, Hlc { ms: 3, n: 0 }, &gui));
        assert!(!d.overrides(&pi, &trust).contains_key("history.size"));
        assert_eq!(d.overridden_on("history.size", &trust), Vec::<String>::new());
        assert_eq!(d.overridden_on("aliases.gs", &trust), std::slice::from_ref(&pi));
    }

    #[test]
    fn exchanges_send_only_what_the_peer_lacks() {
        let (a, b) = (signer(1), signer(2));
        let mut left = SharedDoc::default();
        let mut right = SharedDoc::default();
        left.insert(set("history.size", Value::from(1), 1, &a));
        left.insert(set("prompt.style", "lean".into(), 2, &a));
        right.insert(set("history.size", Value::from(2), 3, &b));
        right.merge(left.newer_than(&right.version()));
        assert_eq!(right.len(), 3);
        assert!(right.newer_than(&right.version()).is_empty());
        let missing = right.newer_than(&left.version());
        assert_eq!(missing.len(), 1);
        left.merge(missing);
        assert_eq!(left, right);
    }

    #[test]
    fn paths_escape_dots_and_flatten_round_trips() {
        assert_eq!(join_path(&["aliases", "g.s~"]), "aliases.g~1s~0");
        assert_eq!(split_path("aliases.g~1s~0"), ["aliases", "g.s~"]);
        let mut h = HostSettings::default();
        h.history.size = 5000;
        h.aliases.insert("g.s".into(), crate::host::UserAlias { command: "git status".into(), ..Default::default() });
        h.shell.path = vec!["~/bin".into()];
        let flat = flatten(&sparse(&h));
        assert!(flat.contains_key("aliases.g~1s"), "{flat:?}");
        assert!(flat.contains_key("shell.path"));
        let back = store::parse::<HostSettings>(&unflatten(&flat).to_string()).value;
        assert_eq!(back, h);
    }

    #[test]
    fn machine_scope() {
        assert_eq!(HostSettings::scope("shell.program"), Scope::Machine);
        assert_eq!(HostSettings::scope("shell.import.enabled"), Scope::Machine);
        assert_eq!(HostSettings::scope("shell.importer"), Scope::Shared);
        assert_eq!(HostSettings::scope("shell.path"), Scope::Shared);
        assert_eq!(HostSettings::scope("prompt.style"), Scope::Shared);
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.json");
        let s = signer(1);
        let mut d = SharedDoc::default();
        d.insert(set("history.size", Value::from(5000), 1, &s));
        d.insert(Entry::new(&Key::Setting("prompt.style".into()), None, Hlc { ms: 2, n: 0 }, &s));
        d.save(&path).unwrap();
        assert_eq!(SharedDoc::load(&path), d);
        // A tampered entry is dropped on load.
        let text = std::fs::read_to_string(&path).unwrap().replace("5000", "6000");
        std::fs::write(&path, text).unwrap();
        assert_eq!(SharedDoc::load(&path).len(), 1);
        std::fs::write(&path, "garbage").unwrap();
        assert!(SharedDoc::load(&path).is_empty());

        let roots_path = dir.path().join("shared-trust.json");
        let mut roots = Roots::load(&roots_path);
        assert!(roots.add(&s.device(), "laptop"));
        assert!(!roots.add(&s.device(), "laptop"));
        assert!(!roots.add("not-a-key", "x"));
        roots.save(&roots_path).unwrap();
        assert_eq!(Roots::load(&roots_path).devices(), [s.device()].into());
    }

    #[test]
    fn migrates_the_old_sync_choices() {
        let mut c = ClientState::default();
        let json = r#"{"ssh": {"sync": "never", "hosts": [{"host": "pi", "sync": "always"}, {"host": "nas"}]}}"#;
        c.migrate(json, &["box".into()]);
        let mut a = ClientState::default();
        a.migrate(r#"{"ssh": {"sync": "always", "hosts": [{"host": "pi", "sync": "ask"}]}}"#, &[]);
        assert_eq!((a.intent("pi"), a.intent("other")), (None, Some(true)), "an explicit ask stays an ask");
        assert!(c.migrated);
        assert_eq!((c.intent("pi"), c.intent("box"), c.intent("nas")), (Some(true), Some(true), Some(false)));
        // Once only.
        c.migrate(r#"{"ssh": {"sync": "always"}}"#, &[]);
        assert_eq!(c.intent("other"), Some(false));
    }

    #[test]
    fn plugins_and_membership() {
        let gui = signer(1);
        let pi = signer(2).device();
        let trust = trust_of(&[&gui]);
        let mut d = SharedDoc::default();
        let pin = PluginPin { version: "1.0.0".into(), registry: "goatty".into(), keys: vec![], builtin: true };
        d.insert(Entry::new(&Key::Plugin("eza".into()), Some(serde_json::to_value(&pin).unwrap()), Hlc { ms: 1, n: 0 }, &gui));
        d.insert(Entry::new(&Key::Plugin("bat".into()), None, Hlc { ms: 1, n: 0 }, &gui));
        d.insert(Entry::new(&Key::Member(pi.clone()), Some(true.into()), Hlc { ms: 1, n: 0 }, &gui));
        d.insert(Entry::new(&Key::Name(pi.clone()), Some("pi".into()), Hlc { ms: 1, n: 0 }, &signer(2)));
        assert_eq!(d.plugins(&trust)["eza"], pin);
        assert_eq!(d.removed_plugins(&trust), ["bat"]);
        assert_eq!(d.member(&pi, &trust), Some(true));
        assert_eq!(d.name(&pi, &trust).as_deref(), Some("pi"), "a host may name itself");
    }
}
