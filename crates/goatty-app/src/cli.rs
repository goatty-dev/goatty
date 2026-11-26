//! Command line of the `goatty` binary.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use goatty_client::Endpoint;

use crate::session::AttachMode;

#[derive(Debug, Default, Clone, clap::Args)]
pub struct RemoteArgs {
    /// Use goattyd on HOST, via `ssh HOST goattyd proxy`.
    #[arg(long, value_name = "HOST", conflicts_with_all = ["via", "tailnet"])]
    pub ssh: Option<String>,

    /// Path of goattyd on the SSH host, if it isn't in PATH (requires `--ssh`).
    #[arg(long, value_name = "PATH")]
    pub remote_daemon: Option<String>,

    /// Use goattyd behind any stdio command, e.g. 'docker exec -i box goattyd proxy'.
    /// It runs under `sh -c`, so quote arguments as in a shell (on Windows it is split
    /// into words and run directly, e.g. 'wsl.exe -d Ubuntu -- goattyd proxy').
    #[arg(long, value_name = "CMD", conflicts_with_all = ["ssh", "tailnet"])]
    pub via: Option<String>,

    /// Use goattyd on a paired Tailscale host (name, MagicDNS, or IP).
    #[arg(long, value_name = "HOST", conflicts_with_all = ["ssh", "via"])]
    pub tailnet: Option<String>,
}

impl RemoteArgs {
    pub fn is_empty(&self) -> bool {
        self.ssh.is_none() && self.via.is_none() && self.tailnet.is_none() && self.remote_daemon.is_none()
    }
}

#[derive(Debug, Parser)]
#[command(name = "goatty", version, about = "A fast, batteries-included terminal")]
pub struct Cli {
    /// Always create a new session: a fresh tab in the most recently active workspace (default: restore the windows of the last run, else join that workspace, with a fresh shell when it has no tabs).
    #[arg(long, conflicts_with_all = ["attach", "local"])]
    pub new: bool,

    /// Attach to a specific session (see `goattyd list`).
    #[arg(long, value_name = "ID", conflicts_with = "local")]
    pub attach: Option<u64>,

    /// No daemon: this window owns the shell, which ends when it closes.
    #[arg(long, conflicts_with_all = ["ssh", "via", "tailnet"])]
    pub local: bool,

    #[command(flatten)]
    pub remote: RemoteArgs,

    /// With --ssh: a plain `ssh HOST` in a local session instead of goattyd on HOST.
    /// Nothing is installed there, and goatty's shell setup doesn't reach it.
    #[arg(long, requires = "ssh", conflicts_with_all = ["new", "attach", "remote_daemon"])]
    pub plain: bool,

    /// Also open the settings window, optionally at a page (appearance,
    /// shell, path, prompt, history, aliases, plugins, shortcuts, workspace, profile).
    #[arg(long, value_name = "PAGE", num_args = 0..=1, default_missing_value = "appearance")]
    pub settings: Option<String>,

    /// `goatty://` link(s) to open (shorthand for `goatty open <URL>`).
    #[arg(value_name = "URL")]
    pub urls: Vec<String>,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

/// Work without a window.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open a `goatty://` link in a running goatty window (or start goatty).
    Open {
        /// The `goatty://` link to open.
        url: String,
    },
    /// Inspect and change settings, export and import profiles.
    #[command(subcommand)]
    Settings(SettingsCmd),
    /// List, import, export and pick color themes.
    #[command(subcommand)]
    Theme(ThemeCmd),
    /// Pair with and manage remote computers on your tailnet (client side).
    #[command(subcommand)]
    Net(NetCmd),
    /// Print the completion script for SHELL (goatty's managed zsh has it already).
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Debug, Subcommand)]
pub enum NetCmd {
    /// Show Tailscale status, paired hosts, and discovered peers running goatty.
    #[command(visible_alias = "status")]
    List {
        /// Also list tagged and other people's devices on your tailnet.
        #[arg(long)]
        all: bool,
        /// Skip probing tailnet peers; only show paired hosts.
        #[arg(long)]
        no_discover: bool,
    },
    /// Pair with a remote computer on your tailnet: pass a `goatty-pair:…` code
    /// from `goattyd net pair`, a Tailscale host[:port], or `--ssh HOST` to enable
    /// Tailscale and pair over an existing SSH connection.
    Pair {
        /// A `goatty-pair:…` code, or a Tailscale hostname / MagicDNS / IP (optional :port).
        #[arg(value_name = "CODE_OR_HOST")]
        target: Option<String>,
        /// Pair a host reachable over SSH right now (enables Tailscale Serve and authorizes this device).
        #[arg(long, value_name = "SSH_HOST", conflicts_with = "target")]
        ssh: Option<String>,
        /// Path of goattyd on the SSH host, if it isn't in PATH (requires `--ssh`).
        #[arg(long, value_name = "PATH")]
        remote_daemon: Option<String>,
    },
    /// Forget a paired host on this computer (name, MagicDNS, or IP).
    Forget { host: String },
}

#[derive(Debug, Subcommand)]
pub enum SettingsCmd {
    /// Where the settings files are.
    Path,
    /// Print this computer's settings: what's stored, `--all` values, or one KEY.
    Show {
        key: Option<String>,
        #[arg(long, conflicts_with = "key")]
        all: bool,
    },
    /// Check settings.json (or FILE) and the themes; exit status 1 on problems.
    Validate { file: Option<PathBuf> },
    /// Set a dotted KEY (e.g. font.size 15, theme.name nord). The running app picks it up.
    Set { key: String, value: String },
    /// Reset KEY to its default.
    Unset { key: String },
    /// Write a profile (these settings + the host's shell settings) to FILE, `-` for stdout.
    /// Use `--ssh` or `--via` for another host.
    Export {
        file: PathBuf,
        #[command(flatten)]
        remote: RemoteArgs,
    },
    /// Apply a profile from FILE (`-` for stdin). Host settings go to the local goattyd,
    /// or to the host given with `--ssh` / `--via`.
    Import {
        file: PathBuf,
        /// Don't ask.
        #[arg(short, long)]
        yes: bool,
        #[command(flatten)]
        remote: RemoteArgs,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ThemeFor {
    Light,
    Dark,
}

#[derive(Debug, Subcommand)]
pub enum ThemeCmd {
    /// Built-in and your own themes.
    List,
    /// Import color schemes (base16 YAML/JSON, iTerm2 .itermcolors, Windows Terminal, goatty).
    Import {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Name for the imported theme (one scheme only).
        #[arg(long)]
        name: Option<String>,
    },
    /// Write a theme as a goatty theme file (stdout without FILE).
    Export { id: String, file: Option<PathBuf> },
    /// Use theme ID; `--for light|dark` sets that side and follows the system.
    Set {
        id: String,
        #[arg(long = "for", value_enum)]
        target: Option<ThemeFor>,
    },
    /// Delete one of your themes.
    Delete { id: String },
}

/// goatty's completion script for `shell`.
pub fn completion_script(shell: clap_complete::Shell) -> String {
    use clap::CommandFactory;
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "goatty", &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn merge_opt<'a>(a: Option<&'a str>, b: Option<&'a str>, flag: &str) -> Result<Option<&'a str>> {
    match (a, b) {
        (Some(_), Some(_)) => anyhow::bail!("{flag} cannot be used more than once"),
        (Some(v), None) | (None, Some(v)) => Ok(Some(v)),
        (None, None) => Ok(None),
    }
}

impl Cli {
    /// `prefer_new`: the workspace setting asks for a new session by default.
    pub fn attach_mode(&self, prefer_new: bool) -> AttachMode {
        match (self.new, self.attach) {
            (true, _) => AttachMode::New,
            (false, Some(id)) => AttachMode::Id(id),
            (false, None) if prefer_new => AttachMode::New,
            (false, None) => AttachMode::Join(None),
        }
    }

    /// The daemon to use, or `None` for `--local`.
    pub fn endpoint(&self) -> Result<Option<Endpoint>> {
        let sub_remote = match &self.cmd {
            Some(Command::Settings(SettingsCmd::Export { remote, .. } | SettingsCmd::Import { remote, .. })) => Some(remote),
            _ => None,
        };
        let ssh = merge_opt(self.remote.ssh.as_deref(), sub_remote.and_then(|r| r.ssh.as_deref()), "--ssh")?;
        let via = merge_opt(self.remote.via.as_deref(), sub_remote.and_then(|r| r.via.as_deref()), "--via")?;
        let tailnet = merge_opt(self.remote.tailnet.as_deref(), sub_remote.and_then(|r| r.tailnet.as_deref()), "--tailnet")?;
        let remote_daemon =
            merge_opt(self.remote.remote_daemon.as_deref(), sub_remote.and_then(|r| r.remote_daemon.as_deref()), "--remote-daemon")?;

        if self.local && (ssh.is_some() || via.is_some() || tailnet.is_some() || remote_daemon.is_some()) {
            anyhow::bail!("--local cannot be used with --ssh, --via, or --tailnet");
        }
        if remote_daemon.is_some() && ssh.is_none() {
            anyhow::bail!("--remote-daemon requires --ssh");
        }
        let chosen = [ssh.is_some(), via.is_some(), tailnet.is_some()].into_iter().filter(|&b| b).count();
        if chosen > 1 {
            anyhow::bail!("--ssh, --via, and --tailnet cannot be used together");
        }
        Ok(match (ssh, via, tailnet) {
            _ if self.local => None,
            (Some(host), None, None) => Some(Endpoint::ssh(host, remote_daemon)),
            (None, Some(cmd), None) => Some(Endpoint::command_line(cmd)?),
            (None, None, Some(host)) => Some(tailnet_endpoint(host)?),
            (None, None, None) => Some(Endpoint::Local),
            _ => unreachable!(),
        })
    }
}

fn tailnet_endpoint(query: &str) -> Result<Endpoint> {
    let paths = crate::tailnet::paths().ok_or_else(|| anyhow::anyhow!("no $HOME: goatty keeps paired tailnet hosts there"))?;
    let host = goatty_client::tailnet::find_known_host(&paths, query).ok_or_else(|| {
        anyhow::anyhow!(
            "{query} isn't paired yet: run `goattyd net pair` on that host and `goatty net pair <code|host>` here (or Settings → Network)"
        )
    })?;
    Ok(Endpoint::Tailnet(goatty_client::TailnetTarget {
        dns: host.dns,
        name: host.name,
        paths: Some(paths),
        tailscale: goatty_net::tailscale::Tailscale::system(),
    }))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn modes() {
        let cli = Cli::parse_from(["goatty", "--attach", "3"]);
        assert!(matches!(cli.attach_mode(true), AttachMode::Id(3)));
        assert!(matches!(Cli::parse_from(["goatty"]).attach_mode(true), AttachMode::New));
        assert!(matches!(Cli::parse_from(["goatty"]).attach_mode(false), AttachMode::Join(None)));
        assert!(matches!(cli.endpoint().unwrap(), Some(Endpoint::Local)));
        assert!(Cli::parse_from(["goatty", "--local"]).endpoint().unwrap().is_none());
        assert!(Cli::try_parse_from(["goatty", "--local", "--ssh", "h"]).is_err());
        assert!(Cli::parse_from(["goatty", "--remote-daemon", "/x"]).endpoint().is_err());
        assert!(Cli::parse_from(["goatty", "--ssh", "box", "--plain"]).plain);
        assert!(Cli::try_parse_from(["goatty", "--plain"]).is_err());
        assert!(matches!(Cli::parse_from(["goatty", "--ssh", "box"]).endpoint().unwrap(), Some(Endpoint::Ssh(t)) if t.host == "box"));
        let cli = Cli::parse_from(["goatty", "settings", "set", "font.size", "15"]);
        assert!(matches!(cli.cmd, Some(Command::Settings(SettingsCmd::Set { .. }))));
        let cli = Cli::parse_from(["goatty", "theme", "set", "nord", "--for", "dark"]);
        assert!(matches!(cli.cmd, Some(Command::Theme(ThemeCmd::Set { target: Some(ThemeFor::Dark), .. }))));

        // Endpoint flags work before, after, or split across `settings export` / `settings import`.
        let cli = Cli::parse_from(["goatty", "settings", "import", "p.json", "--ssh", "prod", "--remote-daemon", "/opt/goattyd", "--yes"]);
        assert!(matches!(cli.cmd, Some(Command::Settings(SettingsCmd::Import { yes: true, .. }))));
        assert!(matches!(
            cli.endpoint().unwrap(),
            Some(Endpoint::Ssh(t)) if t.host == "prod" && t.remote_daemon.as_deref() == Some("/opt/goattyd")
        ));

        let cli = Cli::parse_from(["goatty", "--ssh", "prod", "settings", "export", "-", "--remote-daemon", "/opt/goattyd"]);
        assert!(matches!(cli.cmd, Some(Command::Settings(SettingsCmd::Export { .. }))));
        assert!(matches!(
            cli.endpoint().unwrap(),
            Some(Endpoint::Ssh(t)) if t.host == "prod" && t.remote_daemon.as_deref() == Some("/opt/goattyd")
        ));

        let cli = Cli::parse_from(["goatty", "settings", "export", "-", "--via", "docker exec -i box goattyd proxy"]);
        assert!(matches!(cli.cmd, Some(Command::Settings(SettingsCmd::Export { .. }))));
        assert!(matches!(cli.endpoint().unwrap(), Some(Endpoint::Command { label, .. }) if label == "docker"));

        assert!(Cli::try_parse_from(["goatty", "settings", "import", "p.json", "--ssh", "a", "--via", "b"]).is_err());
        assert!(Cli::parse_from(["goatty", "--ssh", "a", "settings", "export", "-", "--via", "b"]).endpoint().is_err());
        assert!(Cli::parse_from(["goatty", "--via", "b", "settings", "export", "-", "--ssh", "a"]).endpoint().is_err());
        assert!(Cli::parse_from(["goatty", "--ssh", "staging", "settings", "import", "p.json", "--ssh", "prod"]).endpoint().is_err());
        assert!(Cli::parse_from(["goatty", "--via", "a", "settings", "import", "p.json", "--via", "b"]).endpoint().is_err());
        assert!(
            Cli::parse_from(["goatty", "--ssh", "prod", "--remote-daemon", "/a", "settings", "export", "-", "--remote-daemon", "/b",])
                .endpoint()
                .is_err()
        );
        assert!(Cli::parse_from(["goatty", "--local", "settings", "import", "p.json", "--ssh", "a"]).endpoint().is_err());
        assert!(Cli::parse_from(["goatty", "settings", "import", "p.json", "--remote-daemon", "/x"]).endpoint().is_err());
        assert!(Cli::try_parse_from(["goatty", "settings", "import", "p.json", "--plain"]).is_err());

        // `goatty net` subcommands and `--tailnet` flag.
        let cli = Cli::parse_from(["goatty", "net", "list", "--all", "--no-discover"]);
        assert!(matches!(cli.cmd, Some(Command::Net(NetCmd::List { all: true, no_discover: true }))));
        let cli = Cli::parse_from(["goatty", "net", "pair", "goatty-pair:abc"]);
        assert!(matches!(cli.cmd, Some(Command::Net(NetCmd::Pair { target: Some(_), ssh: None, .. }))));
        let cli = Cli::parse_from(["goatty", "net", "pair", "--ssh", "vps"]);
        assert!(matches!(cli.cmd, Some(Command::Net(NetCmd::Pair { target: None, ssh: Some(_), .. }))));
        let cli = Cli::parse_from(["goatty", "net", "forget", "vps"]);
        assert!(matches!(cli.cmd, Some(Command::Net(NetCmd::Forget { .. }))));
        assert!(Cli::try_parse_from(["goatty", "--ssh", "a", "--tailnet", "b"]).is_err());
        assert!(Cli::try_parse_from(["goatty", "--local", "--tailnet", "b"]).is_err());

        // `goatty open <URL>` and bare `goatty goatty://…` shorthand.
        let cli = Cli::parse_from(["goatty", "open", "goatty://settings/terminal?setting=links.open_on"]);
        assert!(matches!(cli.cmd, Some(Command::Open { ref url }) if url == "goatty://settings/terminal?setting=links.open_on"));
        let cli = Cli::parse_from(["goatty", "goatty://plugins/eza"]);
        assert!(cli.cmd.is_none());
        assert_eq!(cli.urls, ["goatty://plugins/eza"]);
    }

    #[test]
    fn zsh_completions() {
        let s = completion_script(clap_complete::Shell::Zsh);
        assert!(s.starts_with("#compdef goatty"), "{s}");
        assert!(s.contains("--settings") && s.contains("theme"), "{s}");
    }
}
