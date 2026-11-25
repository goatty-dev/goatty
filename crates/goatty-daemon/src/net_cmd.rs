//! `goattyd net …`: this host on the user's tailnet, through the running
//! daemon (it owns the gateway and the pairing window).

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Result, bail};
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
    /// Allow a new device: open the pairing window and confirm each request's code.
    Pair {
        /// How long the window stays open, in seconds.
        #[arg(long, default_value_t = 300)]
        secs: u64,
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

pub fn run(cmd: NetCmd, client: &str) -> Result<()> {
    let mut c = Control::connect_local(client)?;
    match cmd {
        NetCmd::Status => print_status(&request(&mut c, &Request::Status)?),
        NetCmd::Enable { port } => match request(&mut c, &Request::Enable { port }) {
            Ok(s) => {
                println!("{}", s.route.describe(s.port));
                println!("Other devices pair with `goattyd net pair` here (or Settings → Network).");
            }
            Err(e) => {
                let s = request(&mut c, &Request::Status)?;
                if let RouteStatus::Permission(p) = &s.route {
                    if cfg!(windows) {
                        println!("Tailscale doesn't let goatty add its Serve route yet:");
                        println!("    {}", p.operator);
                        println!("then `goattyd net enable` again.");
                    } else {
                        println!("Tailscale doesn't let goatty add its Serve route yet. Run once:");
                        println!("    {}", p.operator);
                        println!(
                            "then `goattyd net enable` again (needs sudo rights: Tailscale checks them before it serves a Unix socket)."
                        );
                    }
                    println!("Or add the route yourself:");
                    println!("    {}", p.direct);
                    std::process::exit(1);
                }
                return Err(e);
            }
        },
        NetCmd::Disable => {
            let s = request(&mut c, &Request::Disable)?;
            println!("{}", s.route.describe(s.port));
        }
        NetCmd::Pair { secs } => pair(&mut c, secs)?,
        NetCmd::Devices => print_devices(&request(&mut c, &Request::Status)?),
        NetCmd::Revoke { key } => {
            let s = request(&mut c, &Request::Revoke { key })?;
            println!("Revoked. {} paired device(s) left.", s.devices.len());
        }
        NetCmd::Leave => {
            request(&mut c, &Request::Leave)?;
            println!("This computer isn't available on the tailnet any more, and no device is paired.");
        }
    }
    Ok(())
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
        println!("This computer: not available on the tailnet (`goattyd net enable`)");
    }
    println!("Its goatty key: {}", short(&s.key));
    match &s.pairing {
        Some(w) => println!("Pairing: open for {}:{:02}", w.secs_left / 60, w.secs_left % 60),
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

fn pair(c: &mut Control, secs: u64) -> Result<()> {
    let s = request(c, &Request::OpenPairing { secs: Some(secs) })?;
    if !s.available {
        bail!("this computer isn't available on the tailnet: `goattyd net enable` first");
    }
    println!("Pairing is open for {} minutes. On the other computer, open the host switcher (Ctrl+Shift+H)", secs.div_ceil(60));
    println!("and pick \"Tailnet: Pair with\" this one. Its key here: {}. Ctrl+C stops.", short(&s.key));
    let stdin = std::io::stdin();
    let mut seen = std::collections::HashSet::new();
    let result = loop {
        std::thread::sleep(Duration::from_millis(500));
        let s = match request(c, &Request::Status) {
            Ok(s) => s,
            Err(e) => break Err(e),
        };
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
