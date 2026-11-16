//! What goatty sees of the real Tailscale on this machine, read-only:
//! `cargo run -p goatty-net --example tailscale_status`. Runs only
//! `tailscale status --json` and `tailscale serve status --json`; nothing
//! is changed. (Tests use the fake `tailscale` in `tests/`.)

use goatty_net::discovery;
use goatty_net::tailscale::{State, Tailscale};

fn main() -> anyhow::Result<()> {
    let ts = Tailscale::system();
    let state = ts.detect();
    println!("{}", state.describe());
    let State::Running(status) = state else { return Ok(()) };
    println!("tun: {}, me: {} ({}), tagged: {}", status.tun, status.me.name, status.me.dns, status.me.tagged);
    println!("{} peers, {} online", status.peers.len(), status.peers.iter().filter(|p| p.online).count());
    let mine: Vec<_> = discovery::candidates(&status, false).iter().map(|n| n.name.clone()).collect();
    let all: Vec<_> = discovery::candidates(&status, true).iter().map(|n| n.name.clone()).collect();
    println!("would probe: {mine:?} (all devices: {all:?}), owner known: {}", discovery::owner_known(&status));
    let cfg = ts.serve_config()?;
    println!("serve: tcp ports {:?}, web ports {:?}", cfg.tcp.keys().collect::<Vec<_>>(), cfg.web_ports);
    Ok(())
}
