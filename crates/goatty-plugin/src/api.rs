//! Plugin operations as JSON documents, shared by goattyd, `goatty
//! --local` and the GUI. They travel inside one protocol message pair
//! (`ClientMsg::Plugin` / `ServerMsg::Plugin`), so adding fields or
//! operations doesn't need a protocol bump.
//!
//! Installing is two steps: [`Request::Plan`] returns what would happen
//! (downloads, permissions, shell code) with a [`Plan::digest`];
//! [`Request::Install`] carries that digest back. goattyd recomputes the
//! plan and refuses when anything security-relevant changed in between, so
//! what gets installed is exactly what the user approved.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::{Category, Manifest};
use crate::permissions::{Granted, Permission};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// What's installed and what the registries offer. `refresh`: fetch
    /// the registries first (otherwise only when never fetched).
    Catalog {
        refresh: bool,
    },
    /// What's installed on this host (no registry load or network fetch).
    /// Reply: [`Reply::Installed`].
    Installed,
    /// What installing (or updating to) `id` would do.
    Plan {
        id: String,
        version: Option<String>,
    },
    /// Install or update `id` as planned; `approved` is the plan's digest.
    Install {
        id: String,
        version: Option<String>,
        approved: String,
    },
    Uninstall {
        id: String,
    },
    /// Every snippet and expansion on this host: the user's own (from
    /// `host.json`) and the enabled plugins' (#89).
    Snippets,
    /// Suggestions for one snippet input: the host looks its source up in
    /// its own snippets (a client never sends a command), fills in the
    /// earlier `values` and runs it in `cwd` (the pane's directory there).
    Suggest {
        /// `""` for the user's own snippets, else the plugin's id.
        owner: String,
        snippet: String,
        input: String,
        values: std::collections::BTreeMap<String, String>,
        cwd: Option<String>,
    },
    /// Suggestions for one input of a plugin's palette command
    /// (`[command.input.source]`): like [`Request::Suggest`], the host
    /// looks the source up in the enabled plugin's own manifest.
    CommandSuggest {
        plugin: String,
        command: String,
        input: String,
        values: std::collections::BTreeMap<String, String>,
        cwd: Option<String>,
    },
    /// The installs running on the host, and the ones that finished
    /// recently (#102). Reply: [`Reply::Installs`].
    Installs,
    /// Stop the running install of `id` before it's activated (#102).
    /// Reply: [`Reply::Installs`]; refused once it's being activated.
    CancelInstall {
        id: String,
    },
    /// Installed plugins with a newer version in their registry (#95).
    /// Registries older than a day are fetched first. Reply: [`Reply::Updates`].
    Updates,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Catalog(Catalog),
    Installed { installed: Vec<Installed> },
    Plan(Box<Plan>),
    Changed(Changed),
    Snippets(SnippetList),
    Suggestions(Suggestions),
    Installs { installs: Vec<InstallProgress> },
    Updates { updates: Vec<Update> },
}

/// One install on the host ([`Request::Installs`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallProgress {
    pub id: String,
    pub name: String,
    pub version: String,
    pub step: Step,
    /// A cancel was asked for; the install stops at its next check.
    #[serde(default)]
    pub cancelling: bool,
}

/// Where an install is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// Waiting for another install or uninstall on the host to finish.
    Waiting,
    /// Downloading package `index` (from 1) of `count`: `bytes` of `total`
    /// (`total` is 0 when unknown).
    Downloading {
        package: String,
        index: usize,
        count: usize,
        bytes: u64,
        total: u64,
    },
    /// Unpacking, swapping it in and building the shell configuration: too
    /// late to cancel.
    Activating,
    Done(Changed),
    Failed {
        error: String,
    },
    Cancelled,
}

impl Step {
    pub fn finished(&self) -> bool {
        matches!(self, Step::Done(_) | Step::Failed { .. } | Step::Cancelled)
    }

    /// Cancelling still leaves everything as it was.
    pub fn cancellable(&self) -> bool {
        matches!(self, Step::Waiting | Step::Downloading { .. })
    }
}

/// A newer version of an installed plugin ([`Request::Updates`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub id: String,
    pub name: String,
    pub installed: String,
    pub version: String,
    /// Permissions the new version has that the installed one doesn't.
    pub new_permissions: Vec<Permission>,
}

/// A host's snippets and expansions ([`Request::Snippets`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SnippetList {
    pub snippets: Vec<HostSnippet>,
    pub expansions: Vec<HostExpansion>,
    /// Expansions are on (`shell.expansions`).
    #[serde(default)]
    pub expansions_on: bool,
    /// Snippets or expansions that were left out, and why.
    pub issues: Vec<String>,
}

/// Where a snippet or expansion comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// `""`: the user's own; else the plugin's id.
    pub owner: String,
    /// "Yours", or the plugin's name.
    pub name: String,
}

impl Origin {
    pub fn mine(&self) -> bool {
        self.owner.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSnippet {
    pub origin: Origin,
    pub snippet: crate::snippets::Snippet,
    /// Programs the line runs that this host doesn't have (`docker`).
    #[serde(default)]
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostExpansion {
    pub origin: Origin,
    /// The key in `host.json` for the user's own (`""` for a plugin's).
    #[serde(default)]
    pub id: String,
    pub expansion: crate::snippets::Expansion,
    /// The user turned it off (their own) or its plugin is off.
    #[serde(default)]
    pub enabled: bool,
}

/// What a snippet or command input's source suggested ([`Request::Suggest`],
/// [`Request::CommandSuggest`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Suggestions {
    pub items: Vec<crate::snippets::Item>,
    /// Shown under the field: the source's `on_fail`, or what went wrong.
    pub note: Option<String>,
    /// There were more than the cap.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub installed: Vec<Installed>,
    pub available: Vec<Available>,
    pub registries: Vec<RegistryStatus>,
    /// Problems: broken installs, unreadable registries, bad settings.
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    pub id: String,
    pub version: String,
    /// Registry name, `local` for a plugin installed from a directory, or
    /// `goatty` for one that comes with goatty.
    pub source: String,
    /// Came from a registry with a valid signature.
    pub verified: bool,
    /// The full manifest (the settings window renders its settings).
    pub manifest: Manifest,
    pub enabled: bool,
    /// `None`: usable here; else why not.
    pub incompatible: Option<String>,
    /// A newer version in its registry.
    pub update: Option<String>,
    pub installed_unix: u64,
    /// Comes with goatty: can be turned off, not uninstalled.
    #[serde(default)]
    pub builtin: bool,
    /// Where each of its packages is on this host.
    #[serde(default)]
    pub packages: Vec<PackageStatus>,
    /// Ids of this plugin's `[[command]]`s that don't apply here (a `when`
    /// condition on settings is false, or the program `run` starts isn't
    /// installed on this host).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_commands: Vec<String>,
}

/// One package of an installed plugin, as found on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageStatus {
    pub id: String,
    /// The version the plugin pins.
    pub version: String,
    pub state: PackageState,
    /// goatty has a build of it for this host (it can install it).
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PackageState {
    /// The system's copy (preferred over goatty's own).
    System {
        path: String,
    },
    /// goatty's copy; an older `version` than the pinned one can be updated.
    Managed {
        version: String,
    },
    Missing,
}

impl PackageStatus {
    /// Installing the plugin would download something.
    pub fn needs_install(&self) -> bool {
        match &self.state {
            PackageState::Missing => true,
            PackageState::Managed { version } => *version != self.version,
            PackageState::System { .. } => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Available {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub publisher: String,
    pub category: Category,
    pub keywords: Vec<String>,
    pub homepage: Option<String>,
    pub registry: String,
    pub version: String,
    pub permissions: Vec<Permission>,
    /// `None`: can be installed here; else why not.
    pub incompatible: Option<String>,
    /// The installed version, if any.
    pub installed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryStatus {
    pub name: String,
    pub url: String,
    /// Built into goatty (can't be removed).
    pub builtin: bool,
    pub sequence: Option<u64>,
    pub fetched_unix: Option<u64>,
    pub plugins: usize,
    pub error: Option<String>,
    /// The public keys (hex) trusted to sign its index. Empty from a
    /// goattyd that predates this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

/// What installing a plugin will do, for the user to approve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub description: String,
    pub publisher: String,
    pub homepage: Option<String>,
    pub license: Option<String>,
    /// Registry name, or the directory for a local plugin.
    pub source: String,
    /// From a signed registry (`false`: a local directory, unreviewed).
    pub verified: bool,
    /// Comes with goatty: the plan only concerns its packages.
    #[serde(default)]
    pub builtin: bool,
    /// The installed version this replaces (an update).
    pub replaces: Option<String>,
    pub downloads: Vec<Download>,
    pub permissions: Vec<Granted>,
    /// `ls → eza --icons=auto` with the current settings.
    pub aliases: Vec<String>,
    pub env: Vec<String>,
    pub files: Vec<String>,
    /// The plugin's shell code, to read before approving: (file, contents).
    pub shell_code: Option<(String, String)>,
    /// Commands the plugin needs that goatty won't install.
    pub requires: Vec<String>,
    pub warnings: Vec<String>,
    /// Plugins this one needs (`requires.plugins`, and theirs) that aren't
    /// on the host yet, each with its own plan, in the order they're
    /// installed (before this one). The one approval covers them all: their
    /// digests are part of [`Plan::digest`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Plan>,
    /// Over everything security-relevant; [`Request::Install`] sends it back.
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Download {
    pub package: String,
    pub version: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub bins: Vec<String>,
    /// A zsh script in the package that the shell sources.
    #[serde(default)]
    pub script: Option<String>,
    /// Already installed on the system: goatty uses that copy and downloads nothing.
    pub system: Option<String>,
    /// goatty's copy of exactly this version is already installed: nothing to download.
    #[serde(default)]
    pub current: bool,
}

impl Download {
    /// Something will be downloaded.
    pub fn fetches(&self) -> bool {
        self.system.is_none() && !self.current
    }
}

impl Plan {
    /// Bytes to download, dependencies included.
    pub fn download_size(&self) -> u64 {
        self.downloads.iter().filter(|d| d.fetches()).map(|d| d.size).sum::<u64>()
            + self.dependencies.iter().map(Plan::download_size).sum::<u64>()
    }

    /// This plan and its dependencies' (which come first), in install order.
    pub fn all(&self) -> impl Iterator<Item = &Plan> {
        self.dependencies.iter().chain(std::iter::once(self))
    }
}

impl Plan {
    /// The digest over what the user approves: identity, source, every
    /// download and file checksum, permissions, shell code, and the
    /// dependencies' digests (computed first).
    pub fn compute_digest(&self, files: &std::collections::BTreeMap<String, String>) -> String {
        #[derive(Serialize)]
        struct Approved<'a> {
            id: &'a str,
            version: &'a str,
            source: &'a str,
            verified: bool,
            files: &'a std::collections::BTreeMap<String, String>,
            downloads: Vec<(&'a str, &'a str, &'a str, bool)>,
            permissions: Vec<Permission>,
            shell_code: Option<&'a (String, String)>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            dependencies: Vec<&'a str>,
        }
        let a = Approved {
            id: &self.id,
            version: &self.version,
            source: &self.source,
            verified: self.verified,
            files,
            downloads: self.downloads.iter().map(|d| (d.package.as_str(), d.url.as_str(), d.sha256.as_str(), d.system.is_some())).collect(),
            permissions: self.permissions.iter().map(|g| g.permission).collect(),
            shell_code: self.shell_code.as_ref(),
            dependencies: self.dependencies.iter().map(|d| d.digest.as_str()).collect(),
        };
        let json = serde_json::to_vec(&a).expect("serializes");
        Sha256::digest(&json).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Runs code as the user (programs or shell code), itself or through a dependency.
    pub fn runs_code(&self) -> bool {
        self.all().any(|p| p.permissions.iter().any(|g| g.permission.runs_code()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Changed {
    /// What happened, for the user.
    pub lines: Vec<String>,
    /// The shell configuration generation now active.
    pub generation: Option<u32>,
    /// The shell configuration changed: running shells can reload.
    pub reload: bool,
    /// After an uninstall: plugins that were installed only because
    /// another plugin needed them, and that nothing needs now. In an order
    /// they can be uninstalled in (a plugin before the ones it needs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unneeded: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let r = Request::Install { id: "eza".into(), version: None, approved: "ab".into() };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"op":"install","id":"eza","version":null,"approved":"ab"}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), r);
        let c = Reply::Changed(Changed { lines: vec!["ok".into()], generation: Some(3), reload: true, unneeded: vec!["base".into()] });
        let back: Reply = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn install_progress_round_trips() {
        let steps = [
            Step::Waiting,
            Step::Downloading { package: "eza".into(), index: 1, count: 2, bytes: 10, total: 20 },
            Step::Activating,
            Step::Done(Changed::default()),
            Step::Failed { error: "no".into() },
            Step::Cancelled,
        ];
        let list: Vec<InstallProgress> = steps
            .into_iter()
            .map(|step| InstallProgress { id: "eza".into(), name: "eza".into(), version: "1".into(), step, cancelling: false })
            .collect();
        let r = Reply::Installs { installs: list };
        let back: Reply = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert_eq!(serde_json::to_string(&Request::CancelInstall { id: "eza".into() }).unwrap(), r#"{"op":"cancel_install","id":"eza"}"#);
    }

    #[test]
    fn installed_unavailable_commands_defaults_and_round_trips() {
        let m: Manifest = toml::from_str(
            r#"
schema = 1
id = "demo"
name = "Demo"
version = "1.0.0"
summary = "Test"
publisher = "tests"
"#,
        )
        .unwrap();
        let inst = Installed {
            id: "demo".into(),
            version: "1.0.0".into(),
            source: "test".into(),
            verified: true,
            manifest: m,
            enabled: true,
            incompatible: None,
            update: None,
            installed_unix: 0,
            builtin: false,
            packages: vec![],
            unavailable_commands: vec![],
        };
        let empty_json = serde_json::to_string(&inst).unwrap();
        assert!(!empty_json.contains("unavailable_commands"), "{empty_json}");
        let mut with_unavail = inst.clone();
        with_unavail.unavailable_commands = vec!["exec".into(), "logs".into()];
        let full_json = serde_json::to_string(&with_unavail).unwrap();
        assert!(full_json.contains("unavailable_commands"));
        let back: Installed = serde_json::from_str(&full_json).unwrap();
        assert_eq!(back.unavailable_commands, ["exec", "logs"]);
        let back_empty: Installed = serde_json::from_str(&empty_json).unwrap();
        assert!(back_empty.unavailable_commands.is_empty());

        let req_json = serde_json::to_string(&Request::Installed).unwrap();
        assert_eq!(req_json, r#"{"op":"installed"}"#);
        assert_eq!(serde_json::from_str::<Request>(&req_json).unwrap(), Request::Installed);

        for list in [vec![], vec![with_unavail]] {
            let reply = Reply::Installed { installed: list };
            let json = serde_json::to_string(&reply).unwrap();
            let back: Reply = serde_json::from_str(&json).unwrap();
            assert_eq!(back, reply);
        }
    }
}
