//! Host settings operations: read, validate + preview, apply (build →
//! activate → save), generations, rollback. goattyd serves these over the
//! protocol; `goatty --local` (no daemon) calls them directly.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use goatty_config::{Effective, HostSettings, store};
use goatty_plugin::api::{self, Changed};
use goatty_plugin::fetch::Fetch;

use crate::generation::{self, FileDiff, Generation};
use crate::launch::Launcher;
use crate::plugins::jobs::Jobs;
use crate::plugins::registry::Source;
use crate::plugins::{Candidate, Plugins};

/// The host settings as stored, for an editor.
#[derive(Debug, Clone)]
pub struct HostSnapshot {
    /// The settings document (non-default values only).
    pub json: String,
    pub issues: Vec<String>,
    pub env_overrides: Vec<String>,
    pub generation: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub issues: Vec<String>,
    /// Generated-file changes relative to the active generation, computed
    /// before applying.
    pub diff: Vec<FileDiff>,
    pub applied: bool,
    pub generation: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct HostService {
    pub launcher: Launcher,
    /// `None` without `$HOME`.
    pub host_file: Option<PathBuf>,
    /// Running managed shells (pids), for [`HostService::reload_shells`].
    /// Shared between clones.
    shells: Arc<Mutex<BTreeSet<u32>>>,
    /// Downloads for plugins (curl; fixed responses in tests).
    fetch: Arc<dyn Fetch>,
    /// Built-in plugin registries (the official one).
    registries: Vec<Source>,
    /// Plugin installs, running and recently finished. Shared between clones.
    installs: Jobs,
}

impl HostService {
    pub fn new(launcher: Launcher, host_file: Option<PathBuf>) -> Self {
        let cache = launcher.dirs().map(|d| d.cache.clone()).unwrap_or_else(std::env::temp_dir);
        Self {
            launcher,
            host_file,
            shells: Arc::default(),
            fetch: Arc::new(goatty_plugin::fetch::Curl { cache }),
            registries: vec![Source::official()],
            installs: Jobs::default(),
        }
    }

    /// Use other downloads and built-in registries (tests, sandboxes).
    pub fn with_plugin_sources(mut self, fetch: Arc<dyn Fetch>, registries: Vec<Source>) -> Self {
        self.fetch = fetch;
        self.registries = registries;
        self
    }

    /// Run `f` with this host's plugin operations.
    pub fn plugins<R>(&self, f: impl FnOnce(&Plugins<'_>) -> Result<R>) -> Result<R> {
        let dirs = self.launcher.dirs().context("$HOME is not set, so goatty has nowhere to keep plugins")?;
        f(&Plugins { dirs, fetch: self.fetch.as_ref(), builtin: &self.registries })
    }

    /// This process's host: XDG dirs, `goatty_config::host_file()`.
    pub fn from_env(daemon_exe: Option<PathBuf>) -> Self {
        Self::new(Launcher::from_env(daemon_exe), goatty_config::host_file())
    }

    /// The settings as of now: the file plus the environment.
    pub fn settings(&self) -> Effective<HostSettings> {
        let file = self.host_file.as_deref().map(store::load).unwrap_or_default();
        goatty_config::with_host_env(file)
    }

    pub fn active_generation(&self) -> Option<u32> {
        self.launcher.dirs().and_then(generation::current)
    }

    /// Problems only this host can see: plugins' settings against their
    /// schemas, a shell program that isn't installed.
    fn environment_issues(&self, value: &HostSettings) -> Vec<String> {
        let mut issues = Vec::new();
        if let Some(dirs) = self.launcher.dirs() {
            issues.extend(crate::plugins::active(dirs, value).1);
        }
        issues.extend(crate::launch::program_issue(value.shell.program.as_deref()).map(|i| i.to_string()));
        issues
    }

    pub fn snapshot(&self) -> HostSnapshot {
        let host = self.settings();
        let mut issues: Vec<String> = host.file.issues.iter().map(ToString::to_string).collect();
        issues.extend(self.environment_issues(&host.value));
        HostSnapshot {
            json: store::to_json(&host.file.value, &host.file.unknown),
            issues,
            env_overrides: host.env_overrides.iter().map(|s| s.to_string()).collect(),
            generation: self.active_generation(),
        }
    }

    /// Validate `json` and compute the diff; with `apply`, build and
    /// activate the generation, then save the file. A configuration that
    /// fails validation is rejected before anything is written.
    pub fn set(&self, json: &str, apply: bool) -> Result<ApplyOutcome> {
        let path = self.host_file.as_deref().context("no $HOME on this host: nowhere to store settings")?;
        let loaded = store::parse::<HostSettings>(json);
        let mut issues: Vec<String> = loaded.issues.iter().map(ToString::to_string).collect();
        let host = goatty_config::with_host_env(loaded);
        issues.extend(self.environment_issues(&host.value));
        let diff = self.launcher.preview(&host.value);
        if !apply {
            return Ok(ApplyOutcome { issues, diff, applied: false, generation: self.active_generation() });
        }
        let activated = self.launcher.apply(&host).context("not applied")?;
        store::save(path, &host.file.value, &host.file.unknown)
            .with_context(|| format!("built generation {} but couldn't save {}", activated.generation, path.display()))?;
        log::info!("host settings saved; generation {} active", activated.generation);
        // Shells reloaded after this apply read the import straight away.
        self.launcher.prepare_import(&host.value);
        Ok(ApplyOutcome { issues, diff, applied: true, generation: Some(activated.generation) })
    }

    /// Change the stored host settings with `f` and apply them like the
    /// settings window does (validated generation, then the save).
    pub fn edit(&self, f: impl FnOnce(&mut HostSettings)) -> Result<ApplyOutcome> {
        let host = self.settings();
        let mut value = host.file.value.clone();
        f(&mut value);
        self.set(&store::to_json(&value, &host.file.unknown), true)
    }

    /// Answer a plugin request (the protocol's `Plugin` message, the GUI).
    /// Installs download: call off the UI thread.
    pub fn plugin_request(&self, req: api::Request) -> Result<api::Reply> {
        let settings = self.settings().value;
        match req {
            api::Request::Catalog { refresh } => self.plugins(|p| Ok(api::Reply::Catalog(p.catalog(refresh, &settings)))),
            api::Request::Installed => self.plugins(|p| Ok(api::Reply::Installed { installed: p.installed(&settings) })),
            api::Request::Plan { id, version } => {
                self.plugins(|p| Ok(api::Reply::Plan(Box::new(p.plan(&p.find(&id, version.as_deref())?, &settings)?))))
            }
            api::Request::Install { id, version, approved } => {
                let c = self.plugins(|p| p.find(&id, version.as_deref()))?;
                Ok(api::Reply::Changed(self.install_plugin(&c, &approved)?))
            }
            api::Request::Uninstall { id } => Ok(api::Reply::Changed(self.uninstall_plugin(&id)?)),
            api::Request::Snippets => self.plugins(|p| Ok(api::Reply::Snippets(crate::snippets::list(p.dirs, &settings)))),
            api::Request::Suggest { owner, snippet, input, values, cwd } => self.plugins(|p| {
                crate::snippets::suggest(p.dirs, &settings, &owner, &snippet, &input, &values, cwd.as_deref()).map(api::Reply::Suggestions)
            }),
            api::Request::CommandSuggest { plugin, command, input, values, cwd } => self.plugins(|p| {
                crate::snippets::suggest_command(p.dirs, &settings, &plugin, &command, &input, &values, cwd.as_deref())
                    .map(api::Reply::Suggestions)
            }),
            api::Request::Installs => Ok(api::Reply::Installs { installs: self.installs.list() }),
            api::Request::CancelInstall { id } => {
                self.installs.cancel(&id)?;
                Ok(api::Reply::Installs { installs: self.installs.list() })
            }
            api::Request::Updates => self.plugins(|p| Ok(api::Reply::Updates { updates: p.updates(true) })),
        }
    }

    /// Fetch the plugin registries that are older than a day, when a
    /// plugin from one is installed (goattyd does this now and then, #95).
    pub fn refresh_stale_registries(&self) {
        let _ = self.plugins(|p| {
            p.updates(true);
            Ok(())
        });
    }

    /// Install `c` as approved (`approved`: the plan's digest), enable it,
    /// and build the shell configuration.
    /// The install runs as a job: [`api::Request::Installs`] shows its
    /// progress and [`api::Request::CancelInstall`] stops it.
    pub fn install_plugin(&self, c: &Candidate, approved: &str) -> Result<Changed> {
        let m = &c.manifest;
        self.installs.run(&m.id, &m.name, &m.version, |job| self.install_plugin_with(c, approved, job))
    }

    fn install_plugin_with(&self, c: &Candidate, approved: &str, job: &crate::plugins::jobs::Job) -> Result<Changed> {
        let settings = self.settings().value;
        let plan = self.plugins(|p| p.install_with(c, approved, &settings, job))?;
        let id = plan.id.clone();
        let outcome = self.edit(|h| {
            // Dependencies are never built in (those are always there).
            for d in &plan.dependencies {
                h.plugins.entry(d.id.clone()).or_default().enabled = true;
            }
            match c.builtin {
                // Built-in plugins are on by default: keep host.json sparse.
                Some(_) => h.edit_plugin(&id, |p| p.enabled = true),
                None => h.plugins.entry(id).or_default().enabled = true,
            }
        })?;
        let mut verb = match &plan.replaces {
            Some(old) if *old != plan.version => format!("Updated {} from {old} to {}", plan.name, plan.version),
            Some(_) => format!("Reinstalled {} {}", plan.name, plan.version),
            None => format!("Installed {} {}", plan.name, plan.version),
        };
        if !plan.dependencies.is_empty() {
            let deps: Vec<String> = plan.dependencies.iter().map(|d| format!("{} {}", d.name, d.version)).collect();
            verb.push_str(&format!(" with {}, which it needs", deps.join(", ")));
        }
        Ok(changed(verb, &outcome))
    }

    /// Remove a plugin and its settings, and build the shell configuration.
    pub fn uninstall_plugin(&self, id: &str) -> Result<Changed> {
        let (entry, unneeded) = self.plugins(|p| p.uninstall(id))?;
        let id = id.to_owned();
        let outcome = self.edit(|h| {
            h.plugins.remove(&id);
        })?;
        let mut c = changed(format!("Uninstalled {id} {}", entry.version), &outcome);
        if !unneeded.is_empty() {
            c.lines.push(format!("Installed only because other plugins needed them, and unused now: {}.", unneeded.join(", ")));
        }
        c.unneeded = unneeded;
        Ok(c)
    }

    pub fn generations(&self) -> Vec<Generation> {
        self.launcher.dirs().map(generation::list).unwrap_or_default()
    }

    /// Remember a managed shell this process started, so
    /// [`reload_shells`](Self::reload_shells) can reach it.
    pub fn track_shell(&self, pid: u32) {
        self.shells.lock().unwrap_or_else(|e| e.into_inner()).insert(pid);
    }

    /// The shell exited.
    pub fn untrack_shell(&self, pid: u32) {
        self.shells.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
    }

    /// Ask every tracked managed shell to restart with the active
    /// generation. They get `SIGURG`: the managed zsh reloads at once when
    /// it's idle at an empty prompt, otherwise at its next prompt (and not
    /// at all while it has jobs; it says so instead). `SIGURG` is ignored
    /// by default, so a process that isn't our zsh (the user ran
    /// `exec bash`, or the pid was reused) is unaffected. Returns how many
    /// shells were signaled.
    pub fn reload_shells(&self) -> usize {
        let mut shells = self.shells.lock().unwrap_or_else(|e| e.into_inner());
        shells.retain(|&pid| {
            let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
            // SAFETY: kill(2) has no memory-safety preconditions; a stale
            // pid at worst gets an ignored-by-default signal.
            unsafe { libc::kill(pid, libc::SIGURG) == 0 }
        });
        log::info!("asked {} managed shell(s) to reload", shells.len());
        shells.len()
    }

    /// Activate generation `to` (default: the previous one) and restore its
    /// settings file.
    pub fn rollback(&self, to: Option<u32>) -> Result<HostSnapshot> {
        let dirs = self.launcher.dirs().context("no $HOME on this host")?;
        let path = self.host_file.as_deref().context("no $HOME on this host")?;
        let g = crate::cmd::rollback_to(dirs, path, to)?;
        log::info!("rolled back to generation {}", g.number);
        Ok(self.snapshot())
    }
}

fn changed(what: String, o: &ApplyOutcome) -> Changed {
    let mut lines = vec![match o.generation {
        Some(g) if !o.diff.is_empty() => {
            format!("{what}. New goatty shells have it (generation {g}); reload running shells to use it there too.")
        }
        _ => format!("{what}."),
    }];
    lines.extend(o.issues.iter().map(|i| format!("⚠ {i}")));
    Changed { lines, generation: o.generation, reload: !o.diff.is_empty(), unneeded: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::Dirs;

    #[test]
    fn set_preview_apply_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config/host.json");
        let svc = HostService::new(Launcher::new(Some(Dirs::under(tmp.path())), None), Some(file.clone()));
        assert_eq!(
            svc.snapshot().json,
            format!("{{\n  \"version\": {}\n}}\n", <goatty_config::HostSettings as goatty_config::Document>::VERSION)
        );

        let preview = svc.set(r#"{"prompt": {"two_line": false}}"#, false).unwrap();
        assert!(!preview.applied && !preview.diff.is_empty() && !file.exists());

        let a = svc.set(r#"{"prompt": {"two_line": false}}"#, true).unwrap();
        assert_eq!(a.generation, Some(1));
        assert!(svc.snapshot().json.contains("two_line"));
        let b = svc.set(r#"{"prompt": {"two_line": false}, "history": {"size": 2000}}"#, true).unwrap();
        assert_eq!(b.generation, Some(2));
        assert_eq!(b.diff.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["settings.zsh"]);

        let back = svc.rollback(None).unwrap();
        assert_eq!(back.generation, Some(1));
        assert!(!back.json.contains("2000"));
        assert_eq!(svc.generations().len(), 2);
    }

    #[test]
    fn plugin_requests_install_enable_and_uninstall() {
        let w = crate::plugins::tests::world();
        let file = w.tmp.path().join("config/host.json");
        let svc = HostService::new(Launcher::new(Some(w.dirs.clone()), None), Some(file.clone()))
            .with_plugin_sources(Arc::new(w.fetch.clone()), w.sources.clone());

        let api::Reply::Plan(plan) = svc.plugin_request(api::Request::Plan { id: "demo".into(), version: None }).unwrap() else {
            panic!("expected a plan")
        };
        let req = api::Request::Install { id: "demo".into(), version: None, approved: plan.digest.clone() };
        let api::Reply::Changed(c) = svc.plugin_request(req).unwrap() else { panic!("expected a change") };
        assert!(c.lines[0].starts_with("Installed Demo 1.0.0"), "{:?}", c.lines);
        assert!(c.reload);
        // The install's outcome stays listed for a client that lost its reply.
        let api::Reply::Installs { installs } = svc.plugin_request(api::Request::Installs).unwrap() else { panic!() };
        assert!(matches!(&installs[..], [p] if p.id == "demo" && p.step == api::Step::Done(c.clone())), "{installs:?}");
        let e = svc.plugin_request(api::Request::CancelInstall { id: "demo".into() }).unwrap_err();
        assert!(e.to_string().contains("isn't being installed any more"), "{e}");
        let api::Reply::Updates { updates } = svc.plugin_request(api::Request::Updates).unwrap() else { panic!() };
        assert!(updates.is_empty(), "{updates:?}");
        assert!(std::fs::read_to_string(&file).unwrap().contains("\"demo\""), "enabled in host.json");
        let plugins_zsh = std::fs::read_to_string(w.dirs.zsh_dir().join("plugins.zsh")).unwrap();
        assert!(plugins_zsh.contains("alias -- d='demo'"), "{plugins_zsh}");

        // A setting changed through the host settings reaches the generation.
        svc.edit(|h| {
            h.plugins.get_mut("demo").unwrap().settings.insert("loud".into(), "yes".into());
        })
        .map(|o| assert!(o.issues.iter().any(|i| i.contains("plugins.demo.settings.loud")), "{:?}", o.issues))
        .unwrap();

        let api::Reply::Catalog(cat) = svc.plugin_request(api::Request::Catalog { refresh: false }).unwrap() else { panic!() };
        let demo: Vec<_> = cat.installed.iter().filter(|i| !i.builtin).collect();
        assert_eq!(demo.len(), 1);
        assert!(demo[0].enabled);

        let api::Reply::Installed { installed: local_installed } = svc.plugin_request(api::Request::Installed).unwrap() else { panic!() };
        let demo_local: Vec<_> = local_installed.iter().filter(|i| !i.builtin).collect();
        assert_eq!(demo_local.len(), 1);
        assert_eq!(demo_local[0].id, "demo");

        let api::Reply::Changed(c) = svc.plugin_request(api::Request::Uninstall { id: "demo".into() }).unwrap() else { panic!() };
        assert!(c.lines[0].starts_with("Uninstalled demo"));
        assert!(!std::fs::read_to_string(&file).unwrap().contains("\"demo\""));
        assert!(!std::fs::read_to_string(w.dirs.zsh_dir().join("plugins.zsh")).unwrap().contains("alias --"));
    }

    #[test]
    fn installing_a_plugin_enables_what_it_needs_and_uninstalling_offers_it() {
        let w = crate::plugins::tests::world();
        crate::plugins::tests::publish_dependents(&w);
        let file = w.tmp.path().join("config/host.json");
        let svc = HostService::new(Launcher::new(Some(w.dirs.clone()), None), Some(file.clone()))
            .with_plugin_sources(Arc::new(w.fetch.clone()), w.sources.clone());
        let api::Reply::Plan(plan) = svc.plugin_request(api::Request::Plan { id: "top".into(), version: None }).unwrap() else { panic!() };
        // Through the protocol's JSON, as a client sees it.
        let plan: api::Plan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(plan.dependencies.len(), 2);
        let req = api::Request::Install { id: "top".into(), version: None, approved: plan.digest };
        let api::Reply::Changed(c) = svc.plugin_request(req).unwrap() else { panic!() };
        assert!(c.lines[0].starts_with("Installed TOP 1.0.0 with Demo 1.0.0, MID 1.0.0, which it needs"), "{:?}", c.lines);
        let host = svc.settings().value;
        assert!(["demo", "mid", "top"].iter().all(|id| host.plugins.get(*id).is_some_and(|p| p.enabled)), "{:?}", host.plugins);
        assert!(std::fs::read_to_string(w.dirs.zsh_dir().join("plugins.zsh")).unwrap().contains("alias -- d='demo'"));

        let api::Reply::Changed(c) = svc.plugin_request(api::Request::Uninstall { id: "top".into() }).unwrap() else { panic!() };
        assert_eq!(c.unneeded, ["mid", "demo"]);
        assert!(c.lines.iter().any(|l| l.contains("unused now: mid, demo")), "{:?}", c.lines);
    }
}
