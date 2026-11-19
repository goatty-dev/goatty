//! `goattyd`: run the goatty session daemon, or talk to a running one.

use anyhow::Result;
use clap::{Parser, Subcommand};
use goatty_client::Control;
use goatty_proto::SessionId;

mod net_cmd;

#[derive(Parser)]
#[command(name = "goattyd", version, about = "goatty session daemon: owns shells so they outlive the window")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (default).
    Serve {
        /// Take over from the previous goattyd (what a restart runs).
        #[arg(long, hide = true, value_name = "FILE")]
        restore: Option<std::path::PathBuf>,
    },
    /// Relay stdin/stdout to the daemon, starting it if needed (`ssh host goattyd proxy`).
    Proxy,
    /// List sessions.
    List,
    /// Kill a session.
    Kill { id: SessionId },
    /// Type text into a session. `\r`, `\n` and `\e` are unescaped (`\r` = Enter).
    Send { id: SessionId, text: String },
    /// Restart the daemon with this goattyd (e.g. after an update). Sessions keep running.
    Restart {
        /// Also restart a daemon too old to keep its sessions (they end).
        #[arg(long)]
        force: bool,
    },
    /// The newest restore format this goattyd reads (a restart checks it).
    #[command(hide = true)]
    RestoreVersion,
    /// Stop the daemon and all its sessions.
    Stop {
        /// Also stop a daemon this goattyd can't talk to (another protocol
        /// version), found through its socket.
        #[arg(long)]
        force: bool,
    },
    /// Run goattyd as a user service (systemd on Linux, launchd on macOS).
    #[command(subcommand)]
    Service(ServiceCmd),
    /// goatty's managed zsh on this host: status, tool installs and updates.
    #[command(subcommand)]
    Shell(ShellCmd),
    /// This host's settings (host.json: shell, prompt, history).
    #[command(subcommand)]
    Settings(SettingsCmd),
    /// Plugins on this host: search, install (with a review of what they do), configure, remove.
    #[command(subcommand)]
    Plugin(PluginCmd),
    /// Aliases in goatty's shells on this host: yours and the ones plugins bring.
    #[command(subcommand)]
    Alias(AliasCmd),
    /// Snippets (commands with blanks) and expansions on this host: yours and the plugins'.
    #[command(subcommand)]
    Snippet(SnippetCmd),
    /// This computer on your tailnet: make it available to your other devices, pair them.
    #[command(subcommand)]
    Net(net_cmd::NetCmd),
    /// Print the completion script for SHELL (goatty's managed zsh has it already).
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Install and enable the user service (systemd units on Linux, LaunchAgent on macOS).
    Install,
    /// Disable and remove the user service.
    Uninstall,
    /// Show whether the user service is installed and running.
    Status,
}

#[derive(Subcommand)]
enum SnippetCmd {
    /// Your snippets, then each plugin's, and the expansions.
    List,
    /// One snippet: its line, blanks and where it comes from.
    Show {
        id: String,
        /// The plugin's snippet with that id (default: the first found, yours first).
        #[arg(short, long)]
        plugin: Option<String>,
    },
    /// Add a snippet: `goattyd snippet add "Find big files" 'du -ah {dir} | sort -rh | head -n {count}'`.
    /// Every {name} becomes a blank goatty asks for.
    Add {
        title: String,
        line: String,
        #[arg(short, long)]
        description: Option<String>,
        /// A topic to filter by (repeatable).
        #[arg(short, long = "tag")]
        tags: Vec<String>,
    },
    /// Remove snippets or expansions of yours.
    #[command(visible_alias = "rm")]
    Remove {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Write your snippets and expansions as TOML (to stdout without FILE).
    Export { file: Option<std::path::PathBuf> },
    /// Add the snippets and expansions of a TOML file to yours.
    Import {
        file: std::path::PathBuf,
        /// Replace the ones you have with the same id.
        #[arg(short, long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum AliasCmd {
    /// Your aliases by group, then each plugin's, with what they run.
    List,
    /// Add an alias: `goattyd alias add gs git status`. Quote a command with options, pipes or
    /// quotes (`'git log --oneline'`), or put it after `--`.
    Add {
        name: String,
        /// What typing NAME runs.
        #[arg(required = true)]
        command: Vec<String>,
        /// List it under this heading.
        #[arg(short, long)]
        group: Option<String>,
        #[arg(short, long)]
        description: Option<String>,
        /// Add it switched off.
        #[arg(long)]
        disabled: bool,
        /// Replace an alias of yours with that name.
        #[arg(short, long)]
        force: bool,
    },
    /// Change one of your aliases.
    Edit {
        name: String,
        /// New command.
        #[arg(short, long, allow_hyphen_values = true)]
        command: Option<String>,
        /// Rename it.
        #[arg(short = 'n', long = "name", value_name = "NEW")]
        rename: Option<String>,
        /// Move it to another group ("" = none).
        #[arg(short, long)]
        group: Option<String>,
        #[arg(short, long)]
        description: Option<String>,
    },
    /// Remove aliases of yours.
    #[command(visible_alias = "rm")]
    Remove {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Switch aliases on: yours or plugins' by name, a group of yours, or a plugin's.
    Enable {
        names: Vec<String>,
        #[arg(short, long, conflicts_with = "plugin")]
        group: Option<String>,
        /// That plugin's aliases (all of them without names).
        #[arg(short, long)]
        plugin: Option<String>,
    },
    /// Switch aliases off without removing them (plugin aliases too).
    Disable {
        names: Vec<String>,
        #[arg(short, long, conflicts_with = "plugin")]
        group: Option<String>,
        #[arg(short, long)]
        plugin: Option<String>,
    },
}

#[derive(Subcommand)]
enum PluginCmd {
    /// What the registries offer (QUERY matches names, summaries, keywords).
    Search {
        query: Option<String>,
        /// Fetch the registries first.
        #[arg(long)]
        refresh: bool,
    },
    /// Details, settings and permissions of a plugin.
    Info { id: String },
    /// Review and install plugins (ID or ID@VERSION), or one from a directory.
    Install {
        plugins: Vec<String>,
        /// Install the plugin in DIR (development; not verified by any registry).
        #[arg(long, value_name = "DIR")]
        path: Option<std::path::PathBuf>,
        /// Approve the plan without asking (it's still printed).
        #[arg(short, long)]
        yes: bool,
    },
    /// Install newer versions from the registries (default: all plugins).
    Update {
        plugins: Vec<String>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove plugins, their programs and their settings.
    Uninstall {
        plugins: Vec<String>,
        /// Also remove the plugins they needed that nothing else needs, without asking.
        #[arg(short, long)]
        yes: bool,
    },
    /// Installed plugins and their state.
    List,
    /// Load an installed plugin in new shells again.
    Enable { id: String },
    /// Keep a plugin installed but out of the shell.
    Disable { id: String },
    /// Change a plugin setting (checked against the plugin's settings).
    Set { id: String, key: String, value: String },
    /// Reset a plugin setting to its default.
    Unset { id: String, key: String },
    /// Plugin registries.
    #[command(subcommand)]
    Registry(RegistryCmd),
}

#[derive(Subcommand)]
enum RegistryCmd {
    /// The registries and their state.
    List {
        #[arg(long)]
        refresh: bool,
    },
    /// Fetch every registry now.
    Refresh,
    /// Trust another registry: URL of its .tar.gz (or an absolute directory) and its signing key.
    Add {
        name: String,
        url: String,
        /// Public key (hex) that signs its index (repeatable).
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
    },
    /// Stop using an added registry (installed plugins stay).
    Remove { name: String },
}

#[derive(Subcommand)]
enum SettingsCmd {
    /// Where host.json is.
    Path,
    /// Print what's stored, `--all` values, or one dotted KEY.
    Show {
        key: Option<String>,
        #[arg(long, conflicts_with = "key")]
        all: bool,
    },
    /// Check host.json (or FILE); exit status 1 on problems.
    Validate { file: Option<std::path::PathBuf> },
    /// Set a dotted KEY (e.g. history.size 5000); builds and activates the shell configuration.
    Set { key: String, value: String },
    /// Reset KEY to its default.
    Unset { key: String },
}

#[derive(Subcommand)]
enum ShellCmd {
    /// What the managed shell loads, what's missing, and its startup time.
    Status,
    /// Install the tools of goatty's built-in plugins into goatty's data
    /// directory (no sudo). Without names: missing default ones plus outdated
    /// managed copies (what `goatty-setup` runs).
    Install {
        /// Built-in plugins: fzf, fzf-tab, autosuggestions, syntax-highlighting, starship.
        tools: Vec<String>,
        /// Don't ask before downloading.
        #[arg(short, long)]
        yes: bool,
    },
    /// Update goatty's managed copies of built-in plugins' tools to the pinned
    /// versions (no sudo). Without names: all outdated managed copies.
    Update {
        /// Built-in plugins: fzf, fzf-tab, autosuggestions, syntax-highlighting, starship.
        tools: Vec<String>,
        /// Don't ask before downloading.
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove goatty's downloads of built-in plugins' tools (the plugins stay on; system copies still work).
    Uninstall {
        tools: Vec<String>,
        #[arg(long, conflicts_with = "tools")]
        all: bool,
    },
    /// Stop new shells from mentioning missing tools.
    Dismiss,
    /// List the built shell configurations (generations) and what each changed.
    Generations,
    /// Name a generation, so it's easy to find again: `goattyd shell name 7 before trying fish`.
    /// Without a name: remove it.
    Name { generation: u32, name: Vec<String> },
    /// Show how the current settings would change the generated files; with a
    /// generation, what that one changed.
    Diff { generation: Option<u32> },
    /// Activate an older generation (default: the previous one) and restore
    /// the settings it was built from.
    Rollback { generation: Option<u32> },
    /// Ask the daemon's running managed shells to restart with the active
    /// configuration (idle ones now, busy ones at their next prompt).
    Reload,
    /// What goatty imports from your own shell (PATH folders and variables
    /// from .zshrc or .bashrc), and the aliases it can copy into goatty's.
    Import {
        /// Run your shell now to see what it sets up (also while the import is off).
        #[arg(long)]
        refresh: bool,
    },
}

const CLIENT: &str = concat!("goattyd-cli ", env!("CARGO_PKG_VERSION"));

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Cmd::Serve { restore: None });
    if matches!(cmd, Cmd::Serve { .. }) {
        // systemd's variables, before any thread starts.
        goatty_daemon::systemd::capture_env();
    }
    // The proxy's stderr goes back over ssh into the GUI's log: keep it quiet.
    // Silence rio_vt's [unhandled osc_dispatch] warnings (e.g. systemd OSC 3008) by default (#20).
    let default_level = if matches!(
        cmd,
        Cmd::Proxy
            | Cmd::RestoreVersion
            | Cmd::Shell(_)
            | Cmd::Settings(_)
            | Cmd::Plugin(_)
            | Cmd::Alias(_)
            | Cmd::Snippet(_)
            | Cmd::Net(_)
    ) {
        "warn,rio_vt=error"
    } else {
        "info,rio_vt=error"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level)).init();

    match cmd {
        Cmd::Serve { restore } => {
            // Keep goattyd's completions (and those of goatty's tool copies)
            // in the managed shell current, off the startup path.
            let _ = std::thread::Builder::new().name("completions".into()).spawn(|| {
                let dirs = goatty_shell::Dirs::from_env();
                goatty_shell::completions::write_own(dirs.as_ref(), "goattyd", &completion_script(clap_complete::Shell::Zsh));
                if let Some(dirs) = &dirs {
                    goatty_shell::completions::backfill(dirs);
                }
            });
            let start = match restore {
                Some(file) => goatty_daemon::Start::Restore(file),
                None => goatty_daemon::Start::Normal,
            };
            goatty_daemon::serve(&goatty_proto::socket_path(), start)
        }
        Cmd::RestoreVersion => {
            println!("{}", goatty_daemon::STATE_VERSION);
            Ok(())
        }
        Cmd::Restart { force } => restart(force),
        Cmd::Completions { shell } => {
            print!("{}", completion_script(shell));
            Ok(())
        }
        Cmd::Proxy => goatty_daemon::proxy(),
        Cmd::List => {
            let list = Control::connect_local(CLIENT)?.list_sessions()?;
            if list.is_empty() {
                println!("no sessions");
            }
            for s in list {
                println!(
                    "{:>3}  pid {:<8} {}x{:<4} attached {}  running {:<10} cwd {}  {}",
                    s.id,
                    s.pid.map(|p| p.to_string()).unwrap_or_default(),
                    s.cols,
                    s.rows,
                    s.attached,
                    s.process.unwrap_or_else(|| "?".into()),
                    s.cwd.unwrap_or_default(),
                    s.title
                );
            }
            Ok(())
        }
        Cmd::Kill { id } => Control::connect_local(CLIENT)?.kill(id),
        Cmd::Send { id, text } => {
            let data = text.replace("\\r", "\r").replace("\\n", "\n").replace("\\e", "\x1b").into_bytes();
            Control::connect_local(CLIENT)?.send_input(id, data)
        }
        Cmd::Stop { force } => stop(force),
        Cmd::Net(cmd) => net_cmd::run(cmd, CLIENT),
        Cmd::Service(cmd) => match cmd {
            ServiceCmd::Install => service_install(),
            ServiceCmd::Uninstall => service_uninstall(),
            ServiceCmd::Status => service_status(),
        },
        Cmd::Settings(cmd) => {
            use goatty_shell::cmd;
            let svc = goatty_shell::HostService::from_env(std::env::current_exe().ok());
            match cmd {
                SettingsCmd::Path => cmd::settings_path(&svc),
                SettingsCmd::Show { key, all } => cmd::settings_show(&svc, key.as_deref(), all),
                SettingsCmd::Validate { file } => cmd::settings_validate(&svc, file.as_deref()),
                SettingsCmd::Set { key, value } => cmd::settings_set(&svc, &key, &value),
                SettingsCmd::Unset { key } => cmd::settings_unset(&svc, &key),
            }
        }
        Cmd::Plugin(cmd) => {
            use goatty_shell::plugins::cmd;
            let svc = goatty_shell::HostService::from_env(std::env::current_exe().ok());
            match cmd {
                PluginCmd::Search { query, refresh } => cmd::search(&svc, query.as_deref(), refresh),
                PluginCmd::Info { id } => cmd::info(&svc, &id),
                PluginCmd::Install { plugins, path, yes } => cmd::install(&svc, &plugins, path.as_deref(), yes),
                PluginCmd::Update { plugins, yes } => cmd::update(&svc, &plugins, yes),
                PluginCmd::Uninstall { plugins, yes } => cmd::uninstall(&svc, &plugins, yes),
                PluginCmd::List => cmd::list(&svc),
                PluginCmd::Enable { id } => cmd::set_enabled(&svc, &id, true),
                PluginCmd::Disable { id } => cmd::set_enabled(&svc, &id, false),
                PluginCmd::Set { id, key, value } => cmd::set(&svc, &id, &key, Some(&value)),
                PluginCmd::Unset { id, key } => cmd::set(&svc, &id, &key, None),
                PluginCmd::Registry(r) => match r {
                    RegistryCmd::List { refresh } => cmd::registries(&svc, refresh),
                    RegistryCmd::Refresh => cmd::registries(&svc, true),
                    RegistryCmd::Add { name, url, keys } => cmd::registry_add(&svc, &name, &url, &keys),
                    RegistryCmd::Remove { name } => cmd::registry_remove(&svc, &name),
                },
            }
        }
        Cmd::Alias(cmd) => {
            use goatty_shell::aliases::cmd;
            let svc = goatty_shell::HostService::from_env(std::env::current_exe().ok());
            match cmd {
                AliasCmd::List => cmd::list(&svc),
                AliasCmd::Add { name, command, group, description, disabled, force } => {
                    cmd::add(&svc, &name, &cmd::command_from(&command), group.as_deref(), description.as_deref(), !disabled, force)
                }
                AliasCmd::Edit { name, command, rename, group, description } => {
                    cmd::edit(&svc, &name, cmd::Edit { rename, command, group, description })
                }
                AliasCmd::Remove { names } => cmd::remove(&svc, &names),
                AliasCmd::Enable { names, group, plugin } => cmd::set_enabled(&svc, cmd::Selection { names, group, plugin }, true),
                AliasCmd::Disable { names, group, plugin } => cmd::set_enabled(&svc, cmd::Selection { names, group, plugin }, false),
            }
        }
        Cmd::Snippet(cmd) => {
            use goatty_shell::snippet_cmd as cmd;
            let svc = goatty_shell::HostService::from_env(std::env::current_exe().ok());
            match cmd {
                SnippetCmd::List => cmd::list(&svc),
                SnippetCmd::Show { id, plugin } => cmd::show(&svc, &id, plugin.as_deref()),
                SnippetCmd::Add { title, line, description, tags } => cmd::add(&svc, &title, &line, description.as_deref(), &tags),
                SnippetCmd::Remove { ids } => cmd::remove(&svc, &ids),
                SnippetCmd::Export { file } => cmd::export(&svc, file.as_deref()),
                SnippetCmd::Import { file, force } => cmd::import(&svc, &file, force),
            }
        }
        Cmd::Shell(cmd) => {
            use goatty_shell::cmd;
            let host = goatty_config::load_host();
            let launcher = goatty_shell::Launcher::from_env(std::env::current_exe().ok());
            let host_file = || goatty_config::host_file().ok_or_else(|| anyhow::anyhow!("$HOME is not set"));
            match cmd {
                ShellCmd::Status => cmd::status(&goatty_shell::HostService::from_env(std::env::current_exe().ok())),
                ShellCmd::Install { tools, yes } => cmd::install(&host, &launcher, &tools, yes),
                ShellCmd::Update { tools, yes } => cmd::update(&host, &launcher, &tools, yes),
                ShellCmd::Generations => cmd::generations(&launcher),
                ShellCmd::Name { generation, name } => cmd::name(&launcher, generation, &name),
                ShellCmd::Diff { generation } => cmd::diff(&host, &launcher, generation),
                ShellCmd::Rollback { generation } => cmd::rollback(&launcher, &host_file()?, generation),
                ShellCmd::Uninstall { tools, all } => cmd::uninstall(&launcher, &tools, all),
                ShellCmd::Dismiss => cmd::dismiss(&launcher),
                ShellCmd::Import { refresh } => cmd::import(&goatty_shell::HostService::from_env(std::env::current_exe().ok()), refresh),
                ShellCmd::Reload => {
                    let n = Control::connect_local(CLIENT)?.reload_shells()?;
                    println!("asked {n} running shell(s) to reload");
                    Ok(())
                }
            }
        }
    }
}

/// goattyd's completion script for `shell`.
fn completion_script(shell: clap_complete::Shell) -> String {
    use clap::CommandFactory;
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "goattyd", &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// `goattyd restart`: replace the running daemon with this goattyd.
fn restart(force: bool) -> Result<()> {
    use goatty_client::local::{Restarted, Stopped, restart_daemon};
    let exe = std::env::current_exe()?;
    match restart_daemon(Some(&exe), force)? {
        Restarted::NotRunning => eprintln!("goattyd isn't running"),
        Restarted::Kept { sessions } => eprintln!("goattyd runs {} now; its {sessions} session(s) kept running", exe.display()),
        Restarted::Stopped(how) => {
            let why = match how {
                Stopped::Service { why } | Stopped::Killed { why, .. } => format!(" ({why})"),
                Stopped::NotRunning | Stopped::Asked => String::new(),
            };
            eprintln!("stopped the old goattyd{why}, which ended its sessions; the next connection starts this one");
        }
    }
    Ok(())
}

/// `goattyd stop`: ask the daemon to shut down; with `force`, stop one that
/// doesn't speak our protocol too (old versions, after an upgrade).
fn stop(force: bool) -> Result<()> {
    use goatty_client::local::{Stopped, stop_daemon};
    match stop_daemon(force)? {
        Stopped::NotRunning => eprintln!("goattyd isn't running"),
        Stopped::Asked => {}
        Stopped::Service { why } => eprintln!("goattyd: {why}\nstopped {}", goatty_client::service::service_name()),
        Stopped::Killed { pid, why } => eprintln!("goattyd: {why}\nstopped pid {pid}"),
    }
    Ok(())
}

fn service_install() -> Result<()> {
    use goatty_client::service;
    let exe = std::env::current_exe()?;
    let done = service::install(&exe, &goatty_proto::socket_path())?;
    #[cfg(target_os = "macos")]
    {
        println!("installed {} in {}", service::LAUNCHD_PLIST, done.unit_dir.display());
        if done.started {
            println!("goattyd now starts through launchd on the first connection (logs: {})", service::logs_hint());
        } else {
            println!(
                "a goattyd started outside launchd is still running; launchd takes over once it stops\n\
                 (`goattyd stop` ends its sessions now, or it happens at your next login)"
            );
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        println!("installed {} and {} in {}", service::SOCKET_UNIT, service::SERVICE_UNIT, done.unit_dir.display());
        if done.started {
            println!("goattyd now starts through systemd on the first connection (logs: journalctl --user -u goattyd)");
        } else {
            println!(
                "a goattyd started outside systemd is still running; systemd takes over once it stops\n\
                 (`goattyd stop` ends its sessions now, or it happens at your next login)"
            );
        }
        print_linger(done.linger);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn print_linger(linger: Option<bool>) {
    match linger {
        Some(true) => println!("lingering is on: goattyd and its sessions keep running after you log out"),
        Some(false) => println!(
            "lingering is off: systemd stops goattyd, and every session in it, when your last login session ends.\n\
             On a server, keep them with `loginctl enable-linger` (may ask for your password)."
        ),
        None => println!("couldn't check lingering (`loginctl show-user $USER -p Linger`)"),
    }
}

fn service_uninstall() -> Result<()> {
    let removed = goatty_client::service::uninstall()?;
    #[cfg(target_os = "macos")]
    {
        if removed {
            println!("unloaded and removed {}", goatty_client::service::LAUNCHD_PLIST);
        } else {
            println!("goattyd's launchd agent isn't installed");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if removed {
            println!("removed goattyd's systemd units; a running goattyd keeps its sessions until `goattyd stop`");
        } else {
            println!("goattyd's systemd units aren't installed");
        }
    }
    Ok(())
}

fn service_status() -> Result<()> {
    let s = goatty_client::service::status();
    let dir = s.unit_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "?".into());
    if !s.installed {
        #[cfg(target_os = "macos")]
        println!("not installed (plist would go to {dir}); `goattyd service install` sets it up");
        #[cfg(not(target_os = "macos"))]
        println!("not installed (units would go to {dir}); `goattyd service install` sets them up");
        return Ok(());
    }
    println!("installed in {dir}");
    if let Some(exec) = &s.exec_start {
        println!("runs      {exec}");
    }
    let state = |on: bool| if on { "active" } else { "inactive" };
    println!("socket    {}", state(s.socket_active));
    println!("service   {}", state(s.service_active));
    #[cfg(target_os = "macos")]
    {
        if let Some(log) = &s.log_path {
            println!("logs      {}", log.display());
        }
        println!("sessions  survive `goattyd restart` (re-exec); crashes and `launchctl kickstart -k` end them (launchd has no fd store)");
    }
    #[cfg(not(target_os = "macos"))]
    {
        if s.keeps_sessions {
            println!("sessions  survive daemon restarts and crashes; `systemctl --user stop goattyd` ends them");
        } else {
            println!("sessions  end when the daemon does: units from an older goattyd (`goattyd service install` updates them)");
        }
        print_linger(s.linger);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_is_consistent() {
        use clap::CommandFactory;
        super::Cli::command().debug_assert();
    }

    #[test]
    fn zsh_completions() {
        let s = super::completion_script(clap_complete::Shell::Zsh);
        assert!(s.starts_with("#compdef goattyd"), "{s}");
        assert!(s.contains("rollback") && s.contains("registry"), "subcommands are in it");
    }
}
