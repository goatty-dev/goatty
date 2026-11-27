//! The `shell` commands (`goattyd shell status|install|uninstall|dismiss|generations|name|diff|rollback`),
//! printing to stdout. They run on the host whose shells they manage, which
//! for a remote goattyd is the remote host: `goatty-setup` in a remote
//! session installs there.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use goatty_config::{Effective, HostSettings, store};

use crate::builtins::{self, Builtin};
use crate::dirs::Dirs;
use crate::doctor;
use crate::generation;
use crate::history;
use crate::launch::Launcher;
use crate::packages::{self, Location};
use crate::plugins::{Candidate, Plugins};
use crate::setup::{self, FeatureState, ToolLocation, size, tilde};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

fn dirs_of(launcher: &Launcher) -> Result<&Dirs> {
    launcher.dirs().context("$HOME is not set, so goatty has nowhere to keep its shell environment")
}

/// `goattyd shell import [--refresh]`.
pub fn import(svc: &crate::HostService, refresh: bool) -> Result<()> {
    let s = svc.import_status(refresh)?;
    println!("goatty import of your shell setup");
    println!("  import      {}", if s.enabled { "on" } else { "off (goattyd settings set shell.import.enabled true)" });
    if !s.managed {
        println!("  note        only goatty's managed zsh applies it, and sessions here don't use it");
    }
    match &s.source {
        Some(src) => println!("  from        {} ({})", src.program, src.why),
        None => println!("  from        -"),
    }
    if let Some(p) = &s.problem {
        println!("  problem     {p}");
    }
    for (i, f) in s.files.iter().enumerate() {
        println!("  {:<10}  {}{}", if i == 0 { "files" } else { "" }, f.path, if f.exists { "" } else { " (not there)" });
    }
    if let Some(f) = &s.failure {
        println!("  last run    failed {} ago: {}", ago(s.now.saturating_sub(f.at)), f.message);
        if !f.stderr.is_empty() {
            println!("              {}", f.stderr.replace('\n', "\n              "));
        }
    }
    // `--refresh` that didn't work fails the command (after saying why).
    let result = || if refresh && s.refreshed == Some(false) { anyhow::bail!("checking your shell setup failed") } else { Ok(()) };
    let Some(h) = &s.harvest else {
        println!("  harvest     none yet{}", if refresh { "" } else { " (--refresh runs your shell now)" });
        return result();
    };
    println!("  harvest     {} ago, took {} ms", ago(s.now.saturating_sub(h.at)), h.ms);
    let added = h.added_path();
    println!();
    println!("PATH folders added ({})", added.len());
    for d in added {
        println!("  {d}");
    }
    println!("variables ({})", h.vars.len());
    for v in &h.vars {
        println!("  {}={}", v.name, v.value);
    }
    let offer = crate::import::offer_aliases(h, &svc.settings().value);
    println!("aliases goatty doesn't have yet ({}; not imported: copy them under Settings → Aliases, or goattyd alias add)", offer.len());
    for a in offer {
        println!("  {}={}{}", a.name, a.value, if a.own { "" } else { "  (from a framework or plugin)" });
    }
    if !h.stderr.is_empty() {
        println!("stderr");
        println!("  {}", h.stderr.replace('\n', "\n  "));
    }
    result()
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h", secs / 3600),
        _ => format!("{} days", secs / 86400),
    }
}

/// `goattyd shell status`: configuration, what a real shell loads, startup
/// time. The same [`setup::Status`] the GUI's Diagnostics page shows.
pub fn status(svc: &crate::HostService) -> Result<()> {
    let s = setup::status(svc)?;
    println!("goatty shell");
    println!("  program     {}", s.program);
    if let Some(why) = &s.unmanaged {
        println!("  config      {why}");
        return Ok(());
    }
    match s.generation {
        Some(n) => println!("  config      {} (generation {n}; `goattyd shell generations`)", s.config_dir),
        None => println!("  config      {} (not built)", s.config_dir),
    }
    for issue in &s.settings_issues {
        println!("  settings    \u{26a0} {issue}");
    }
    if !s.env_overrides.is_empty() {
        println!("  overridden  by {}", s.env_overrides.join(", "));
    }
    for (i, f) in s.user_files.iter().enumerate() {
        let what = match (f.exists, f.sourced, f.path.ends_with(".zshrc")) {
            (false, ..) => "not there",
            (true, true, _) => "sourced",
            (true, false, true) => "not sourced (`goattyd shell import` brings what it sets up; GOATTY_ZSH_USER_RC=1 runs it)",
            (true, false, false) => "not sourced",
        };
        println!("  {:<10}  {}: {what}", if i == 0 { "your files" } else { "" }, f.path);
    }
    println!("  additions   {}{}", s.additions, if s.additions_exist { "" } else { " (create it for your own aliases etc.)" });
    if let Ok(e) = svc.everywhere(crate::everywhere::Action::Status, None) {
        let state = match e.state {
            crate::everywhere::State::On => format!("on ({})", e.files_text()),
            crate::everywhere::State::Off => "off (`goattyd shell everywhere on` uses goatty's shell in every terminal)".into(),
            crate::everywhere::State::Changed => "a goatty line in your .zshenv was edited by hand (`goattyd shell everywhere`)".into(),
        };
        println!("  everywhere  {state}");
    }

    match &s.startup {
        Some(st) => {
            let installed_plugins = svc.plugins(|p| Ok(p.installed(&svc.settings().value).0)).unwrap_or_default();
            println!("  startup     {} ms (login + interactive, measured just now)", st.ms);
            println!();
            for f in &st.features {
                let line = match &f.state {
                    FeatureState::Loaded { managed: Some(v), .. } => match s.tool(&f.id) {
                        Some(t) if t.updatable() => {
                            format!("\u{26a0} managed {} {v} \u{2192} {}: goattyd shell update {}", f.name, t.version, t.id)
                        }
                        Some(t) if t.outdated().is_some() => format!(
                            "\u{26a0} managed {} {v} (pinned {}: {})",
                            f.name,
                            t.version,
                            t.unavailable.as_deref().unwrap_or("not available")
                        ),
                        _ => format!("\u{2713} managed {} {v}", f.name),
                    },
                    FeatureState::Loaded { detail, .. } => format!("\u{2713} {detail}"),
                    FeatureState::Off if f.is_rc_skipped() => "- skipped in your .zshrc: goatty loads its own".into(),
                    FeatureState::Off => "- off".into(),
                    FeatureState::Missing if f.is_import() => "- nothing imported yet (`goattyd shell import --refresh`)".into(),
                    FeatureState::Missing if f.is_helper() => {
                        format!("- not installed (optional: {}; goattyd plugin install {})", setup::helper_use(&f.id), f.name)
                    }
                    FeatureState::Missing => match s.tool(&f.id) {
                        Some(t) => format!("\u{2717} not installed: goattyd shell install {} ({} {})", t.id, t.name, t.version),
                        None => match f.id.strip_prefix("plugin:") {
                            Some(pid)
                                if installed_plugins.iter().any(|i| {
                                    i.id == pid && i.incompatible.is_none() && i.packages.iter().any(|p| p.available && p.needs_install())
                                }) =>
                            {
                                format!("\u{2717} not installed: goattyd plugin repair {pid}")
                            }
                            Some(pid) => format!("\u{2717} its command isn't installed (see `goattyd plugin info {pid}`)"),
                            None => "\u{2717} its command isn't installed (see `goattyd plugin info`)".into(),
                        },
                    },
                };
                println!("  {:<20} {line}", f.id);
            }
            if !st.stderr.is_empty() {
                println!("\nThe shell printed errors while starting:\n{}", st.stderr);
            }
            match (s.offer().next().is_some(), s.updates().next().is_some()) {
                (true, true) => println!(
                    "\nInstall missing tools and update outdated ones with `goattyd shell install` (or `goatty-setup` in a goatty shell); `goattyd shell update` updates only outdated ones."
                ),
                (true, false) => println!("\nInstall what's missing with `goattyd shell install` (or `goatty-setup` in a goatty shell)."),
                (false, true) => println!("\nUpdate outdated tools with `goattyd shell update` (or `goatty-setup` in a goatty shell)."),
                (false, false) => {}
            }
        }
        None => {
            println!("  startup     failed: {}", s.startup_error.as_deref().unwrap_or("unknown error"));
            println!("\n  as seen from goattyd's PATH:");
            for t in &s.tools {
                let line = match &t.location {
                    ToolLocation::System { path } => format!("\u{2713} {path}"),
                    ToolLocation::Managed { version } if *version != t.version => {
                        format!("\u{26a0} managed {version} \u{2192} {} (outdated)", t.version)
                    }
                    ToolLocation::Managed { version } => format!("\u{2713} managed {version}"),
                    ToolLocation::Missing => "\u{2717} not found".into(),
                };
                println!("  {:<20} {line}", t.id);
            }
        }
    }
    Ok(())
}

fn probe_shell(host: &Effective<HostSettings>, launcher: &Launcher) -> Option<doctor::Probe> {
    let launch = launcher.launch(host);
    match doctor::probe(&launch, PROBE_TIMEOUT) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("couldn't probe the shell ({e:#}); checking goattyd's PATH instead");
            None
        }
    }
}

fn is_missing(b: &Builtin, dirs: &Dirs, path: Option<&str>, probe: Option<&doctor::Probe>) -> bool {
    let shell = builtins::host_managed_shell();
    match probe {
        Some(p) => p.get(b.id) == Some(&doctor::State::Missing),
        None => {
            b.manifest().packages.iter().filter(|p| p.applies_to_shell(shell)).any(|p| packages::locate(dirs, p, path) == Location::Missing)
        }
    }
}

/// Returns the older managed version if `m` uses a goatty-managed copy whose
/// version differs from its pinned version (and isn't shadowed by a system
/// copy found by probing the shell).
fn outdated_managed_manifest(
    m: &goatty_plugin::manifest::Manifest,
    dirs: &Dirs,
    path: Option<&str>,
    probe: Option<&doctor::Probe>,
) -> Option<String> {
    if let Some(doctor::State::Ok(detail)) = probe.and_then(|p| p.get(&m.id))
        && !setup::is_managed_detail(dirs, detail)
    {
        return None;
    }
    let shell = builtins::host_managed_shell();
    m.packages.iter().filter(|p| p.applies_to_shell(shell)).find_map(|p| match packages::locate(dirs, p, path) {
        Location::Managed { version, .. } if version != p.version => Some(version),
        _ => None,
    })
}

fn outdated_managed(b: &Builtin, dirs: &Dirs, path: Option<&str>, probe: Option<&doctor::Probe>) -> Option<String> {
    outdated_managed_manifest(b.manifest(), dirs, path, probe)
}

/// Built-in plugins that `goattyd shell install` (without names) handles:
/// enabled default tools that are missing, plus enabled tools with outdated
/// managed copies.
fn default_install_targets(host: &Effective<HostSettings>, launcher: &Launcher, dirs: &Dirs) -> Vec<&'static Builtin> {
    let probe = probe_shell(host, launcher);
    let path = std::env::var("PATH").ok();
    builtins::tools()
        .filter(|b| host.value.plugin_enabled(b.id))
        .filter(|b| {
            (b.default_install && is_missing(b, dirs, path.as_deref(), probe.as_ref()))
                || outdated_managed(b, dirs, path.as_deref(), probe.as_ref()).is_some()
        })
        .collect()
}

/// Built-in plugins with outdated managed copies.
fn outdated_targets(host: &Effective<HostSettings>, launcher: &Launcher, dirs: &Dirs) -> Vec<&'static Builtin> {
    let probe = probe_shell(host, launcher);
    let path = std::env::var("PATH").ok();
    builtins::tools().filter(|b| outdated_managed(b, dirs, path.as_deref(), probe.as_ref()).is_some()).collect()
}

fn resolve_names(names: &[String]) -> Result<Vec<&'static Builtin>> {
    let mut out: Vec<&'static Builtin> = Vec::new();
    for n in names {
        let Some(b) = builtins::find(n) else {
            let ids: Vec<_> = builtins::tools().map(|b| b.id).collect();
            bail!("{n:?} isn't one of goatty's built-in plugins ({}); other plugins: goattyd plugin install {n}", ids.join(", "));
        };
        if !out.iter().any(|o| o.id == b.id) {
            out.push(b);
        }
    }
    Ok(out)
}

/// `goattyd shell install [NAME...]`: download the tools of goatty's
/// built-in plugins. No names installs missing default ones and updates
/// outdated managed copies (`goatty-setup`). Asks before downloading unless
/// `yes`.
pub fn install(host: &Effective<HostSettings>, launcher: &Launcher, names: &[String], yes: bool) -> Result<()> {
    let dirs = dirs_of(launcher)?;
    let fetch = goatty_plugin::fetch::Curl { cache: dirs.cache.clone() };
    let requested = if names.is_empty() { default_install_targets(host, launcher, dirs) } else { resolve_names(names)? };
    run_tool_plans(host, dirs, &fetch, requested.into_iter().map(Candidate::builtin).collect(), yes, false)
}

/// `goattyd shell update [NAME...]`: update goatty's managed copies of
/// built-in plugins' tools to the pinned versions. Without names: all
/// outdated managed copies. Asks before downloading unless `yes`.
pub fn update(host: &Effective<HostSettings>, launcher: &Launcher, names: &[String], yes: bool) -> Result<()> {
    let dirs = dirs_of(launcher)?;
    let fetch = goatty_plugin::fetch::Curl { cache: dirs.cache.clone() };
    update_with_fetch(host, launcher, dirs, &fetch, names, yes)
}

fn update_with_fetch(
    host: &Effective<HostSettings>,
    launcher: &Launcher,
    dirs: &Dirs,
    fetch: &dyn goatty_plugin::fetch::Fetch,
    names: &[String],
    yes: bool,
) -> Result<()> {
    if names.is_empty() {
        let requested = outdated_targets(host, launcher, dirs).into_iter().map(Candidate::builtin).collect();
        run_tool_plans(host, dirs, fetch, requested, yes, true)
    } else {
        let path = std::env::var("PATH").ok();
        let requested = resolve_names(names)?.into_iter().map(Candidate::builtin).collect();
        update_explicit_candidates(host, dirs, fetch, requested, path.as_deref(), yes)
    }
}

fn update_explicit_candidates(
    host: &Effective<HostSettings>,
    dirs: &Dirs,
    fetch: &dyn goatty_plugin::fetch::Fetch,
    candidates: Vec<Candidate>,
    path: Option<&str>,
    yes: bool,
) -> Result<()> {
    let mut out = Vec::new();
    for c in candidates {
        if outdated_managed_manifest(&c.manifest, dirs, path, None).is_some() {
            out.push(c);
            continue;
        }
        let shell = builtins::host_managed_shell();
        for p in c.manifest.packages.iter().filter(|p| p.applies_to_shell(shell)) {
            match packages::locate(dirs, p, path) {
                Location::System(sys) => println!("{} is already installed at {}; goatty uses that.", c.manifest.name, tilde(&sys)),
                Location::Managed { version, .. } => println!("{} {version} is already up to date.", c.manifest.name),
                Location::Missing => {
                    println!("{} isn't installed (`goattyd shell install {}` installs it).", c.manifest.name, c.manifest.id);
                }
            }
        }
    }
    run_tool_plans(host, dirs, fetch, out, yes, true)
}

fn run_tool_plans(
    host: &Effective<HostSettings>,
    dirs: &Dirs,
    fetch: &dyn goatty_plugin::fetch::Fetch,
    requested: Vec<Candidate>,
    yes: bool,
    updating_only: bool,
) -> Result<()> {
    let plugins = Plugins { dirs, fetch, builtin: &[] };
    let mut todo = Vec::new();
    let mut plan_failed = 0usize;
    for c in requested {
        let plan = match plugins.plan(&c, &host.value) {
            Ok(plan) => plan,
            Err(e) => {
                println!("{e:#}");
                plan_failed += 1;
                continue;
            }
        };
        if plan.downloads.iter().any(|d| d.fetches()) {
            todo.push((c, plan));
            continue;
        }
        for d in &plan.downloads {
            match &d.system {
                Some(p) => println!("{} is already installed at {}; goatty uses that.", plan.name, tilde(Path::new(p))),
                None => println!("{} {} is already installed.", plan.name, d.version),
            }
        }
    }
    if todo.is_empty() {
        if plan_failed > 0 {
            bail!("{plan_failed} {} failed", if updating_only { "updates" } else { "installs" });
        }
        println!("{}", if updating_only { "Nothing to update." } else { "Nothing to install." });
        return Ok(());
    }

    let mut rows: Vec<(String, u64, &str)> = Vec::new();
    let mut has_install = false;
    let mut has_update = false;
    for (c, plan) in &todo {
        for d in plan.downloads.iter().filter(|d| d.fetches()) {
            let old = c
                .manifest
                .packages
                .iter()
                .find(|p| p.id == d.package)
                .and_then(|p| packages::managed(dirs, p))
                .map(|(v, _)| v)
                .filter(|v| *v != d.version);
            let label = match old {
                Some(old) => {
                    has_update = true;
                    format!("{} {old} \u{2192} {}", plan.name, d.version)
                }
                None => {
                    has_install = true;
                    format!("{} {}", plan.name, d.version)
                }
            };
            rows.push((label, d.size, plan.summary.as_str()));
        }
    }
    let header = match (has_install, has_update) {
        (true, true) => "goatty will download and install/update into",
        (false, true) => "goatty will download and update in",
        _ => "goatty will download and install into",
    };
    println!("{header} {} (no sudo):", tilde(&dirs.data));
    let width = rows.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
    for (name, bytes, summary) in &rows {
        println!("  {name:<width$}  {:>8}  {summary}", size(*bytes));
    }
    println!("  {:<width$}  {:>8}", "", size(rows.iter().map(|(_, b, _)| b).sum()));
    println!("Each download is checked against the sha256 checksum goatty pins for it.");
    let verb = if updating_only || !has_install { "update" } else { "install" };
    if !yes && !confirm("Continue? [Y/n] ", verb)? {
        println!("Cancelled.");
        return Ok(());
    }

    let mut failed = 0usize;
    for (c, plan) in &todo {
        print!("  {} ... ", plan.name);
        let _ = std::io::stdout().flush();
        match plugins.install(c, &plan.digest, &host.value) {
            Ok(_) => println!("\u{2713}"),
            Err(e) => {
                println!("\u{2717} {e:#}");
                failed += 1;
            }
        }
    }
    let total_failed = failed + plan_failed;
    if total_failed > 0 {
        bail!("{total_failed} of {} {} failed", todo.len() + plan_failed, if verb == "update" { "updates" } else { "installs" });
    }
    println!("Done. New goatty shells use them; `goatty-reload` restarts the current one.");
    Ok(())
}

fn confirm(prompt: &str, verb: &str) -> Result<bool> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!("not asking without a terminal; pass --yes to {verb}");
    }
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    // End of input (Ctrl+D) is no; an empty line (Enter) is the default yes.
    if stdin.lock().read_line(&mut line)? == 0 {
        println!();
        return Ok(false);
    }
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes"))
}

/// `goattyd agent-hooks [status|install|remove] [AGENT]`: coding agents'
/// session hooks. Installing names the files it changes and asks first.
pub fn agent_hooks(
    svc: &crate::HostService,
    action: crate::agent_hooks::Action,
    agent: Option<crate::agent_hooks::Agent>,
    yes: bool,
) -> Result<()> {
    use crate::agent_hooks::{Action, State};
    let before = svc.agent_hooks(Action::Status, None)?;
    if action == Action::Install {
        if let Some(why) = &before.unavailable {
            bail!("{why}");
        }
        let todo: Vec<_> = before
            .hooks
            .iter()
            .filter(|h| agent.is_none_or(|a| a == h.agent) && h.available && h.state != State::On && h.problem.is_none())
            .collect();
        if todo.is_empty() {
            if let Some(h) = agent.and_then(|a| before.hooks.iter().find(|h| h.agent == a)) {
                match (&h.problem, h.available) {
                    (Some(p), _) => bail!("{p}"),
                    (None, false) => bail!("{} isn't set up here", h.name),
                    _ => {}
                }
            }
            println!("nothing to change");
            return Ok(());
        }
        println!("goatty's session hook goes into (each file is backed up first):");
        for h in &todo {
            println!("  {}  ({})", h.file, h.name);
        }
        if !yes && !confirm("\nAdd it? [Y/n] ", "add it")? {
            return Ok(());
        }
    }
    let status = if action == Action::Status { before } else { svc.agent_hooks(action, agent)? };
    for h in &status.hooks {
        let state = match (h.available, h.state) {
            (false, State::Off) => "not set up here",
            (_, State::Off) => "no hook",
            (_, State::On) => "hook installed",
            (_, State::Outdated) => "hook runs another goattyd (install again to update)",
        };
        println!("{:<12} {state:<16} {}", h.agent.id(), h.file);
        if let Some(p) = &h.problem {
            println!("{:<12} {p}", "");
        }
    }
    if let Some(b) = &status.backup {
        println!("the file as it was: {b}");
    }
    Ok(())
}

/// `goattyd shell uninstall NAME...|--all`: remove goatty's downloads of
/// built-in plugins' tools (the plugins stay; system copies still work).
/// Downloads of tools that used to come with goatty (Atuin) are removed by
/// name and with `--all`, unless an installed plugin uses them now.
pub fn uninstall(launcher: &Launcher, names: &[String], all: bool) -> Result<()> {
    let dirs = dirs_of(launcher)?;
    let fetch = goatty_plugin::fetch::Curl { cache: dirs.cache.clone() };
    let plugins = Plugins { dirs, fetch: &fetch, builtin: &[] };
    let retired: Vec<_> = builtins::RETIRED.iter().filter(|(id, _)| all || names.iter().any(|n| n == id)).collect();
    let names: Vec<String> = names.iter().filter(|n| !retired.iter().any(|(id, _)| id == n)).cloned().collect();
    let list = if all { builtins::tools().collect() } else { resolve_names(&names)? };
    if list.is_empty() && retired.is_empty() {
        bail!("name the tools to remove, or pass --all");
    }
    for (id, bins) in &retired {
        match plugins.remove_retired(id, bins)? {
            Some(true) => println!("Removed goatty's old copy of {id}."),
            Some(false) if !all => println!("{id} has no goatty-managed copy."),
            None if !all => println!("The {id} plugin uses goatty's copy of {id}: goattyd plugin uninstall {id} removes it."),
            _ => {}
        }
    }
    for b in list {
        let name = &b.manifest().name;
        if !plugins.remove_packages(b)?.is_empty() {
            println!("Removed goatty's copy of {name}.");
        } else if !all {
            println!("{name} has no goatty-managed copy.");
        }
    }
    Ok(())
}

/// `goattyd shell dismiss`: stop the "not installed" hint in new shells.
pub fn dismiss(launcher: &Launcher) -> Result<()> {
    let marker = dirs_of(launcher)?.hint_marker();
    std::fs::create_dir_all(marker.parent().context("marker has no parent")?)?;
    std::fs::write(&marker, "")?;
    println!("goatty won't mention missing tools again (`goattyd shell status` still lists them; delete {} to undo).", tilde(&marker));
    Ok(())
}

/// `goattyd shell generations`: the built configurations, newest last,
/// with their names and what each changed.
pub fn generations(launcher: &Launcher) -> Result<()> {
    let dirs = dirs_of(launcher)?;
    let list = history::list(dirs);
    if list.is_empty() {
        println!("No generations yet: one is built when the first managed shell starts.");
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    for g in list {
        let age = now.saturating_sub(g.created_unix);
        let ago = match age {
            0..60 => format!("{age}s ago"),
            60..3600 => format!("{}m ago", age / 60),
            3600..86400 => format!("{}h ago", age / 3600),
            _ => format!("{}d ago", age / 86400),
        };
        let name = g.name.as_deref().map(|n| format!("\"{n}\"  ")).unwrap_or_default();
        println!("{} {:>4}  {ago:<9} {name}{}", if g.active { "*" } else { " " }, g.number, g.summary());
        if let Some(c) = &g.changes {
            for o in &c.overridden {
                println!("        replaced: {}", o.text());
            }
        }
    }
    Ok(())
}

/// `goattyd shell name N [NAME]`: name a generation (no name: remove it).
pub fn name(launcher: &Launcher, number: u32, name: &[String]) -> Result<()> {
    let name = name.join(" ");
    history::set_name(dirs_of(launcher)?, number, Some(&name))?;
    if name.trim().is_empty() {
        println!("Generation {number} has no name now.");
    } else {
        println!("Generation {number} is \"{}\" now.", name.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    Ok(())
}

/// `goattyd shell diff`: what the next session's configuration would
/// change; with a generation, what that one changed in the generated files.
pub fn diff(host: &Effective<HostSettings>, launcher: &Launcher, generation: Option<u32>) -> Result<()> {
    let diffs: Vec<generation::FileDiff> = match generation {
        Some(n) => history::file_diff(dirs_of(launcher)?, n)?,
        None => launcher.preview(&host.value),
    };
    if diffs.is_empty() {
        println!(
            "{}",
            if generation.is_some() { "No generated file changed." } else { "The active generation matches the current settings." }
        );
    }
    for d in diffs {
        print!("{}", d.unified);
    }
    Ok(())
}

/// `goattyd shell rollback [N]`: activate an older generation and restore
/// the host settings it was built from (the current file is backed up).
pub fn rollback(launcher: &Launcher, host_file: &Path, to: Option<u32>) -> Result<()> {
    let dirs = dirs_of(launcher)?;
    let g = rollback_to(dirs, host_file, to)?;
    println!("Generation {} is active again and {} has its settings. New sessions use it.", g.number, tilde(host_file));
    Ok(())
}

/// Roll back and restore the settings file. Shared with the daemon API.
pub fn rollback_to(dirs: &Dirs, host_file: &Path, to: Option<u32>) -> Result<generation::Generation> {
    let g = generation::rollback(dirs, to)?;
    if let Some(json) = g.settings_json() {
        store::write_text(host_file, &json).with_context(|| format!("restore {}", host_file.display()))?;
    }
    Ok(g)
}

// ---- `goattyd settings` --------------------------------------------------------

fn host_path(svc: &crate::HostService) -> Result<&Path> {
    svc.host_file.as_deref().context("$HOME is not set: no settings directory")
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// `goattyd settings path`.
pub fn settings_path(svc: &crate::HostService) -> Result<()> {
    println!("{}", host_path(svc)?.display());
    Ok(())
}

/// `goattyd settings show [KEY] [--all]`: what's stored, every value, or one key.
pub fn settings_show(svc: &crate::HostService, key: Option<&str>, all: bool) -> Result<()> {
    let host = svc.settings();
    for i in &host.file.issues {
        eprintln!("\u{26a0} {i}");
    }
    if !host.env_overrides.is_empty() {
        eprintln!("(overridden by {})", host.env_overrides.join(", "));
    }
    match key {
        Some(k) => {
            let v = store::get_key(&host.value, k).with_context(|| format!("no setting {k:?}"))?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        None if all => println!("{}", serde_json::to_string_pretty(&host.value)?),
        None => print!("{}", store::to_json(&host.file.value, &host.file.unknown)),
    }
    Ok(())
}

/// `goattyd settings validate [FILE]`: exit status 1 on problems.
pub fn settings_validate(svc: &crate::HostService, file: Option<&Path>) -> Result<()> {
    let path = match file {
        Some(f) => f.to_path_buf(),
        None => host_path(svc)?.to_path_buf(),
    };
    let loaded = store::parse::<HostSettings>(&read_or_empty(&path)?);
    if loaded.issues.is_empty() {
        println!("{}: OK", path.display());
        return Ok(());
    }
    for i in &loaded.issues {
        eprintln!("\u{26a0} {}: {i}", path.display());
    }
    bail!("{} problem(s); goatty uses fallbacks for them", loaded.issues.len())
}

/// Where `goattyd settings set|unset` puts a change on a host that follows
/// the shared config (#313).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditScope {
    /// This host only: its override (the default).
    #[default]
    Host,
    /// Every host: a shared entry signed by this host. It counts wherever
    /// this host's device is trusted (a GUI computer's own goattyd).
    Shared,
}

/// Apply `edit` (of a settings document's text) to this host's settings,
/// or with [`EditScope::Shared`] on a following host, to the shared config.
fn apply_edit(
    svc: &crate::HostService,
    key: &str,
    edit: impl Fn(&str) -> Result<goatty_config::Loaded<HostSettings>>,
    scope: EditScope,
) -> Result<()> {
    use goatty_config::shared::{self, Signer};
    let before = svc.settings().file.value;
    let loaded = edit(&read_or_empty(host_path(svc)?)?)?;
    let value = store::get_key(&loaded.value, key).map(|v| v.to_string()).unwrap_or_default();
    let shared_key = shared::Scope::Shared == HostSettings::scope(key);
    if scope == EditScope::Shared && shared_key {
        let (doc, _, trust) = svc.shared_doc()?;
        let me = svc.device()?.device();
        if doc.member(&me, &trust) != Some(true) {
            bail!("this host doesn't follow the shared config (choose that from goatty when you connect); leave out --shared");
        }
        let shared_before = doc.resolve(None, &trust, &before).value;
        let shared_after = edit(&store::to_json(&shared_before, &[]))?.value;
        let n = svc.publish(|_, _, hlc, device| shared::shared_entries(&shared_before, &shared_after, hlc, device))?;
        println!("{key} = {value} (in the shared config; {n} change(s))");
        if !svc.shared_doc()?.2.contains(&me) {
            println!(
                "This host isn't trusted to change the shared config yet: it applies once a goatty computer you use trusts it (Settings → Hosts)."
            );
        }
        return Ok(());
    }
    let outcome = svc.set(&store::to_json(&loaded.value, &loaded.unknown), true)?;
    if !outcome.applied {
        for i in &outcome.issues {
            eprintln!("{i}");
        }
        bail!("{key} wasn't applied");
    }
    // On a host that follows the shared config, the change is its override.
    let kept = match shared_key.then(|| svc.capture(&before, &loaded.value)) {
        Some(Ok(n)) => n,
        Some(Err(e)) => {
            eprintln!(
                "warning: saved here, but couldn't keep it as this host's override in the shared config ({e:#}); a later shared update may undo it"
            );
            0
        }
        None => 0,
    };
    match outcome.generation {
        Some(g) => println!("{key} = {value} (generation {g}; new sessions use it)"),
        None => println!("{key} = {value}"),
    }
    if kept > 0 {
        println!("This host follows the shared config: kept as its own override. Use --shared to change it on every host.");
    }
    Ok(())
}

/// `goattyd settings set KEY VALUE`: validated, built and activated like
/// an Apply in the settings window.
pub fn settings_set(svc: &crate::HostService, key: &str, value: &str, scope: EditScope) -> Result<()> {
    apply_edit(svc, key, |text| store::set_key::<HostSettings>(text, key, value).map_err(anyhow::Error::msg), scope)
}

/// `goattyd settings unset KEY`: back to the default.
pub fn settings_unset(svc: &crate::HostService, key: &str, scope: EditScope) -> Result<()> {
    apply_edit(svc, key, |text| store::unset_key::<HostSettings>(text, key).map_err(anyhow::Error::msg), scope)
}

/// `goattyd sync status`: this host's view of the shared config (#313).
pub fn sync_status(svc: &crate::HostService, json: bool) -> Result<()> {
    let st = svc.shared_status()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(());
    }
    let (doc, _, trust) = svc.shared_doc()?;
    let short = goatty_config::shared::short_id;
    println!("host        {} ({})", st.name, short(&st.host));
    println!(
        "follows     {}",
        match st.member {
            Some(true) => "yes: this host uses the shared config, with its overrides",
            Some(false) => "no: this host keeps its own settings",
            None => "not decided yet (connect from goatty to choose)",
        }
    );
    println!("entries     {} from {} device(s)", st.entries, st.have.len());
    let label = |d: &str| doc.name(d, &trust).map_or_else(|| short(d).to_owned(), |n| format!("{n} ({})", short(d)));
    println!("trusted     {}", st.trusted.iter().map(|d| label(d)).collect::<Vec<_>>().join(", "));
    for u in &st.untrusted {
        println!("untrusted   {}: {} change(s) waiting until a trusted device vouches for it", label(&u.device), u.entries);
    }
    if !st.local_changes.is_empty() {
        let paths: Vec<String> = st.local_changes.iter().map(|p| goatty_config::shared::display_path(p)).collect();
        println!("local       changed here only: {} (share or keep them from goatty's Hosts settings)", paths.join(", "));
    }
    let overrides = doc.overrides(&st.host, &trust);
    if !overrides.is_empty() {
        println!("overrides   {}", overrides.keys().map(|p| goatty_config::shared::display_path(p)).collect::<Vec<_>>().join(", "));
    }
    for r in &st.needs_review {
        println!("review      {}: {}", r.id, r.reason);
    }
    if let Some(a) = &st.last_apply {
        println!("last apply  {} ({})", if a.ok { "ok" } else { "failed" }, a.lines.join("; "));
    }
    Ok(())
}

/// `goattyd sync apply`: resolve and apply the shared config now.
pub fn sync_apply(svc: &crate::HostService) -> Result<()> {
    match svc.apply_shared() {
        None => println!("This host doesn't follow the shared config; nothing to apply."),
        Some(a) if a.lines.is_empty() => println!("Up to date with the shared config."),
        Some(a) => {
            for l in &a.lines {
                println!("{l}");
            }
            if !a.ok {
                bail!("the shared config wasn't applied completely");
            }
        }
    }
    Ok(())
}

/// `goattyd shell everywhere [status|on|off]`: goatty's shell in every
/// terminal on this host (the line in `.zshenv`). `on` shows the lines and
/// asks first (`yes`: don't ask).
pub fn everywhere(svc: &crate::HostService, action: crate::everywhere::Action, yes: bool) -> Result<()> {
    use crate::everywhere::{Action, State};
    let before = svc.everywhere(Action::Status, None)?;
    // Asks whenever a file would change (also to fill in one that's missing the line).
    if action == Action::On && (before.state != State::On || !before.todo.is_empty()) {
        if let Some(why) = &before.unavailable {
            bail!("{why}");
        }
        if before.state == State::Changed {
            let at = before.changed.first().map_or_else(|| before.files_text(), ToString::to_string);
            bail!("a goatty line was edited by hand ({at}). Edit or remove it there, then try again");
        }
        let todo = if before.todo.is_empty() { before.files_text() } else { before.todo.join(" and ") };
        println!("goatty adds these lines to the end of {todo}:\n");
        for l in &before.lines {
            println!("    {l}");
        }
        println!();
        println!("Every new interactive zsh on this host then runs goatty's shell: IDE terminals, other terminals,");
        println!("tmux, SSH logins. Scripts and `zsh -c` stay plain, and your login shell doesn't change.");
        println!("  - Your ~/.zshrc stops running there, as in goatty's tabs. The PATH folders and variables it sets up");
        println!("    still come along; put functions, aliases and key bindings in ~/.config/goatty/zshrc.");
        println!("  - Plain zsh for one shell: GOATTY_SHELL_MANAGED=0 zsh. Undo: goattyd shell everywhere off.");
        if !yes && !confirm("\nAdd them? [Y/n] ", "add them")? {
            println!("Nothing changed.");
            return Ok(());
        }
    }
    let s = if action == Action::Status { before.clone() } else { svc.everywhere(action, Some(&before.files))? };
    let state = match (s.state, s.active) {
        (State::On, true) => "on: new interactive zsh shells on this host run goatty's shell",
        (State::On, false) => "on, but goatty's managed zsh is off here, so the line does nothing",
        (State::Off, _) => "off: only goatty's own sessions run goatty's shell",
        (State::Changed, _) => "changed: a goatty line was edited by hand",
    };
    println!("goatty's shell in every terminal");
    println!("  state       {state}");
    println!("  {:<10}  {}", if s.files.len() == 1 { "file" } else { "files" }, s.files_text());
    for c in &s.changed {
        println!("  changed     {c}");
    }
    for w in &s.warnings {
        println!("  \u{26a0} warning   {w}");
    }
    if let Some(why) = s.unavailable.as_deref().filter(|_| s.state != State::On) {
        println!("  unavailable {why}");
    }
    if action != Action::Status && before.state != s.state {
        if let Some(b) = s.backup.as_ref().filter(|b| before.backup.as_ref() != Some(*b)) {
            println!("  backup      {b}");
        }
        if s.state == State::Off {
            println!("\nShells that are open keep goatty's shell until they exit.");
        } else {
            println!("\nNew shells use it; open ones don't change.");
        }
    }
    Ok(())
}

/// `goattyd shell refresh` without a running goattyd: bring the shell
/// configuration and the import of the user's own shell setup up to date
/// in this process, as a new goatty session would.
pub fn refresh(svc: &crate::HostService) -> Result<()> {
    svc.refresh()
}

/// `goattyd shell refresh --wait SECS`, which shells outside goatty run
/// when a startup file changed (`entry.zsh`): the refresh runs in a process
/// of its own, in its own session (closing the terminal doesn't stop it),
/// and this one returns once it's done or after `wait`, whichever is first.
/// That process starts without `ZDOTDIR`: the shell asking is in the middle
/// of the user's `.zshenv`, which may have set it.
pub fn refresh_detached(wait: Duration) -> Result<()> {
    let exe = std::env::current_exe().context("where is goattyd")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["shell", "refresh"])
        .env_remove("ZDOTDIR")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and only changes the child.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = cmd.spawn().context("start goattyd shell refresh")?;
    let start = std::time::Instant::now();
    while start.elapsed() < wait {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn targz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        for (path, data, mode) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_cksum();
            b.append_data(&mut h, path, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    fn seed_outdated_plugin(dirs: &Dirs, id: &str, old_version: &str, root: &str, entry: &str, is_bin: bool) {
        let ver_dir = dirs.tools_dir().join(id).join(old_version).join(root);
        std::fs::create_dir_all(&ver_dir).unwrap();
        let target = ver_dir.join(entry);
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        if is_bin {
            std::fs::create_dir_all(dirs.bin_dir()).unwrap();
            let link = dirs.bin_dir().join(id);
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&target, link).unwrap();
        } else {
            std::fs::create_dir_all(dirs.plugins_dir()).unwrap();
            let link = dirs.plugins_dir().join(id);
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&ver_dir, link).unwrap();
        }
    }

    fn isolated_candidate(id: &str) -> Candidate {
        let mut c = Candidate::builtin(builtins::get(id).unwrap());
        for p in &mut c.manifest.packages {
            p.system_paths.clear();
            p.prefer_system = false;
        }
        c
    }

    #[test]
    fn outdated_managed_ignores_system_shadow_and_current_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let fzf_tab = isolated_candidate("fzf-tab").manifest;
        let mut fzf = isolated_candidate("fzf").manifest;
        fzf.packages[0].prefer_system = true;
        let pinned_tab = &fzf_tab.packages[0].version;

        // Missing -> None.
        assert_eq!(outdated_managed_manifest(&fzf_tab, &dirs, Some(""), None), None);

        // Managed at an older version -> Some(old).
        seed_outdated_plugin(&dirs, "fzf-tab", "1.0.0", "fzf-tab-1.0.0", "fzf-tab.plugin.zsh", false);
        assert_eq!(outdated_managed_manifest(&fzf_tab, &dirs, Some(""), None).as_deref(), Some("1.0.0"));

        // If the shell probe reports a system copy (e.g. added by .zprofile), skip updating.
        let mut probe = doctor::Probe {
            elapsed: Duration::ZERO,
            features: vec![doctor::Feature {
                name: "fzf-tab".into(),
                state: doctor::State::Ok("/opt/homebrew/share/fzf-tab/fzf-tab.plugin.zsh".into()),
            }],
            stderr: String::new(),
        };
        assert_eq!(outdated_managed_manifest(&fzf_tab, &dirs, Some(""), Some(&probe)), None);

        // If the probe reports goatty's own managed copy, it's still outdated.
        probe.features[0].state = doctor::State::Ok(dirs.plugins_dir().join("fzf-tab/fzf-tab.plugin.zsh").to_string_lossy().into_owned());
        assert_eq!(outdated_managed_manifest(&fzf_tab, &dirs, Some(""), Some(&probe)).as_deref(), Some("1.0.0"));

        // Managed at the pinned version -> None.
        seed_outdated_plugin(&dirs, "fzf-tab", pinned_tab, &format!("fzf-tab-{pinned_tab}"), "fzf-tab.plugin.zsh", false);
        assert_eq!(outdated_managed_manifest(&fzf_tab, &dirs, Some(""), None), None);

        // System binary on PATH shadows a managed copy.
        seed_outdated_plugin(&dirs, "fzf", "0.50.0", "", "fzf", true);
        let sysbin = tmp.path().join("sysbin");
        std::fs::create_dir_all(&sysbin).unwrap();
        std::fs::write(sysbin.join("fzf"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(sysbin.join("fzf"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(outdated_managed_manifest(&fzf, &dirs, Some(sysbin.to_str().unwrap()), None), None);
    }

    #[test]
    fn update_with_fetch_replaces_outdated_managed_package() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let base = isolated_candidate("fzf-tab");
        let pkg = base.manifest.packages[0].clone();
        let asset = pkg.asset_for("any").unwrap();
        let root = asset.root.as_str();
        let source = pkg.source.as_deref().unwrap();
        let archive = targz(&[(&format!("{root}/{source}"), b"# updated fzf-tab\n", 0o644)]);
        let archive_sha = goatty_plugin::bundle::sha256_hex(&archive);
        let mut map = BTreeMap::new();
        map.insert(asset.url.clone(), archive);
        let fetch = goatty_plugin::fetch::Fixed(map);
        let host = Effective::plain(HostSettings::default());

        // Missing -> no error, prints hint and returns Ok.
        update_explicit_candidates(&host, &dirs, &fetch, vec![base.clone()], Some(""), true).unwrap();

        // Outdated managed copy with invalid lock.json -> planning fails and update returns Err
        // instead of silently returning Ok("Nothing to update.").
        seed_outdated_plugin(&dirs, "fzf-tab", "1.0.0", "fzf-tab-1.0.0", source, false);
        std::fs::create_dir_all(dirs.installed_dir()).unwrap();
        let lock_file = dirs.installed_dir().join("lock.json");
        std::fs::write(&lock_file, "{not valid json").unwrap();
        let err = update_explicit_candidates(&host, &dirs, &fetch, vec![base.clone()], Some(""), true).unwrap_err();
        assert!(err.to_string().contains("failed"), "{err:#}");
        std::fs::remove_file(&lock_file).unwrap();

        // Outdated managed copy -> attempts update and validates sha256 against pinned checksum.
        let err = update_explicit_candidates(&host, &dirs, &fetch, vec![base.clone()], Some(""), true).unwrap_err();
        assert!(err.to_string().contains("failed"), "{err:#}");
        // Because install is staged, the older managed copy remains intact on failure.
        assert!(dirs.tools_dir().join("fzf-tab/1.0.0/fzf-tab-1.0.0/fzf-tab.plugin.zsh").exists());

        // Matching checksum -> replaces the outdated managed copy with the pinned version.
        let mut valid = base.clone();
        valid.manifest.packages[0].assets.iter_mut().for_each(|a| a.sha256 = archive_sha.clone());
        update_explicit_candidates(&host, &dirs, &fetch, vec![valid.clone()], Some(""), true).unwrap();
        assert_eq!(packages::managed(&dirs, &valid.manifest.packages[0]).map(|(v, _)| v).as_deref(), Some(pkg.version.as_str()));

        // Already at pinned version -> succeeds as a no-op.
        update_explicit_candidates(&host, &dirs, &fetch, vec![valid], Some(""), true).unwrap();
    }
}
