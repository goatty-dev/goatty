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
