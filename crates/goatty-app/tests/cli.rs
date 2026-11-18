//! `goatty settings …` and `goatty theme …`: no window, no display, a
//! sandboxed `$HOME`, and a socket path with no daemon behind it (host
//! settings then go to the sandbox's own host.json).

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// The only installed font in the sandbox: the bundled one, copied to
/// `~/.local/share/fonts` and named by a fontconfig file of its own, so the
/// font checks don't depend on the machine's fonts.
const TEST_FONT: &str = "JetBrainsMono Nerd Font";

fn sandbox_fonts(home: &Path) -> std::path::PathBuf {
    let dir = home.join(".local/share/fonts");
    let conf = home.join("fonts.conf");
    if !conf.exists() {
        std::fs::create_dir_all(&dir).unwrap();
        let font = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/JetBrainsMonoNerdFont-Regular.ttf");
        std::fs::copy(font, dir.join("test.ttf")).unwrap();
        std::fs::write(&conf, format!("<?xml version=\"1.0\"?>\n<fontconfig><dir>{}</dir></fontconfig>\n", dir.display())).unwrap();
    }
    conf
}

fn goatty(home: &Path, args: &[&str]) -> Output {
    goatty_with_path(home, None, args)
}

fn goatty_with_path(home: &Path, extra_path: Option<&Path>, args: &[&str]) -> Output {
    let fonts_conf = sandbox_fonts(home);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_goatty"));
    cmd.args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("GOATTY_THEME")
        .env_remove("GOATTY_FONT")
        .env_remove("GOATTY_UI_FONT")
        .env_remove("GOATTY_FONT_SIZE")
        .env("GOATTY_SOCKET", home.join("none.sock"))
        .env("FONTCONFIG_FILE", fonts_conf)
        .stdin(Stdio::null());
    if let Some(bin) = extra_path {
        let mut paths = vec![bin.to_path_buf()];
        if let Some(cur) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&cur));
        }
        cmd.env("PATH", std::env::join_paths(paths).unwrap());
    }
    cmd.output().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn ok(o: Output) -> Output {
    assert!(o.status.success(), "stdout:\n{}\nstderr:\n{}", out(&o), err(&o));
    o
}

#[test]
fn settings_set_show_validate() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let file = home.join(".config/goatty/settings.json");
    assert!(out(&ok(goatty(home, &["settings", "path"]))).contains(&file.display().to_string()));

    ok(goatty(home, &["settings", "set", "font.size", "16"]));
    ok(goatty(home, &["settings", "set", "font.ui_family", TEST_FONT]));
    ok(goatty(home, &["settings", "set", "theme.overrides.accent", "#98c379"]));
    ok(goatty(home, &["settings", "set", "keybindings.ctrl-k", "paste"]));
    assert_eq!(out(&ok(goatty(home, &["settings", "show", "font.size"]))).trim(), "16.0");
    assert_eq!(out(&ok(goatty(home, &["settings", "show", "font.ui_family"]))).trim(), format!("{TEST_FONT:?}"));
    assert_eq!(out(&ok(goatty(home, &["settings", "show", "theme.overrides.accent"]))).trim(), "\"#98c379\"");
    let stored = out(&ok(goatty(home, &["settings", "show"])));
    assert!(
        stored.contains("\"size\": 16.0")
            && stored.contains(&format!("\"ui_family\": {TEST_FONT:?}"))
            && stored.contains("\"#98c379\"")
            && stored.contains("ctrl-k"),
        "{stored}"
    );
    assert!(!goatty(home, &["settings", "set", "font.size", "huge"]).status.success());
    assert!(!goatty(home, &["settings", "set", "keybindings.ctrl-j", "explode"]).status.success());
    ok(goatty(home, &["settings", "validate"]));

    // Fonts are checked against the installed ones, as in the settings window (#150).
    ok(goatty(home, &["settings", "set", "font.family", "No Such Mono"]));
    let o = goatty(home, &["settings", "validate"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("font.family") && err(&o).contains("isn't installed"), "{}", err(&o));
    assert!(!err(&o).contains("font.ui_family"), "{}", err(&o));
    ok(goatty(home, &["settings", "unset", "font.family"]));

    std::fs::write(&file, r#"{"version": 1, "theme": {"name": "nope"}, "keybindings": {"ctrl-+-+": "paste"}}"#).unwrap();
    let o = goatty(home, &["settings", "validate"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("theme.name") && err(&o).contains("keybindings.ctrl-+-+"), "{}", err(&o));

    ok(goatty(home, &["settings", "unset", "theme.name"]));
    assert!(!std::fs::read_to_string(&file).unwrap().contains("nope"));
}

#[test]
fn themes_import_set_export_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let scheme = home.join("ocean.yaml");
    let mut yaml = String::from("scheme: \"Ocean Test\"\n");
    for (i, k) in ["00", "01", "02", "03", "04", "05", "06", "07", "08", "09", "0A", "0B", "0C", "0D", "0E", "0F"].iter().enumerate() {
        yaml += &format!("base{k}: \"{:02x}{:02x}{:02x}\"\n", i * 16, 64, 128);
    }
    std::fs::write(&scheme, yaml).unwrap();

    let o = ok(goatty(home, &["theme", "import", scheme.to_str().unwrap()]));
    assert!(out(&o).contains("as ocean-test"), "{}", out(&o));
    assert!(home.join(".config/goatty/themes/ocean-test.json").is_file());
    let list = out(&ok(goatty(home, &["theme", "list"])));
    assert!(list.contains("ocean-test") && list.contains("mine") && list.contains("nord"), "{list}");

    ok(goatty(home, &["theme", "set", "ocean-test", "--for", "dark"]));
    ok(goatty(home, &["theme", "set", "solarized-light", "--for", "light"]));
    let theme = out(&ok(goatty(home, &["settings", "show", "theme"])));
    assert!(theme.contains("\"follow_system\": true") && theme.contains("\"dark\": \"ocean-test\""), "{theme}");
    assert!(out(&ok(goatty(home, &["theme", "list"]))).contains("dark"));
    assert!(!goatty(home, &["theme", "set", "does-not-exist"]).status.success());

    let exported = out(&ok(goatty(home, &["theme", "export", "ocean-test"])));
    assert!(exported.contains("\"name\": \"Ocean Test\""), "{exported}");
    assert!(!exported.contains("\"accent\""), "absent accent omitted on export: {exported}");
    // Importing an export with a custom primary/accent round-trips it.
    let with_accent = exported.replacen("\"colors\": {", "\"colors\": {\n    \"accent\": \"#50fa7b\",", 1);
    let again = home.join("again.json");
    std::fs::write(&again, with_accent).unwrap();
    assert!(out(&ok(goatty(home, &["theme", "import", again.to_str().unwrap()]))).contains("ocean-test-2"));
    let exported_accent = out(&ok(goatty(home, &["theme", "export", "ocean-test-2"])));
    assert!(exported_accent.contains("\"accent\": \"#50fa7b\""), "{exported_accent}");

    ok(goatty(home, &["theme", "delete", "ocean-test-2"]));
    assert!(!goatty(home, &["theme", "delete", "nord"]).status.success(), "built-ins can't be deleted");
}

#[test]
fn profile_export_import_without_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    ok(goatty(home, &["settings", "set", "theme.name", "dracula"]));
    std::fs::create_dir_all(home.join(".config/goatty")).unwrap();
    std::fs::write(home.join(".config/goatty/host.json"), "{\"version\": 1, \"history\": {\"size\": 4321}}\n").unwrap();

    let profile = home.join("profile.json");
    ok(goatty(home, &["settings", "export", profile.to_str().unwrap()]));
    let text = std::fs::read_to_string(&profile).unwrap();
    assert!(text.contains("dracula") && text.contains("4321"), "{text}");

    // A second machine imports it (no terminal: needs --yes).
    let other = tempfile::tempdir().unwrap();
    assert!(!goatty(other.path(), &["settings", "import", profile.to_str().unwrap()]).status.success());
    ok(goatty(other.path(), &["settings", "import", "--yes", profile.to_str().unwrap()]));
    assert!(std::fs::read_to_string(other.path().join(".config/goatty/settings.json")).unwrap().contains("dracula"));
    assert!(std::fs::read_to_string(other.path().join(".config/goatty/host.json")).unwrap().contains("4321"));
    assert!(other.path().join(".local/share/goatty/current/zsh/.zshrc").is_file(), "host settings were applied as a generation");
}

fn build_greet_registry(reg: &Path, key_path: &Path) -> String {
    let public = goatty_plugin::sign::keygen_file(key_path).unwrap();
    let key = goatty_plugin::sign::load_secret(key_path).unwrap();
    let plugin_dir = reg.join("plugins/greet/0.1.0");
    std::fs::create_dir_all(plugin_dir.join("zsh")).unwrap();
    std::fs::write(reg.join(goatty_plugin::index::META), format!("name = \"test\"\nkeys = [\"{public}\"]\n")).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        "schema = 1\nid = \"greet\"\nname = \"Greet\"\nversion = \"0.1.0\"\nsummary = \"Says hi\"\npublisher = \"tests\"\n[zsh]\ninit = \"zsh/init.zsh\"\n[permissions]\nshell-code = \"defines greet\"\n",
    )
    .unwrap();
    std::fs::write(plugin_dir.join("zsh/init.zsh"), "greet() { echo hi }\n").unwrap();
    let idx = goatty_plugin::index::build(reg).unwrap();
    std::fs::write(reg.join(goatty_plugin::index::INDEX), idx.to_bytes()).unwrap();
    goatty_plugin::index::sign_dir(reg, &key).unwrap();
    serde_json::json!({
        "version": 1,
        "builtin": false,
        "registries": [{ "name": "test", "url": reg, "keys": [public] }]
    })
    .to_string()
}

#[test]
fn profile_import_lists_uninstalled_plugins() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let registries_json = build_greet_registry(&tmp.path().join("registry"), &tmp.path().join("reg.key"));

    // Point the sandbox at the local signed registry instead of the network.
    let config_dir = home.join(".config/goatty");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("registries.json"), registries_json).unwrap();

    let profile = tmp.path().join("profile.json");
    std::fs::write(&profile, r#"{"goatty_profile": 1, "host": {"version": 3, "plugins": {"greet": {"settings": {"name": "ada"}}}}}"#)
        .unwrap();

    let o = ok(goatty(home, &["settings", "import", "--yes", profile.to_str().unwrap()]));
    let stdout = out(&o);
    let stderr = err(&o);
    assert!(!stderr.contains("plugins.greet: not installed on this host"), "redundant issue should be suppressed:\n{stderr}");
    assert!(stdout.contains("1 plugin(s) in this profile are not installed"), "{stdout}");
    assert!(stdout.contains("greet (test) 0.1.0"), "{stdout}");
    assert!(stdout.contains("Review and install with: goattyd plugin install greet"), "{stdout}");
    assert!(!home.join(".local/share/goatty/plugins/installed/greet").exists(), "--yes must not bypass plan approval");
}

#[test]
fn remote_flags_reject_unsupported_subcommands() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let o = goatty(home, &["settings", "show", "--ssh", "box"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--ssh and --via only apply to"), "{}", err(&o));

    let o = goatty(home, &["settings", "set", "font.size", "14", "--via", "true"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--ssh and --via only apply to"), "{}", err(&o));

    let o = goatty(home, &["theme", "list", "--ssh", "box"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--ssh and --via don't apply"), "{}", err(&o));

    let o = goatty(home, &["completions", "zsh", "--via", "true"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--ssh and --via don't apply"), "{}", err(&o));

    let o = goatty(home, &["--ssh", "box", "--plain", "settings", "export", "-"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--plain"), "{}", err(&o));

    let o = goatty(home, &["--local", "settings", "import", "-", "--ssh", "box"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--local cannot be used with --ssh or --via"), "{}", err(&o));

    let o = goatty(home, &["--ssh", "a", "settings", "export", "-", "--via", "b"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--ssh and --via cannot be used together"), "{}", err(&o));

    let o = goatty(home, &["--via", "b", "settings", "export", "-", "--ssh", "a"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--ssh and --via cannot be used together"), "{}", err(&o));

    let o = goatty(home, &["settings", "export", "-", "--remote-daemon", "/opt/goattyd"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--remote-daemon requires --ssh"), "{}", err(&o));
}

fn spawn_fake_remote_daemon(
    in_fifo: &Path,
    out_fifo: &Path,
    remote_home: &Path,
    force_version: Option<u32>,
) -> std::thread::JoinHandle<()> {
    use goatty_proto::{ClientMsg, FileDiff, HostInfo, PROTOCOL_VERSION, ServerMsg, encode, read_msg};
    use goatty_shell::HostService;
    use goatty_shell::dirs::Dirs;
    use goatty_shell::launch::Launcher;
    use std::io::Write;

    let in_fifo = in_fifo.to_path_buf();
    let out_fifo = out_fifo.to_path_buf();
    let remote_home = remote_home.to_path_buf();
    std::thread::spawn(move || {
        let mut r = std::fs::File::open(&in_fifo).unwrap();
        let mut w = std::fs::OpenOptions::new().write(true).open(&out_fifo).unwrap();
        let Ok(ClientMsg::Hello { version, .. }) = read_msg::<_, ClientMsg>(&mut r) else {
            return;
        };
        let host = HostInfo {
            hostname: "remote-box".into(),
            user: "deploy".into(),
            os: std::env::consts::OS.into(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
        };
        let negotiated = force_version.unwrap_or_else(|| version.min(PROTOCOL_VERSION));
        let hello = ServerMsg::Hello { version: negotiated, daemon_pid: 4242, host };
        if w.write_all(&encode(&hello).unwrap()).is_err() {
            return;
        }
        if force_version.is_some() {
            return;
        }
        let svc =
            HostService::new(Launcher::new(Some(Dirs::under(&remote_home)), None), Some(remote_home.join(".config/goatty/host.json")));
        while let Ok(msg) = read_msg::<_, ClientMsg>(&mut r) {
            let reply = match msg {
                ClientMsg::Compress { .. } => continue,
                ClientMsg::Ping { nonce } => ServerMsg::Pong { nonce },
                ClientMsg::GetHostSettings { req } => {
                    let s = svc.snapshot();
                    ServerMsg::HostSettings {
                        req,
                        json: s.json,
                        issues: s.issues,
                        env_overrides: s.env_overrides,
                        generation: s.generation,
                    }
                }
                ClientMsg::SetHostSettings { req, json, apply } => match svc.set(&json, apply) {
                    Ok(o) => ServerMsg::HostSettingsApplied {
                        req,
                        issues: o.issues,
                        diff: o.diff.into_iter().map(|d| FileDiff { name: d.name, unified: d.unified }).collect(),
                        applied: o.applied,
                        generation: o.generation,
                    },
                    Err(e) => ServerMsg::Error { req: Some(req), message: format!("{e:#}") },
                },
                ClientMsg::Plugin { req, request } => {
                    match serde_json::from_str(&request)
                        .map_err(anyhow::Error::from)
                        .and_then(|r| svc.plugin_request(r))
                        .and_then(|rep| serde_json::to_string(&rep).map_err(Into::into))
                    {
                        Ok(reply) => ServerMsg::Plugin { req, reply },
                        Err(e) => ServerMsg::Error { req: Some(req), message: format!("{e:#}") },
                    }
                }
                other => ServerMsg::Error { req: None, message: format!("unexpected test msg: {other:?}") },
            };
            if w.write_all(&encode(&reply).unwrap()).is_err() {
                break;
            }
        }
    })
}

fn make_fifo(path: &Path) {
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success(), "mkfifo failed for {}", path.display());
}

fn via_bridge(in_fifo: &Path, out_fifo: &Path) -> String {
    format!("exec 3<&0; cat <&3 > '{}' & p=$!; exec 3<&-; cat < '{}'; wait $p 2>/dev/null", in_fifo.display(), out_fifo.display())
}

#[test]
fn profile_export_import_via_remote_connection() {
    use std::io::Write;

    let tmp = tempfile::tempdir().unwrap();
    let local_home = tmp.path().join("local-home");
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(local_home.join(".config/goatty")).unwrap();
    std::fs::create_dir_all(remote_home.join(".config/goatty")).unwrap();

    // Distinct local vs remote settings so we verify remote is what travels.
    ok(goatty(&local_home, &["settings", "set", "theme.name", "nord"]));
    std::fs::write(local_home.join(".config/goatty/host.json"), "{\"version\": 3, \"history\": {\"size\": 1111}}\n").unwrap();
    std::fs::write(
        remote_home.join(".config/goatty/host.json"),
        "{\"version\": 3, \"history\": {\"size\": 7654}, \"shell\": {\"program\": \"/opt/custom/zsh\"}}\n",
    )
    .unwrap();

    // 1. Export over --via from the remote host.
    let in_fifo = tmp.path().join("via-in.fifo");
    let out_fifo = tmp.path().join("via-out.fifo");
    make_fifo(&in_fifo);
    make_fifo(&out_fifo);
    let server = spawn_fake_remote_daemon(&in_fifo, &out_fifo, &remote_home, None);
    let via_cmd = via_bridge(&in_fifo, &out_fifo);

    let exported_path = tmp.path().join("exported.json");
    let o = ok(goatty(&local_home, &["settings", "export", exported_path.to_str().unwrap(), "--via", &via_cmd]));
    server.join().unwrap();
    assert!(err(&o).contains("goattyd on deploy@remote-box"), "{}", err(&o));
    let exported = std::fs::read_to_string(&exported_path).unwrap();
    assert!(exported.contains("nord") && exported.contains("7654"), "{exported}");
    assert!(!exported.contains("1111") && !exported.contains("/opt/custom/zsh"), "{exported}");

    // 2. Import over split --ssh + --remote-daemon using a fake ssh script.
    let in_fifo2 = tmp.path().join("ssh-in.fifo");
    let out_fifo2 = tmp.path().join("ssh-out.fifo");
    make_fifo(&in_fifo2);
    make_fifo(&out_fifo2);
    let server2 = spawn_fake_remote_daemon(&in_fifo2, &out_fifo2, &remote_home, None);

    let in_fifo3 = tmp.path().join("old-in.fifo");
    let out_fifo3 = tmp.path().join("old-out.fifo");
    make_fifo(&in_fifo3);
    make_fifo(&out_fifo3);

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let ssh_log = tmp.path().join("ssh-cmd.log");
    let fake_ssh = bin_dir.join("ssh");
    let script = format!(
        r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in --) shift; break;; -e|-o) shift 2;; -*) shift;; *) break;; esac
done
host="$1"; shift
printf '%s\n' "$*" > '{log}'
if [ "$host" = "missing" ]; then
  echo 'goatty-remote: Linux x86_64 -' >&2
  exit 127
fi
echo 'goatty-remote: Linux x86_64 /opt/goattyd' >&2
if [ "$host" = "outdated" ]; then
  exec 3<&0
  cat <&3 > '{old_inf}' & p=$!
  exec 3<&-
  cat < '{old_outf}'
  wait $p 2>/dev/null
  exit 0
fi
exec 3<&0
cat <&3 > '{inf}' & p=$!
exec 3<&-
cat < '{outf}'
wait $p 2>/dev/null
"#,
        log = ssh_log.display(),
        inf = in_fifo2.display(),
        outf = out_fifo2.display(),
        old_inf = in_fifo3.display(),
        old_outf = out_fifo3.display(),
    );
    let mut child =
        Command::new("sh").args(["-c", "cat > \"$1\" && chmod 0755 \"$1\"", "sh"]).arg(&fake_ssh).stdin(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());

    let profile = tmp.path().join("import.json");
    std::fs::write(&profile, "{\"goatty_profile\": 1, \"host\": {\"version\": 3, \"history\": {\"size\": 4321}}}\n").unwrap();
    let o = ok(goatty_with_path(
        &local_home,
        Some(&bin_dir),
        &["--ssh", "prod", "settings", "import", profile.to_str().unwrap(), "--remote-daemon", "/opt/goattyd", "--yes"],
    ));
    server2.join().unwrap();

    let stdout = out(&o);
    assert!(stdout.contains("Host settings applied (goattyd on deploy@remote-box)."), "{stdout}");
    assert!(std::fs::read_to_string(&ssh_log).unwrap().contains("/opt/goattyd"));
    assert!(std::fs::read_to_string(remote_home.join(".config/goatty/host.json")).unwrap().contains("4321"));
    assert!(remote_home.join("data/current/zsh/.zshrc").is_file(), "remote generation was built");
    // Local client & host files stayed untouched for a host-only profile.
    assert!(std::fs::read_to_string(local_home.join(".config/goatty/settings.json")).unwrap().contains("nord"));
    assert!(std::fs::read_to_string(local_home.join(".config/goatty/host.json")).unwrap().contains("1111"));

    // 3. Missing goattyd on SSH host surfaces a clear hint.
    let o = goatty_with_path(&local_home, Some(&bin_dir), &["settings", "export", "-", "--ssh", "missing"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("goatty --ssh missing"), "{}", err(&o));

    // 4. Outdated goattyd on SSH host surfaces the update hint.
    let old_server = spawn_fake_remote_daemon(&in_fifo3, &out_fifo3, &remote_home, Some(1));
    let o = goatty_with_path(&local_home, Some(&bin_dir), &["settings", "export", "-", "--ssh", "outdated"]);
    old_server.join().unwrap();
    assert!(!o.status.success());
    assert!(err(&o).contains("run `goatty --ssh outdated` to update goattyd there"), "{}", err(&o));

    // 5. Client-only profile with --ssh skips connecting and informs the user.
    let client_only = tmp.path().join("client-only.json");
    std::fs::write(&client_only, "{\"goatty_profile\": 1, \"client\": {\"theme\": {\"name\": \"gruvbox-dark\"}}}\n").unwrap();
    let o = ok(goatty_with_path(
        &local_home,
        Some(&bin_dir),
        &["settings", "import", client_only.to_str().unwrap(), "--ssh", "missing", "--yes"],
    ));
    assert!(err(&o).contains("this profile has no host settings; nothing will be sent to missing"), "{}", err(&o));
    assert!(std::fs::read_to_string(local_home.join(".config/goatty/settings.json")).unwrap().contains("gruvbox-dark"));

    // 6. Importing a profile with uninstalled plugins over --via queries the remote catalog
    //    and prints remote-aware instructions.
    let registries_json = build_greet_registry(&tmp.path().join("remote-registry"), &tmp.path().join("remote-reg.key"));
    std::fs::create_dir_all(remote_home.join("config")).unwrap();
    std::fs::write(remote_home.join("config/registries.json"), registries_json).unwrap();

    let in_fifo4 = tmp.path().join("plugin-in.fifo");
    let out_fifo4 = tmp.path().join("plugin-out.fifo");
    make_fifo(&in_fifo4);
    make_fifo(&out_fifo4);
    let server4 = spawn_fake_remote_daemon(&in_fifo4, &out_fifo4, &remote_home, None);
    let via_plugin = via_bridge(&in_fifo4, &out_fifo4);

    let plugin_profile = tmp.path().join("plugin-profile.json");
    std::fs::write(
        &plugin_profile,
        r#"{"goatty_profile": 1, "host": {"version": 3, "plugins": {"greet": {"settings": {"name": "ada"}}}}}"#,
    )
    .unwrap();
    let o = ok(goatty(&local_home, &["settings", "import", "--yes", plugin_profile.to_str().unwrap(), "--via", &via_plugin]));
    server4.join().unwrap();

    let stdout = out(&o);
    let stderr = err(&o);
    assert!(!stderr.contains("plugins.greet: not installed on this host"), "redundant issue should be suppressed:\n{stderr}");
    assert!(stdout.contains("1 plugin(s) in this profile are not installed (goattyd on deploy@remote-box):"), "{stdout}");
    assert!(stdout.contains("Review and install on that host with: goattyd plugin install greet"), "{stdout}");
}
