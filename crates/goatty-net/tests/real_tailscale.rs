//! Read-only checks against the real Tailscale on this machine:
//! `cargo test -p goatty-net --test real_tailscale -- --ignored --nocapture`.
//! They only run `tailscale status --json` and `tailscale serve status
//! --json`; nothing is changed.

use goatty_net::discovery;
use goatty_net::tailscale::{State, Tailscale};

#[test]
#[ignore = "needs Tailscale installed, running and logged in"]
fn reads_the_real_status_and_serve_config() {
    let ts = Tailscale::system();
    let state = ts.detect();
    println!("{}", state.describe());
    let State::Running(status) = state else { panic!("Tailscale isn't running here") };
    println!("tun: {}, me: {} ({}), tagged: {}", status.tun, status.me.name, status.me.dns, status.me.tagged);
    println!("{} peers, {} online", status.peers.len(), status.peers.iter().filter(|p| p.online).count());
    let mine: Vec<_> = discovery::candidates(&status, false).iter().map(|n| n.name.clone()).collect();
    let all: Vec<_> = discovery::candidates(&status, true).iter().map(|n| n.name.clone()).collect();
    println!("would probe: {mine:?} (all devices: {all:?}), owner known: {}", discovery::owner_known(&status));
    let cfg = ts.serve_config().expect("serve status --json");
    println!("serve: tcp ports {:?}, web ports {:?}", cfg.tcp.keys().collect::<Vec<_>>(), cfg.web_ports);
}
