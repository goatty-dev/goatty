//! Aliases on a host (#88): the user's own (`aliases` in `host.json`) and
//! the ones installed plugins bring, as one table for the settings
//! window's Aliases page and `goattyd alias list`, and the generated
//! `aliases.zsh`.
//!
//! The managed `.zshrc` reads `plugins.zsh` (plugin aliases) and then
//! `aliases.zsh` (the user's), so a user alias with a plugin alias's name
//! replaces it. Between plugins, the one loaded later (by id) wins. A
//! plugin alias the user switched off is left out of `plugins.zsh`
//! ([`crate::plugins::active`] drops it from the manifest it renders).

pub mod cmd;

use std::collections::BTreeMap;

use goatty_config::{HostSettings, UserAlias};
use goatty_plugin::Manifest;
use goatty_plugin::alias::views;
use goatty_plugin::schema::resolve;

use crate::zsh::quote;

/// An installed plugin, for [`table`].
#[derive(Debug, Clone, Copy)]
pub struct PluginAliases<'a> {
    pub manifest: &'a Manifest,
    /// Installed but switched off (`plugins.<id>.enabled = false`).
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The user's: `group` is the heading ("" = no group).
    User {
        group: String,
    },
    Plugin {
        id: String,
        name: String,
        enabled: bool,
    },
}

/// A heading in the list and its aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub source: Source,
    pub rows: Vec<Row>,
}

impl Group {
    /// The heading: the group's name, "Ungrouped", or the plugin's name.
    pub fn title(&self) -> &str {
        match &self.source {
            Source::User { group } if group.is_empty() => "Ungrouped",
            Source::User { group } => group,
            Source::Plugin { name, .. } => name,
        }
    }

    pub fn plugin_id(&self) -> Option<&str> {
        match &self.source {
            Source::Plugin { id, .. } => Some(id),
            Source::User { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    /// What it runs (a plugin's: with its current settings).
    pub expansion: String,
    pub description: String,
    /// The user's switch for this alias.
    pub enabled: bool,
    /// New shells define it as shown.
    pub active: bool,
    /// Why it isn't active, or what it replaces.
    pub note: Option<String>,
    /// A plugin alias's condition, in words (`"Short aliases" is on`).
    pub condition: Option<String>,
}

/// Every alias for `settings`: the user's groups (ungrouped first, then by
/// name), then each plugin with aliases (in load order, by id).
pub fn table(settings: &HostSettings, plugins: &[PluginAliases<'_>]) -> Vec<Group> {
    let mut plugins: Vec<PluginAliases<'_>> = plugins.iter().filter(|p| !p.manifest.aliases.is_empty()).copied().collect();
    plugins.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));

    // Plugin rows first: which ones are defined, and which plugin defines a
    // name last (that one wins in the shell).
    let mut plugin_groups = Vec::new();
    let mut defined_by: BTreeMap<String, String> = BTreeMap::new(); // name → plugin name
    for p in &plugins {
        let m = p.manifest;
        let config = settings.plugins.get(&m.id);
        let stored = config.map(|c| c.settings.clone()).unwrap_or_default();
        let values = resolve(m, &stored).0;
        let rows: Vec<Row> = views(m, &values)
            .into_iter()
            .map(|v| {
                let enabled = config.is_none_or(|c| !c.disabled_aliases.contains(&v.name));
                let note = if !p.enabled {
                    Some("the plugin is disabled".to_owned())
                } else if !v.on {
                    v.condition.as_ref().map(|c| format!("only while {c} in the plugin's settings"))
                } else if !enabled {
                    Some("switched off".to_owned())
                } else {
                    None
                };
                let active = p.enabled && v.on && enabled;
                if active {
                    defined_by.insert(v.name.clone(), m.name.clone());
                }
                Row { name: v.name, expansion: v.expansion, description: v.description, enabled, active, note, condition: v.condition }
            })
            .collect();
        plugin_groups.push(Group { source: Source::Plugin { id: m.id.clone(), name: m.name.clone(), enabled: p.enabled }, rows });
    }
    // Later plugins and the user's aliases replace earlier definitions.
    for g in &mut plugin_groups {
        let name = g.title().to_owned();
        for r in g.rows.iter_mut().filter(|r| r.active) {
            if let Some(user) = settings.aliases.get(&r.name).filter(|a| a.enabled) {
                r.active = false;
                r.note = Some(format!("replaced by your alias ({})", user.command));
            } else if let Some(other) = defined_by.get(&r.name).filter(|o| **o != name) {
                r.active = false;
                r.note = Some(format!("replaced by {other}'s alias"));
            }
        }
    }

    // Groups compare without case ("git" and "Git" are one, titled by the
    // first alias's spelling).
    let mut by_group: BTreeMap<String, (String, Vec<Row>)> = BTreeMap::new();
    for (name, a) in &settings.aliases {
        let note = (a.enabled)
            .then(|| defined_by.get(name).map(|p| format!("replaces {p}'s alias")))
            .flatten()
            .or_else(|| (!a.enabled).then(|| "switched off".to_owned()));
        let row = Row {
            name: name.clone(),
            expansion: a.command.clone(),
            description: a.description.clone(),
            enabled: a.enabled,
            active: a.enabled,
            note,
            condition: None,
        };
        by_group.entry(a.group.to_lowercase()).or_insert_with(|| (a.group.clone(), Vec::new())).1.push(row);
    }
    let mut out: Vec<Group> = by_group.into_values().map(|(group, rows)| Group { source: Source::User { group }, rows }).collect();
    out.extend(plugin_groups);
    out
}

/// The user's group names, sorted, compared without case (no empty one).
pub fn user_groups(settings: &HostSettings) -> Vec<String> {
    let mut groups: BTreeMap<String, String> = BTreeMap::new();
    for a in settings.aliases.values().filter(|a| !a.group.is_empty()) {
        groups.entry(a.group.to_lowercase()).or_insert_with(|| a.group.clone());
    }
    groups.into_values().collect()
}

/// An alias with its name.
type Named<'a> = (&'a String, &'a UserAlias);

/// `aliases.zsh`: the user's enabled aliases (always written, so the
/// `.zshrc` can source it unconditionally).
pub fn render(settings: &HostSettings) -> String {
    let mut out = String::from(
        "# goatty: your aliases (\"aliases\" in host.json), read after the plugins' aliases so yours win.\n\
         # Generated by goatty: change them in goatty's settings window (Aliases) or with `goattyd alias`.\n",
    );
    // Lowercase group → (its title, its aliases).
    let mut groups: BTreeMap<String, (&str, Vec<Named<'_>>)> = BTreeMap::new();
    for (name, a) in &settings.aliases {
        groups.entry(a.group.to_lowercase()).or_insert_with(|| (&a.group, Vec::new())).1.push((name, a));
    }
    for (group, aliases) in groups.into_values() {
        out.push('\n');
        if !group.is_empty() {
            out.push_str(&format!("# {}\n", group.replace('\n', " ")));
        }
        for (name, a) in aliases {
            if a.enabled {
                // Names are restricted to characters zsh takes literally (`is_alias_name`).
                out.push_str(&format!("alias -- {name}={}\n", quote(&a.command)));
            } else {
                out.push_str(&format!("# off: {name}\n"));
            }
        }
    }
    out
}

/// Check whether `command` can be embedded safely inside `{ <command> @args }`
/// in both PowerShell 7 and Windows PowerShell 5.1. Returns a short reason when
/// it would fail to parse.
pub fn powershell_command_problem(command: &str) -> Option<&'static str> {
    let body = command.trim();
    if body.is_empty() {
        return Some("empty command");
    }
    if body.contains(['\n', '\r']) {
        return Some("multi-line commands aren't supported in PowerShell aliases");
    }
    if body.chars().any(|c| c.is_control() && c != '\t') {
        return Some("contains control characters");
    }
    #[derive(PartialEq, Clone, Copy)]
    enum Q {
        None,
        Single,
        Double,
    }
    let is_sq = |c: char| matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}');
    let is_dq = |c: char| matches!(c, '"' | '\u{201C}' | '\u{201D}' | '\u{201E}');

    let chars: Vec<char> = body.chars().collect();
    let mut q = Q::None;
    let mut stack: Vec<char> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match q {
            Q::None => {
                if c == '`' {
                    if next.is_none() {
                        return Some("ends with a lone backtick");
                    }
                    i += 2;
                    continue;
                } else if is_sq(c) {
                    q = Q::Single;
                } else if is_dq(c) {
                    q = Q::Double;
                } else if c == '#' {
                    return Some("contains a `#` comment");
                } else if c == '<' && next == Some('#') {
                    return Some("contains a `<#` block comment");
                } else if (c == '&' && next == Some('&')) || (c == '|' && next == Some('|')) {
                    return Some("`&&` and `||` aren't supported by Windows PowerShell 5.1");
                } else if matches!(c, '{' | '(' | '[') {
                    stack.push(match c {
                        '{' => '}',
                        '(' => ')',
                        _ => ']',
                    });
                } else if matches!(c, '}' | ')' | ']') {
                    match stack.pop() {
                        Some(expected) if expected == c => {}
                        Some(_) => return Some("mismatched delimiters"),
                        None => {
                            return Some(match c {
                                '}' => "unbalanced `}`",
                                ')' => "unbalanced `)`",
                                _ => "unbalanced `]`",
                            });
                        }
                    }
                }
            }
            Q::Single => {
                if is_sq(c) {
                    if next.is_some_and(is_sq) {
                        i += 2;
                        continue;
                    }
                    q = Q::None;
                }
            }
            Q::Double => {
                if c == '`' {
                    if next.is_none() {
                        return Some("ends with a lone backtick");
                    }
                    i += 2;
                    continue;
                } else if is_dq(c) {
                    if next.is_some_and(is_dq) {
                        i += 2;
                        continue;
                    }
                    q = Q::None;
                }
            }
        }
        i += 1;
    }
    if q != Q::None {
        return Some("unclosed quote");
    }
    if let Some(expected) = stack.last() {
        return Some(match expected {
            '}' => "unbalanced `{`",
            ')' => "unbalanced `(`",
            _ => "unbalanced `[`",
        });
    }
    None
}

fn format_ps_alias_def(name: &str, body: &str) -> String {
    let is_ident = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if is_ident {
        format!("function global:{name} {{ {body} @args }}")
    } else {
        let fn_path = goatty_plugin::powershell::quote(&format!("Function:global:{name}"));
        format!("Set-Item -LiteralPath {fn_path} -Value {{ {body} @args }}")
    }
}

/// Validate rendered PowerShell alias definitions with PowerShell's AST parser
/// (`[System.Management.Automation.Language.Parser]::ParseInput`) in a single
/// subprocess call, returning `Ok(name -> first parse error message)` or `Err`
/// if the parser subprocess failed to run.
fn parse_check_ps_defs(defs: &[(String, String)], pwsh: &str) -> Result<BTreeMap<String, String>, String> {
    if defs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut script = String::new();
    for (name, def) in defs {
        let q_name = goatty_plugin::powershell::quote(name);
        let q_def = goatty_plugin::powershell::quote(def);
        script.push_str(&format!(
            "$t = $null; $e = $null; [void][System.Management.Automation.Language.Parser]::ParseInput({q_def}, [ref]$t, [ref]$e)\n\
             if ($e -and $e.Count -gt 0) {{ [Console]::Out.WriteLine({q_name} + \"`t\" + $e[0].Message.Replace(\"`r\", ' ').Replace(\"`n\", ' ')) }}\n"
        ));
    }
    let mut cmd = std::process::Command::new(pwsh);
    cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let out = cmd.output().map_err(|e| format!("couldn't run PowerShell parser: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("PowerShell parser exited with {}", out.status)
        } else {
            format!("PowerShell parser failed: {stderr}")
        });
    }
    let mut bad = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((name, msg)) = line.split_once('\t') {
            bad.entry(name.to_owned()).or_insert_with(|| msg.trim().to_owned());
        }
    }
    Ok(bad)
}

/// Validate a single alias `(name, command)` for PowerShell, using both the
/// static check and PowerShell's AST parser when installed on Windows.
pub fn powershell_alias_problem(name: &str, command: &str) -> Option<String> {
    let body = command.trim();
    if let Some(why) = powershell_command_problem(body) {
        return Some(why.to_owned());
    }
    if cfg!(windows)
        && let Some(pwsh) = crate::launch::find_powershell()
    {
        let def = format_ps_alias_def(name, body);
        match parse_check_ps_defs(&[(name.to_owned(), def)], &pwsh) {
            Ok(bad) => {
                if let Some(err) = bad.get(name) {
                    return Some(err.clone());
                }
            }
            Err(why) => return Some(why),
        }
    }
    None
}

/// Render `aliases.ps1` and collect any skipped-alias issues, validating
/// candidate definitions against `pwsh`'s AST parser when `pwsh` is `Some`.
/// If the parser subprocess fails to run, unverified aliases are skipped and
/// reported rather than silently emitted.
pub fn render_powershell_checked(settings: &HostSettings, pwsh: Option<&str>) -> (String, Vec<String>) {
    let mut out = String::from(
        "# goatty: your aliases (\"aliases\" in host.json), read after the plugins' aliases so yours win.\n\
         # Generated by goatty: change them in goatty's settings window (Aliases) or with `goattyd alias`.\n",
    );
    let mut issues = Vec::new();
    let mut static_bad: BTreeMap<String, String> = BTreeMap::new();
    let mut candidates: Vec<(String, String)> = Vec::new();
    for (name, a) in &settings.aliases {
        if !a.enabled {
            continue;
        }
        let body = a.command.trim();
        if let Some(why) = powershell_command_problem(body) {
            static_bad.insert(name.clone(), why.to_owned());
        } else {
            candidates.push((name.clone(), format_ps_alias_def(name, body)));
        }
    }
    let ast_bad: BTreeMap<String, String> = match pwsh {
        Some(p) => match parse_check_ps_defs(&candidates, p) {
            Ok(bad) => bad,
            Err(why) => candidates.iter().map(|(name, _)| (name.clone(), why.clone())).collect(),
        },
        None => BTreeMap::new(),
    };

    let mut groups: BTreeMap<String, (&str, Vec<Named<'_>>)> = BTreeMap::new();
    for (name, a) in &settings.aliases {
        groups.entry(a.group.to_lowercase()).or_insert_with(|| (&a.group, Vec::new())).1.push((name, a));
    }
    for (group, aliases) in groups.into_values() {
        out.push('\n');
        if !group.is_empty() {
            out.push_str(&format!("# {}\n", group.replace('\n', " ")));
        }
        for (name, a) in aliases {
            if a.enabled {
                if let Some(why) = static_bad.get(name).or_else(|| ast_bad.get(name)) {
                    let clean_why = why.replace(['\r', '\n'], " ");
                    out.push_str(&format!("# skipped: {name} ({clean_why})\n"));
                    issues.push(format!("aliases.{name}: skipped for PowerShell ({clean_why})"));
                    continue;
                }
                let alias_path = goatty_plugin::powershell::quote(&format!("Alias:{name}"));
                let remove = format!(
                    "if (Test-Path -LiteralPath {alias_path}) {{ Remove-Item -LiteralPath {alias_path} -Force -ErrorAction SilentlyContinue }}"
                );
                let def = format_ps_alias_def(name, a.command.trim());
                out.push_str(&format!("{remove}\n{def}\n"));
            } else {
                out.push_str(&format!("# off: {name}\n"));
            }
        }
    }
    (out, issues)
}

/// Enabled user aliases that cannot be rendered as valid PowerShell.
pub fn powershell_issues(settings: &HostSettings) -> Vec<String> {
    let pwsh = if cfg!(windows) { crate::launch::find_powershell() } else { None };
    render_powershell_checked(settings, pwsh.as_deref()).1
}

/// `aliases.ps1`: the user's enabled aliases rendered as PowerShell
/// `global:` functions (removing any built-in PowerShell alias of the same
/// name, so aliases with arguments work and override `ls`/`cat`/`curl`) (#278).
///
/// Aliases whose command cannot safely sit inside a PowerShell scriptblock on
/// both PS 7 and Windows PowerShell 5.1 are skipped with a comment (and
/// reported via [`powershell_issues`]) so a single bad alias never prevents
/// future generations from building.
pub fn render_powershell(settings: &HostSettings) -> String {
    let pwsh = if cfg!(windows) { crate::launch::find_powershell() } else { None };
    render_powershell_checked(settings, pwsh.as_deref()).0
}

#[cfg(test)]
mod tests {
    use goatty_config::PluginConfig;

    use super::*;

    const DOCKER: &str = r#"
schema = 1
id = "docker"
name = "Docker"
version = "1.0.0"
summary = "s"
publisher = "p"
[requires]
commands = ["docker"]
[[setting]]
id = "aliases"
label = "Short aliases"
type = "bool"
default = true
[[alias]]
name = "dps"
command = "docker"
args = ["ps"]
when = "aliases"
description = "Running containers"
[[alias]]
name = "di"
command = "docker"
args = ["images"]
when = "aliases"
[[alias]]
name = "d"
command = "docker"
"#;

    fn docker() -> Manifest {
        goatty_plugin::manifest::parse(DOCKER, None).unwrap()
    }

    fn settings() -> HostSettings {
        let mut s = HostSettings::default();
        s.aliases.insert("gs".into(), UserAlias { group: "Git".into(), ..UserAlias::new("git status") });
        s.aliases.insert("gl".into(), UserAlias { group: "git".into(), enabled: false, ..UserAlias::new("git log") });
        s.aliases.insert("..".into(), UserAlias::new("cd .."));
        s.aliases.insert("d".into(), UserAlias::new("docker compose"));
        s
    }

    #[test]
    fn groups_and_overrides() {
        let m = docker();
        let mut s = settings();
        s.plugins.insert("docker".into(), PluginConfig { disabled_aliases: ["di".to_owned()].into(), ..Default::default() });
        let t = table(&s, &[PluginAliases { manifest: &m, enabled: true }]);
        let titles: Vec<&str> = t.iter().map(Group::title).collect();
        assert_eq!(titles, ["Ungrouped", "git", "Docker"], "{t:#?}");

        let ungrouped = &t[0].rows;
        assert_eq!(ungrouped.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["..", "d"]);
        assert_eq!(ungrouped[1].note.as_deref(), Some("replaces Docker's alias"));
        assert_eq!(t[1].rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["gl", "gs"], "one group without case");
        assert_eq!(t[1].rows[0].note.as_deref(), Some("switched off"));
        assert!(!t[1].rows[0].active);

        let docker = &t[2];
        assert_eq!(docker.plugin_id(), Some("docker"));
        let row = |n: &str| docker.rows.iter().find(|r| r.name == n).unwrap();
        assert!(row("dps").active);
        assert_eq!(row("dps").expansion, "docker ps");
        assert_eq!(row("dps").description, "Running containers");
        assert!(!row("di").enabled && !row("di").active);
        assert_eq!(row("d").note.as_deref(), Some("replaced by your alias (docker compose)"));
        assert!(!row("d").active);

        // The plugin's condition, and the plugin switched off.
        s.plugins.get_mut("docker").unwrap().settings.insert("aliases".into(), false.into());
        let t = table(&s, &[PluginAliases { manifest: &m, enabled: true }]);
        let dps = t[2].rows.iter().find(|r| r.name == "dps").unwrap();
        assert!(!dps.active);
        assert_eq!(dps.note.as_deref(), Some("only while \"Short aliases\" is on in the plugin's settings"));
        let t = table(&s, &[PluginAliases { manifest: &m, enabled: false }]);
        assert!(t[2].rows.iter().all(|r| !r.active && r.note.as_deref() == Some("the plugin is disabled")));
        assert_eq!(t[0].rows[1].note, None, "nothing to replace from a disabled plugin");
    }

    #[test]
    fn later_plugins_win() {
        let a = docker();
        let mut b = docker();
        b.id = "podman".into();
        b.name = "Podman".into();
        let t = table(
            &HostSettings::default(),
            &[PluginAliases { manifest: &b, enabled: true }, PluginAliases { manifest: &a, enabled: true }],
        );
        assert_eq!(t[0].title(), "Docker", "in load order");
        assert_eq!(t[0].rows[0].note.as_deref(), Some("replaced by Podman's alias"));
        assert!(t[1].rows[0].active);
    }

    #[test]
    fn renders_enabled_aliases_quoted() {
        let mut s = settings();
        s.aliases.insert("q".into(), UserAlias::new("echo 'it''s' | less"));
        let out = render(&s);
        assert!(out.contains("alias -- ..='cd ..'\n"), "{out}");
        assert!(out.contains("# git\n# off: gl\nalias -- gs='git status'\n"), "{out}");
        assert!(out.contains(r"alias -- q='echo '\''it'\'''\''s'\'' | less'"), "{out}");
        assert_eq!(user_groups(&s), ["git"]);
        assert!(render(&HostSettings::default()).starts_with("# goatty"));
    }

    #[test]
    fn renders_powershell_aliases_removing_builtins() {
        let mut s = settings();
        s.aliases.insert("ls".into(), UserAlias::new("eza --icons=auto"));
        let out = render_powershell(&s);
        assert!(out.contains("Remove-Item -LiteralPath 'Alias:ls'"), "{out}");
        assert!(out.contains("function global:ls { eza --icons=auto @args }"), "{out}");
        assert!(out.contains("Set-Item -LiteralPath 'Function:global:..' -Value { cd .. @args }"), "{out}");
        assert!(out.contains("# off: gl\n"), "{out}");
    }

    #[test]
    fn powershell_skips_unparseable_alias_commands_in_both_forms() {
        let mut s = HostSettings::default();
        s.aliases.insert("ok".into(), UserAlias::new("git status"));
        s.aliases.insert("commented".into(), UserAlias::new("git status # trailing"));
        s.aliases.insert("multiline".into(), UserAlias::new("echo a\necho b"));
        s.aliases.insert("unbalanced".into(), UserAlias::new("echo }"));
        s.aliases.insert("misnested".into(), UserAlias::new("Write-Output ([)]"));
        s.aliases.insert("g:misnested".into(), UserAlias::new("Write-Output ([)]"));
        s.aliases.insert("chained".into(), UserAlias::new("git add -A && git commit"));
        s.aliases.insert("g:bad".into(), UserAlias::new("git pull || git status"));
        s.aliases.insert("quoted-hash".into(), UserAlias::new("Write-Output 'a # b && c'"));

        // Verify fallback static validation when PowerShell is unavailable (`pwsh = None`).
        let (out, issues) = render_powershell_checked(&s, None);
        assert!(out.contains("function global:ok { git status @args }"), "{out}");
        assert!(out.contains("function global:quoted-hash { Write-Output 'a # b && c' @args }"), "{out}");
        for bad in ["commented", "multiline", "unbalanced", "misnested", "g:misnested", "chained", "g:bad"] {
            assert!(out.contains(&format!("# skipped: {bad} (")), "{bad} should be skipped:\n{out}");
            assert!(!out.contains(&format!("function global:{bad}")), "{out}");
            assert!(!out.contains(&format!("Function:global:{bad}")), "{out}");
        }
        assert_eq!(issues.len(), 7, "{issues:?}");

        // Parser subprocess failure must skip and report unverified aliases in both forms.
        let mut s2 = HostSettings::default();
        s2.aliases.insert("ok".into(), UserAlias::new("git status"));
        s2.aliases.insert("..".into(), UserAlias::new("cd .."));
        let (out2, issues2) = render_powershell_checked(&s2, Some("goatty-nonexistent-pwsh-parser"));
        for name in ["ok", ".."] {
            assert!(out2.contains(&format!("# skipped: {name} (")), "{name} should be skipped on parser failure:\n{out2}");
            assert!(!out2.contains(&format!("function global:{name}")), "{out2}");
            assert!(!out2.contains(&format!("Function:global:{name}")), "{out2}");
            assert!(issues2.iter().any(|i| i.starts_with(&format!("aliases.{name}:"))), "{issues2:?}");
        }
    }
}
