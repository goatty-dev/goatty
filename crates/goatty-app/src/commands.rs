//! `goatty settings …` and `goatty theme …`: everything the settings window
//! does with profiles and themes, without a window (scripts, SSH sessions,
//! dotfile setups). Host settings go through the local goattyd when it
//! runs, else straight to this machine's `host.json`; `goattyd settings`
//! on a server edits that host's file.

use std::collections::BTreeSet;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use goatty_client::{Connection, Control, Endpoint, RemoteProblem, ReplyLost};
use goatty_config::theme_import;
use goatty_config::themes::ThemeFile;
use goatty_config::{ClientSettings, Document, HostSettings, Loaded, ThemeLibrary, store};
use goatty_plugin::api;
use goatty_shell::HostService;

use crate::cli::{SettingsCmd, ThemeCmd, ThemeFor};

const CLIENT: &str = concat!("goatty-cli ", env!("CARGO_PKG_VERSION"));
const VIA_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_APPLY_TIMEOUT: Duration = Duration::from_secs(60);

fn client_path() -> Result<PathBuf> {
    goatty_config::client_file().context("$HOME is not set: goatty has no settings directory")
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Escape control characters in daemon-provided strings before printing to the terminal.
fn sanitize_terminal(s: &str) -> String {
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

fn print_issues(label: &str, issues: &[String]) {
    let label = sanitize_terminal(label);
    for i in issues {
        eprintln!("⚠ {label}: {}", sanitize_terminal(i));
    }
}

/// The host side: a connected goattyd (remote over `--ssh`/`--via`, or the
/// running local one), else this machine's files.
enum Host {
    Remote(Arc<Connection>),
    Daemon(Control),
    Local(Box<HostService>),
}

impl Host {
    fn open(endpoint: Option<Endpoint>) -> Result<Self> {
        let local_daemon = || {
            let name = if cfg!(windows) { "goattyd.exe" } else { "goattyd" };
            std::env::current_exe().ok().map(|e| e.with_file_name(name)).filter(|d| d.exists())
        };
        match endpoint {
            None => Ok(Host::Local(Box::new(HostService::from_env(local_daemon())))),
            Some(Endpoint::Local) => Ok(match Control::connect_local(CLIENT) {
                Ok(c) => Host::Daemon(c),
                Err(_) => Host::Local(Box::new(HostService::from_env(local_daemon()))),
            }),
            Some(remote) => {
                let custom_daemon = match &remote {
                    Endpoint::Ssh(t) => t.remote_daemon.clone(),
                    _ => None,
                };
                // Bound `--via` handshakes so a silent relay cannot hang the CLI; leave
                // SSH unbounded so interactive password/passphrase prompts aren't cut short.
                let handshake_timeout = matches!(&remote, Endpoint::Command { .. }).then_some(VIA_HANDSHAKE_TIMEOUT);
                let conn = Connection::open_once(remote, CLIENT, handshake_timeout).map_err(|e| {
                    let hint = match e.downcast_ref::<RemoteProblem>() {
                        Some(RemoteProblem::NotInstalled { host, platform }) if platform.target().is_some() => match &custom_daemon {
                            Some(path) => Some(format!(
                                "check `--remote-daemon {path}` on {host}, or drop `--remote-daemon` and run `goatty --ssh {host}` once to install goattyd there"
                            )),
                            None => Some(format!("run `goatty --ssh {host}` once to install goattyd there")),
                        },
                        Some(RemoteProblem::Incompatible { host, mismatch, .. }) if !mismatch.daemon_is_newer => match &custom_daemon {
                            Some(path) => Some(format!(
                                "update `{path}` on {host}, or drop `--remote-daemon` and run `goatty --ssh {host}` to update goattyd there"
                            )),
                            None => Some(format!("run `goatty --ssh {host}` to update goattyd there")),
                        },
                        _ => None,
                    };
                    let sanitized = anyhow::anyhow!("{}", sanitize_terminal(&format!("{e:#}")));
                    match hint {
                        Some(msg) => sanitized.context(sanitize_terminal(&msg)),
                        None => sanitized,
                    }
                })?;
                Ok(Host::Remote(conn))
            }
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self, Host::Remote(_))
    }

    fn label(&self) -> String {
        match self {
            Host::Remote(c) => {
                let h = c.host();
                match (h.user.is_empty(), h.hostname.is_empty()) {
                    (false, false) => format!("goattyd on {}@{}", sanitize_terminal(&h.user), sanitize_terminal(&h.hostname)),
                    (true, false) => format!("goattyd on {}", sanitize_terminal(&h.hostname)),
                    _ => format!("goattyd via {}", sanitize_terminal(c.endpoint().label())),
                }
            }
            Host::Daemon(c) => format!("goattyd on {}", sanitize_terminal(&c.host().hostname)),
            Host::Local(_) => "this machine (no goattyd running)".into(),
        }
    }

    fn json(&mut self) -> Result<String> {
        match self {
            Host::Remote(c) => Ok(c.host_settings().map_err(|e| anyhow::anyhow!("{}", sanitize_terminal(&format!("{e:#}"))))?.json),
            Host::Daemon(c) => c.host_settings_json(),
            Host::Local(s) => Ok(s.snapshot().json),
        }
    }

    /// Validate, build the generation, save. Returns the issues.
    fn apply(&mut self, json: String) -> Result<Vec<String>> {
        match self {
            Host::Remote(c) => {
                let r = match c.set_host_settings_within(json, true, REMOTE_APPLY_TIMEOUT) {
                    Ok(r) => r,
                    Err(e) if e.downcast_ref::<ReplyLost>().is_some() => {
                        bail!(
                            "lost confirmation while applying host settings on {}; the daemon may still have applied them — check the host before retrying",
                            self.label()
                        );
                    }
                    Err(e) => bail!("{}", sanitize_terminal(&format!("{e:#}"))),
                };
                if !r.applied {
                    bail!("host settings were not applied ({})", self.label());
                }
                Ok(r.issues)
            }
            Host::Daemon(c) => c.apply_host_settings(json),
            Host::Local(s) => Ok(s.set(&json, true)?.issues),
        }
    }

    fn plugin(&mut self, req: api::Request) -> Result<api::Reply> {
        match self {
            Host::Remote(c) => {
                let raw = c.plugin(serde_json::to_string(&req)?).map_err(|e| {
                    if e.downcast_ref::<ReplyLost>().is_some() { e } else { anyhow::anyhow!("{}", sanitize_terminal(&format!("{e:#}"))) }
                })?;
                Ok(serde_json::from_str(&raw)?)
            }
            Host::Daemon(c) => Ok(serde_json::from_str(&c.plugin(serde_json::to_string(&req)?)?)?),
            Host::Local(s) => s.plugin_request(req),
        }
    }
}

fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!("not asking without a terminal; pass --yes");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn missing_plugin_issue_id(issue: &str) -> Option<&str> {
    let rest = issue.strip_prefix("plugins.")?;
    let (id, msg) = rest.split_once(": ")?;
    msg.starts_with("not installed on this host; ignored").then_some(id)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ImportPluginReport {
    listed: Vec<String>,
    installed: Vec<String>,
    skipped: Vec<String>,
    failed: Vec<String>,
}

fn offer_imported_plugins(
    target: &mut Host,
    profile_plugins: &[(String, bool)],
    apply_issues: &[String],
    yes: bool,
) -> Result<ImportPluginReport> {
    offer_imported_plugins_with(target, profile_plugins, apply_issues, yes, std::io::stdin().is_terminal(), |q| confirm(q, false))
}

fn offer_imported_plugins_with(
    target: &mut Host,
    profile_plugins: &[(String, bool)],
    apply_issues: &[String],
    yes: bool,
    is_terminal: bool,
    mut ask: impl FnMut(&str) -> Result<bool>,
) -> Result<ImportPluginReport> {
    let mut report = ImportPluginReport::default();
    let on_host = if target.is_remote() { " on that host" } else { "" };
    let cat = match target.plugin(api::Request::Catalog { refresh: false }) {
        Ok(api::Reply::Catalog(c)) => c,
        Ok(other) => {
            print_issues("host", apply_issues);
            println!("Host settings applied ({}).", target.label());
            eprintln!("⚠ Couldn't check plugins: unexpected reply {}", sanitize_terminal(&format!("{other:?}")));
            return Ok(report);
        }
        Err(e) => {
            print_issues("host", apply_issues);
            println!("Host settings applied ({}).", target.label());
            eprintln!("⚠ Couldn't check plugins: {}", sanitize_terminal(&format!("{e:#}")));
            let ids: Vec<String> = profile_plugins.iter().map(|(id, _)| sanitize_terminal(id)).collect();
            println!("Review and install profile plugins{on_host} with: goattyd plugin install {}", ids.join(" "));
            return Ok(report);
        }
    };

    let installed_ids: BTreeSet<&str> = cat.installed.iter().map(|i| i.id.as_str()).collect();
    let missing: Vec<(String, bool)> = profile_plugins.iter().filter(|(id, _)| !installed_ids.contains(id.as_str())).cloned().collect();
    let missing_ids: BTreeSet<&str> = missing.iter().map(|(id, _)| id.as_str()).collect();

    let other_issues: Vec<String> =
        apply_issues.iter().filter(|issue| missing_plugin_issue_id(issue).is_none_or(|id| !missing_ids.contains(id))).cloned().collect();
    print_issues("host", &other_issues);
    println!("Host settings applied ({}).", target.label());

    if missing.is_empty() {
        return Ok(report);
    }

    let reg_errors = cat.registries.iter().any(|r| r.error.is_some());
    let mut installable = Vec::new();
    let mut unfound = false;
    println!("\n{} plugin(s) in this profile are not installed ({}):", missing.len(), target.label());
    for (id, enabled) in &missing {
        report.listed.push(id.clone());
        let safe_id = sanitize_terminal(id);
        let disabled_note = if !enabled { " [disabled in profile]" } else { "" };
        match cat.available.iter().find(|a| a.id == *id) {
            Some(a) if a.incompatible.is_none() => {
                println!(
                    "  {} ({}) {} — {}{disabled_note}",
                    sanitize_terminal(&a.id),
                    sanitize_terminal(&a.registry),
                    sanitize_terminal(&a.version),
                    sanitize_terminal(&a.summary),
                );
                installable.push((id.clone(), *enabled));
            }
            Some(a) => {
                let why = sanitize_terminal(a.incompatible.as_deref().unwrap_or("not compatible"));
                println!("  {} — not for this host ({why}){disabled_note}", sanitize_terminal(&a.id));
            }
            None if reg_errors => {
                println!("  {safe_id} — not in a readable registry on this host{disabled_note}");
            }
            None => {
                println!("  {safe_id} — not in any registry on this host{disabled_note}");
                unfound = true;
            }
        }
    }
    for r in cat.registries.iter().filter(|r| r.error.is_some()) {
        eprintln!("⚠ registry {}: {}", sanitize_terminal(&r.name), sanitize_terminal(r.error.as_deref().unwrap_or_default()),);
    }

    if installable.is_empty() {
        if unfound {
            println!(
                "Add a registry{on_host} with `goattyd plugin registry add …` or install{on_host} from a directory with `goattyd plugin install --path DIR`."
            );
        }
        return Ok(report);
    }

    let ids: Vec<String> = installable.iter().map(|(id, _)| sanitize_terminal(id)).collect();
    if yes || !is_terminal {
        println!("Review and install{on_host} with: goattyd plugin install {}", ids.join(" "));
        return Ok(report);
    }

    if !ask("Review and install them now?")? {
        println!("Install later{on_host} with: goattyd plugin install {}", ids.join(" "));
        report.skipped.extend(installable.into_iter().map(|(id, _)| id));
        return Ok(report);
    }

    // A plan includes the plugins it needs: ones installed along with an
    // earlier plugin aren't asked about again.
    for (id, enabled) in installable {
        if report.installed.contains(&id) {
            continue;
        }
        let safe_id = sanitize_terminal(&id);
        let plan = match target.plugin(api::Request::Plan { id: id.clone(), version: None }) {
            Ok(api::Reply::Plan(p)) => p,
            Ok(other) => {
                eprintln!("⚠ Couldn't plan {safe_id}: unexpected reply {}", sanitize_terminal(&format!("{other:?}")));
                report.failed.push(id);
                continue;
            }
            Err(e) => {
                eprintln!("⚠ Couldn't install {safe_id}: {}", sanitize_terminal(&format!("{e:#}")));
                report.failed.push(id);
                continue;
            }
        };
        if !enabled {
            println!("Note: {safe_id} was disabled in the imported profile; installing it will enable it.");
        }
        goatty_shell::plugins::cmd::print_plan(&plan);
        if !ask(&sanitize_terminal(&goatty_shell::plugins::cmd::question(&plan)))? {
            println!("Skipped {}.", sanitize_terminal(&plan.name));
            report.skipped.push(id);
            continue;
        }
        match target.plugin(api::Request::Install {
            id: plan.id.clone(),
            version: Some(plan.version.clone()),
            approved: plan.digest.clone(),
        }) {
            Ok(api::Reply::Changed(c)) => {
                let remote_host = target.is_remote().then(|| target.label());
                goatty_shell::plugins::cmd::print_changed_for(&c, remote_host.as_deref());
                report.installed.extend(plan.all().map(|p| p.id.clone()));
            }
            Ok(other) => {
                eprintln!("⚠ Couldn't install {safe_id}: unexpected reply {}", sanitize_terminal(&format!("{other:?}")));
                report.failed.push(id);
            }
            Err(e) if e.downcast_ref::<ReplyLost>().is_some() => {
                eprintln!(
                    "⚠ Lost confirmation while installing {safe_id} ({}); it may still have been installed — check the host before retrying",
                    target.label()
                );
                report.failed.push(id);
            }
            Err(e) => {
                eprintln!("⚠ Couldn't install {safe_id}: {}", sanitize_terminal(&format!("{e:#}")));
                report.failed.push(id);
            }
        }
    }
    Ok(report)
}

/// Print a document: one key, every value (`all`), or what's stored.
fn show<T: Document + serde::Serialize>(loaded: &Loaded<T>, effective: &T, key: Option<&str>, all: bool) -> Result<()> {
    match key {
        Some(k) => {
            let v = store::get_key(effective, k).with_context(|| format!("no setting {k:?}"))?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        None if all => println!("{}", serde_json::to_string_pretty(effective)?),
        None => print!("{}", store::to_json(&loaded.value, &loaded.unknown)),
    }
    Ok(())
}

fn parse_host_settings_for_export(raw: &str, remote: bool, label: &str) -> Result<HostSettings> {
    if remote && !matches!(serde_json::from_str::<serde_json::Value>(raw), Ok(serde_json::Value::Object(_))) {
        bail!("unexpected host settings from {label}: expected a JSON object");
    }
    Ok(store::parse::<HostSettings>(raw).value)
}

pub fn settings(cmd: SettingsCmd, endpoint: Option<Endpoint>) -> Result<()> {
    match cmd {
        SettingsCmd::Path => {
            let dir = goatty_config::config_dir().context("$HOME is not set")?;
            println!("client  {}", dir.join("settings.json").display());
            println!("host    {}  (this machine; goattyd on other hosts keeps its own)", dir.join("host.json").display());
            println!("themes  {}", dir.join("themes").display());
            Ok(())
        }
        SettingsCmd::Show { key, all } => {
            let c = goatty_config::load_client();
            print_issues("settings.json", &c.file.issues.iter().map(ToString::to_string).collect::<Vec<_>>());
            if !c.env_overrides.is_empty() {
                eprintln!("(overridden by {})", c.env_overrides.join(", "));
            }
            show(&c.file, &c.value, key.as_deref(), all)
        }
        SettingsCmd::Validate { file } => {
            let path = match file {
                Some(f) => f,
                None => client_path()?,
            };
            let text = read_or_empty(&path)?;
            let loaded = store::parse::<ClientSettings>(&text);
            let mut problems: Vec<String> = loaded.issues.iter().map(ToString::to_string).collect();
            let lib = ThemeLibrary::load_default();
            problems.extend(lib.issues.iter().map(ToString::to_string));
            problems.extend(lib.check(&loaded.value.theme).iter().map(ToString::to_string));
            problems.extend(ClientSettings::lossy_colors(&text).iter().map(ToString::to_string));
            problems.extend(lib.lossy_colors().iter().map(ToString::to_string));
            // The checks the settings window makes, with the installed fonts
            // read from disk since there's no GPUI here (#150). The file's
            // values, without `GOATTY_*` overrides: this checks the file.
            let fonts = crate::fonts::installed_families();
            problems.extend(loaded.value.check_environment(&fonts, crate::settings::has_program).iter().map(ToString::to_string));
            // Keystrokes goatty can't read are already in `issues`; this
            // catches what only GPUI rejects.
            for key in loaded.value.keybindings.keys().filter(|k| goatty_config::keys::canonical_keystroke(k).is_ok()) {
                if gpui::Keystroke::parse(Box::leak(key.clone().into_boxed_str())).is_err() {
                    problems.push(format!("keybindings.{key}: not a valid keystroke"));
                }
            }
            if problems.is_empty() {
                println!("{}: OK", path.display());
                Ok(())
            } else {
                print_issues(&path.display().to_string(), &problems);
                bail!("{} problem(s); goatty uses fallbacks for them", problems.len())
            }
        }
        SettingsCmd::Set { key, value } => {
            let path = client_path()?;
            let loaded = store::set_key::<ClientSettings>(&read_or_empty(&path)?, &key, &value).map_err(anyhow::Error::msg)?;
            store::save(&path, &loaded.value, &loaded.unknown)?;
            println!("{key} = {}", store::get_key(&loaded.value, &key).map(|v| v.to_string()).unwrap_or_default());
            // Colors are stored as #rrggbb: say what a CSS color lost on the way
            // (`value` read as `set_key` reads it: JSON, else a string).
            if key == "theme" || key.starts_with("theme.") {
                let raw = serde_json::from_str(&value).unwrap_or_else(|_| serde_json::Value::String(value.clone()));
                for (path, loss) in goatty_config::colorspace::lossy_colors(&raw, &key) {
                    eprintln!("note: {path}: {loss}");
                }
            }
            Ok(())
        }
        SettingsCmd::Unset { key } => {
            let path = client_path()?;
            let loaded = store::unset_key::<ClientSettings>(&read_or_empty(&path)?, &key).map_err(anyhow::Error::msg)?;
            store::save(&path, &loaded.value, &loaded.unknown)?;
            println!("{key} = {} (default)", store::get_key(&loaded.value, &key).map(|v| v.to_string()).unwrap_or_default());
            Ok(())
        }
        SettingsCmd::Export { file, .. } => {
            let client = goatty_config::load_client().file.value;
            let mut host = Host::open(endpoint)?;
            let raw = host.json()?;
            let host_settings = parse_host_settings_for_export(&raw, host.is_remote(), &host.label())?;
            let text = goatty_config::profile::export(&client, &host_settings);
            if file.as_os_str() == "-" {
                print!("{text}");
            } else {
                std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;
                eprintln!("Exported this computer's settings and {}'s shell settings to {}.", host.label(), file.display());
            }
            Ok(())
        }
        SettingsCmd::Import { file, yes, .. } => {
            let text = if file.as_os_str() == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?
            };
            let (client, host) = goatty_config::profile::import(&text).map_err(anyhow::Error::msg)?;
            if !yes && !std::io::stdin().is_terminal() {
                bail!("not asking without a terminal; pass --yes");
            }
            let remote_label = endpoint.as_ref().filter(|e| !e.is_local()).map(|e| sanitize_terminal(e.label()));
            let mut target = if host.is_some() { Some(Host::open(endpoint)?) } else { None };
            if host.is_none()
                && let Some(label) = remote_label
            {
                eprintln!("Note: this profile has no host settings; nothing will be sent to {label}.");
            }
            println!("This profile replaces:");
            if client.is_some() {
                println!("  appearance, shortcuts and workspace settings on this computer");
            }
            if let Some(t) = &target {
                println!("  shell, prompt and history settings of {}", t.label());
            }
            println!("(the current files are kept in backups/, and the shell configuration can be rolled back)");
            if !confirm("Import?", yes)? {
                println!("Cancelled.");
                return Ok(());
            }
            if let Some(c) = client {
                print_issues("client", &c.issues.iter().map(ToString::to_string).collect::<Vec<_>>());
                store::save(&client_path()?, &c.value, &c.unknown)?;
                println!("Client settings saved.");
            }
            if let (Some(h), Some(t)) = (host, target.as_mut()) {
                let profile_plugins: Vec<(String, bool)> = h
                    .value
                    .plugins
                    .iter()
                    .filter(|(id, _)| goatty_shell::builtins::get(id).is_none())
                    .map(|(id, cfg)| (id.clone(), cfg.enabled))
                    .collect();
                print_issues("host", &h.issues.iter().map(ToString::to_string).collect::<Vec<_>>());
                let issues = t.apply(store::to_json(&h.value, &h.unknown))?;
                if profile_plugins.is_empty() {
                    print_issues("host", &issues);
                    println!("Host settings applied ({}).", t.label());
                } else {
                    offer_imported_plugins(t, &profile_plugins, &issues, yes)?;
                }
            }
            Ok(())
        }
    }
}

/// Import every scheme in `path` as a user theme. Returns (id, name, format).
pub fn import_theme_file(path: &Path) -> Result<Vec<(String, String, &'static str)>> {
    import_theme_file_named(path, None)
}

fn import_theme_file_named(path: &Path, name: Option<&str>) -> Result<Vec<(String, String, &'static str)>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "imported".into());
    let schemes = theme_import::import(&text, &stem).map_err(anyhow::Error::msg)?;
    if name.is_some() && schemes.len() > 1 {
        bail!("{} holds {} schemes; --name works with one", path.display(), schemes.len());
    }
    let mut out = Vec::new();
    for s in schemes {
        // Reload so names saved a moment ago count as taken.
        let lib = ThemeLibrary::load_default();
        let name = name.map(str::to_owned).unwrap_or(s.name);
        let id = lib.unique_id(&name);
        lib.save(&id, &name, &s.colors)?;
        out.push((id, name, s.format.label()));
    }
    Ok(out)
}

pub fn theme(cmd: ThemeCmd) -> Result<()> {
    let lib = ThemeLibrary::load_default();
    for i in &lib.issues {
        eprintln!("⚠ {i}");
    }
    match cmd {
        ThemeCmd::List => {
            let c = goatty_config::load_client().value;
            let t = &c.theme;
            for e in lib.all() {
                let mut marks = Vec::new();
                if !t.follow_system && e.id == t.name {
                    marks.push("active");
                }
                if t.follow_system && e.id == t.light {
                    marks.push("light");
                }
                if t.follow_system && e.id == t.dark {
                    marks.push("dark");
                }
                let kind = if e.user { "mine" } else { "built-in" };
                println!("{:<22} {:<24} {kind:<8} {}", e.id, e.name, marks.join(", "));
            }
            Ok(())
        }
        ThemeCmd::Import { files, name } => {
            if name.is_some() && files.len() > 1 {
                bail!("--name works with one file");
            }
            for f in files {
                for (id, n, format) in import_theme_file_named(&f, name.as_deref())? {
                    println!("Imported {n:?} ({format}) as {id}. Use it: goatty theme set {id}");
                }
            }
            Ok(())
        }
        ThemeCmd::Export { id, file } => {
            let e = lib.get(&id).with_context(|| format!("no theme {id:?} (see `goatty theme list`)"))?;
            let mut text = serde_json::to_string_pretty(&ThemeFile { name: e.name, colors: e.colors })?;
            text.push('\n');
            match file {
                Some(f) => std::fs::write(&f, text).with_context(|| format!("write {}", f.display()))?,
                None => print!("{text}"),
            }
            Ok(())
        }
        ThemeCmd::Set { id, target } => {
            if lib.get(&id).is_none() {
                bail!("no theme {id:?} (see `goatty theme list`)");
            }
            let path = client_path()?;
            let mut loaded = store::parse::<ClientSettings>(&read_or_empty(&path)?);
            let t = &mut loaded.value.theme;
            match target {
                None => {
                    t.name = id.clone();
                    t.follow_system = false;
                }
                Some(ThemeFor::Light) => {
                    t.light = id.clone();
                    t.follow_system = true;
                }
                Some(ThemeFor::Dark) => {
                    t.dark = id.clone();
                    t.follow_system = true;
                }
            }
            store::save(&path, &loaded.value, &loaded.unknown)?;
            println!("Theme set to {id}{}.", if target.is_some() { " (following the system)" } else { "" });
            Ok(())
        }
        ThemeCmd::Delete { id } => {
            let e = lib.get(&id).filter(|e| e.user).with_context(|| format!("{id:?} isn't one of your themes"))?;
            lib.delete(&id)?;
            println!("Deleted {:?}.", e.name);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use goatty_config::PluginConfig;
    use goatty_plugin::fetch::Fixed;
    use goatty_plugin::{index, sign};
    use goatty_shell::dirs::Dirs;
    use goatty_shell::launch::Launcher;
    use goatty_shell::plugins::registry::Source;

    use super::*;

    fn test_world() -> (tempfile::TempDir, Host) {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(&tmp.path().join("host"));
        let reg = tmp.path().join("registry");
        let greet = r#"
schema = 1
id = "greet"
name = "Greet"
version = "0.1.0"
summary = "Defines a greet function"
publisher = "tests"

[compat]
os = ["linux", "macos", "windows"]
shells = ["zsh", "powershell"]

[[setting]]
id = "name"
label = "Name"
type = "string"
default = "world"

[zsh]
init = "zsh/init.zsh"

[powershell]
init = "pwsh/init.ps1"

[permissions]
shell-code = "defines the greet function in every shell"
"#;
        let addon = r#"
schema = 1
id = "addon"
name = "Addon"
version = "0.2.0"
summary = "Depends on greet"
publisher = "tests"

[compat]
os = ["linux", "macos", "windows"]
shells = ["zsh", "powershell"]

[requires]
plugins = ["greet"]

[zsh]
init = "zsh/init.zsh"

[powershell]
init = "pwsh/init.ps1"

[permissions]
shell-code = "uses greet"
"#;
        let key_path = tmp.path().join("reg.key");
        let public = sign::keygen_file(&key_path).unwrap();
        let key = sign::load_secret(&key_path).unwrap();
        std::fs::create_dir_all(&reg).unwrap();
        std::fs::write(reg.join(index::META), format!("name = \"test\"\nkeys = [\"{public}\"]\n")).unwrap();
        for (id, v, manifest, zsh_init, pwsh_init) in [
            ("greet", "0.1.0", greet, "greet() { echo hi }\n", "function global:greet { Write-Output 'hi' }\n"),
            ("addon", "0.2.0", addon, "addon() { greet }\n", "function global:addon { greet }\n"),
        ] {
            let p = reg.join("plugins").join(id).join(v);
            std::fs::create_dir_all(p.join("zsh")).unwrap();
            std::fs::create_dir_all(p.join("pwsh")).unwrap();
            std::fs::write(p.join("plugin.toml"), manifest).unwrap();
            std::fs::write(p.join("zsh/init.zsh"), zsh_init).unwrap();
            std::fs::write(p.join("pwsh/init.ps1"), pwsh_init).unwrap();
        }
        let idx = index::build(&reg).unwrap();
        std::fs::write(reg.join(index::INDEX), idx.to_bytes()).unwrap();
        index::sign_dir(&reg, &key).unwrap();

        let source = Source { name: "test".into(), url: reg.to_string_lossy().into_owned(), keys: vec![public], builtin: true };
        let svc = HostService::new(Launcher::new(Some(dirs), None), Some(tmp.path().join("host.json")))
            .with_plugin_sources(Arc::new(Fixed(BTreeMap::new())), vec![source]);
        (tmp, Host::Local(Box::new(svc)))
    }

    #[test]
    fn missing_plugin_issue_id_matches_only_uninstalled_plugin_issues() {
        assert_eq!(
            missing_plugin_issue_id("plugins.greet: not installed on this host; ignored (goattyd plugin install greet)"),
            Some("greet")
        );
        assert_eq!(missing_plugin_issue_id("plugins.greet.settings.name: bad value"), None);
        assert_eq!(missing_plugin_issue_id("shell.program: not found"), None);
    }

    #[test]
    fn import_lists_plugins_and_requires_review_in_non_interactive_or_yes_mode() {
        let (_tmp, mut host) = test_world();
        let issues = host.apply(r#"{"version": 3, "plugins": {"greet": {"settings": {"name": "ada"}}, "missing": {}}}"#.into()).unwrap();
        let report =
            offer_imported_plugins_with(&mut host, &[("greet".into(), true), ("missing".into(), false)], &issues, true, true, |_| {
                panic!("--yes must not auto-approve or prompt")
            })
            .unwrap();
        assert_eq!(report.listed, vec!["greet", "missing"]);
        assert!(report.installed.is_empty());
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn import_reviews_and_installs_approved_plugins_with_dependencies() {
        let (_tmp, mut host) = test_world();
        // Put `addon` before `greet` in the list: `addon` requires `greet`,
        // which its plan installs first; `greet` isn't asked about again.
        let issues = host.apply(r#"{"version": 3, "plugins": {"addon": {}, "greet": {}}}"#.into()).unwrap();
        let mut prompts = Vec::new();
        let report = offer_imported_plugins_with(&mut host, &[("addon".into(), true), ("greet".into(), true)], &issues, false, true, |q| {
            prompts.push(q.to_owned());
            Ok(true)
        })
        .unwrap();
        assert_eq!(report.listed, vec!["addon", "greet"]);
        assert_eq!(report.installed, vec!["greet", "addon"]);
        assert!(report.failed.is_empty());
        assert_eq!(prompts, vec!["Review and install them now?", "Install Greet and Addon?"]);
        let api::Reply::Catalog(cat) = host.plugin(api::Request::Catalog { refresh: false }).unwrap() else { panic!("expected catalog") };
        let installed: BTreeSet<_> = cat.installed.iter().map(|i| i.id.as_str()).collect();
        assert!(installed.contains("greet") && installed.contains("addon"), "{installed:?}");
    }

    #[test]
    fn import_respects_declined_plugin_and_never_reprompts() {
        let (_tmp, mut host) = test_world();
        let issues = host.apply(r#"{"version": 3, "plugins": {"addon": {}, "greet": {}}}"#.into()).unwrap();
        let mut prompts = Vec::new();
        let mut answers = [true, false].into_iter();
        let report = offer_imported_plugins_with(&mut host, &[("addon".into(), true), ("greet".into(), true)], &issues, false, true, |q| {
            prompts.push(q.to_owned());
            Ok(answers.next().unwrap_or(false))
        })
        .unwrap();
        assert_eq!(report.installed, Vec::<String>::new());
        assert_eq!(report.skipped, vec!["addon", "greet"]);
        assert!(report.failed.is_empty());
        assert_eq!(prompts, vec!["Review and install them now?", "Install Greet and Addon?", "Install Greet?"]);
    }

    #[test]
    fn import_skips_already_installed_plugins() {
        let (_tmp, mut host) = test_world();
        let api::Reply::Plan(plan) = host.plugin(api::Request::Plan { id: "greet".into(), version: None }).unwrap() else {
            panic!("expected plan")
        };
        host.plugin(api::Request::Install { id: "greet".into(), version: Some(plan.version), approved: plan.digest }).unwrap();
        let h = HostSettings { plugins: BTreeMap::from([("greet".into(), PluginConfig::default())]), ..Default::default() };
        let issues = host.apply(store::to_json(&h, &[])).unwrap();
        let report = offer_imported_plugins_with(&mut host, &[("greet".into(), true)], &issues, false, true, |_| {
            panic!("nothing missing, should not prompt")
        })
        .unwrap();
        assert_eq!(report, ImportPluginReport::default());
    }

    #[test]
    fn sanitize_terminal_escapes_control_sequences() {
        assert_eq!(sanitize_terminal("goattyd on deploy@box-1"), "goattyd on deploy@box-1");
        assert_eq!(sanitize_terminal("hello \x1b[31mred\x1b[0m\r\n"), "hello \\u{1b}[31mred\\u{1b}[0m\\r\\n");
        assert_eq!(sanitize_terminal("title\x1b]0;evil\x07\u{009b}K"), "title\\u{1b}]0;evil\\u{7}\\u{9b}K");
    }

    #[test]
    fn remote_export_rejects_non_object_json_and_accepts_valid_objects() {
        assert!(parse_host_settings_for_export("[]", true, "goattyd on box").is_err());
        assert!(parse_host_settings_for_export("null", true, "goattyd on box").is_err());
        assert!(parse_host_settings_for_export("not json", true, "goattyd on box").is_err());

        assert_eq!(
            parse_host_settings_for_export("{}", true, "goattyd on box").unwrap().history.size,
            HostSettings::default().history.size
        );
        let with_unknown =
            parse_host_settings_for_export(r#"{"version": 999, "future_key": true, "history": {"size": 2500}}"#, true, "goattyd on box")
                .unwrap();
        assert_eq!(with_unknown.history.size, 2500);
    }
}
