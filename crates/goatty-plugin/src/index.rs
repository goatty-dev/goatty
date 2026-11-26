//! Registries: a Git repository of plugin directories plus a signed index.
//!
//! ```text
//! registry.toml                  name, description, trusted public keys, yanked versions
//! plugins/<id>/<version>/        a plugin directory (plugin.toml + its files)
//! index.json                     generated: every plugin version with its files' sha256
//! index.json.sig                 Ed25519 signature over index.json (made offline)
//! ```
//!
//! The chain of trust: the client pins the registry's public key → the
//! signature covers `index.json` → the index pins every file of every
//! plugin version by sha256 → the manifest pins every download by sha256.
//! Files that aren't in the signed index are never used. `sequence` only
//! goes up, so a client refuses an older (replayed) index. `expires` limits
//! how long a client can be held on an old index by someone who blocks the
//! newer one (a freeze attack): past it, goatty warns (but keeps working),
//! and `build` renews the index well before it.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::manifest::{self, Category, Manifest};
use crate::permissions::{self, Permission};
use crate::sign::{self, Detached};

pub const FORMAT: u32 = 1;
pub const INDEX: &str = "index.json";
pub const SIGNATURE: &str = "index.json.sig";
pub const META: &str = "registry.toml";
/// How long a built index is valid.
pub const LIFETIME_SECS: u64 = 90 * 24 * 60 * 60;
/// `build` renews an unchanged index with less than this left.
pub const RENEW_BEFORE_SECS: u64 = 30 * 24 * 60 * 60;

/// `registry.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryMeta {
    /// Short name (`official`).
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Public keys (hex) allowed to sign the index.
    pub keys: Vec<String>,
    /// `id@version` entries that stay listed but aren't offered anymore.
    #[serde(default)]
    pub yanked: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub format: u32,
    pub registry: String,
    /// Increases with every published change.
    pub sequence: u64,
    /// Unix seconds.
    pub generated: u64,
    /// Unix seconds after which clients warn that the registry may be
    /// holding them on an old index. Absent in indexes from before #100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    pub plugins: BTreeMap<String, Entry>,
}

/// A plugin, described by its newest version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub summary: String,
    pub publisher: String,
    pub category: Category,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    pub versions: BTreeMap<String, Version>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// Relative path → sha256, every file of the plugin directory.
    pub files: BTreeMap<String, String>,
    pub permissions: Vec<Permission>,
    /// Download targets of its packages (`any`, `x86_64-linux`, …); empty
    /// without packages.
    pub targets: Vec<String>,
    /// The goatty version requirement.
    #[serde(default)]
    pub goatty: String,
    #[serde(default)]
    pub yanked: bool,
    /// `compat.os`: the OSes it supports; empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    /// The shells it integrates with (`compat.shells`, with its default
    /// `["zsh"]` spelled out). Empty in indexes built before goatty wrote
    /// it: unknown, so the install plan decides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shells: Vec<String>,
}

impl Version {
    /// Why this version can't be installed with goatty `goatty_version` on
    /// `target` (`x86_64-windows`), judged from the index alone; `None`
    /// when it looks installable (the install plan checks the manifest).
    pub fn incompatible(&self, goatty_version: &str, target: &str) -> Option<String> {
        let platform = manifest::Platform::of_target(target);
        if !self.os.is_empty() && !self.os.contains(&platform.os) {
            return Some(format!("supports {} only", self.os.join(", ")));
        }
        if !self.shells.is_empty() && !self.shells.iter().any(|s| s == platform.shell) {
            return Some(format!("integrates with {} only; goatty's managed shell here is {}", self.shells.join(", "), platform.shell));
        }
        let fallback = manifest::fallback_target(target);
        if !self.targets.is_empty() && !self.targets.iter().any(|t| t == target || t == "any" || Some(t.as_str()) == fallback) {
            return Some(format!("no build for {target}"));
        }
        if !self.goatty.is_empty()
            && let (Ok(req), Ok(v)) = (semver::VersionReq::parse(&self.goatty), semver::Version::parse(goatty_version))
            && !req.matches(&v)
        {
            return Some(format!("needs goatty {}", self.goatty));
        }
        None
    }
}

impl Index {
    /// The newest version that isn't yanked.
    pub fn latest(&self, id: &str) -> Option<(&str, &Version)> {
        let e = self.plugins.get(id)?;
        e.versions
            .iter()
            .filter(|(_, v)| !v.yanked)
            .filter_map(|(k, v)| semver::Version::parse(k).ok().map(|sv| (sv, k, v)))
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, k, v)| (k.as_str(), v))
    }

    /// Whether the index is past its `expires` at `now` (Unix seconds).
    pub fn expired(&self, now: u64) -> bool {
        self.expires.is_some_and(|e| now >= e)
    }

    /// Whether `build` would renew this index at `now`: it never expires,
    /// or less than [`RENEW_BEFORE_SECS`] are left.
    pub fn renewal_due(&self, now: u64) -> bool {
        self.expires.is_none_or(|e| e.saturating_sub(now) < RENEW_BEFORE_SECS)
    }

    pub fn version(&self, id: &str, version: &str) -> Option<&Version> {
        self.plugins.get(id)?.versions.get(version)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = serde_json::to_string_pretty(self).expect("index serializes");
        s.push('\n');
        s.into_bytes()
    }
}

pub fn read_meta(dir: &Path) -> Result<RegistryMeta> {
    let text = std::fs::read_to_string(dir.join(META)).with_context(|| format!("read {}", dir.join(META).display()))?;
    let meta: RegistryMeta = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{META}: {}", e.message()))?;
    if !manifest::is_id(&meta.name) {
        bail!("{META}: name {:?} must look like a plugin id", meta.name);
    }
    Ok(meta)
}

/// The plugin directories of a registry: (id, version, path).
fn plugin_dirs(dir: &Path) -> Result<Vec<(String, String, std::path::PathBuf)>> {
    let mut out = Vec::new();
    let root = dir.join("plugins");
    if !root.is_dir() {
        return Ok(out);
    }
    for id in std::fs::read_dir(&root)?.flatten() {
        let id_name = id.file_name().to_string_lossy().into_owned();
        if id_name.starts_with('.') || !id.path().is_dir() {
            continue;
        }
        for v in std::fs::read_dir(id.path())?.flatten() {
            let v_name = v.file_name().to_string_lossy().into_owned();
            if v_name.starts_with('.') || !v.path().is_dir() {
                continue;
            }
            out.push((id_name.clone(), v_name, v.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// The index entry for one plugin version.
pub fn version_entry(m: &Manifest, files: BTreeMap<String, String>, yanked: bool) -> Version {
    let mut targets: Vec<String> = m.packages.iter().flat_map(|p| p.assets.iter().map(|a| a.target.clone())).collect();
    targets.sort();
    targets.dedup();
    Version {
        files,
        permissions: permissions::all(m),
        targets,
        goatty: m.compat.goatty.clone(),
        yanked,
        os: m.compat.os.clone(),
        shells: m.shells().into_iter().map(str::to_owned).collect(),
    }
}

/// Validate every plugin in the registry and build its index. The
/// sequence continues from the committed `index.json`: the same when the
/// plugins are unchanged and its expiry isn't due ([`Index::renewal_due`]),
/// +1 otherwise with a fresh `expires`. Problems in any plugin fail the build.
pub fn build(dir: &Path) -> Result<Index> {
    build_at(dir, now())
}

fn build_at(dir: &Path, now: u64) -> Result<Index> {
    let meta = read_meta(dir)?;
    let mut plugins: BTreeMap<String, Entry> = BTreeMap::new();
    let mut problems = Vec::new();
    for (id, version, path) in plugin_dirs(dir)? {
        let where_ = format!("plugins/{id}/{version}");
        let files = match crate::bundle::list_files(&path) {
            Ok(f) => f,
            Err(e) => {
                problems.push(format!("{where_}: {e:#}"));
                continue;
            }
        };
        let m = match manifest::load_dir(&path) {
            Ok(m) => m,
            Err(e) => {
                problems.push(format!("{where_}: {e:#}"));
                continue;
            }
        };
        if m.id != id || m.version != version {
            problems.push(format!("{where_}: the manifest says {}@{}; the directory must be plugins/<id>/<version>", m.id, m.version));
            continue;
        }
        let yanked = meta.yanked.contains(&format!("{id}@{version}"));
        let entry = plugins.entry(id.clone()).or_insert_with(|| Entry {
            name: m.name.clone(),
            summary: m.summary.clone(),
            publisher: m.publisher.clone(),
            category: m.category,
            keywords: m.keywords.clone(),
            homepage: m.homepage.clone(),
            versions: BTreeMap::new(),
        });
        entry.versions.insert(version.clone(), version_entry(&m, files, yanked));
        // Describe the plugin by its newest version.
        let newest = entry.versions.keys().filter_map(|k| semver::Version::parse(k).ok()).max();
        if newest.is_some_and(|n| n.to_string() == version) {
            entry.name = m.name.clone();
            entry.summary = m.summary.clone();
            entry.publisher = m.publisher.clone();
            entry.category = m.category;
            entry.keywords = m.keywords.clone();
            entry.homepage = m.homepage.clone();
        }
    }
    if !problems.is_empty() {
        bail!("the registry has problems:\n  {}", problems.join("\n  "));
    }
    let previous: Option<Index> = std::fs::read(dir.join(INDEX)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let (sequence, generated, expires) = match &previous {
        Some(p) if p.plugins == plugins && p.registry == meta.name && !p.renewal_due(now) => (p.sequence, p.generated, p.expires),
        Some(p) => (p.sequence + 1, now, Some(now + LIFETIME_SECS)),
        None => (1, now, Some(now + LIFETIME_SECS)),
    };
    Ok(Index { format: FORMAT, registry: meta.name, sequence, generated, expires, plugins })
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Whether the committed index matches the plugin directories and isn't
/// due for renewal.
pub fn is_current(dir: &Path) -> Result<bool> {
    let built = build(dir)?;
    let committed: Index = serde_json::from_slice(&std::fs::read(dir.join(INDEX)).context("no index.json")?).context("parse index.json")?;
    Ok(committed == built)
}

/// Check a registry snapshot: the signature over `index.json` by a trusted
/// key, the format, the sequence (not older than `min_sequence`), and every
/// listed file's checksum. Returns the index.
pub fn verify_snapshot(dir: &Path, trusted: &[String], min_sequence: u64) -> Result<Index> {
    let bytes = std::fs::read(dir.join(INDEX)).context("the registry has no index.json")?;
    let sig: Detached = serde_json::from_slice(&std::fs::read(dir.join(SIGNATURE)).context("the registry's index isn't signed")?)
        .context("unreadable index.json.sig")?;
    sign::verify(&bytes, &sig, trusted)?;
    let index: Index = serde_json::from_slice(&bytes).context("unreadable index.json")?;
    if index.format != FORMAT {
        bail!("index format {} needs a newer goatty", index.format);
    }
    if index.sequence < min_sequence {
        bail!("the registry served an older index (sequence {} < {min_sequence} seen before); refusing it", index.sequence);
    }
    for (id, e) in &index.plugins {
        if !manifest::is_id(id) {
            bail!("bad plugin id {id:?} in the index");
        }
        for (v, ver) in &e.versions {
            if !manifest::is_bundle_path(v) {
                bail!("bad version {v:?} for {id}");
            }
            let pdir = dir.join("plugins").join(id).join(v);
            for (rel, sha) in &ver.files {
                if !manifest::is_bundle_path(rel) {
                    bail!("bad file name {rel:?} in {id}@{v}");
                }
                let data = std::fs::read(pdir.join(rel)).with_context(|| format!("{id}@{v}: {rel} is missing"))?;
                if crate::bundle::sha256_hex(&data) != *sha {
                    bail!("{id}@{v}: {rel} doesn't match the signed index");
                }
            }
        }
    }
    Ok(index)
}

/// Sign `dir/index.json` with `key`, writing `index.json.sig`. The key
/// must be one of `registry.toml`'s keys.
pub fn sign_dir(dir: &Path, key: &ed25519_dalek::SigningKey) -> Result<Detached> {
    let meta = read_meta(dir)?;
    let public = sign::public_of(key);
    if !meta.keys.iter().any(|k| k.eq_ignore_ascii_case(&public)) {
        bail!("key {public} is not listed in {META}'s keys");
    }
    let bytes = std::fs::read(dir.join(INDEX)).context("build the index first (goatty-plugin index build)")?;
    let sig = sign::sign(key, &bytes);
    let mut text = serde_json::to_string_pretty(&sig)?;
    text.push('\n');
    std::fs::write(dir.join(SIGNATURE), text)?;
    Ok(sig)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::manifest::tests::EXAMPLE;

    /// A registry with the example plugin, signed. Returns the public key.
    pub fn make_registry(dir: &Path) -> (String, ed25519_dalek::SigningKey) {
        let keyfile = dir.join("k.key");
        let public = sign::keygen_file(&keyfile).unwrap();
        let key = sign::load_secret(&keyfile).unwrap();
        std::fs::remove_file(&keyfile).unwrap();
        std::fs::write(dir.join(META), format!("name = \"test\"\nkeys = [\"{public}\"]\n")).unwrap();
        let p = dir.join("plugins/eza/1.0.0");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("plugin.toml"), EXAMPLE).unwrap();
        let index = build(dir).unwrap();
        std::fs::write(dir.join(INDEX), index.to_bytes()).unwrap();
        sign_dir(dir, &key).unwrap();
        (public, key)
    }

    #[test]
    fn build_sign_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let (public, key) = make_registry(dir);
        let trusted = vec![public];
        let index = verify_snapshot(dir, &trusted, 0).unwrap();
        assert_eq!(index.sequence, 1);
        assert_eq!(index.latest("eza").unwrap().0, "1.0.0");
        assert_eq!(index.plugins["eza"].versions["1.0.0"].permissions, [Permission::Binaries, Permission::Aliases, Permission::Env]);
        assert!(is_current(dir).unwrap());
        assert!(verify_snapshot(dir, &trusted, 2).unwrap_err().to_string().contains("older index"));

        // A file changed after signing.
        let manifest = dir.join("plugins/eza/1.0.0/plugin.toml");
        std::fs::write(&manifest, EXAMPLE.replace("A modern ls", "A modern, evil ls")).unwrap();
        assert!(verify_snapshot(dir, &trusted, 0).unwrap_err().to_string().contains("doesn't match the signed index"));
        assert!(!is_current(dir).unwrap());

        // Rebuilt and re-signed: the sequence goes up.
        let rebuilt = build(dir).unwrap();
        assert_eq!(rebuilt.sequence, 2);
        std::fs::write(dir.join(INDEX), rebuilt.to_bytes()).unwrap();
        assert!(verify_snapshot(dir, &trusted, 0).unwrap_err().to_string().contains("bad signature"));
        sign_dir(dir, &key).unwrap();
        assert_eq!(verify_snapshot(dir, &trusted, 2).unwrap().plugins["eza"].summary, "A modern, evil ls");
        assert_eq!(build(dir).unwrap().sequence, 2, "unchanged plugins keep the sequence");
    }

    #[test]
    fn expiry_is_renewed_before_it_runs_out() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let (public, _) = make_registry(dir);
        let index = verify_snapshot(dir, &[public], 0).unwrap();
        let expires = index.expires.unwrap();
        assert_eq!(expires, index.generated + LIFETIME_SECS);
        assert!(!index.expired(expires - 1));
        assert!(index.expired(expires));

        // Unchanged and plenty of time left: the same index.
        let early = build_at(dir, expires - RENEW_BEFORE_SECS).unwrap();
        assert_eq!(early, index);
        // Close to expiring: renewed, with a new sequence so the old one can't be replayed.
        let late = build_at(dir, expires - RENEW_BEFORE_SECS + 1).unwrap();
        assert_eq!(late.sequence, 2);
        assert_eq!(late.expires, Some(expires - RENEW_BEFORE_SECS + 1 + LIFETIME_SECS));
        assert_eq!(late.plugins, index.plugins);

        // An index from before `expires` never expires, and is renewed by the next build.
        let mut old = index.clone();
        old.expires = None;
        assert!(!String::from_utf8(old.to_bytes()).unwrap().contains("expires"));
        assert!(!old.expired(u64::MAX));
        std::fs::write(dir.join(INDEX), old.to_bytes()).unwrap();
        assert_eq!(build(dir).unwrap().sequence, 2);
    }

    #[test]
    fn versions_say_where_they_install() {
        let m = manifest::parse(EXAMPLE, None).unwrap();
        let v = version_entry(&m, BTreeMap::new(), false);
        assert_eq!(v.shells, ["zsh"], "the default is spelled out");
        assert_eq!(v.incompatible("0.1.0", "x86_64-linux"), None);
        assert!(v.incompatible("0.1.0", "x86_64-windows").unwrap().contains("integrates with zsh only"));
        assert!(v.incompatible("0.1.0", "aarch64-linux").unwrap().contains("no build"));

        let win =
            Version { os: vec!["windows".into()], shells: vec!["powershell".into()], targets: vec!["x86_64-windows".into()], ..v.clone() };
        assert_eq!(win.incompatible("0.1.0", "aarch64-windows"), None, "x64 builds run on Windows on ARM");
        assert!(win.incompatible("0.1.0", "x86_64-linux").unwrap().contains("supports windows only"));
        // An index from before `shells` was written: the plan decides.
        let old = Version { shells: Vec::new(), targets: vec!["any".into()], ..v.clone() };
        assert_eq!(old.incompatible("0.1.0", "x86_64-windows"), None);
        let json = serde_json::to_value(&old).unwrap();
        assert!(json.get("os").is_none() && json.get("shells").is_none(), "{json}");
        let newer = Version { goatty: ">=9".into(), ..v };
        assert!(newer.incompatible("0.1.0", "x86_64-linux").unwrap().contains("needs goatty"));
    }

    #[test]
    fn rejects_mismatched_dirs_and_unknown_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let (_, key) = make_registry(dir);
        std::fs::rename(dir.join("plugins/eza/1.0.0"), dir.join("plugins/eza/1.0.1")).unwrap();
        assert!(build(dir).unwrap_err().to_string().contains("plugins/<id>/<version>"));
        let other = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        assert!(sign_dir(dir, &other).unwrap_err().to_string().contains("not listed"));
        drop(key);
    }
}
