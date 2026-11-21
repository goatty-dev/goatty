//! goatty desktop client.
//!
//! The window is always a client of a goattyd (local or remote) unless
//! `--local` is given. See `goatty --help`, and `goatty_config` for the
//! `GOATTY_*` environment variables.

mod actions;
mod agent;
mod app;
mod askpass;
mod cli;
mod close_confirm;
mod command_palette;
mod commands;
mod connect;
mod fonts;
mod hosts;
mod output_view;
mod paste_confirm;
mod pickers;
mod plugin_commands;
mod plugin_updates;
mod session;
mod settings;
mod settings_window;
mod shell_path;
mod shell_setup;
mod shell_sync;
mod snippets;
mod tailnet;
mod terminal;
mod toast;
mod typing;
mod ui;
mod welcome;
mod workspace;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;

use goatty_client::Endpoint;
use goatty_config::OnLaunch;
use goatty_shell::HostService;
use goatty_term::TermSize;

use crate::app::Initial;
use crate::cli::Cli;
use crate::connect::ConnectPlan;
use crate::session::{AttachMode, Session, Spawner};
use crate::workspace::saved::Startup;

pub const CLIENT: &str = concat!("goatty ", env!("CARGO_PKG_VERSION"));

fn main() -> Result<()> {
    // Started by ssh to ask a question (we set SSH_ASKPASS to ourselves).
    if let Some(req) = goatty_client::askpass::request_from_env() {
        askpass::run(req);
    }
    let cli = Cli::parse();
    // Subcommands work without a window (and without a display).
    if cli.cmd.is_some() {
        if cli.plain {
            anyhow::bail!("--plain only applies when opening a terminal window");
        }
        let remote = !cli.remote.is_empty();
        match &cli.cmd {
            Some(cli::Command::Settings(c)) => {
                if remote && !matches!(c, cli::SettingsCmd::Export { .. } | cli::SettingsCmd::Import { .. }) {
                    anyhow::bail!(
                        "--ssh and --via only apply to `settings export` and `settings import` (run `goattyd settings` on the host for its host.json)"
                    );
                }
                let endpoint = cli.endpoint()?;
                let Some(cli::Command::Settings(c)) = cli.cmd else { unreachable!() };
                return commands::settings(c, endpoint);
            }
            Some(cli::Command::Theme(_)) => {
                if remote {
                    anyhow::bail!("--ssh and --via don't apply to `theme`");
                }
                let Some(cli::Command::Theme(c)) = cli.cmd else { unreachable!() };
                return commands::theme(c);
            }
            Some(cli::Command::Completions { shell }) => {
                if remote {
                    anyhow::bail!("--ssh and --via don't apply to `completions`");
                }
                print!("{}", cli::completion_script(*shell));
                return Ok(());
            }
            None => unreachable!(),
        }
    }
    // goatty's own crates at info; dependencies (wgpu, zbus, ...) only warn, and
    // rio_vt only logs errors so unhandled OSC sequences (e.g. systemd OSC 3008) stay quiet (#20).
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,goatty=info,goatty_client=info,goatty_term=info,rio_vt=error"),
    )
    .init();
    // goatty's completions in the managed shell, kept current off the UI thread.
    let _ = std::thread::Builder::new().name("completions".into()).spawn(|| {
        let script = cli::completion_script(clap_complete::Shell::Zsh);
        goatty_shell::completions::write_own(goatty_shell::Dirs::from_env().as_ref(), "goatty", &script);
    });
    let client = goatty_config::load_client().value;
    let on_launch = client.workspace.on_launch;

    let mode = cli.attach_mode(on_launch == OnLaunch::New);
    let open_settings = cli.settings.clone();
    let endpoint = cli.endpoint()?;
    // Plain `goatty`: every window of the last run, with its tabs on every
    // host (#252). `--ssh`, `--new` and `--attach` ask for something else.
    let mut reopen = if on_launch == OnLaunch::Reattach && mode == AttachMode::Auto && matches!(endpoint, Some(Endpoint::Local)) {
        workspace::reopen::take_for_launch()
    } else {
        Vec::new()
    };
    if !reopen.is_empty() {
        let first = Initial::Reopen(reopen.remove(0));
        app::run(first, reopen.into_iter().map(Initial::Reopen).collect(), open_settings);
        return Ok(());
    }
    let initial = match endpoint {
        None => {
            // No daemon: this process owns the shell and the host settings.
            // Initial size is a guess; the terminal resizes on first layout.
            let daemon = std::env::current_exe().ok().map(|e| e.with_file_name("goattyd")).filter(|d| d.exists());
            let host = Arc::new(HostService::from_env(daemon));
            log::info!("backend: local pty");
            let session = Session::local(TermSize::new(100, 30), &host, None)?;
            Initial::Ready { startup: Startup::Session(session), spawner: Spawner::Local(host) }
        }
        // Connecting (and creating or attaching the session) happens in the
        // window, which opens right away (#37). `--plain`, or the host set to
        // plain SSH (Settings → Hosts), runs `ssh host` in a local session.
        Some(mut endpoint) => {
            let plain = match &mut endpoint {
                Endpoint::Ssh(target) => {
                    // ssh's prompts come up in goatty's dialogs.
                    target.askpass = std::env::current_exe().ok();
                    cli.plain || client.ssh.plain(&target.host)
                }
                _ => false,
            };
            log::info!("backend: goattyd via {}", endpoint.label());
            Initial::Connect(ConnectPlan { endpoint, mode, open_settings: None, plain, conn: None })
        }
    };
    app::run(initial, Vec::new(), open_settings);
    Ok(())
}
