//! What an update changes (#244): the plan of the new version compared
//! with the installed one ([`Plan::previous`]), so a review shows only the
//! differences, and whether anything in them needs the user's eyes before
//! it's installed ([`Changes::needs_review`]).
//!
//! An update needs a review when it can do something the installed
//! version couldn't: a new permission or new items under one (a new or
//! changed alias, variable value, config file, command, snippet source),
//! different shell code, a new program, new executables, sourced scripts
//! or setup commands of a known program, a download from another site, a
//! different download under the same version, plugins installed along
//! with it, or a different or unsigned source. Newer pinned builds of the
//! same programs, from the same site and the same signed registry, don't:
//! their checksums come from the signed index the user already trusts for
//! that plugin.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::api::{Plan, Previous, PreviousPackage};
use crate::permissions::{Granted, Permission};

/// Lines of context around each change in the shell code diff.
const CONTEXT: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Changes {
    /// The installed version.
    pub from: String,
    /// The version the update installs.
    pub to: String,
    /// (old, new) registry, when it comes from another one.
    pub source: Option<(String, String)>,
    /// The new version isn't from a signed registry.
    pub unverified: bool,
    pub permissions: Vec<PermissionChange>,
    pub packages: Vec<PackageChange>,
    pub aliases: ListChange,
    pub env: ListChange,
    pub files: ListChange,
    pub requires: ListChange,
    pub shell_code: Option<CodeChange>,
    /// Plugins installed along with the update (names).
    pub dependencies: Vec<String>,
    /// What known programs newly bring: executables, sourced scripts,
    /// setup commands (`eza: eza, eza-extra`).
    pub program_changes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Added,
    Removed,
    Changed,
}

/// A permission the update adds, drops, or covers different items with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionChange {
    pub kind: Kind,
    /// The new version's (the old one's for [`Kind::Removed`]).
    pub granted: Granted,
    /// Items it newly covers / no longer covers ([`Kind::Changed`]).
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl PermissionChange {
    /// It lets the plugin do something it couldn't before.
    pub fn widens(&self) -> bool {
        match self.kind {
            Kind::Added => true,
            Kind::Removed => false,
            // Programs, aliases, variables and files are compared on their
            // own ([`Changes::program_changes`], the lists), with values.
            Kind::Changed => {
                !matches!(self.granted.permission, Permission::Binaries | Permission::Aliases | Permission::Env | Permission::ConfigFiles)
                    && !self.added.is_empty()
            }
        }
    }
}

/// A program the update adds, drops or moves to another version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageChange {
    pub package: String,
    /// `None`: new with this update.
    pub from: Option<String>,
    /// `None`: the update removes it.
    pub to: Option<String>,
    /// Bytes to download (0: nothing, a system copy is used).
    pub size: u64,
    /// Where it's downloaded from (`github.com`).
    pub host: Option<String>,
    /// Why this download needs a look (another site, or a different file
    /// under the same version).
    pub concern: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ListChange {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl ListChange {
    fn between(old: &[String], new: &[String]) -> ListChange {
        let o: BTreeSet<&String> = old.iter().collect();
        let n: BTreeSet<&String> = new.iter().collect();
        ListChange {
            added: new.iter().filter(|x| !o.contains(x)).cloned().collect(),
            removed: old.iter().filter(|x| !n.contains(x)).cloned().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// The shell code's diff, in hunks with a little context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeChange {
    /// The new version's file (the old one's when it's dropped).
    pub file: String,
    pub added: usize,
    pub removed: usize,
    /// The installed version had no shell code.
    pub new_code: bool,
    /// The update drops the shell code.
    pub dropped: bool,
    pub hunks: Vec<Vec<DiffLine>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    /// `None`: context.
    pub kind: Option<Kind>,
    /// Line number in the new file (the old one for a removed line).
    pub line: usize,
    pub text: String,
}

impl Plan {
    /// What this update changes, compared with the installed version.
    /// `None` for an install, or a plan without [`Plan::previous`].
    pub fn changes(&self) -> Option<Changes> {
        self.previous.as_deref().map(|p| between(p, self))
    }
}

/// The differences from `old` to `new`.
pub fn between(old: &Previous, new: &Plan) -> Changes {
    let mut permissions = Vec::new();
    for g in &new.permissions {
        match old.permissions.iter().find(|o| o.permission == g.permission) {
            None => {
                permissions.push(PermissionChange { kind: Kind::Added, granted: g.clone(), added: g.details.clone(), removed: Vec::new() })
            }
            Some(o) => {
                let d = ListChange::between(&o.details, &g.details);
                if !d.is_empty() {
                    permissions.push(PermissionChange { kind: Kind::Changed, granted: g.clone(), added: d.added, removed: d.removed });
                }
            }
        }
    }
    for o in old.permissions.iter().filter(|o| !new.permissions.iter().any(|g| g.permission == o.permission)) {
        permissions.push(PermissionChange { kind: Kind::Removed, granted: o.clone(), added: Vec::new(), removed: o.details.clone() });
    }

    let mut packages = Vec::new();
    for d in &new.downloads {
        let old_pkg: Option<&PreviousPackage> = old.packages.iter().find(|p| p.id == d.package);
        let host = (d.fetches() && !d.url.is_empty()).then(|| url_host(&d.url));
        let old_host = old_pkg.filter(|p| !p.url.is_empty()).map(|p| url_host(&p.url));
        let mut concern = None;
        if let (Some(a), Some(b)) = (&old_host, &host)
            && a != b
        {
            concern = Some(format!("{} downloads from another site ({a} → {b})", d.package));
        }
        if let Some(p) = old_pkg
            && p.version == d.version
        {
            if !p.sha256.is_empty() && !d.sha256.is_empty() && p.sha256 != d.sha256 && d.fetches() {
                concern = Some(format!("{} {} is a different download than the installed one", d.package, d.version));
            }
            if concern.is_none() {
                continue;
            }
        }
        packages.push(PackageChange {
            package: d.package.clone(),
            from: old_pkg.map(|p| p.version.clone()),
            to: Some(d.version.clone()),
            size: if d.fetches() { d.size } else { 0 },
            host,
            concern,
        });
    }
    for p in old.packages.iter().filter(|p| !new.downloads.iter().any(|d| d.package == p.id)) {
        packages.push(PackageChange { package: p.id.clone(), from: Some(p.version.clone()), to: None, size: 0, host: None, concern: None });
    }

    // "eza 0.23.4: eza, `eza completions` once…" → "eza: eza, …": what a
    // known program brings, whatever its version.
    let programs = |g: Option<&Granted>| -> Vec<String> {
        g.map(|g| {
            g.details
                .iter()
                .map(|d| match d.split_once(": ") {
                    Some((head, what)) => format!("{}: {what}", head.split(' ').next().unwrap_or(head)),
                    None => d.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
    };
    let binaries = |list: &[Granted]| list.iter().find(|g| g.permission == Permission::Binaries).cloned();
    let (old_programs, new_programs) = (programs(binaries(&old.permissions).as_ref()), programs(binaries(&new.permissions).as_ref()));
    let program_changes = new_programs
        .into_iter()
        .filter(|d| !old_programs.contains(d))
        .filter(|d| d.split_once(": ").is_some_and(|(id, _)| old.packages.iter().any(|p| p.id == id)))
        .collect();

    let shell_code = match (&old.shell_code, &new.shell_code) {
        (None, None) => None,
        (Some((_, a)), Some((_, b))) if a == b => None,
        (old_code, new_code) => {
            let a = old_code.as_ref().map_or("", |(_, c)| c.as_str());
            let b = new_code.as_ref().map_or("", |(_, c)| c.as_str());
            let file = new_code.as_ref().or(old_code.as_ref()).map(|(f, _)| f.clone()).unwrap_or_default();
            let mut c = diff(a, b);
            c.file = file;
            c.new_code = old_code.is_none();
            c.dropped = new_code.is_none();
            Some(c)
        }
    };

    Changes {
        from: old.version.clone(),
        to: new.version.clone(),
        source: (old.source != new.source).then(|| (old.source.clone(), new.source.clone())),
        unverified: !new.verified,
        permissions,
        packages,
        aliases: ListChange::between(&old.aliases, &new.aliases),
        env: ListChange::between(&old.env, &new.env),
        files: ListChange::between(&old.files, &new.files),
        requires: ListChange::between(&old.requires, &new.requires),
        shell_code,
        dependencies: new.dependencies.iter().map(|d| d.name.clone()).collect(),
        program_changes,
    }
}

impl Changes {
    /// Why the user should look before it's installed, one line each.
    /// Empty: it does nothing the installed version couldn't, so one click
    /// may install it.
    pub fn review_reasons(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.unverified {
            out.push("It isn't from a signed registry".to_string());
        }
        if let Some((a, b)) = &self.source {
            out.push(format!("It comes from another registry ({a} → {b})"));
        }
        for p in self.permissions.iter().filter(|p| p.widens()) {
            out.push(match p.kind {
                Kind::Added => format!("New permission: {}", p.granted.title),
                _ => format!("{}: {} new", p.granted.title, p.added.len()),
            });
        }
        if let Some(c) = &self.shell_code
            && !c.dropped
        {
            out.push(if c.new_code {
                format!("New shell code ({} lines)", c.added)
            } else {
                format!("Its shell code changed (+{} −{})", c.added, c.removed)
            });
        }
        for p in self.packages.iter().filter(|p| p.from.is_none()) {
            out.push(format!("New program: {}", p.package));
        }
        out.extend(self.packages.iter().filter_map(|p| p.concern.clone()));
        for c in &self.program_changes {
            out.push(format!("A program brings something new: {c}"));
        }
        let added = |p: Permission| self.permissions.iter().any(|c| c.kind == Kind::Added && c.granted.permission == p);
        for (list, what, p) in [
            (&self.aliases, "Aliases", Permission::Aliases),
            (&self.env, "Environment variables", Permission::Env),
            (&self.files, "Config files", Permission::ConfigFiles),
        ] {
            if !list.added.is_empty() && !added(p) {
                out.push(format!("{what}: {} new or changed", list.added.len()));
            }
        }
        if !self.dependencies.is_empty() {
            out.push(format!("Also installs {}", self.dependencies.join(", ")));
        }
        out
    }

    pub fn needs_review(&self) -> bool {
        !self.review_reasons().is_empty()
    }

    /// Nothing the user would notice changes besides the version.
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.permissions.is_empty()
            && self.packages.is_empty()
            && self.aliases.is_empty()
            && self.env.is_empty()
            && self.files.is_empty()
            && self.requires.is_empty()
            && self.shell_code.is_none()
            && self.dependencies.is_empty()
    }

    /// Bytes the update downloads.
    pub fn download_size(&self) -> u64 {
        self.packages.iter().map(|p| p.size).sum()
    }
}

/// A line diff of `a` → `b` in hunks with [`CONTEXT`] lines around changes.
fn diff(a: &str, b: &str) -> CodeChange {
    use similar::{ChangeTag, TextDiff};
    let d = TextDiff::from_lines(a, b);
    let (mut added, mut removed) = (0, 0);
    let mut hunks = Vec::new();
    for group in d.grouped_ops(CONTEXT) {
        let mut lines = Vec::new();
        for op in &group {
            for c in d.iter_changes(op) {
                let (kind, line) = match c.tag() {
                    ChangeTag::Equal => (None, c.new_index()),
                    ChangeTag::Insert => {
                        added += 1;
                        (Some(Kind::Added), c.new_index())
                    }
                    ChangeTag::Delete => {
                        removed += 1;
                        (Some(Kind::Removed), c.old_index())
                    }
                };
                let text = c.value().trim_end_matches(['\n', '\r']).to_string();
                lines.push(DiffLine { kind, line: line.map_or(0, |i| i + 1), text });
            }
        }
        hunks.push(lines);
    }
    CodeChange { file: String::new(), added, removed, new_code: false, dropped: false, hunks }
}

/// `https://github.com/x/y/releases/…` → `github.com`.
pub fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Download;

    fn granted(p: Permission, details: &[&str]) -> Granted {
        Granted {
            permission: p,
            title: p.title().into(),
            meaning: String::new(),
            reason: String::new(),
            details: details.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn download(package: &str, version: &str) -> Download {
        Download {
            package: package.into(),
            version: version.into(),
            url: format!("https://github.com/x/{package}/releases/{version}.tar.gz"),
            size: 2_000_000,
            sha256: "00".into(),
            bins: vec![package.into()],
            script: None,
            system: None,
            current: false,
        }
    }

    fn previous() -> Previous {
        Previous {
            version: "1.0.0".into(),
            source: "official".into(),
            verified: true,
            packages: vec![PreviousPackage {
                id: "eza".into(),
                version: "0.23.4".into(),
                url: "https://github.com/x/eza/releases/0.23.4.tar.gz".into(),
                sha256: "00".into(),
            }],
            permissions: vec![granted(Permission::Binaries, &["eza 0.23.4: eza"]), granted(Permission::Aliases, &["ls → eza"])],
            aliases: vec!["ls → eza --icons=auto".into()],
            env: Vec::new(),
            files: Vec::new(),
            shell_code: None,
            requires: Vec::new(),
        }
    }

    fn update() -> Plan {
        Plan {
            id: "eza".into(),
            name: "eza".into(),
            version: "1.1.0".into(),
            summary: String::new(),
            description: String::new(),
            publisher: "goatty".into(),
            homepage: None,
            license: None,
            source: "official".into(),
            verified: true,
            builtin: false,
            replaces: Some("1.0.0".into()),
            downloads: vec![download("eza", "0.24.0")],
            permissions: vec![granted(Permission::Binaries, &["eza 0.24.0: eza"]), granted(Permission::Aliases, &["ls → eza"])],
            aliases: vec!["ls → eza --icons=auto".into()],
            env: Vec::new(),
            files: Vec::new(),
            shell_code: None,
            requires: Vec::new(),
            warnings: Vec::new(),
            dependencies: Vec::new(),
            digest: "d".into(),
            previous: Some(Box::new(previous())),
        }
    }

    #[test]
    fn a_new_build_of_the_same_program_needs_no_review() {
        let c = update().changes().unwrap();
        assert_eq!(c.packages.len(), 1);
        assert_eq!(c.packages[0].from.as_deref(), Some("0.23.4"));
        assert_eq!(c.packages[0].to.as_deref(), Some("0.24.0"));
        assert_eq!(c.packages[0].host.as_deref(), Some("github.com"));
        assert_eq!(c.download_size(), 2_000_000);
        assert!(c.review_reasons().is_empty(), "{:?}", c.review_reasons());
        assert!(!c.is_empty());
    }

    #[test]
    fn new_aliases_permissions_programs_and_code_need_a_review() {
        let mut p = update();
        p.permissions[1].details.push("ll → eza -l".into());
        p.aliases.push("ll → eza -l".into());
        let c = p.changes().unwrap();
        assert_eq!(c.aliases.added, ["ll → eza -l"]);
        assert_eq!(c.review_reasons(), ["Aliases: 1 new or changed"]);

        // A changed alias argument or variable value counts too.
        let mut p = update();
        p.aliases = vec!["ls → eza --icons=always --git".into()];
        assert_eq!(p.changes().unwrap().review_reasons(), ["Aliases: 1 new or changed"]);

        // A known program gaining an executable or a sourced script.
        let mut p = update();
        p.permissions[0].details = vec!["eza 0.24.0: eza, eza-sh (sourced by the shell)".into()];
        assert_eq!(p.changes().unwrap().review_reasons(), ["A program brings something new: eza: eza, eza-sh (sourced by the shell)"]);

        // Another download site, or another file under the same version.
        let mut p = update();
        p.downloads[0].url = "https://evil.example/eza.tar.gz".into();
        assert_eq!(p.changes().unwrap().review_reasons(), ["eza downloads from another site (github.com → evil.example)"]);
        let mut p = update();
        p.downloads[0].version = "0.23.4".into();
        p.downloads[0].url = "https://github.com/x/eza/releases/0.23.4.tar.gz".into();
        p.permissions[0].details = vec!["eza 0.23.4: eza".into()];
        assert!(p.changes().unwrap().packages.is_empty(), "same build: nothing to show");
        p.downloads[0].sha256 = "ff".into();
        assert_eq!(p.changes().unwrap().review_reasons(), ["eza 0.23.4 is a different download than the installed one"]);

        let mut p = update();
        p.permissions.push(granted(Permission::Network, &[]));
        assert_eq!(p.changes().unwrap().review_reasons(), [format!("New permission: {}", Permission::Network.title())]);

        let mut p = update();
        p.downloads.push(download("fzf", "0.60.0"));
        assert_eq!(p.changes().unwrap().review_reasons(), ["New program: fzf"]);

        let mut p = update();
        p.previous.as_mut().unwrap().shell_code = Some(("init.zsh".into(), "a\nb\nc\n".into()));
        p.shell_code = Some(("init.zsh".into(), "a\nB\nc\nd\n".into()));
        let c = p.changes().unwrap();
        let code = c.shell_code.as_ref().unwrap();
        assert_eq!((code.added, code.removed), (2, 1));
        assert_eq!(code.hunks.len(), 1);
        assert!(code.hunks[0].iter().any(|l| l.kind == Some(Kind::Removed) && l.text == "b"));
        assert_eq!(c.review_reasons(), ["Its shell code changed (+2 −1)"]);

        let mut p = update();
        p.verified = false;
        p.source = "/home/me/eza".into();
        assert_eq!(p.changes().unwrap().review_reasons().len(), 2);
    }

    #[test]
    fn dropping_things_needs_no_review() {
        let mut p = update();
        p.permissions.truncate(1);
        p.aliases.clear();
        p.previous.as_mut().unwrap().shell_code = Some(("init.zsh".into(), "x\n".into()));
        let c = p.changes().unwrap();
        assert_eq!(c.permissions.iter().map(|p| p.kind).collect::<Vec<_>>(), [Kind::Changed, Kind::Removed]);
        assert_eq!(c.aliases.removed, ["ls → eza --icons=auto"]);
        assert!(c.shell_code.as_ref().is_some_and(|s| s.dropped));
        assert!(!c.needs_review(), "{:?}", c.review_reasons());
    }

    #[test]
    fn an_install_has_no_changes() {
        let mut p = update();
        p.previous = None;
        assert!(p.changes().is_none());
    }
}
