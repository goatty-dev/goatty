//! `goattyd plugin …`: search, inspect, install, configure and remove
//! plugins on this host from the command line. Same operations as the
//! settings window's Plugins page (through [`HostService`]).

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use goatty_plugin::api::{Changed, Installed, PackageState, Plan};
use goatty_plugin::manifest::SettingKind;
use goatty_plugin::{Permission, schema};

use super::Candidate;
use super::registry::{self, Source};
use crate::service::HostService;

fn size(bytes: u64) -> String {
    if bytes < 1_000_000 { format!("{} kB", bytes.div_ceil(1000)) } else { format!("{:.1} MB", bytes as f64 / 1e6) }
}

fn badges(perms: &[Permission]) -> String {
    let mut b = Vec::new();
    if perms.contains(&Permission::Binaries) {
        b.push("installs programs");
    }
    if perms.contains(&Permission::ShellCode) {
        b.push("runs shell code");
    }
    if perms.contains(&Permission::Network) {
        b.push("uses the network");
    }
    if b.is_empty() { String::new() } else { format!("  [{}]", b.join(", ")) }
}

/// Ask; anything but y/yes is no. Without a terminal, `--yes` is required.
fn approve(prompt: &str) -> Result<bool> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        bail!("not asking without a terminal; review the plan above and pass --yes to approve it");
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// Escape control characters in daemon-provided strings before writing to stdout.
fn safe(s: &str) -> String {
    if !s.chars().any(char::is_control) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            for e in c.escape_default() {
                out.push(e);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn reload_hint(remote_host: Option<&str>) -> String {
    match remote_host {
        Some(host) => format!("Running shells ({}): run `goattyd shell reload` there (or `goatty-reload` in one shell).", safe(host)),
        None => "Running shells: `goattyd shell reload` (or `goatty-reload` in one shell).".to_string(),
    }
}

pub fn print_changed(c: &Changed) {
    print_changed_for(c, None);
}

pub fn print_changed_for(c: &Changed, remote_host: Option<&str>) {
    for l in &c.lines {
        println!("{}", safe(l));
    }
    if c.reload {
        println!("{}", reload_hint(remote_host));
    }
}

/// `goattyd plugin search [QUERY]`.
pub fn search(svc: &HostService, query: Option<&str>, refresh: bool) -> Result<()> {
    let cat = svc.plugins(|p| Ok(p.catalog(refresh, &svc.settings().value)))?;
    for r in cat.registries.iter().filter(|r| r.error.is_some()) {
        println!("registry {}: {}", r.name, r.error.as_deref().unwrap_or_default());
    }
    let q = query.map(str::to_lowercase);
    let hits: Vec<_> = cat
        .available
        .iter()
        .filter(|a| {
            q.as_deref().is_none_or(|q| {
                a.id.contains(q)
                    || a.name.to_lowercase().contains(q)
                    || a.summary.to_lowercase().contains(q)
                    || a.keywords.iter().any(|k| k.contains(q))
            })
        })
        .collect();
    if hits.is_empty() {
        println!("No plugins found{}.", query.map(|q| format!(" for {q:?}")).unwrap_or_default());
        return Ok(());
    }
    let width = hits.iter().map(|a| a.id.len()).max().unwrap_or(0);
    for a in hits {
        let state = match (&a.installed, &a.incompatible) {
            (Some(v), _) if *v == a.version => "  (installed)".to_string(),
            (Some(v), _) => format!("  (installed {v}; update available)"),
            (None, Some(why)) => format!("  (not for this host: {why})"),
            _ => String::new(),
        };
        println!("{:<width$}  {:<8} {}{}{state}", a.id, a.version, a.summary, badges(&a.permissions));
    }
    println!("\nDetails: goattyd plugin info ID · install: goattyd plugin install ID");
    Ok(())
}

/// `goattyd plugin info ID`.
pub fn info(svc: &HostService, id: &str) -> Result<()> {
    let c = svc.plugins(|p| match p.find(id, None) {
        Ok(c) => Ok(c),
        Err(e) => match p.find_installed(id)? {
            Some(c) => Ok(c),
            None => Err(e),
        },
    })?;
    // What this host would use of it.
    let m = &c.manifest.for_platform(&goatty_plugin::manifest::Platform::host());
    println!("{} {}  ({})", m.name, m.version, m.id);
    println!("  {}", m.summary);
    if !m.description.trim().is_empty() {
        for line in m.description.trim().lines() {
            println!("  {line}");
        }
    }
    println!("  publisher  {}", m.publisher);
    if let Some(h) = &m.homepage {
        println!("  homepage   {h}");
    }
    let how = match (c.builtin, c.verified) {
        (Some(_), _) => " (comes with goatty)",
        (None, true) => " (signed registry)",
        (None, false) => " (local, unverified)",
    };
    println!("  source     {}{how}", c.source);
    let stored = svc.settings().value.plugins.get(id).map(|p| p.settings.clone()).unwrap_or_default();
    let (values, _) = schema::resolve(m, &stored);
    if !m.settings.is_empty() {
        println!("\nSettings (goattyd plugin set {id} KEY VALUE):");
        for s in &m.settings {
            let v = values.get(&s.id).map(schema::plain_string).unwrap_or_default();
            let kind = match s.kind {
                SettingKind::Choice => s.choices.iter().map(|c| c.value.as_str()).collect::<Vec<_>>().join("|"),
                k => k.label().to_owned(),
            };
            println!("  {:<18} {v:<12} {} ({kind})", s.id, s.label);
            if !s.help.is_empty() {
                println!("  {:<18} {:<12} {}", "", "", s.help);
            }
        }
    }
    println!("\nPermissions:");
    for g in goatty_plugin::permissions::describe(m) {
        println!("  {} {}{}", if g.permission.runs_code() { "⚠" } else { "•" }, g.title, details(&g.details));
        if !g.reason.is_empty() {
            println!("      why: {}", g.reason);
        }
    }
    Ok(())
}

fn details(d: &[String]) -> String {
    if d.is_empty() { String::new() } else { format!(": {}", d.join(", ")) }
}

/// Print an install plan the way the user approves it: the plugins it
/// needs first, then the plugin.
pub fn print_plan(plan: &Plan) {
    if !plan.dependencies.is_empty() {
        let names: Vec<String> = plan.dependencies.iter().map(|d| safe(&d.name)).collect();
        println!(
            "{} needs plugins that aren't installed here: {}. They're installed first, as planned below.\n",
            safe(&plan.name),
            names.join(", ")
        );
    }
    for p in plan.all() {
        print_one(p);
    }
}

/// The approval question for `plan`: "Install Base and Addon?".
pub fn question(plan: &Plan) -> String {
    let verb = match &plan.replaces {
        Some(old) if *old != plan.version => "Update",
        Some(_) => "Reinstall",
        None => "Install",
    };
    let mut names: Vec<&str> = plan.all().map(|p| p.name.as_str()).collect();
    let last = names.pop().unwrap_or_default();
    if names.is_empty() { format!("{verb} {last}?") } else { format!("{verb} {} and {last}?", names.join(", ")) }
}

fn print_one(plan: &Plan) {
    let name = safe(&plan.name);
    let version = safe(&plan.version);
    let verb = match &plan.replaces {
        Some(old) if *old != plan.version => format!("Update {name} {} → {version}", safe(old)),
        Some(_) => format!("Reinstall {name} {version}"),
        None => format!("Install {name} {version}"),
    };
    let from = if plan.builtin {
        "goatty (built in)".to_string()
    } else if plan.verified {
        format!("registry {:?} (signature checked)", safe(&plan.source))
    } else {
        safe(&plan.source)
    };
    println!("{verb} from {from}");
    println!("  {}", safe(&plan.summary));
    let mut who = vec![format!("by {}", safe(&plan.publisher))];
    who.extend(plan.homepage.as_deref().map(safe));
    who.extend(plan.license.as_deref().map(safe));
    println!("  {}", who.join(" · "));

    if !plan.downloads.is_empty() {
        println!("\nDownloads into goatty's own directory (no sudo; checked against the sha256 before anything is unpacked):");
        for d in &plan.downloads {
            let pkg = safe(&d.package);
            let ver = safe(&d.version);
            match &d.system {
                Some(p) => println!("  {pkg} {ver}: already installed at {}; goatty uses that copy", safe(p)),
                None if d.current => println!("  {pkg} {ver}: goatty's copy is already installed"),
                None => {
                    println!("  {pkg} {ver}  {}  {}", size(d.size), safe(&d.url));
                    println!("      sha256 {}", safe(&d.sha256));
                }
            }
        }
        let bins: Vec<String> = plan.downloads.iter().flat_map(|d| d.bins.iter().map(|b| safe(b))).collect();
        if !bins.is_empty() {
            println!("  On PATH in goatty shells: {}", bins.join(", "));
        }
        let scripts: Vec<String> = plan.downloads.iter().filter_map(|d| d.script.as_deref().map(safe)).collect();
        if !scripts.is_empty() {
            println!("  Sourced by goatty shells: {}", scripts.join(", "));
        }
    }
    println!("\nPermissions:");
    for g in &plan.permissions {
        println!("  {} {}{}", if g.permission.runs_code() { "⚠" } else { "•" }, safe(&g.title), safe(&details(&g.details)));
        if !g.reason.is_empty() {
            println!("      why: {}", safe(&g.reason));
        }
        println!("      {}", safe(&g.meaning));
    }
    if !plan.aliases.is_empty() {
        println!("\nAliases:");
        for a in &plan.aliases {
            println!("  {}", safe(a));
        }
    }
    if !plan.env.is_empty() {
        println!("\nEnvironment:");
        for e in &plan.env {
            println!("  {}", safe(e));
        }
    }
    if let Some((file, code)) = &plan.shell_code {
        println!("\nShell code ({}), run at the start of every goatty shell:", safe(file));
        for line in code.lines() {
            println!("  │ {}", safe(line));
        }
    }
    for r in &plan.requires {
        println!("\nNeeds {}", safe(r));
    }
    for w in &plan.warnings {
        println!("\n⚠ {}", safe(w));
    }
    println!();
}

/// `goattyd plugin install ID[@VERSION]… | --path DIR`.
pub fn install(svc: &HostService, targets: &[String], path: Option<&Path>, yes: bool) -> Result<()> {
    let mut candidates = Vec::new();
    if let Some(dir) = path {
        candidates.push(Candidate::local(dir)?);
    }
    for t in targets {
        let (id, version) = match t.split_once('@') {
            Some((id, v)) => (id, Some(v)),
            None => (t.as_str(), None),
        };
        candidates.push(svc.plugins(|p| p.find(id, version))?);
    }
    if candidates.is_empty() {
        bail!("name the plugins to install (goattyd plugin search lists them), or pass --path DIR");
    }
    let settings = svc.settings().value;
    for c in candidates {
        let plan = svc.plugins(|p| p.plan(&c, &settings))?;
        print_plan(&plan);
        if !yes && !approve(&question(&plan))? {
            println!("Not installed.");
            continue;
        }
        print_changed(&svc.install_plugin(&c, &plan.digest)?);
    }
    Ok(())
}

/// `goattyd plugin update [ID…]`: newer versions from the registries.
pub fn update(svc: &HostService, ids: &[String], yes: bool) -> Result<()> {
    let cat = svc.plugins(|p| Ok(p.catalog(true, &svc.settings().value)))?;
    let todo: Vec<_> = cat.installed.iter().filter(|i| i.update.is_some() && (ids.is_empty() || ids.contains(&i.id))).collect();
    if todo.is_empty() {
        println!("Everything is up to date.");
        return Ok(());
    }
    let settings = svc.settings().value;
    for i in todo {
        let c = svc.plugins(|p| p.find(&i.id, i.update.as_deref()))?;
        let plan = svc.plugins(|p| p.plan(&c, &settings))?;
        print_plan(&plan);
        if !yes && !approve(&question(&plan))? {
            println!("Not updated.");
            continue;
        }
        print_changed(&svc.install_plugin(&c, &plan.digest)?);
    }
    Ok(())
}

/// `goattyd plugin uninstall ID…`. Offers to remove the plugins that
/// were installed only for these and that nothing needs now (`yes`:
/// without asking).
pub fn uninstall(svc: &HostService, ids: &[String], yes: bool) -> Result<()> {
    if ids.is_empty() {
        bail!("name the plugins to remove");
    }
    let mut unneeded = Vec::new();
    for id in ids {
        let c = svc.uninstall_plugin(id)?;
        print_changed(&c);
        unneeded = c.unneeded;
    }
    // Ones named on the command line are gone already.
    unneeded.retain(|u| !ids.contains(u));
    if unneeded.is_empty() {
        return Ok(());
    }
    let ask = !yes && std::io::stdin().is_terminal();
    if !yes && !ask {
        println!("Remove them with: goattyd plugin uninstall {}", unneeded.join(" "));
        return Ok(());
    }
    if ask && !approve(&format!("Also uninstall {}?", unneeded.join(", ")))? {
        return Ok(());
    }
    for id in &unneeded {
        print_changed(&svc.uninstall_plugin(id)?);
    }
    Ok(())
}

/// `goattyd plugin repair [ID…]`: download missing (or outdated) managed
/// tools for already-installed plugins without changing their versions.
pub fn repair(svc: &HostService, ids: &[String], yes: bool) -> Result<()> {
    let settings = svc.settings().value;
    let (installed, issues) = svc.plugins(|p| Ok(p.installed(&settings)))?;
    for issue in &issues {
        println!("⚠ {issue}");
    }
    let todo = select_repair_targets(&installed, ids)?;
    if ids.is_empty() {
        for i in installed
            .iter()
            .filter(|i| !i.builtin && i.enabled && i.incompatible.is_none() && i.packages.iter().any(|p| !p.available && p.needs_install()))
        {
            let missing: Vec<&str> = i.packages.iter().filter(|p| !p.available && p.needs_install()).map(|p| p.id.as_str()).collect();
            println!(
                "{} has no build for this host ({}); install it with your package manager.",
                safe(&i.manifest.name),
                missing.join(", ")
            );
        }
    }
    if todo.is_empty() {
        if ids.is_empty() {
            println!("All installed plugin tools are present.");
            if installed
                .iter()
                .any(|i| i.builtin && i.enabled && i.incompatible.is_none() && i.packages.iter().any(|p| p.available && p.needs_install()))
            {
                println!("Built-in shell tools can be installed with `goattyd shell install`.");
            }
        }
        return Ok(());
    }
    for i in todo {
        let Some(c) = svc.plugins(|p| p.find_installed(&i.id))? else {
            bail!("{} isn't installed", i.id);
        };
        let plan = svc.plugins(|p| p.plan(&c, &settings))?;
        if !plan.downloads.iter().any(|d| d.fetches()) {
            println!("{} already has its tools installed.", safe(&plan.name));
            continue;
        }
        print_plan(&plan);
        if !yes && !approve(&question(&plan))? {
            println!("Not installed.");
            continue;
        }
        print_changed(&svc.install_plugin(&c, &plan.digest)?);
    }
    Ok(())
}

/// Which installed plugins `goattyd plugin repair [ids...]` should download
/// tools for. Explicit `ids` are validated against `installed` (and checked
/// for compatibility); without `ids`, every enabled non-builtin plugin with a
/// missing or outdated downloadable package is picked.
fn select_repair_targets<'a>(installed: &'a [Installed], ids: &[String]) -> Result<Vec<&'a Installed>> {
    if ids.is_empty() {
        return Ok(installed
            .iter()
            .filter(|i| !i.builtin && i.enabled && i.incompatible.is_none() && i.packages.iter().any(|p| p.available && p.needs_install()))
            .collect());
    }
    let mut selected = Vec::new();
    for id in ids {
        let Some(inst) = installed.iter().find(|i| i.id == *id) else {
            bail!("{id:?} isn't installed (try `goattyd plugin list`)");
        };
        if !selected.iter().any(|r: &&Installed| r.id == inst.id) {
            selected.push(inst);
        }
    }
    let mut todo = Vec::new();
    for inst in selected {
        if let Some(why) = &inst.incompatible {
            bail!("{} can't be repaired here: {why}", inst.manifest.name);
        }
        if !inst.packages.iter().any(|p| p.needs_install()) {
            println!("{} already has its tools installed.", safe(&inst.manifest.name));
            continue;
        }
        todo.push(inst);
    }
    Ok(todo)
}

/// Parenthetical status notes for one plugin in `goattyd plugin list`.
fn list_notes(i: &Installed) -> Vec<String> {
    let mut notes = Vec::new();
    if !i.enabled {
        notes.push(if i.builtin { "off" } else { "disabled" }.to_string());
    }
    if i.builtin {
        notes.push("built in".into());
    } else if !i.verified {
        notes.push("local, unverified".into());
    }
    if i.incompatible.is_none() {
        for p in i.packages.iter().filter(|p| p.needs_install()) {
            let cmd = if i.builtin { format!("goattyd shell install {}", i.id) } else { format!("goattyd plugin repair {}", i.id) };
            notes.push(match &p.state {
                PackageState::Managed { version } if p.available => format!("{} {version} → {}: {cmd}", p.id, p.version),
                PackageState::Managed { version } => format!("{} {version} → {} (no build for this host)", p.id, p.version),
                _ if p.available => format!("{} not installed: {cmd}", p.id),
                _ => format!("{} not installed (no build for this host)", p.id),
            });
        }
    }
    if let Some(why) = &i.incompatible {
        notes.push(format!("off: {why}"));
    }
    if let Some(u) = &i.update {
        notes.push(format!("{u} available"));
    }
    notes
}

/// `goattyd plugin list`.
pub fn list(svc: &HostService) -> Result<()> {
    let cat = svc.plugins(|p| Ok(p.catalog(false, &svc.settings().value)))?;
    if cat.installed.iter().all(|i| i.builtin) {
        println!("Only goatty's built-in plugins so far. Browse more with `goattyd plugin search`.");
    }
    let mut needs_repair = false;
    for i in &cat.installed {
        if !i.builtin && i.enabled && i.incompatible.is_none() && i.packages.iter().any(|p| p.available && p.needs_install()) {
            needs_repair = true;
        }
        let notes = list_notes(i);
        let notes = if notes.is_empty() { String::new() } else { format!("  ({})", notes.join("; ")) };
        println!("{:<20} {:<8} {:<10} {}{notes}", i.id, i.version, i.source, i.manifest.summary);
    }
    if needs_repair {
        println!("\nInstall missing plugin tools with `goattyd plugin repair`.");
    }
    for issue in &cat.issues {
        println!("⚠ {issue}");
    }
    Ok(())
}

/// The installed plugin `id` as this host uses it ([`Manifest::for_platform`](goatty_plugin::manifest::Manifest::for_platform)).
fn installed_manifest(svc: &HostService, id: &str) -> Result<goatty_plugin::Manifest> {
    let here = goatty_plugin::manifest::Platform::host();
    if let Some(b) = crate::builtins::get(id) {
        return Ok(b.manifest().for_platform(&here));
    }
    let dirs = svc.launcher.dirs().context("$HOME is not set")?;
    let lock = super::lock::Lock::load(dirs)?;
    let e = lock.plugins.get(id).with_context(|| format!("{id} isn't installed"))?;
    Ok(super::lock::manifest_of(dirs, id, e)?.for_platform(&here))
}

fn print_outcome(o: &crate::service::ApplyOutcome) {
    match o.generation {
        Some(g) if !o.diff.is_empty() => println!("Applied (generation {g}). Running shells: `goattyd shell reload`."),
        _ => println!("Saved; the shell configuration didn't change."),
    }
    for i in &o.issues {
        println!("⚠ {i}");
    }
}

/// `goattyd plugin enable|disable ID`.
pub fn set_enabled(svc: &HostService, id: &str, enabled: bool) -> Result<()> {
    installed_manifest(svc, id)?;
    let id = id.to_owned();
    print_outcome(&svc.edit(|h| h.edit_plugin(&id, |p| p.enabled = enabled))?);
    Ok(())
}

/// `goattyd plugin set ID KEY VALUE` (checked against the plugin's schema),
/// or with `value: None` back to the default.
pub fn set(svc: &HostService, id: &str, key: &str, value: Option<&str>) -> Result<()> {
    let m = installed_manifest(svc, id)?;
    let Some(s) = m.setting(key) else {
        let keys: Vec<&str> = m.settings.iter().map(|s| s.id.as_str()).collect();
        bail!("{} has no setting {key:?}; it has: {}", m.name, keys.join(", "));
    };
    let v = value.map(|t| schema::parse_input(s, t).map_err(anyhow::Error::msg)).transpose()?;
    let (id, key) = (id.to_owned(), key.to_owned());
    print_outcome(&svc.edit(|h| {
        h.edit_plugin(&id, |entry| {
            match v {
                Some(v) => entry.settings.insert(key, v),
                None => entry.settings.remove(&key),
            };
        })
    })?);
    Ok(())
}

/// `goattyd plugin registry list`.
pub fn registries(svc: &HostService, refresh: bool) -> Result<()> {
    let (_, status, issues) = svc.plugins(|p| Ok(p.snapshots(refresh, false)))?;
    for r in status {
        let state = match (&r.error, r.sequence) {
            (Some(e), _) => format!("error: {e}"),
            (None, Some(seq)) => format!("{} plugins, index #{seq}", r.plugins),
            (None, None) => "not fetched yet".into(),
        };
        println!("{:<12} {}{}\n             {state}", r.name, r.url, if r.builtin { " (built in)" } else { "" });
    }
    for i in issues {
        println!("⚠ {i}");
    }
    Ok(())
}

/// `goattyd plugin registry add NAME URL --key HEX…`.
pub fn registry_add(svc: &HostService, name: &str, url: &str, keys: &[String]) -> Result<()> {
    let source = Source { name: name.into(), url: url.into(), keys: keys.to_vec(), builtin: false };
    let dirs = svc.launcher.dirs().context("$HOME is not set")?;
    svc.plugins(|p| registry::add_source(dirs, p.builtin, source.clone()))?;
    println!("Added registry {name}. Its plugins are signed by {}.", keys.join(", "));
    match svc.plugins(|p| registry::refresh(dirs, &source, p.fetch)) {
        Ok(s) => println!("Fetched: {} plugins.", s.index.plugins.len()),
        Err(e) => println!("⚠ couldn't fetch it yet: {e:#}"),
    }
    Ok(())
}

/// `goattyd plugin registry remove NAME`.
pub fn registry_remove(svc: &HostService, name: &str) -> Result<()> {
    let dirs = svc.launcher.dirs().context("$HOME is not set")?;
    if registry::remove_source(dirs, name)? {
        println!("Removed registry {name}. Plugins installed from it stay installed.");
    } else {
        bail!("no added registry called {name:?} (built-in registries can't be removed)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_escapes_control_sequences() {
        assert_eq!(safe("Clean summary"), "Clean summary");
        assert_eq!(safe("evil \x1b[31mred\x07\n"), "evil \\u{1b}[31mred\\u{7}\\n");
    }

    #[test]
    fn reload_hint_names_remote_host_only_when_provided() {
        assert_eq!(reload_hint(None), "Running shells: `goattyd shell reload` (or `goatty-reload` in one shell).");
        assert_eq!(
            reload_hint(Some("goattyd on deploy@box")),
            "Running shells (goattyd on deploy@box): run `goattyd shell reload` there (or `goatty-reload` in one shell)."
        );
        assert!(reload_hint(Some("box\x1b[31m")).contains("box\\u{1b}[31m"));
    }

    fn stub_installed(id: &str, builtin: bool, enabled: bool, packages: Vec<goatty_plugin::api::PackageStatus>) -> Installed {
        let m = goatty_plugin::manifest::parse(
            &format!("schema = 1\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nsummary = \"s\"\npublisher = \"t\"\n"),
            None,
        )
        .unwrap();
        Installed {
            id: id.into(),
            version: "1.0.0".into(),
            source: if builtin { "goatty".into() } else { "official".into() },
            verified: true,
            manifest: m,
            enabled,
            incompatible: None,
            update: None,
            installed_unix: 0,
            builtin,
            packages,
            unavailable_commands: Vec::new(),
            command_open_in: Default::default(),
        }
    }

    #[test]
    fn list_notes_surface_missing_and_outdated_packages_for_non_builtin_plugins() {
        use goatty_plugin::api::PackageStatus;

        let eza = stub_installed(
            "eza",
            false,
            true,
            vec![PackageStatus { id: "eza".into(), version: "0.23.4".into(), state: PackageState::Missing, available: true }],
        );
        assert_eq!(list_notes(&eza), ["eza not installed: goattyd plugin repair eza"]);

        let no_build = stub_installed(
            "custom",
            false,
            true,
            vec![PackageStatus { id: "custom".into(), version: "1.0".into(), state: PackageState::Missing, available: false }],
        );
        assert_eq!(list_notes(&no_build), ["custom not installed (no build for this host)"]);

        let outdated = stub_installed(
            "bat",
            false,
            true,
            vec![PackageStatus {
                id: "bat".into(),
                version: "0.25.0".into(),
                state: PackageState::Managed { version: "0.24.0".into() },
                available: true,
            }],
        );
        assert_eq!(list_notes(&outdated), ["bat 0.24.0 → 0.25.0: goattyd plugin repair bat"]);

        let builtin_missing = stub_installed(
            "fzf",
            true,
            true,
            vec![PackageStatus { id: "fzf".into(), version: "0.60.0".into(), state: PackageState::Missing, available: true }],
        );
        assert_eq!(list_notes(&builtin_missing), ["built in", "fzf not installed: goattyd shell install fzf"]);

        let mut incompat = eza.clone();
        incompat.incompatible = Some("supports windows only".into());
        assert_eq!(list_notes(&incompat), ["off: supports windows only"]);
    }

    #[test]
    fn select_repair_targets_filters_defaults_and_validates_explicit_ids() {
        use goatty_plugin::api::PackageStatus;

        let missing = |id: &str, available: bool| {
            vec![PackageStatus { id: id.into(), version: "1.0".into(), state: PackageState::Missing, available }]
        };
        let present = |id: &str| {
            vec![PackageStatus {
                id: id.into(),
                version: "1.0".into(),
                state: PackageState::Managed { version: "1.0".into() },
                available: true,
            }]
        };
        let list = vec![
            stub_installed("fzf", true, true, missing("fzf", true)),
            stub_installed("eza", false, true, missing("eza", true)),
            stub_installed("disabled-p", false, false, missing("disabled-p", true)),
            stub_installed("nobuild", false, true, missing("nobuild", false)),
            stub_installed("healthy", false, true, present("healthy")),
        ];
        // Default: only enabled non-builtin plugins with available missing packages.
        let default_ids: Vec<&str> = select_repair_targets(&list, &[]).unwrap().into_iter().map(|i| i.id.as_str()).collect();
        assert_eq!(default_ids, ["eza"]);

        // Explicit ids can target builtins or disabled plugins, deduplicate, and skip healthy ones.
        let explicit: Vec<&str> = select_repair_targets(&list, &["fzf".into(), "fzf".into(), "healthy".into(), "disabled-p".into()])
            .unwrap()
            .into_iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(explicit, ["fzf", "disabled-p"]);

        // Unknown id errors.
        assert!(select_repair_targets(&list, &["ghost".into()]).unwrap_err().to_string().contains("isn't installed"));
    }

    #[test]
    fn question_uses_reinstall_for_same_version() {
        let mut p = Plan {
            id: "eza".into(),
            name: "eza".into(),
            version: "1.0.0".into(),
            summary: String::new(),
            description: String::new(),
            publisher: "t".into(),
            homepage: None,
            license: None,
            source: "official".into(),
            verified: true,
            builtin: false,
            replaces: None,
            downloads: Vec::new(),
            permissions: Vec::new(),
            aliases: Vec::new(),
            env: Vec::new(),
            files: Vec::new(),
            shell_code: None,
            requires: Vec::new(),
            warnings: Vec::new(),
            dependencies: Vec::new(),
            digest: "d".into(),
            previous: None,
        };
        assert_eq!(question(&p), "Install eza?");
        p.replaces = Some("1.0.0".into());
        assert_eq!(question(&p), "Reinstall eza?");
        p.replaces = Some("0.9.0".into());
        assert_eq!(question(&p), "Update eza?");
    }

    #[cfg(unix)]
    #[test]
    fn repair_command_downloads_missing_binary_without_upgrading_version() {
        use std::sync::Arc;

        let w = crate::plugins::tests::world();
        let file = w.tmp.path().join("config/host.json");
        let svc = HostService::new(crate::launch::Launcher::new(Some(w.dirs.clone()), None), Some(file))
            .with_plugin_sources(Arc::new(w.fetch.clone()), w.sources.clone());

        // Install `demo` 1.0.0, then delete its binary and record it as if a
        // previously-used system binary vanished.
        let c = svc.plugins(|p| p.find("demo", None)).unwrap();
        let plan = svc.plugins(|p| p.plan(&c, &svc.settings().value)).unwrap();
        svc.install_plugin(&c, &plan.digest).unwrap();
        crate::install::uninstall_package("demo", &["demo".into()], &w.dirs).unwrap();
        let mut lock = super::super::lock::Lock::load(&w.dirs).unwrap();
        lock.plugins.get_mut("demo").unwrap().packages[0].system = Some("/nix/store/gone/bin/demo".into());
        lock.save(&w.dirs).unwrap();
        assert!(!w.dirs.bin_dir().join("demo").exists());

        // `goattyd plugin repair --yes` restores the managed binary.
        repair(&svc, &[], true).unwrap();
        assert!(w.dirs.bin_dir().join("demo").is_file());
        let lock = super::super::lock::Lock::load(&w.dirs).unwrap();
        assert_eq!(lock.plugins["demo"].version, "1.0.0");
        assert_eq!(lock.plugins["demo"].packages[0].system, None);
    }
}
