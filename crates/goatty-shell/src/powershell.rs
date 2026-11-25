//! The managed PowerShell environment on Windows (#278): sibling to
//! [`crate::zsh`].
//!
//! Static startup files live in `pwsh/` (`init.ps1`, `integration.ps1`,
//! `expand.ps1`); everything that depends on settings or paths goes into the
//! generated `settings.ps1`, `aliases.ps1`, `expansions.ps1`, `plugins-*.ps1`,
//! and `starship.toml`. [`crate::generation`] validates every `.ps1` file with
//! PowerShell's AST parser and activates the generation via an NTFS directory
//! junction (`current -> generations/<n>`).

use std::path::Path;

use goatty_config::HostSettings;
use goatty_plugin::manifest::Load;
use goatty_plugin::powershell as ps;

use crate::builtins;
use crate::dirs::Dirs;
use crate::plugins::Active;
use crate::zsh::GeneratedFile;

pub const INIT: &str = include_str!("../pwsh/init.ps1");
pub const INTEGRATION: &str = include_str!("../pwsh/integration.ps1");
pub const EXPAND: &str = include_str!("../pwsh/expand.ps1");

/// Inputs for [`render`].
pub struct PowerShellConfig<'a> {
    pub settings: &'a HostSettings,
    pub dirs: &'a Dirs,
    /// The `goattyd.exe` binary `goatty-setup` runs. `None`: `goattyd` from PATH.
    pub daemon_exe: Option<&'a Path>,
    /// Enabled plugins, built-in and installed, in load order
    /// ([`crate::plugins::active`]).
    pub plugins: &'a [Active],
}

/// Every file of the managed PowerShell configuration directory (`current/pwsh/`).
pub fn render(cfg: &PowerShellConfig<'_>) -> Vec<GeneratedFile> {
    let mut files = vec![
        GeneratedFile::new("settings.ps1", render_settings(cfg)),
        GeneratedFile::new("init.ps1", INIT),
        GeneratedFile::new("integration.ps1", INTEGRATION),
        GeneratedFile::new("expand.ps1", EXPAND),
        GeneratedFile::new("expansions.ps1", crate::snippets::render_expansions_powershell(cfg.settings, cfg.plugins).0),
        GeneratedFile::new("starship.toml", crate::starship::render(cfg.settings)),
        GeneratedFile::new("starship-plain.toml", crate::starship::render(&plain_prompt(cfg.settings))),
        GeneratedFile::new("aliases.ps1", crate::aliases::render_powershell(cfg.settings)),
    ];
    files.extend(render_plugins(cfg));
    files
}

fn plain_prompt(settings: &HostSettings) -> HostSettings {
    let mut s = settings.clone();
    s.prompt = s.prompt.plain();
    s
}

fn slot_header(load: Load) -> &'static str {
    match load {
        Load::LineEditor => "line editor tools (fzf, fzf-tab, PSReadLine autosuggestions, ...)",
        Load::Prompt => "the prompt (goatty sets a small one when none loads)",
        Load::Normal => "aliases and the plugins' PowerShell code (before your aliases and additions)",
        Load::Last => "what loads after your additions (PSReadLine syntax highlighting)",
    }
}

/// `plugins-early.ps1` and one file per load slot, plus each plugin's
/// generated config files and PowerShell init scripts.
fn render_plugins(cfg: &PowerShellConfig<'_>) -> Vec<GeneratedFile> {
    const NOTE: &str = "# Generated from the plugins and their settings in host.json: change them in goatty's settings window.\n";
    let mut early = format!("# goatty plugins: environment and generated config files.\n{NOTE}");
    for b in builtins::BUILTINS.iter().filter(|b| cfg.settings.plugins.get(b.id).is_some_and(|c| !c.enabled)) {
        early.push_str(&format!("$_goatty_state[{}] = 'off'\n", ps::quote(b.id)));
    }
    let mut slots: Vec<(Load, String)> = Load::ALL.iter().map(|&l| (l, format!("# goatty plugins: {}.\n{NOTE}", slot_header(l)))).collect();
    let mut files = Vec::new();
    let home = Dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
    for a in cfg.plugins {
        let id = &a.manifest.id;
        let mut shipped = Vec::new();
        let checked: Result<(), String> = a.shell_files_for("powershell").into_iter().try_for_each(|rel| {
            let bytes = a.read(cfg.dirs, &rel).map_err(|e| format!("{e:#}"))?;
            let text = String::from_utf8(bytes).map_err(|_| format!("{id}: {rel} isn't text"))?;
            shipped.push(GeneratedFile::new(format!("plugins/{id}/{rel}"), text));
            Ok(())
        });
        if let Err(e) = checked {
            log::warn!("leaving plugin {id} out of the PowerShell configuration: {e}");
            continue;
        }
        let ctx = ps::Ctx {
            dir: format!("(Join-Path $_goatty_dir {})", ps::quote(&format!("plugins/{id}"))),
            home: home.as_deref(),
            label: a.label.clone(),
            feature: a.feature(),
        };
        let r = ps::render(&a.manifest, &a.values, &ctx);
        if !r.early.is_empty() {
            early.push('\n');
            early.push_str(&r.early);
        }
        let slot = &mut slots.iter_mut().find(|(l, _)| *l == r.load).expect("every slot").1;
        slot.push('\n');
        slot.push_str(&r.main);
        files.extend(shipped);
        files.extend(r.files.into_iter().map(|(name, contents)| GeneratedFile::new(format!("plugins/{id}/{name}"), contents)));
    }
    let mut out = vec![GeneratedFile::new("plugins-early.ps1", early)];
    out.extend(slots.into_iter().map(|(l, text)| GeneratedFile::new(l.ps_file(), text)));
    out.extend(files);
    out
}

/// goatty's own PowerShell features (not plugins) that are enabled.
pub fn enabled_features(s: &HostSettings) -> Vec<&'static str> {
    [("integration", s.shell.powershell.integration), ("expansions", s.shell.expansions)]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect()
}

fn ps_bool(b: bool) -> &'static str {
    if b { "$true" } else { "$false" }
}

fn render_settings(cfg: &PowerShellConfig<'_>) -> String {
    let d = cfg.dirs;
    let s = cfg.settings;
    let mut out = String::from(
        "# goatty managed PowerShell: settings and paths, read first by init.ps1.\n\
         # Generated by goatty from host.json: change settings in goatty's settings window, not here.\n",
    );
    let scalar = |out: &mut String, name: &str, value: &str| out.push_str(&format!("$global:{name} = {}\n", ps::quote(value)));
    let flag = |out: &mut String, name: &str, b: bool| out.push_str(&format!("$global:{name} = {}\n", ps_bool(b)));

    scalar(&mut out, "_goatty_dir", &d.pwsh_dir().to_string_lossy());
    scalar(&mut out, "_goatty_bin", &d.bin_dir().to_string_lossy());
    scalar(&mut out, "_goatty_tools", &d.tools_dir().to_string_lossy());
    scalar(&mut out, "_goatty_completions", &d.pwsh_completions_dir().to_string_lossy());
    scalar(&mut out, "_goatty_cache", &d.cache.to_string_lossy());
    scalar(&mut out, "_goatty_user_additions", &d.user_ps_profile().to_string_lossy());
    scalar(&mut out, "_goatty_hint_marker", &d.hint_marker().to_string_lossy());
    scalar(&mut out, "_goatty_offer_marker", &d.offer_marker().to_string_lossy());
    scalar(&mut out, "_goatty_reload_sentinel", &d.reload_sentinel().to_string_lossy());
    scalar(&mut out, "_goatty_daemon", &cfg.daemon_exe.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());

    flag(&mut out, "_goatty_user_profile", s.shell.powershell.user_profile);
    out.push_str(&format!("$global:_goatty_hist_size = {}\n", s.history.size));
    flag(&mut out, "_goatty_hist_no_dups", s.history.ignore_duplicates);
    flag(&mut out, "_goatty_hist_ignore_space", s.history.ignore_space);

    flag(&mut out, "_goatty_prompt_two_line", s.prompt.two_line);
    flag(&mut out, "_goatty_prompt_transient", s.prompt.transient);
    scalar(&mut out, "_goatty_transient_ok", &crate::starship::transient_prompt_ansi(&s.prompt, false));
    scalar(&mut out, "_goatty_transient_err", &crate::starship::transient_prompt_ansi(&s.prompt, true));
    let plain = s.prompt.plain();
    flag(&mut out, "_goatty_prompt_plain", s.prompt.plain_elsewhere());
    scalar(&mut out, "_goatty_transient_ok_plain", &crate::starship::transient_prompt_ansi(&plain, false));
    scalar(&mut out, "_goatty_transient_err_plain", &crate::starship::transient_prompt_ansi(&plain, true));

    let features: Vec<String> = enabled_features(s).into_iter().map(ps::quote).collect();
    out.push_str(&format!("$global:_goatty_features = @({})\n", features.join(", ")));

    let path: Vec<String> = s.shell.path.iter().map(|p| ps::quote(p)).collect();
    out.push_str(&format!("$global:_goatty_path = @({})\n", path.join(", ")));

    // Built-in plugins with Windows-applicable packages that goatty-setup installs.
    let defaults: Vec<String> = builtins::BUILTINS
        .iter()
        .filter(|b| {
            b.default_install
                && s.plugins.get(b.id).is_none_or(|c| c.enabled)
                && b.manifest().packages.iter().any(|p| p.applies_to_shell("powershell"))
        })
        .map(|b| ps::quote(b.id))
        .collect();
    out.push_str(&format!("$global:_goatty_setup_defaults = @({})\n", defaults.join(", ")));

    let pins: Vec<String> = builtins::BUILTINS
        .iter()
        .filter_map(|b| {
            b.manifest()
                .packages
                .iter()
                .find(|p| p.applies_to_shell("powershell"))
                .map(|p| format!("{} = {}", ps::quote(b.id), ps::quote(&p.version)))
        })
        .collect();
    out.push_str(&format!("$global:_goatty_pins = @{{ {} }}\n", pins.join("; ")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_reflect_toggles() {
        let dirs = Dirs::under(Path::new("/r o/ot's"));
        let mut s = HostSettings::default();
        s.plugins.entry("starship".into()).or_default().enabled = false;
        s.shell.powershell.user_profile = true;
        s.history.size = 5000;
        s.history.ignore_duplicates = false;
        s.shell.path = vec!["~/my bin".into(), "C:\\it's".into()];
        let out = render_settings(&PowerShellConfig {
            settings: &s,
            dirs: &dirs,
            daemon_exe: Some(Path::new("C:\\bin\\goattyd.exe")),
            plugins: &[],
        });
        assert!(out.contains("$global:_goatty_user_profile = $true"), "{out}");
        assert!(out.contains("$global:_goatty_hist_size = 5000"), "{out}");
        assert!(out.contains("$global:_goatty_hist_no_dups = $false"), "{out}");
        assert!(out.contains("$global:_goatty_path = @('~/my bin', 'C:\\it''s')"), "{out}");
        assert!(out.contains("$global:_goatty_setup_defaults = @('fzf')"), "starship is off, zsh-only plugins excluded: {out}");
        assert!(out.contains("$global:_goatty_features = @('integration', 'expansions')"), "{out}");
    }

    #[test]
    fn plugins_render_into_powershell_slots_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let mut s = HostSettings::default();
        s.plugins.entry("starship".into()).or_default().enabled = false;
        let (plugins, issues) = crate::plugins::active(&dirs, &s);
        assert!(issues.is_empty(), "{issues:?}");
        let files = render(&PowerShellConfig { settings: &s, dirs: &dirs, daemon_exe: None, plugins: &plugins });
        let get = |n: &str| &files.iter().find(|f| f.name == n).unwrap_or_else(|| panic!("no {n}")).contents;
        assert!(get("plugins-early.ps1").contains("$_goatty_state['starship'] = 'off'"));
        let le = get("plugins-line-editor.ps1");
        let pos = |id: &str| le.find(&format!("# --- {id} ")).unwrap_or_else(|| panic!("no {id}:\n{le}"));
        assert!(pos("fzf") < pos("fzf-tab"), "{le}");
        assert!(pos("fzf-tab") < pos("autosuggestions"), "{le}");
        assert!(!get("plugins-prompt.ps1").contains("$_goatty_prompt = 'starship'"));
        assert!(get("plugins-last.ps1").contains("# --- syntax-highlighting "));
        assert!(files.iter().any(|f| f.name == "plugins/fzf/pwsh/init.ps1"));
        assert!(files.iter().any(|f| f.name == "plugins/fzf/fzfrc"));
    }
}
