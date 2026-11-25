//! `goattyd net …`: this host on the user's tailnet, through the running
//! daemon (it owns the gateway and the pairing window).

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use goatty_client::Control;
use goatty_net::api::{NetStatus, Request, RouteStatus};
use goatty_net::identity::short;
use goatty_net::pairing::spaced;

#[derive(Subcommand)]
pub enum NetCmd {
    /// Tailscale's state, whether this computer is available, the pairing window, paired devices.
    Status,
    /// Make this computer available to your other devices on your tailnet (adds one `tailscale serve` route).
    Enable {
        /// The tailnet port (default: 7422, or the one used before).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Remove goatty's Serve route. Paired devices stay paired.
    Disable,
    /// Allow a new device: enables tailnet access if needed, prints a one-time pairing code, and waits for a device.
    Pair {
        /// How long the window stays open, in seconds.
        #[arg(long, default_value_t = 300)]
        secs: u64,
        /// The tailnet port (default: 7422, or the one used before).
        #[arg(long)]
        port: Option<u16>,
        /// Print the pairing code and return immediately, leaving the pairing window open in goattyd.
        #[arg(long)]
        no_wait: bool,
        /// Print only the raw `goatty-pair:…` pairing code and return immediately (for scripts).
        #[arg(long)]
        token_only: bool,
    },
    /// Authorize a device's public key directly (64 hex characters).
    Authorize {
        /// The client device's ed25519 public key (hex).
        key: String,
        /// Friendly name for the device.
        #[arg(default_value = "goatty")]
        name: String,
    },
    /// The devices paired with this computer.
    Devices,
    /// Forget a paired device (the first 8 or more characters of its key, from `devices`).
    Revoke { key: String },
    /// Turn availability off and forget every paired device.
    Leave,
}

fn request(control: &mut Control, req: &Request) -> Result<NetStatus> {
    let reply = control.net(serde_json::to_string(req)?)?;
    serde_json::from_str::<goatty_net::api::Reply>(&reply)?.into_result()
}

/// Connect to the running local `goattyd`, starting it first if needed.
fn connect_or_start(client: &str) -> Result<Control> {
    if let Ok(c) = Control::connect_local(client) {
        return Ok(c);
    }
    let _stream = goatty_client::connect_or_spawn_daemon(std::env::current_exe().ok())?;
    Control::connect_local(client)
}

fn enable(c: &mut Control, port: Option<u16>) -> Result<NetStatus> {
    match request(c, &Request::Enable { port }) {
        Ok(s) => Ok(s),
        Err(e) => {
            if let Ok(s) = request(c, &Request::Status)
                && let RouteStatus::Permission(p) = &s.route
            {
                eprintln!("Tailscale doesn't let goatty add its Serve route yet. Run once:");
                eprintln!("    {}", p.operator);
                eprintln!("then run this command again (needs sudo rights: Tailscale checks them before it serves a Unix socket).");
                eprintln!("Or add the route yourself:");
                eprintln!("    {}", p.direct);
                std::process::exit(1);
            }
            Err(e)
        }
    }
}

pub fn run(cmd: NetCmd, client: &str) -> Result<()> {
    match cmd {
        NetCmd::Status => {
            let mut c = Control::connect_local(client)?;
            print_status(&request(&mut c, &Request::Status)?);
        }
        NetCmd::Enable { port } => {
            let mut c = connect_or_start(client)?;
            let s = enable(&mut c, port)?;
            println!("{}", s.route.describe(s.port));
            println!("Pair a device with `goattyd net pair` here (or Settings → Network).");
            maybe_print_service_tip();
        }
        NetCmd::Disable => {
            let mut c = Control::connect_local(client)?;
            let s = request(&mut c, &Request::Disable)?;
            println!("{}", s.route.describe(s.port));
        }
        NetCmd::Pair { secs, port, no_wait, token_only } => {
            let mut c = connect_or_start(client)?;
            pair(&mut c, secs, port, no_wait, token_only)?;
        }
        NetCmd::Authorize { key, name } => {
            let mut c = connect_or_start(client)?;
            let s = request(&mut c, &Request::AuthorizeDevice { key: key.clone(), name: name.clone() })?;
            println!("Authorized {name} ({}). {} paired device(s) total.", short(&key), s.devices.len());
        }
        NetCmd::Devices => {
            let mut c = Control::connect_local(client)?;
            print_devices(&request(&mut c, &Request::Status)?);
        }
        NetCmd::Revoke { key } => {
            let mut c = Control::connect_local(client)?;
            let s = request(&mut c, &Request::Revoke { key })?;
            println!("Revoked. {} paired device(s) left.", s.devices.len());
        }
        NetCmd::Leave => {
            let mut c = Control::connect_local(client)?;
            request(&mut c, &Request::Leave)?;
            println!("This computer isn't available on the tailnet any more, and no device is paired.");
        }
    }
    Ok(())
}

fn maybe_print_service_tip() {
    #[cfg(unix)]
    if std::env::var_os("GOATTY_SOCKET").is_none() && !goatty_client::service::installed() {
        println!("Tip: on a headless server, run `goattyd service install` so goattyd stays available across restarts.");
    }
}

fn print_status(s: &NetStatus) {
    let mode = s.tailscale.running().map(|r| if r.tun { " (TUN)" } else { " (userspace networking)" }).unwrap_or_default();
    println!("{}{mode}", s.tailscale.describe());
    if matches!(s.tailscale, goatty_net::tailscale::State::NotInstalled) {
        println!("  goatty reaches your other computers through Tailscale when you have it: https://tailscale.com/download");
    }
    if s.available {
        println!("This computer: {}", s.route.describe(s.port));
    } else {
        println!("This computer: not available on the tailnet (`goattyd net enable` or `goattyd net pair`)");
    }
    println!("Its goatty key: {}", short(&s.key));
    match &s.pairing {
        Some(w) => {
            println!("Pairing: open for {}:{:02}", w.secs_left / 60, w.secs_left % 60);
            if let Some(tok) = &w.token {
                println!("Pairing code: {tok}");
            }
        }
        None => println!("Pairing: closed"),
    }
    print_devices(s);
}

fn print_devices(s: &NetStatus) {
    if s.devices.is_empty() {
        println!("No paired devices.");
        return;
    }
    println!("Paired devices:");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    for d in &s.devices {
        println!("  {:<24} {}  paired {}", d.name, &d.key[..16.min(d.key.len())], ago(now.saturating_sub(d.paired_unix)));
    }
}

/// `today`, `yesterday`, `5 days ago`.
pub fn ago(secs: u64) -> String {
    match secs / 86_400 {
        0 => "today".into(),
        1 => "yesterday".into(),
        n => format!("{n} days ago"),
    }
}

fn pair(c: &mut Control, secs: u64, port: Option<u16>, no_wait: bool, token_only: bool) -> Result<()> {
    let current = request(c, &Request::Status)?;
    if !current.available || port.is_some_and(|p| p != current.port) || current.route != RouteStatus::Active {
        let enabled = enable(c, port)?;
        if !token_only {
            println!("{}", enabled.route.describe(enabled.port));
        }
    }
    let before: BTreeMap<String, u64> = request(c, &Request::Status)?.devices.into_iter().map(|d| (d.key, d.paired_unix)).collect();
    let s = request(c, &Request::OpenPairing { secs: Some(secs) })?;
    if !s.available {
        bail!("this computer isn't available on the tailnet: `goattyd net enable` first");
    }
    let token = s.pairing.as_ref().and_then(|w| w.token.clone());
    if token_only {
        let tok = token.context("couldn't build a pairing code (is Tailscale running?)")?;
        println!("{tok}");
        return Ok(());
    }

    let mins = secs.div_ceil(60);
    let host_name = s.tailscale.running().map(|r| r.me.name.as_str()).filter(|n| !n.is_empty()).unwrap_or("this host");
    let host_dns = s.tailscale.running().map(|r| r.me.host_id()).filter(|d| !d.is_empty()).unwrap_or("");
    if host_dns.is_empty() {
        println!("Ready to pair {host_name} (port {}) for {mins} minute(s).", s.port);
    } else {
        println!("Ready to pair {host_name} ({host_dns}, port {}) for {mins} minute(s).", s.port);
    }
    if let Some(tok) = &token {
        println!();
        println!(
            "Fastest — paste this pairing code in goatty (Settings → Network, or Ctrl+Shift+P → \"Tailnet: Pair with a host or code…\"):"
        );
        println!("  {tok}");
        println!();
        println!("Or from a terminal on your other computer:");
        println!("  goatty net pair {tok}");
    }
    println!();
    println!(
        "Or open goatty's host switcher (Ctrl+Shift+H), pick \"{host_name}\", and confirm the 6-digit code here (key {}).",
        short(&s.key)
    );

    if no_wait || !std::io::stdin().is_terminal() {
        println!("\nPairing stays open in goattyd for {mins} minute(s).");
        return Ok(());
    }

    println!("\nWaiting for a device… (Ctrl+C cancels)");
    let stdin = std::io::stdin();
    let mut seen = HashSet::new();
    let result = loop {
        std::thread::sleep(Duration::from_millis(500));
        let s = match request(c, &Request::Status) {
            Ok(s) => s,
            Err(e) => break Err(e),
        };
        if let Some(added) = s.devices.iter().find(|d| before.get(&d.key).is_none_or(|&prev| d.paired_unix > prev)) {
            println!("Paired with {} ({}).", added.name, short(&added.key));
            break Ok(());
        }
        let Some(window) = s.pairing else {
            println!("The pairing window closed.");
            break Ok(());
        };
        let Some(p) = window.pending.into_iter().find(|p| seen.insert(p.id)) else { continue };
        print!("\n{} ({}) wants to pair. Does its screen show {}? [y/N] ", p.name, short(&p.key), spaced(&p.code));
        std::io::stdout().flush()?;
        let mut answer = String::new();
        stdin.lock().read_line(&mut answer)?;
        let accept = matches!(answer.trim(), "y" | "Y" | "yes");
        match request(c, &Request::Decide { id: p.id, accept }) {
            Ok(_) if accept => {
                println!("Paired with {}.", p.name);
                break Ok(());
            }
            Ok(_) => println!("Rejected."),
            Err(e) => println!("{e:#}"),
        }
    };
    let _ = request(c, &Request::ClosePairing);
    result
}
