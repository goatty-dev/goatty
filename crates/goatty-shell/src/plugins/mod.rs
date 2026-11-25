//! goatty plugins on this host (Stage 5): registries, install plans,
//! installs, and the plugins that go into the shell configuration.
//!
//! The format and the trust machinery are `goatty-plugin`; this module
//! knows where things live ([`Dirs`]) and how they reach a shell:
//!
//! 1. [`Plugins::find`] a plugin in a verified registry snapshot (or a
//!    local directory, [`Candidate::local`]),
//! 2. [`Plugins::plan`] what installing it does (downloads, permissions,
//!    shell code; and the same for each plugin it needs that isn't
//!    installed yet) and hand that to the user,
//! 3. [`Plugins::install`] exactly that plan (its digest must match),
//!    dependencies first,
//!    packages into `tools/` + `bin/`, a verified copy of the plugin's files
//!    into `installed/<id>/`, an entry in `installed/lock.json`,
//! 4. the caller enables it in `host.json` and builds a new generation;
//!    [`active`] is what the generation renders.
//!
//! The plugins that come with goatty ([`crate::builtins`]) take part in
//! all of this except the lock: they're always there, so planning and
//! installing one only concerns its packages.
//!
//! Nothing outside goatty's own directories is touched, and nothing runs
//! as root.

pub mod cmd;
pub mod jobs;
pub mod lock;
pub mod registry;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use goatty_config::HostSettings;
use goatty_plugin::api::{
    Available, Catalog, Download, Installed, PackageState, PackageStatus, Plan, Previous, PreviousPackage, RegistryStatus, Step, Update,
};
use goatty_plugin::fetch::Fetch;
use goatty_plugin::manifest::{self, Load, Manifest};
use goatty_plugin::schema::{self, Values};
use goatty_plugin::{GOATTY_VERSION, current_target, permissions, template};

use crate::builtins::{self, Builtin};
use crate::dirs::Dirs;
use crate::install::{self, PackageInstall};
use crate::packages::{self, Location};
use jobs::Job;
use lock::Lock;
use registry::{Snapshot, Source};

/// Registries are fetched again by [`Plugins::catalog`] when older than this.
pub const STALE_AFTER_SECS: u64 = 24 * 3600;

/// A plugin that can be planned and installed.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub manifest: Manifest,
    /// Every file, relative path → sha256 (from the signed index, or hashed
    /// from the local directory).
    pub files: BTreeMap<String, String>,
    /// Where the files are.
    pub dir: PathBuf,
    /// Registry name, the local directory, or [`builtins::SOURCE`].
    pub source: String,
    pub verified: bool,
    /// One of the plugins that come with goatty (its files are compiled in).
    pub builtin: Option<&'static Builtin>,
}

impl Candidate {
    /// A plugin that comes with goatty.
    pub fn builtin(b: &'static Builtin) -> Candidate {
        Candidate {
            manifest: b.manifest().clone(),
            files: b.checksums(),
            dir: PathBuf::new(),
            source: builtins::SOURCE.into(),
            verified: true,
            builtin: Some(b),
        }
    }

    /// One of the plugin's files.
    fn read(&self, rel: &str) -> Result<Vec<u8>> {
        match self.builtin {
            Some(b) => b.read(rel).map(|t| t.as_bytes().to_vec()).with_context(|| format!("{} has no {rel}", b.id)),
            None => std::fs::read(self.dir.join(rel)).with_context(|| format!("read {rel}")),
        }
    }

    /// A plugin directory on this host (development, private plugins).
    /// Not verified: nobody reviewed or signed it.
    pub fn local(dir: &Path) -> Result<Candidate> {
        let dir = dir.canonicalize().with_context(|| format!("{} doesn't exist", dir.display()))?;
        let files = goatty_plugin::bundle::list_files(&dir)?;
        let manifest = manifest::load_dir(&dir)?;
        Ok(Candidate { manifest, files, source: dir.display().to_string(), dir, verified: false, builtin: None })
    }

    fn is_local(&self) -> bool {
        !self.verified && Path::new(&self.source).is_absolute()
    }
}

/// Plugin operations on one host.
pub struct Plugins<'a> {
    pub dirs: &'a Dirs,
    pub fetch: &'a dyn Fetch,
    /// Built-in registries (normally just the official one).
    pub builtin: &'a [Source],
}

/// A warning when a registry's index is past its `expires` (#100): a newer
/// one may exist that something keeps from reaching this host (a freeze
/// attack hiding a yanked version), or the registry stopped re-signing.
/// Plugins keep working; only the warning tells.
fn expiry_warning(index: &goatty_plugin::index::Index, now: u64) -> Option<String> {
    let expires = index.expires.filter(|_| index.expired(now))?;
    let days = now.saturating_sub(expires) / (24 * 60 * 60);
    let ago = match days {
        0 => "today".to_string(),
        1 => "a day ago".to_string(),
        n => format!("{n} days ago"),
    };
    Some(format!(
        "registry {}: its index expired {ago}. Updates from it may be held back, so a yanked version could still be offered. Refresh it, and if this stays, check the connection to the registry.",
        index.registry
    ))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Serializes installs and uninstalls across goattyd, `goatty --local` and the CLI.
fn op_lock(dirs: &Dirs) -> Result<std::fs::File> {
    std::fs::create_dir_all(dirs.installed_dir())?;
    let f = std::fs::File::create(dirs.installed_dir().join(".lock"))?;
    f.lock()?;
    Ok(f)
}

/// [`op_lock`] for an install: waits without blocking, so a cancel of a
/// queued install takes effect while another one holds the lock.
fn op_lock_for(dirs: &Dirs, job: &Job) -> Result<std::fs::File> {
    std::fs::create_dir_all(dirs.installed_dir())?;
    let f = std::fs::File::create(dirs.installed_dir().join(".lock"))?;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) => {
                job.check()?;
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// Downloads waiting for activation, kept on disk (a plugin's packages
/// can add up to more than goattyd should hold in memory), and the staged
/// copy of the plugin's files. Removed when dropped, whatever happens to
/// the install.
struct Spool(Vec<PathBuf>);

impl Drop for Spool {
    fn drop(&mut self) {
        for f in &self.0 {
            if f.is_dir() {
                let _ = std::fs::remove_dir_all(f);
            } else {
                let _ = std::fs::remove_file(f);
            }
        }
    }
}

/// One plugin of an install, everything downloaded and staged: what's
/// left can't be cancelled ([`Plugins::commit`]).
struct Staged<'c> {
    c: &'c Candidate,
    plan: &'c Plan,
    /// Installed because another plugin of the install needs it.
    dependency: bool,
    /// Per package: the spooled download, if it fetches one.
    fetched: Vec<Option<PathBuf>>,
    /// The plugin's files, checked (not for a built-in plugin).
    staging: PathBuf,
    _spool: Spool,
}

impl Plugins<'_> {
    pub fn sources(&self) -> (Vec<Source>, Vec<String>) {
        registry::sources(self.dirs, self.builtin)
    }

    /// Every registry's snapshot: fetched when `refresh`, when never
    /// fetched, or (with `refresh_stale`) when older than a day.
    pub fn snapshots(&self, refresh: bool, refresh_stale: bool) -> (Vec<Snapshot>, Vec<RegistryStatus>, Vec<String>) {
        let (sources, mut issues) = self.sources();
        let mut snaps = Vec::new();
        let mut status = Vec::new();
        for s in sources {
            let stored = registry::load(self.dirs, &s);
            let mut error = None;
            let mut snap = match stored {
                Ok(snap) => snap,
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    None
                }
            };
            let stale = snap.as_ref().is_none_or(|x| refresh_stale && now().saturating_sub(x.fetched) > STALE_AFTER_SECS);
            if refresh || stale {
                match registry::refresh(self.dirs, &s, self.fetch) {
                    Ok(fresh) => {
                        snap = Some(fresh);
                        error = None;
                    }
                    Err(e) => error = Some(format!("{e:#}")),
                }
            }
            if let Some(warning) = snap.as_ref().and_then(|x| expiry_warning(&x.index, now())) {
                log::warn!("{warning}");
                issues.push(warning);
            }
            status.push(RegistryStatus {
                name: s.name.clone(),
                url: s.url.clone(),
                builtin: s.builtin,
                sequence: snap.as_ref().map(|x| x.index.sequence),
                fetched_unix: snap.as_ref().map(|x| x.fetched),
                plugins: snap.as_ref().map_or(0, |x| x.index.plugins.len()),
                error,
                keys: s.keys.clone(),
            });
            snaps.extend(snap);
        }
        (snaps, status, issues)
    }

    /// `id` (at `version`, default the newest): a built-in plugin, or from
    /// the first registry that has it.
    pub fn find(&self, id: &str, version: Option<&str>) -> Result<Candidate> {
        if let Some(b) = builtins::get(id) {
            let have = &b.manifest().version;
            if version.is_some_and(|v| v != have) {
                bail!("{id} comes with goatty (version {have}); other versions come with other goatty releases");
            }
            return Ok(Candidate::builtin(b));
        }
        let (snaps, status, _) = self.snapshots(false, false);
        for snap in &snaps {
            let Some(entry) = snap.index.plugins.get(id) else { continue };
            let (v, ver) = match version {
                Some(v) => (v, entry.versions.get(v).with_context(|| format!("{id} has no version {v} in registry {}", snap.source.name))?),
                None => snap.index.latest(id).with_context(|| format!("every version of {id} is yanked"))?,
            };
            let dir = snap.plugin_dir(id, v);
            let text = std::fs::read_to_string(dir.join(manifest::FILE))?;
            let files: std::collections::BTreeSet<String> = ver.files.keys().cloned().collect();
            let manifest = manifest::parse(&text, Some(&files)).map_err(|e| anyhow!("{id}@{v}: {e}"))?;
            return Ok(Candidate {
                manifest,
                files: ver.files.clone(),
                dir,
                source: snap.source.name.clone(),
                verified: true,
                builtin: None,
            });
        }
        let errors: Vec<String> = status.iter().filter_map(|s| s.error.as_ref().map(|e| format!("{}: {e}", s.name))).collect();
        if errors.is_empty() {
            bail!("no registry has a plugin called {id:?} (try `goattyd plugin search`)")
        } else {
            bail!("no registry has a plugin called {id:?}; some couldn't be read:\n  {}", errors.join("\n  "))
        }
    }

    /// What installing `c` would do, including the plugins it needs that
    /// aren't installed yet ([`Plan::dependencies`]). For a built-in
    /// plugin: downloading the packages it's missing.
    pub fn plan(&self, c: &Candidate, settings: &HostSettings) -> Result<Plan> {
        Ok(self.plan_all(c, settings)?.0)
    }

    /// [`plan`](Self::plan), and the dependencies' candidates (in the
    /// order of [`Plan::dependencies`]).
    fn plan_all(&self, c: &Candidate, settings: &HostSettings) -> Result<(Plan, Vec<Candidate>)> {
        let lock = Lock::load(self.dirs)?;
        let deps = self.dependencies(c, &lock)?;
        // Plugins of one install can't clash with each other either.
        let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
        let mut package_owners: BTreeMap<&str, &str> = BTreeMap::new();
        for x in deps.iter().chain([c]) {
            for exe in x.manifest.executables() {
                if let Some(other) = owners.insert(exe, &x.manifest.id).filter(|o| *o != x.manifest.id) {
                    bail!("{exe} would be provided by both the {other} and the {} plugin", x.manifest.id);
                }
            }
            for p in &x.manifest.packages {
                if let Some(other) = package_owners.insert(&p.id, &x.manifest.id).filter(|o| *o != x.manifest.id) {
                    bail!("package {} would be declared by both the {other} and the {} plugin", p.id, x.manifest.id);
                }
            }
        }
        let dependencies = deps.iter().map(|d| self.plan_one(d, settings, &lock)).collect::<Result<Vec<_>>>()?;
        let mut plan = self.plan_one(c, settings, &lock)?;
        if !dependencies.is_empty() {
            plan.dependencies = dependencies;
            plan.digest = plan.compute_digest(&c.files);
        }
        Ok((plan, deps))
    }

    /// The plugins `c` needs (`requires.plugins`, and what those need)
    /// that aren't on the host yet, each after the ones it needs.
    fn dependencies(&self, c: &Candidate, lock: &Lock) -> Result<Vec<Candidate>> {
        fn visit(p: &Plugins<'_>, m: &Manifest, lock: &Lock, path: &mut Vec<String>, out: &mut Vec<Candidate>) -> Result<()> {
            for dep in &m.requires.plugins {
                if lock.plugins.contains_key(dep) || builtins::get(dep).is_some() || out.iter().any(|c| c.manifest.id == *dep) {
                    continue;
                }
                if path.contains(dep) {
                    bail!("{} and {dep} need each other; neither can be installed first", m.name);
                }
                let c = p.find(dep, None).with_context(|| format!("{} needs the {dep} plugin", m.name))?;
                path.push(dep.clone());
                visit(p, &c.manifest, lock, path, out)?;
                path.pop();
                out.push(c);
            }
            Ok(())
        }
        let mut out = Vec::new();
        visit(self, &c.manifest, lock, &mut vec![c.manifest.id.clone()], &mut out)?;
        Ok(out)
    }

    /// The plan for `c` alone; the plugins it needs are installed, or
    /// planned along with it.
    fn plan_one(&self, c: &Candidate, settings: &HostSettings, lock: &Lock) -> Result<Plan> {
        let m = &c.manifest;
        let target = current_target();
        // A built-in plugin works with system copies anywhere; only a
        // download needs a build for this host (checked below).
        if c.builtin.is_none() {
            m.compatible(GOATTY_VERSION, &target).map_err(|why| anyhow!("{} can't be installed here: {why}", m.name))?;
        }
        if c.builtin.is_none() {
            if builtins::get(&m.id).is_some() {
                bail!("{} is the id of a plugin that comes with goatty; give this one another id", m.id);
            }
            let reserved = builtins::executables();
            let reserved_pkgs = builtins::package_ids();
            for p in &m.packages {
                if let Some(b) = reserved_pkgs.get(p.id.as_str()) {
                    bail!("package {} belongs to goatty's built-in {b} plugin; give it another id", p.id);
                }
            }
            for exe in m.executables() {
                if let Some(b) = reserved.get(exe) {
                    bail!("{exe} comes with goatty's built-in {b} plugin; a plugin can't replace it");
                }
            }
        }
        let owners = lock.executables();
        for exe in m.executables() {
            if let Some(owner) = owners.get(exe).filter(|o| **o != m.id) {
                bail!("{exe} is already provided by the {owner} plugin");
            }
            if install::link_owner(self.dirs, exe).as_deref() == Some("?") {
                bail!("{} already exists and isn't goatty's; not replacing it", self.dirs.bin_dir().join(exe).display());
            }
        }
        // Packages live in `tools/<id>/`, shared by id: a second plugin
        // with the same package id would replace or remove the first's.
        for p in &m.packages {
            if let Some(owner) = lock.package_user(&p.id, &m.id) {
                bail!("package {} is already declared by the {owner} plugin; give it another id", p.id);
            }
        }

        let path = std::env::var("PATH").ok();
        let managed_bin = self.dirs.bin_dir();
        let on_path = |exe: &str| packages::which(exe, path.as_deref(), &managed_bin);
        let managed_shell = builtins::host_managed_shell();
        let downloads = m
            .packages
            .iter()
            .filter(|p| p.applies_to_shell(managed_shell))
            .map(|p| {
                let names: Vec<String> = p.bin.iter().map(|b| manifest::bin_name(b).to_owned()).collect();
                let system = match packages::locate(self.dirs, p, path.as_deref()) {
                    Location::System(found) => Some(found.display().to_string()),
                    _ => None,
                };
                let current = packages::managed(self.dirs, p).is_some_and(|(v, _)| v == p.version);
                let a = p.asset_for(&target);
                if a.is_none() && system.is_none() && !current {
                    bail!("{} has no goatty-managed build for {target}: install it with your package manager", p.id);
                }
                Ok(Download {
                    package: p.id.clone(),
                    version: p.version.clone(),
                    url: a.map(|a| a.url.clone()).unwrap_or_default(),
                    size: a.map_or(0, |a| a.size),
                    sha256: a.map(|a| a.sha256.clone()).unwrap_or_default(),
                    bins: names,
                    script: p.source.clone(),
                    system,
                    current,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let (aliases, env) = shell_additions(m, settings);
        let files = m.files.iter().map(|f| f.name.clone()).collect();
        let init_file = if cfg!(windows) { &m.powershell.init } else { &m.zsh.init };
        let shell_code = match init_file {
            Some(init) => {
                let bytes = c.read(init)?;
                if c.files.get(init).is_none_or(|sha| goatty_plugin::bundle::sha256_hex(&bytes) != *sha) {
                    bail!("{init} doesn't match its checksum");
                }
                Some((init.clone(), String::from_utf8_lossy(&bytes).into_owned()))
            }
            None => None,
        };
        let requires = required_commands(m, &on_path);

        let mut warnings = Vec::new();
        // (One installed along with it gets turned on.)
        let installed = |d: &str| lock.plugins.contains_key(d) || builtins::get(d).is_some();
        for dep in m.requires.plugins.iter().filter(|d| installed(d) && settings.plugins.get(*d).is_some_and(|p| !p.enabled)) {
            warnings.push(format!("{} needs the {dep} plugin, which is turned off here: turn it on for {} to work.", m.name, m.name));
        }
        if c.is_local() {
            warnings.push(
                "Not from a registry: nobody reviewed this plugin and nothing signed it. Install it only if you wrote it or trust whoever did."
                    .into(),
            );
        }
        for a in &m.aliases {
            if on_path(&a.name).is_some() {
                if cfg!(windows) {
                    warnings.push(format!(
                        "`{}` is an existing command; in goatty shells the alias replaces it (turn it off in the plugin's settings, or remove the function in %APPDATA%\\goatty\\profile.ps1).",
                        a.name
                    ));
                } else {
                    warnings.push(format!(
                        "`{}` is an existing command; in goatty shells the alias replaces it (turn it off in the plugin's settings, or `unalias {}` in ~/.config/goatty/zshrc).",
                        a.name, a.name
                    ));
                }
            }
        }
        let other_aliases: BTreeMap<String, String> = lock
            .plugins
            .iter()
            .filter(|(id, _)| **id != m.id)
            .filter_map(|(id, e)| lock::manifest_of(self.dirs, id, e).ok().map(|om| (id.clone(), om)))
            .flat_map(|(id, om)| om.aliases.into_iter().map(move |a| (a.name, id.clone())))
            .collect();
        for a in &m.aliases {
            if let Some(other) = other_aliases.get(&a.name) {
                warnings.push(format!("The {other} plugin also defines `{}`; the one loaded last wins.", a.name));
            }
        }
        let replaces = match c.builtin {
            Some(_) => None,
            None => lock.plugins.get(&m.id).map(|e| e.version.clone()),
        };
        let mut plan = Plan {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.clone(),
            summary: m.summary.clone(),
            description: m.description.clone(),
            publisher: m.publisher.clone(),
            homepage: m.homepage.clone(),
            license: m.license.clone(),
            source: c.source.clone(),
            verified: c.verified,
            builtin: c.builtin.is_some(),
            replaces,
            downloads,
            permissions: permissions::describe(m),
            aliases,
            env,
            files,
            shell_code,
            requires,
            warnings,
            dependencies: Vec::new(),
            digest: String::new(),
            previous: None,
        };
        plan.digest = plan.compute_digest(&c.files);
        // Not in the digest: what the update replaces, to show only what changes.
        if plan.replaces.is_some() {
            plan.previous = self.previous(&m.id, lock, settings, &on_path).map(Box::new);
        }
        Ok(plan)
    }

    /// The installed version of `id`, as [`Plan`] items, for an update's
    /// review. `None` when it can't be read (the review then shows
    /// everything).
    fn previous(&self, id: &str, lock: &Lock, settings: &HostSettings, on_path: &dyn Fn(&str) -> Option<PathBuf>) -> Option<Previous> {
        let e = lock.plugins.get(id)?;
        let m = lock::manifest_of(self.dirs, id, e).map_err(|err| log::info!("plan: can't read installed {id}: {err:#}")).ok()?;
        let init_file = if cfg!(windows) { &m.powershell.init } else { &m.zsh.init };
        let shell_code = match init_file {
            Some(init) => {
                let bytes = lock::read_checked(self.dirs, id, e, init).ok()?;
                Some((init.clone(), String::from_utf8_lossy(&bytes).into_owned()))
            }
            None => None,
        };
        let (aliases, env) = shell_additions(&m, settings);
        Some(Previous {
            version: e.version.clone(),
            source: e.source.clone(),
            verified: e.verified,
            packages: e
                .packages
                .iter()
                .map(|p| {
                    let asset = m.packages.iter().find(|mp| mp.id == p.id).and_then(|mp| mp.asset_for(&current_target()));
                    PreviousPackage {
                        id: p.id.clone(),
                        version: p.version.clone(),
                        url: asset.map(|a| a.url.clone()).unwrap_or_default(),
                        sha256: asset.map(|a| a.sha256.clone()).unwrap_or_default(),
                    }
                })
                .collect(),
            permissions: permissions::describe(&m),
            aliases,
            env,
            files: m.files.iter().map(|f| f.name.clone()).collect(),
            shell_code,
            requires: required_commands(&m, on_path),
        })
    }

    /// Install `c` if its plan still has the digest the user approved.
    /// Returns the plan that was carried out. The caller enables the plugin
    /// in the host settings and builds the shell configuration.
    pub fn install(&self, c: &Candidate, approved: &str, settings: &HostSettings) -> Result<Plan> {
        self.install_with(c, approved, settings, &Job::untracked())
    }

    /// [`install`](Self::install), reporting its progress to `job` and
    /// stopping (with everything as it was) when `job` is cancelled while
    /// it waits or downloads: nothing on the host changes before every
    /// download (the dependencies' too) is in. Leaves `job` at
    /// [`Step::Activating`].
    pub fn install_with(&self, c: &Candidate, approved: &str, settings: &HostSettings, job: &Job) -> Result<Plan> {
        job.step(Step::Waiting)?;
        let _guard = op_lock_for(self.dirs, job)?;
        job.check()?;
        let (plan, deps) = self.plan_all(c, settings)?;
        if plan.digest != approved {
            bail!("{} changed since you reviewed it (files, downloads or permissions); review the new plan before installing", plan.name);
        }
        let todo = deps.iter().zip(&plan.dependencies).map(|(c, p)| (c, p, true)).chain([(c, &plan, false)]);
        // Everything is downloaded before anything changes, so a cancel
        // (or a failed download) leaves the host as it was.
        let count = plan.all().flat_map(|p| &p.downloads).filter(|d| d.fetches()).count();
        let mut index = 0;
        let mut staged = Vec::new();
        for (c, p, dependency) in todo {
            staged.push(self.stage(c, p, dependency, job, &mut index, count)?);
        }
        job.activate()?;
        // Dependencies first: if a later one fails, the ones before it
        // are complete installs of their own.
        for s in staged {
            self.commit(s)?;
        }
        Ok(plan)
    }

    /// Download `c`'s packages (counting them in `index` of `count`) and
    /// stage its files: everything of its install that can be cancelled.
    fn stage<'c>(
        &self,
        c: &'c Candidate,
        plan: &'c Plan,
        dependency: bool,
        job: &Job,
        index: &mut usize,
        count: usize,
    ) -> Result<Staged<'c>> {
        let m = &c.manifest;
        let target = current_target();
        let staging = self.dirs.installed_dir().join(format!(".{}.partial-{}", m.id, std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        let mut spool = Spool(vec![staging.clone()]);
        let managed_shell = builtins::host_managed_shell();
        let mut fetched: Vec<Option<PathBuf>> = Vec::new();
        for (p, d) in m.packages.iter().filter(|p| p.applies_to_shell(managed_shell)).zip(&plan.downloads) {
            if !d.fetches() {
                fetched.push(None);
                continue;
            }
            let a = p.asset_for(&target).context("no build for this host")?;
            *index += 1;
            job.step(Step::Downloading { package: p.id.clone(), index: *index, count, bytes: 0, total: a.size })?;
            let bytes = self.fetch.fetch_progress(&a.url, &mut |n| job.downloaded(n)).with_context(|| format!("download {}", a.url))?;
            let file = self.dirs.installed_dir().join(format!(".{}.download-{}-{index}", m.id, std::process::id()));
            spool.0.push(file.clone());
            std::fs::write(&file, &bytes).with_context(|| format!("keep the download of {} in {}", p.id, file.display()))?;
            fetched.push(Some(file));
        }
        if c.builtin.is_none() {
            // Stage the plugin's files too: the last step that can fail on its own.
            goatty_plugin::bundle::copy_verified(&c.dir, &staging, &c.files)?;
        }
        Ok(Staged { c, plan, dependency, fetched, staging, _spool: spool })
    }

    /// Activate a staged plugin: its packages, its files, its lock entry.
    fn commit(&self, s: Staged<'_>) -> Result<()> {
        let Staged { c, plan, dependency, fetched, staging, _spool } = s;
        let m = &c.manifest;
        let target = current_target();
        let mut lock = Lock::load(self.dirs)?;
        let previous = lock.plugins.get(&m.id).cloned();
        let mut packages = Vec::new();
        let mut fresh: Vec<(String, Vec<String>)> = Vec::new();
        let managed_shell = builtins::host_managed_shell();
        let result = (|| -> Result<()> {
            for ((p, d), bytes) in m.packages.iter().filter(|p| p.applies_to_shell(managed_shell)).zip(&plan.downloads).zip(fetched) {
                if let Some(sys) = &d.system {
                    packages.push(lock::Package {
                        id: p.id.clone(),
                        version: p.version.clone(),
                        bins: p.bin.clone(),
                        system: Some(sys.clone()),
                    });
                    continue;
                }
                if let Some(file) = bytes {
                    let bytes = std::fs::read(&file).with_context(|| format!("read the download of {}", p.id))?;
                    let a = p.asset_for(&target).context("no build for this host")?;
                    let pi = PackageInstall {
                        id: &p.id,
                        version: &p.version,
                        sha256: &a.sha256,
                        format: a.format,
                        root: &a.root,
                        bins: &p.bin,
                        source: p.source.as_deref(),
                        completions: &p.completions,
                    };
                    install::install_package(&pi, &bytes, self.dirs).with_context(|| format!("install {} {}", p.id, p.version))?;
                    fresh.push((p.id.clone(), p.bin.clone()));
                }
                packages.push(lock::Package { id: p.id.clone(), version: p.version.clone(), bins: p.bin.clone(), system: None });
            }
            if c.builtin.is_some() {
                // Its files are part of goatty: nothing to copy.
                return Ok(());
            }

            // The plugin's files (staged and checked) swapped in.
            let dest = lock::bundle_dir(self.dirs, &m.id);
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            std::fs::rename(&staging, &dest)?;
            Ok(())
        })();
        if let Err(e) = result {
            // Undo packages this attempt added that the old version didn't have.
            let keep: Vec<&str> = previous.iter().flat_map(|p| p.packages.iter().map(|x| x.id.as_str())).collect();
            for (id, bins) in fresh.iter().filter(|(id, _)| !keep.contains(&id.as_str())) {
                let _ = self.drop_package(&lock, &m.id, id, bins);
            }
            return Err(e);
        }
        if c.builtin.is_some() {
            log::info!("installed the packages of built-in plugin {}", m.id);
            return Ok(());
        }
        // Packages the old version had and the new one doesn't.
        if let Some(old) = &previous {
            for p in old.packages.iter().filter(|p| p.system.is_none() && !packages.iter().any(|n| n.id == p.id)) {
                let _ = self.drop_package(&lock, &m.id, &p.id, &p.bins);
            }
        }
        lock.plugins.insert(
            m.id.clone(),
            lock::Entry {
                version: m.version.clone(),
                source: if c.is_local() { "local".into() } else { c.source.clone() },
                path: c.is_local().then(|| c.source.clone()),
                verified: c.verified,
                files: c.files.clone(),
                packages,
                permissions: permissions::all(m),
                digest: plan.digest.clone(),
                installed: now(),
                // An update keeps what it was installed as; installing the
                // same version again on purpose makes it the user's own.
                dependency: match &previous {
                    Some(p) if p.version != m.version => p.dependency,
                    _ => dependency,
                },
            },
        );
        lock.save(self.dirs)?;
        log::info!(
            "installed plugin {} {} from {}{}",
            m.id,
            m.version,
            c.source,
            if dependency { " (needed by another plugin)" } else { "" }
        );
        Ok(())
    }

    /// Remove an installed plugin: its packages, files and lock entry.
    /// The caller drops it from the host settings. Returns its entry and
    /// the plugins nothing needs any more ([`Plugins::unneeded`]), to
    /// offer removing them too.
    pub fn uninstall(&self, id: &str) -> Result<(lock::Entry, Vec<String>)> {
        if let Some(b) = builtins::get(id) {
            bail!(
                "{} comes with goatty: turn it off instead (goattyd plugin disable {id}); `goattyd shell uninstall {id}` removes goatty's download of it",
                b.manifest().name
            );
        }
        let _guard = op_lock(self.dirs)?;
        let mut lock = Lock::load(self.dirs)?;
        let entry = lock.plugins.remove(id).with_context(|| format!("{id} isn't installed"))?;
        for (other, e) in &lock.plugins {
            if let Ok(m) = lock::manifest_of(self.dirs, other, e)
                && m.requires.plugins.iter().any(|d| d == id)
            {
                bail!("the {other} plugin needs {id}; uninstall {other} first");
            }
        }
        for p in entry.packages.iter().filter(|p| p.system.is_none()) {
            self.drop_package(&lock, id, &p.id, &p.bins)?;
        }
        let dir = lock::bundle_dir(self.dirs, id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        }
        lock.save(self.dirs)?;
        log::info!("uninstalled plugin {id}");
        Ok((entry, self.unneeded(&lock)))
    }

    /// Remove plugin `owner`'s use of goatty's download of package `id`
    /// (its executables `bins`): the whole download, or only the
    /// executables no other plugin lists when another installed plugin
    /// still uses the download (#222). A plugin that uses a system copy
    /// doesn't keep the download.
    fn drop_package(&self, lock: &Lock, owner: &str, id: &str, bins: &[String]) -> Result<bool> {
        let others: Vec<&lock::Package> = lock
            .plugins
            .iter()
            .filter(|(plugin, _)| plugin.as_str() != owner)
            .flat_map(|(_, e)| e.packages.iter().filter(|p| p.id == id && p.system.is_none()))
            .collect();
        if others.is_empty() {
            return install::uninstall_package(id, bins, self.dirs);
        }
        let kept: BTreeSet<&str> = others.iter().flat_map(|p| p.bins.iter().map(|b| manifest::bin_name(b))).collect();
        let gone: Vec<String> = bins.iter().filter(|b| !kept.contains(manifest::bin_name(b))).cloned().collect();
        install::remove_links(id, &gone, self.dirs)
    }

    /// The plugins that were installed only because another plugin needed
    /// them, and that no other installed plugin needs now; ordered so each
    /// comes before the ones it needs (the order they can be uninstalled in).
    fn unneeded(&self, lock: &Lock) -> Vec<String> {
        let needs: BTreeMap<&str, Vec<String>> = lock
            .plugins
            .iter()
            .map(|(id, e)| (id.as_str(), lock::manifest_of(self.dirs, id, e).map(|m| m.requires.plugins).unwrap_or_default()))
            .collect();
        let needed_by = |dep: &str, from: &dyn Fn(&str) -> bool| needs.iter().any(|(o, reqs)| from(o) && reqs.iter().any(|r| r == dep));
        let mut set: BTreeSet<&str> = lock.plugins.iter().filter(|(_, e)| e.dependency).map(|(id, _)| id.as_str()).collect();
        // Drop the ones a plugin outside the set needs, until none is left.
        loop {
            let kept: Vec<&str> = set.iter().copied().filter(|d| needed_by(d, &|o| !set.contains(o))).collect();
            if kept.is_empty() {
                break;
            }
            for k in kept {
                set.remove(k);
            }
        }
        let mut out = Vec::new();
        while let Some(&first) = set.iter().next() {
            let next = set.iter().copied().find(|d| !needed_by(d, &|o| o != *d && set.contains(o))).unwrap_or(first);
            set.remove(next);
            out.push(next.to_owned());
        }
        out
    }

    /// Remove goatty's downloaded packages of a built-in plugin (the plugin
    /// stays; system copies are used if there are any). Returns the
    /// packages that were removed.
    pub fn remove_packages(&self, b: &Builtin) -> Result<Vec<String>> {
        let _guard = op_lock(self.dirs)?;
        let mut removed = Vec::new();
        for p in &b.manifest().packages {
            if install::uninstall_package(&p.id, &p.bin, self.dirs)? {
                removed.push(p.id.clone());
            }
        }
        Ok(removed)
    }

    /// Remove goatty's download of a package that used to come with
    /// goatty ([`builtins::RETIRED`]), unless an installed plugin uses it
    /// now. `Ok(None)`: an installed plugin owns it (its id); `Ok(Some(removed))`
    /// otherwise.
    pub fn remove_retired(&self, package: &str, bins: &[&str]) -> Result<Option<bool>> {
        let _guard = op_lock(self.dirs)?;
        let lock = Lock::load(self.dirs)?;
        if lock.plugins.values().any(|e| e.packages.iter().any(|p| p.id == package)) {
            return Ok(None);
        }
        let bins: Vec<String> = bins.iter().map(|b| (*b).to_owned()).collect();
        install::uninstall_package(package, &bins, self.dirs).map(Some)
    }

    /// Installed plugins (built-ins and lock entries) and any problems reading
    /// the lock or installed manifests, without reading or refreshing any
    /// registry snapshot.
    pub fn installed(&self, settings: &HostSettings) -> (Vec<Installed>, Vec<String>) {
        let path = std::env::var("PATH").ok();
        self.installed_with_env_path(settings, path.as_deref())
    }

    pub(crate) fn installed_with_env_path(&self, settings: &HostSettings, env_path: Option<&str>) -> (Vec<Installed>, Vec<String>) {
        let mut issues = Vec::new();
        let lock = self.load_lock(&mut issues);
        let installed = self.installed_entries(&lock, settings, env_path, &[], &mut issues);
        (installed, issues)
    }

    /// Installed plugins and what the registries offer.
    pub fn catalog(&self, refresh: bool, settings: &HostSettings) -> Catalog {
        let path = std::env::var("PATH").ok();
        self.catalog_with_env_path(refresh, settings, path.as_deref())
    }

    pub(crate) fn catalog_with_env_path(&self, refresh: bool, settings: &HostSettings, env_path: Option<&str>) -> Catalog {
        let (snaps, registries, mut issues) = self.snapshots(refresh, true);
        let lock = self.load_lock(&mut issues);
        let installed = self.installed_entries(&lock, settings, env_path, &snaps, &mut issues);
        let target = current_target();
        let mut available: Vec<Available> = Vec::new();
        for s in &snaps {
            for (id, entry) in &s.index.plugins {
                if available.iter().any(|a| a.id == *id) || builtins::get(id).is_some() {
                    continue;
                }
                let Some((v, ver)) = s.index.latest(id) else { continue };
                let incompatible = if !ver.targets.is_empty() && !ver.targets.iter().any(|t| *t == target || t == "any") {
                    Some(format!("no build for {target}"))
                } else if !ver.goatty.is_empty()
                    && semver::VersionReq::parse(&ver.goatty)
                        .is_ok_and(|r| !r.matches(&semver::Version::parse(GOATTY_VERSION).expect("semver")))
                {
                    Some(format!("needs goatty {}", ver.goatty))
                } else {
                    None
                };
                available.push(Available {
                    id: id.clone(),
                    name: entry.name.clone(),
                    summary: entry.summary.clone(),
                    publisher: entry.publisher.clone(),
                    category: entry.category,
                    keywords: entry.keywords.clone(),
                    homepage: entry.homepage.clone(),
                    registry: s.source.name.clone(),
                    version: v.to_owned(),
                    permissions: ver.permissions.clone(),
                    incompatible,
                    installed: lock.plugins.get(id).map(|e| e.version.clone()),
                });
            }
        }
        available.sort_by_key(|a| a.name.to_lowercase());
        issues.extend(active(self.dirs, settings).1);
        Catalog { installed, available, registries, issues }
    }

    fn load_lock(&self, issues: &mut Vec<String>) -> Lock {
        match Lock::load(self.dirs) {
            Ok(l) => l,
            Err(e) => {
                issues.push(format!("{e:#}"));
                Lock::default()
            }
        }
    }

    fn installed_entries(
        &self,
        lock: &Lock,
        settings: &HostSettings,
        env_path: Option<&str>,
        snaps: &[Snapshot],
        issues: &mut Vec<String>,
    ) -> Vec<Installed> {
        /// zsh builtins that run in the shell without an external binary on PATH.
        const ZSH_BUILTINS: &[&str] = &[
            "alias", "autoload", "bg", "bindkey", "break", "builtin", "cd", "command", "continue", "declare", "dirs", "echo", "emulate",
            "eval", "exec", "exit", "export", "false", "fc", "fg", "float", "getopts", "history", "integer", "jobs", "kill", "let",
            "local", "noglob", "popd", "print", "printf", "pushd", "pwd", "r", "read", "readonly", "return", "set", "setopt", "shift",
            "source", "test", "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias", "unset", "unsetopt", "wait",
            "whence", "where", "which", "zcompile", "zmodload", "zstyle",
        ];

        /// PowerShell builtins/cmdlets/aliases that run in the shell without an external binary on PATH.
        const PS_BUILTINS: &[&str] = &[
            "cat",
            "cd",
            "clear",
            "cls",
            "copy",
            "cp",
            "del",
            "dir",
            "echo",
            "exit",
            "foreach-object",
            "gci",
            "get-childitem",
            "get-command",
            "get-content",
            "get-help",
            "get-location",
            "get-process",
            "kill",
            "ls",
            "measure-object",
            "mkdir",
            "move",
            "mv",
            "pop-location",
            "ps",
            "push-location",
            "pwd",
            "remove-item",
            "rm",
            "select-object",
            "select-string",
            "set-location",
            "sl",
            "sort-object",
            "stop-process",
            "type",
            "where-object",
            "write-output",
        ];

        let target = current_target();
        let managed_shell = builtins::host_managed_shell();
        let is_shell_builtin = |prog: &str| {
            if cfg!(windows) { PS_BUILTINS.iter().any(|b| b.eq_ignore_ascii_case(prog)) } else { ZSH_BUILTINS.contains(&prog) }
        };
        let managed_path = std::cell::OnceCell::new();
        let mut found: BTreeMap<String, bool> = BTreeMap::new();
        let mut unavailable_of = |m: &Manifest| -> Vec<String> {
            if m.commands.is_empty() {
                return Vec::new();
            }
            let stored = settings.plugins.get(&m.id).map(|c| c.settings.clone()).unwrap_or_default();
            let (values, _) = schema::resolve(m, &stored);
            m.commands
                .iter()
                .filter(|c| {
                    // A command that sends keys runs nothing on the host.
                    !manifest::applies(&c.when, &values)
                        || (c.keys.is_empty()
                            && c.run.first().is_none_or(|prog| {
                                if !c.open_in.launches_direct() && is_shell_builtin(prog.as_str()) {
                                    false
                                } else {
                                    !*found.entry(prog.clone()).or_insert_with(|| {
                                        let p =
                                            managed_path.get_or_init(|| crate::snippets::managed_path_with(self.dirs, settings, env_path));
                                        crate::snippets::find_in(p, prog).is_some()
                                    })
                                }
                            }))
                })
                .map(|c| c.id.clone())
                .collect()
        };
        let package_status = |m: &Manifest| -> Vec<PackageStatus> {
            m.packages
                .iter()
                .filter(|p| p.applies_to_shell(managed_shell))
                .map(|p| PackageStatus {
                    id: p.id.clone(),
                    version: p.version.clone(),
                    state: match packages::locate(self.dirs, p, env_path) {
                        Location::System(found) => PackageState::System { path: found.display().to_string() },
                        Location::Managed { version, .. } => PackageState::Managed { version },
                        Location::Missing => PackageState::Missing,
                    },
                    available: p.asset_for(&target).is_some(),
                })
                .collect()
        };
        let mut installed = Vec::new();
        for b in builtins::BUILTINS {
            let m = b.manifest().clone();
            installed.push(Installed {
                id: b.id.into(),
                version: m.version.clone(),
                source: builtins::SOURCE.into(),
                verified: true,
                enabled: settings.plugins.get(b.id).is_none_or(|c| c.enabled),
                incompatible: None,
                packages: package_status(&m),
                unavailable_commands: unavailable_of(&m),
                command_open_in: Installed::command_open_in_map(&m),
                builtin: true,
                manifest: m,
                update: None,
                installed_unix: 0,
            });
        }
        for (id, e) in lock.plugins.iter().filter(|(id, _)| builtins::get(id).is_none()) {
            let m = match lock::manifest_of(self.dirs, id, e) {
                Ok(m) => m,
                Err(err) => {
                    issues.push(format!("{err:#}"));
                    continue;
                }
            };
            let update = snaps
                .iter()
                .find(|s| s.source.name == e.source)
                .and_then(|s| s.index.latest(id))
                .filter(|(v, _)| newer(v, &e.version))
                .map(|(v, _)| v.to_owned());
            installed.push(Installed {
                id: id.clone(),
                version: e.version.clone(),
                source: e.source.clone(),
                verified: e.verified,
                enabled: settings.plugins.get(id).is_none_or(|c| c.enabled),
                incompatible: m.compatible(GOATTY_VERSION, &target).err(),
                packages: package_status(&m),
                unavailable_commands: unavailable_of(&m),
                command_open_in: Installed::command_open_in_map(&m),
                builtin: false,
                manifest: m,
                update,
                installed_unix: e.installed,
            });
        }
        installed
    }
}

/// A plugin's aliases and variables as the plan shows them, with the
/// current settings.
fn shell_additions(m: &Manifest, settings: &HostSettings) -> (Vec<String>, Vec<String>) {
    let stored = settings.plugins.get(&m.id).map(|c| c.settings.clone()).unwrap_or_default();
    let (values, _) = schema::resolve(m, &stored);
    let aliases = m
        .aliases
        .iter()
        .map(|a| {
            let mut s = format!("{} → {}", a.name, goatty_plugin::alias::expansion(m, a, &values));
            if let Some(w) = &a.when {
                let on = manifest::applies(&a.when, &values);
                let when = goatty_plugin::alias::describe(m, w);
                s.push_str(&format!("  (while {when}: {})", if on { "on now" } else { "off now" }));
            }
            s
        })
        .collect();
    let env = m
        .env
        .iter()
        .map(|e| {
            let value = template::expand_inline(&e.value, m, &values, None);
            let mut s = if value.is_empty() { format!("{} (not set while empty)", e.name) } else { format!("{}={value}", e.name) };
            if let Some(w) = &e.when {
                s.push_str(&format!("  (when {w}: {})", if manifest::applies(&e.when, &values) { "on now" } else { "off now" }));
            }
            s
        })
        .chain(m.files.iter().filter_map(|f| f.env.as_ref().map(|v| format!("{v}=<goatty's generated {} file>", f.name))))
        .collect();
    (aliases, env)
}

/// The commands a plugin needs that goatty won't install, and whether
/// they're here.
fn required_commands(m: &Manifest, on_path: &dyn Fn(&str) -> Option<PathBuf>) -> Vec<String> {
    m.requires
        .commands
        .iter()
        .map(|cmd| match on_path(cmd) {
            Some(p) => format!("{cmd}: found at {}", p.display()),
            None => format!("{cmd}: not installed here (goatty doesn't install it); the integration stays off until it is"),
        })
        .collect()
}

impl Plugins<'_> {
    /// Installed plugins with a newer version in the registry they came
    /// from. Registries older than a day are fetched first (`refresh_stale`).
    pub fn updates(&self, refresh_stale: bool) -> Vec<Update> {
        let lock = Lock::load(self.dirs).unwrap_or_default();
        if !lock.plugins.values().any(|e| e.verified) {
            // Nothing from a registry: nothing to ask the network about.
            return Vec::new();
        }
        let (snaps, _, _) = self.snapshots(false, refresh_stale);
        let mut out = Vec::new();
        for (id, e) in lock.plugins.iter().filter(|(id, _)| builtins::get(id).is_none()) {
            let Some(snap) = snaps.iter().find(|s| s.source.name == e.source) else { continue };
            let Some((v, ver)) = snap.index.latest(id).filter(|(v, _)| newer(v, &e.version)) else { continue };
            let name = snap.index.plugins.get(id).map_or_else(|| id.clone(), |p| p.name.clone());
            out.push(Update {
                id: id.clone(),
                name,
                installed: e.version.clone(),
                version: v.to_owned(),
                new_permissions: ver.permissions.iter().filter(|p| !e.permissions.contains(p)).copied().collect(),
            });
        }
        out
    }
}

/// `a` is a newer semver version than `b`.
pub fn newer(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

/// An installed (or built-in), enabled, usable plugin, ready to render.
#[derive(Debug, Clone)]
pub struct Active {
    pub manifest: Manifest,
    pub values: Values,
    pub origin: Origin,
    /// `eza 1.0.0 (official)`.
    pub label: String,
}

/// Where an active plugin's files come from.
#[derive(Debug, Clone)]
pub enum Origin {
    /// Compiled into goatty.
    Builtin(&'static Builtin),
    /// `installed/<id>/`, checked against this lock entry.
    Installed(lock::Entry),
}

impl Active {
    /// One of the plugin's files; an installed plugin's is checked against the lock.
    pub fn read(&self, dirs: &Dirs, rel: &str) -> Result<Vec<u8>> {
        match &self.origin {
            Origin::Builtin(b) => b.read(rel).map(|t| t.as_bytes().to_vec()).with_context(|| format!("{}: no {rel}", b.id)),
            Origin::Installed(entry) => lock::read_checked(dirs, &self.manifest.id, entry, rel),
        }
    }

    /// Every file of the plugin (relative paths).
    pub fn file_names(&self) -> Vec<String> {
        match &self.origin {
            Origin::Builtin(b) => b.checksums().into_keys().collect(),
            Origin::Installed(entry) => entry.files.keys().cloned().collect(),
        }
    }

    /// The files the shell sources on this host: the init file and (on zsh)
    /// the `fpath` directories' contents.
    pub fn shell_files(&self) -> Vec<String> {
        self.shell_files_for(builtins::host_managed_shell())
    }

    /// The files `shell` (`"zsh"` or `"powershell"`) sources from this plugin.
    pub fn shell_files_for(&self, shell: &str) -> Vec<String> {
        let m = &self.manifest;
        if shell == "powershell" {
            return m.powershell.init.iter().cloned().collect();
        }
        let mut v: Vec<String> = m.zsh.init.iter().cloned().collect();
        v.extend(self.file_names().into_iter().filter(|f| m.zsh.fpath.iter().any(|d| f.starts_with(&format!("{d}/")))));
        v
    }

    /// Its key in `goatty-status`: the bare id for built-in plugins
    /// (`atuin`), `plugin:<id>` for the others.
    pub fn feature(&self) -> String {
        match self.origin {
            Origin::Builtin(_) => self.manifest.id.clone(),
            Origin::Installed(_) => goatty_plugin::zsh::feature(&self.manifest.id),
        }
    }
}

/// The plugins that go into the shell configuration for `settings`, in
/// load order ([`load_order`]), and the problems found on the way (bad
/// values, missing or changed files, settings for plugins that aren't
/// installed).
pub fn active(dirs: &Dirs, settings: &HostSettings) -> (Vec<Active>, Vec<String>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let resolve = |m: &mut Manifest, issues: &mut Vec<String>| {
        let config = settings.plugins.get(&m.id);
        let stored = config.map(|c| c.settings.clone()).unwrap_or_default();
        let (values, problems) = schema::resolve(m, &stored);
        for (k, msg) in problems {
            issues.push(format!("plugins.{}.settings.{k}: {msg}", m.id));
        }
        // Aliases the user switched off stay out of the shell (#88).
        if let Some(off) = config.map(|c| &c.disabled_aliases).filter(|d| !d.is_empty()) {
            m.aliases.retain(|a| !off.contains(&a.name));
        }
        values
    };
    let enabled = |id: &str| settings.plugins.get(id).is_none_or(|c| c.enabled);
    for b in builtins::BUILTINS.iter().filter(|b| enabled(b.id)) {
        let mut m = b.manifest().clone();
        let values = resolve(&mut m, &mut issues);
        out.push(Active { manifest: m, values, origin: Origin::Builtin(b), label: b.label() });
    }
    let lock = match Lock::load(dirs) {
        Ok(l) => l,
        Err(e) => {
            issues.push(format!("{e:#}"));
            return (load_order(out), issues);
        }
    };
    for id in settings.plugins.keys().filter(|id| !lock.plugins.contains_key(*id) && builtins::get(id).is_none()) {
        issues.push(format!("plugins.{id}: not installed on this host; ignored (goattyd plugin install {id})"));
    }
    let target = current_target();
    for (id, entry) in lock.plugins.iter().filter(|(id, _)| builtins::get(id).is_none() && enabled(id)) {
        let mut m = match lock::manifest_of(dirs, id, entry) {
            Ok(m) => m,
            Err(e) => {
                issues.push(format!("{e:#}"));
                continue;
            }
        };
        if let Err(why) = m.compatible(GOATTY_VERSION, &target) {
            issues.push(format!("plugin {id} is off: {why}"));
            continue;
        }
        let label = format!("{} {} ({})", m.id, m.version, entry.source);
        let a = Active { manifest: m.clone(), values: Values::new(), origin: Origin::Installed(entry.clone()), label };
        // Everything the shell will source must still be what was approved.
        if let Some(e) = a.shell_files().iter().find_map(|rel| a.read(dirs, rel).err()) {
            issues.push(format!("plugin {id} is off: {e:#}"));
            continue;
        }
        let values = resolve(&mut m, &mut issues);
        out.push(Active { manifest: m, values, ..a });
    }
    (load_order(out), issues)
}

/// Sort plugins into the order they load: by [`Load`] slot, then within a
/// slot each after the plugins its `after` list names (when they're there),
/// otherwise in the order given (built-in plugins first, then by id). A
/// cycle is broken by taking the first remaining plugin.
pub fn load_order(list: Vec<Active>) -> Vec<Active> {
    fn slot_of(m: &Manifest) -> Load {
        if cfg!(windows) { goatty_plugin::powershell::load_slot(m) } else { m.zsh.load }
    }
    fn after_of(m: &Manifest) -> &[String] {
        if cfg!(windows) { goatty_plugin::powershell::after_list(m) } else { &m.zsh.after }
    }
    let mut out = Vec::with_capacity(list.len());
    let mut rest = list;
    for slot in Load::ALL {
        let (mut left, others): (Vec<Active>, Vec<Active>) = rest.into_iter().partition(|a| slot_of(&a.manifest) == slot);
        rest = others;
        while !left.is_empty() {
            let ready =
                left.iter().position(|a| !after_of(&a.manifest).iter().any(|dep| left.iter().any(|o| o.manifest.id == *dep))).unwrap_or(0);
            out.push(left.remove(ready));
        }
    }
    out
}

#[cfg(all(test, unix))]
pub(crate) mod tests;
