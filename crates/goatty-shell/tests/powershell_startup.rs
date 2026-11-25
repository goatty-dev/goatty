#![cfg(windows)]
//! Real PowerShell end-to-end tests (#278): builds a managed PowerShell
//! generation in an isolated temp directory and runs it under `pwsh.exe`
//! (or `powershell.exe` fallback) on Windows.
//!
//! Never touches the real user profile or `%APPDATA%\goatty`.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use goatty_config::{Effective, HostSettings, UserAlias};
use goatty_plugin::commands::{CommandInput, InputKind, OpenIn, PaletteCommand, command_line_powershell};
use goatty_shell::{Dirs, HostService, Launcher, doctor, path_report};

fn pwsh() -> Option<String> {
    let found = goatty_shell::launch::find_powershell();
    if found.is_none() {
        eprintln!("PowerShell not installed; skipping");
    }
    found
}

#[test]
fn managed_powershell_startup_reports_status_and_path() {
    let Some(ps) = pwsh() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(tmp.path());
    let extra_bin = tmp.path().join("extra-bin");
    std::fs::create_dir_all(&extra_bin).unwrap();

    let mut host = HostSettings::default();
    host.shell.program = Some(ps);
    host.shell.path.push(extra_bin.to_string_lossy().into_owned());
    host.aliases.insert("ls".into(), UserAlias::new("Write-Output 'custom-ls-ok'"));
    let eff = Effective::plain(host);

    let launcher = Launcher::new(Some(dirs.clone()), None);
    let launch = launcher.launch(&eff);
    assert!(launch.managed, "expected managed PowerShell launch: {launch:?}");

    // Probe goatty-status via doctor::probe.
    let probe = doctor::probe(&launch, Duration::from_secs(20)).expect("doctor::probe should succeed");
    assert!(probe.stderr.trim().is_empty(), "startup stderr: {}", probe.stderr);
    assert!(matches!(probe.get("integration"), Some(doctor::State::Ok(_))), "features: {:?}", probe.features);
    assert!(matches!(probe.get("expansions"), Some(doctor::State::Ok(_))), "features: {:?}", probe.features);
    // Built-in features must be reported (ok or missing depending on runner tools).
    assert!(probe.get("fzf").is_some(), "features: {:?}", probe.features);
    assert!(probe.get("starship").is_some(), "features: {:?}", probe.features);
    assert!(probe.get("autosuggestions").is_some(), "features: {:?}", probe.features);
    assert!(probe.get("syntax-highlighting").is_some(), "features: {:?}", probe.features);

    // User alias `ls` must override PowerShell's built-in `Alias:ls -> Get-ChildItem`.
    let ran = doctor::run(&launch, "ls", Duration::from_secs(20)).expect("run user alias");
    assert!(ran.report.contains("custom-ls-ok"), "expected custom ls output, got: {:?}", ran.report);

    // PATH report attributes `extra_bin` to Settings and `dirs.bin_dir()` to Tools.
    let svc = HostService::new(launcher, Some(dirs.host_file()));
    let json = goatty_config::store::to_json(&eff.file.value, &eff.file.unknown);
    svc.set(&json, true).unwrap();
    let report = svc.path_report();
    assert!(report.probed, "path_report should probe managed PowerShell: {:?}", report.problem);
    assert!(
        report.entries.iter().any(|e| {
            e.origin == path_report::Origin::Settings
                && e.dir.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(extra_bin.to_string_lossy().trim_end_matches(['\\', '/']))
        }),
        "extra_bin should have Origin::Settings: {:?}",
        report.entries
    );
}

#[test]
fn powershell_quoted_command_line_survives_real_shell() {
    let Some(ps) = pwsh() else { return };
    let c = PaletteCommand {
        id: "echo-test".into(),
        title: "Echo test".into(),
        keywords: Vec::new(),
        run: vec!["Write-Output".into(), "{a}".into(), "pre-{b}".into()],
        keys: Vec::new(),
        pane: None,
        inputs: vec![
            CommandInput {
                id: "a".into(),
                label: "A".into(),
                placeholder: String::new(),
                help: String::new(),
                kind: InputKind::Text,
                default: None,
                optional: false,
                min: None,
                max: None,
                pattern: None,
                pattern_help: None,
                choices: Vec::new(),
                must_exist: false,
                source: None,
                strict: false,
            },
            CommandInput {
                id: "b".into(),
                label: "B".into(),
                placeholder: String::new(),
                help: String::new(),
                kind: InputKind::Text,
                default: None,
                optional: false,
                min: None,
                max: None,
                pattern: None,
                pattern_help: None,
                choices: Vec::new(),
                must_exist: false,
                source: None,
                strict: false,
            },
        ],
        when: None,
        open_in: OpenIn::Current,
        menu: None,
    };
    let nasty_a = "it's $(exit 99); $env:USERNAME | 'quoted'";
    let nasty_b = "curly\u{2019}quote; & calc";
    let mut vals = BTreeMap::new();
    vals.insert("a".into(), nasty_a.into());
    vals.insert("b".into(), nasty_b.into());
    let line = command_line_powershell(&c, &vals);

    let out = Command::new(&ps)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &line])
        .output()
        .expect("run powershell");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    assert_eq!(stdout, format!("{nasty_a}\npre-{nasty_b}\n"));
}

/// Find every installed PowerShell binary (`pwsh.exe` and Windows PowerShell 5.1 `powershell.exe`)
/// so compatibility tests verify both PS 7+ and the 5.1 fallback when present.
fn all_powershells() -> Vec<String> {
    let mut out = Vec::new();
    for name in ["pwsh.exe", "powershell.exe"] {
        if let Some(s) = goatty_shell::launch::which(name)
            && !out.iter().any(|x: &String| x.eq_ignore_ascii_case(&s))
        {
            out.push(s);
        }
    }
    out
}

#[test]
fn native_embedded_quotes_and_snippets_survive_pwsh_and_ps51() {
    let shells = all_powershells();
    if shells.is_empty() {
        eprintln!("PowerShell not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Helper script invoked via `-File` (native Win32 CommandLineToArgvW argv parsing).
    let dump_args = tmp.path().join("dump_args.ps1");
    std::fs::write(&dump_args, "foreach ($a in $args) { Write-Output ('ARG:' + $a) }\n").unwrap();
    let dump_str = dump_args.to_string_lossy().into_owned();

    let c = PaletteCommand {
        id: "json-test".into(),
        title: "JSON test".into(),
        keywords: Vec::new(),
        run: vec![
            shells[0].clone(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-File".into(),
            dump_str.clone(),
            "{json}".into(),
            "{spaced}".into(),
            "plain".into(),
        ],
        keys: Vec::new(),
        pane: None,
        inputs: vec![
            CommandInput {
                id: "json".into(),
                label: "JSON".into(),
                placeholder: String::new(),
                help: String::new(),
                kind: InputKind::Text,
                default: None,
                optional: false,
                min: None,
                max: None,
                pattern: None,
                pattern_help: None,
                choices: Vec::new(),
                must_exist: false,
                source: None,
                strict: false,
            },
            CommandInput {
                id: "spaced".into(),
                label: "Spaced".into(),
                placeholder: String::new(),
                help: String::new(),
                kind: InputKind::Text,
                default: None,
                optional: false,
                min: None,
                max: None,
                pattern: None,
                pattern_help: None,
                choices: Vec::new(),
                must_exist: false,
                source: None,
                strict: false,
            },
        ],
        when: None,
        open_in: OpenIn::Current,
        menu: None,
    };
    let json_val = r#"{"name":"value"}"#;
    let spaced_val = r#"{"msg": "hi \"there\""}"#;
    let mut vals = BTreeMap::new();
    vals.insert("json".into(), json_val.into());
    vals.insert("spaced".into(), spaced_val.into());
    let line = command_line_powershell(&c, &vals);

    for ps in &shells {
        let out = Command::new(ps)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &line])
            .output()
            .unwrap_or_else(|e| panic!("run {ps}: {e}"));
        assert!(out.status.success(), "{ps} stderr: {}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        assert_eq!(stdout, format!("ARG:{json_val}\nARG:{spaced_val}\nARG:plain\n"), "host={ps}, line={line}");
    }

    // Embedded snippet placeholders must arrive as single complete arguments.
    let sn: goatty_plugin::snippets::Snippet = toml::from_str(&format!(
        "id = \"emb\"\ntitle = \"Emb\"\nline = \"& '{}' -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File '{}' --out={{dir}}/file.txt --tag=pre-{{x}}-post {{dir}}\"\n[[input]]\nid = \"dir\"\nlabel = \"Dir\"\ntype = \"folder\"\n[[input]]\nid = \"x\"\nlabel = \"X\"\n",
        shells[0].replace('\'', "''"),
        dump_str.replace('\'', "''"),
    ))
    .unwrap();
    let mut sn_vals = BTreeMap::new();
    sn_vals.insert("dir".into(), "~/src".into());
    sn_vals.insert("x".into(), "a\u{2019}b".into());
    let sn_line = sn.render_powershell(&sn_vals);
    let out = Command::new(&shells[0])
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &sn_line])
        .output()
        .expect("run snippet");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 args, got: {lines:?}\nline={sn_line}");
    assert!(lines[0].starts_with("ARG:--out=") && lines[0].ends_with("/src/file.txt"), "{lines:?}");
    assert_eq!(lines[1], "ARG:--tag=pre-a\u{2019}b-post");
    assert!(lines[2].starts_with("ARG:") && lines[2].ends_with("src"), "{lines:?}");
}

#[test]
fn reload_preserves_user_profile_definitions_and_bom_decodes_non_ascii() {
    let shells = all_powershells();
    if shells.is_empty() {
        eprintln!("PowerShell not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Include a non-ASCII directory segment so UTF-8 BOM decoding is exercised on PS 5.1.
    let root = tmp.path().join("caf\u{00e9}-root");
    let dirs = Dirs::under(&root);
    std::fs::create_dir_all(dirs.user_ps_profile().parent().unwrap()).unwrap();
    std::fs::write(
        dirs.user_ps_profile(),
        "function my_profile_fn { Write-Output 'fn-survived' }\n\
         Set-Alias -Name my_profile_al -Value Write-Output\n\
         $MyProfileVar = 'var-survived'\n",
    )
    .unwrap();

    let mut host = HostSettings::default();
    host.shell.program = Some(shells[0].clone());
    // Add case-colliding expansion rules to ensure generated expansions.ps1 parses cleanly.
    host.expansions.insert("exp-lower".into(), serde_json::json!({ "say": "all done", "to": "echo ok" }));
    host.expansions.insert("exp-upper".into(), serde_json::json!({ "say": "all DONE", "to": "echo OK" }));
    let eff = Effective::plain(host);

    let launcher = Launcher::new(Some(dirs.clone()), None);
    let launch = launcher.launch(&eff);
    assert!(launch.managed);

    let init = dirs.pwsh_dir().join("init.ps1");
    let prof_path = dirs.user_ps_profile();
    let prof_q = prof_path.to_string_lossy().replace('\'', "''");
    // Initial `. init.ps1` loads v1, then we modify `profile.ps1` to define a brand-new
    // function, alias, and variable (without `global:`) and call `goatty-reload`.
    // They must survive after `goatty-reload` returns.
    let script = format!(
        ". '{}'; \
         [System.IO.File]::WriteAllText('{}', \"function my_reloaded_fn {{ Write-Output 'fn-reloaded' }}`nSet-Alias -Name my_reloaded_al -Value Write-Output`n`$MyReloadedVar = 'var-reloaded'`n\"); \
         goatty-reload; \
         my_reloaded_fn; \
         my_reloaded_al 'al-reloaded'; \
         Write-Output $MyReloadedVar; \
         Write-Output $global:_goatty_dir",
        init.to_string_lossy().replace('\'', "''"),
        prof_q,
    );
    for ps in &shells {
        // Reset profile.ps1 to v1 before each shell run.
        std::fs::write(&prof_path, "function my_profile_fn { Write-Output 'fn-v1' }\n").unwrap();
        let out = Command::new(ps)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .output()
            .unwrap_or_else(|e| panic!("run {ps}: {e}"));
        assert!(out.status.success(), "{ps} stderr: {}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        assert!(stdout.contains("fn-reloaded"), "{ps}: {stdout}");
        assert!(stdout.contains("al-reloaded"), "{ps}: {stdout}");
        assert!(stdout.contains("var-reloaded"), "{ps}: {stdout}");
        assert!(stdout.contains("caf\u{00e9}-root"), "{ps} should decode non-ASCII path via UTF-8 BOM: {stdout}");
    }
}

#[test]
fn native_verb_noun_and_windows_mode_legacy_exes_preserve_json_args() {
    let shells = all_powershells();
    if shells.is_empty() {
        eprintln!("PowerShell not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Compile a tiny native Win32 argv dumper and place copies named `get-widget.exe`
    // (a Verb-Noun executable name) and `find.exe` (a PowerShell 7.3+ Windows-mode
    // legacy-argument-passing exception) in an isolated temp bin dir.
    let src = tmp.path().join("argv.rs");
    std::fs::write(&src, "fn main() { for a in std::env::args().skip(1) { println!(\"ARG:{a}\"); } }\n").unwrap();
    let get_widget = tmp.path().join("get-widget.exe");
    let compile = Command::new("rustc").arg(&src).arg("-o").arg(&get_widget).output().expect("run rustc");
    assert!(compile.status.success(), "rustc failed: {}", String::from_utf8_lossy(&compile.stderr));
    let find_exe = tmp.path().join("find.exe");
    std::fs::copy(&get_widget, &find_exe).unwrap();

    let make_cmd = |prog: &str| PaletteCommand {
        id: "t".into(),
        title: "T".into(),
        keywords: Vec::new(),
        run: vec![prog.into(), "{json}".into(), "{spaced}".into()],
        keys: Vec::new(),
        pane: None,
        inputs: vec![
            CommandInput {
                id: "json".into(),
                label: "JSON".into(),
                placeholder: String::new(),
                help: String::new(),
                kind: InputKind::Text,
                default: None,
                optional: false,
                min: None,
                max: None,
                pattern: None,
                pattern_help: None,
                choices: Vec::new(),
                must_exist: false,
                source: None,
                strict: false,
            },
            CommandInput {
                id: "spaced".into(),
                label: "Spaced".into(),
                placeholder: String::new(),
                help: String::new(),
                kind: InputKind::Text,
                default: None,
                optional: false,
                min: None,
                max: None,
                pattern: None,
                pattern_help: None,
                choices: Vec::new(),
                must_exist: false,
                source: None,
                strict: false,
            },
        ],
        when: None,
        open_in: OpenIn::Current,
        menu: None,
    };

    let json_val = r#"{"name":"value"}"#;
    let spaced_val = r#"{"msg": "hi \"there\""}"#;
    let mut vals = BTreeMap::new();
    vals.insert("json".into(), json_val.into());
    vals.insert("spaced".into(), spaced_val.into());

    let path_env = format!("{};{}", tmp.path().display(), std::env::var("PATH").unwrap_or_default());
    let find_exe_str = find_exe.to_string_lossy();
    for (prog, expected) in [
        ("get-widget", format!("ARG:{json_val}\nARG:{spaced_val}\n")),
        (find_exe_str.as_ref(), format!("ARG:{json_val}\nARG:{spaced_val}\n")),
        ("Write-Output", format!("{json_val}\n{spaced_val}\n")),
    ] {
        let line = command_line_powershell(&make_cmd(prog), &vals);
        for ps in &shells {
            let out = Command::new(ps)
                .env("PATH", &path_env)
                .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &line])
                .output()
                .unwrap_or_else(|e| panic!("run {ps}: {e}"));
            assert!(out.status.success(), "prog={prog} host={ps} stderr: {}", String::from_utf8_lossy(&out.stderr));
            let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
            assert_eq!(stdout, expected, "prog={prog} host={ps} line={line}");
        }
    }
}

#[test]
fn cached_init_rejects_bomless_cache_and_ast_validates_aliases() {
    let shells = all_powershells();
    if shells.is_empty() {
        eprintln!("PowerShell not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dirs = Dirs::under(tmp.path());

    // 1. PowerShell AST alias validation skips unparseable commands in both
    //    `function global:` and `Set-Item` forms even when delimiters are balanced.
    let mut host = HostSettings::default();
    host.shell.program = Some(shells[0].clone());
    host.aliases.insert("good".into(), UserAlias::new("Write-Output 'ok'"));
    host.aliases.insert("bad-fn".into(), UserAlias::new("if (1) else"));
    host.aliases.insert("g:bad".into(), UserAlias::new("if (1) else"));
    host.aliases.insert("bad-nest".into(), UserAlias::new("Write-Output ([)]"));
    let (rendered, issues) = goatty_shell::aliases::render_powershell_checked(&host, Some(&shells[0]));
    assert!(rendered.contains("function global:good"), "{rendered}");
    for bad in ["bad-fn", "g:bad", "bad-nest"] {
        assert!(rendered.contains(&format!("# skipped: {bad} (")), "{bad} should be skipped:\n{rendered}");
        assert!(issues.iter().any(|i| i.starts_with(&format!("aliases.{bad}:"))), "{issues:?}");
    }

    // 2. `_goatty_cached_init` must reject a pre-existing BOM-less cache file
    //    (even one with a matching `v2` key) and rewrite it with a UTF-8 BOM.
    let eff = Effective::plain(host);
    let launcher = Launcher::new(Some(dirs.clone()), None);
    let launch = launcher.launch(&eff);
    assert!(launch.managed);

    let init = dirs.pwsh_dir().join("init.ps1");
    let helper_src = tmp.path().join("emit_ps.rs");
    std::fs::write(&helper_src, "fn main() { println!(\"$global:_goatty_cache_probe = 'caf\\u{00e9}'\"); }\n").unwrap();
    let helper_exe = tmp.path().join("emit-ps.exe");
    let compile = Command::new("rustc").arg(&helper_src).arg("-o").arg(&helper_exe).output().expect("run rustc");
    assert!(compile.status.success(), "rustc failed: {}", String::from_utf8_lossy(&compile.stderr));

    for ps in &shells {
        // First run generates the BOM cache file; strip its BOM and overwrite body to
        // prove a BOM-less cache entry is rejected and regenerated with a BOM.
        let script = format!(
            ". '{}'; \
             $f = _goatty_cached_init -Name 'probe' -Extra 'k' -Command '{}' -Arguments @(); \
             Write-Output ('FIRST:' + $f); \
             $txt = [System.IO.File]::ReadAllText($f).Replace('café', 'STALE'); \
             [System.IO.File]::WriteAllText($f, $txt, (New-Object System.Text.UTF8Encoding($false))); \
             $f2 = _goatty_cached_init -Name 'probe' -Extra 'k' -Command '{}' -Arguments @(); \
             . $f2; \
             Write-Output ('VAL:' + $global:_goatty_cache_probe)",
            init.to_string_lossy().replace('\'', "''"),
            helper_exe.to_string_lossy().replace('\'', "''"),
            helper_exe.to_string_lossy().replace('\'', "''"),
        );
        let out = Command::new(ps)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .output()
            .unwrap_or_else(|e| panic!("run {ps}: {e}"));
        assert!(out.status.success(), "{ps} stderr: {}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        assert!(stdout.contains("VAL:caf\u{00e9}"), "{ps} must reject BOM-less cache and decode UTF-8 BOM: {stdout}");
        let path_line = stdout.lines().find_map(|l| l.strip_prefix("FIRST:")).expect("FIRST line");
        let raw = std::fs::read(path_line.trim()).unwrap();
        assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]), "{ps} regenerated cache must start with UTF-8 BOM");
    }
}
