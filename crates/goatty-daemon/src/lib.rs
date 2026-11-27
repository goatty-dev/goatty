//! goattyd, the goatty session daemon, as a library.
//!
//! Owns every PTY and a headless, authoritative terminal state per session.
//! Clients connect over a Unix socket, directly or through `goattyd proxy`
//! (which is what `ssh host goattyd proxy` runs). They get a VT snapshot,
//! then a live byte stream. Closing a client detaches; the shell keeps running.
//!
//! * [`server`]: socket setup, accept loop, session registry,
//! * `client`: one connection's protocol loop,
//! * `session`: PTY + terminal + subscribers with flow control,
//! * `workspaces`: the tabs and splits every client window shares (#215),
//! * `settings`: host settings, generations and rollback over the protocol,
//! * `net`: this host on the user's tailnet (the gateway socket, pairing),
//! * [`proxy`]: stdio ⇄ socket relay for remote transports,
//! * `restore`: sessions that outlive goattyd (restart, upgrade, crash),
//! * `resurrect`: tabs that come back when their shells didn't (reboot, #344),
//! * [`systemd`]: Linux socket activation and the fd store,
//! * `launchd`: macOS socket activation (`launch_activate_socket`).
//!
//! The `goattyd` binary is a thin CLI over this crate.

mod client;
mod files;
mod flow;
mod launchd;
mod net;
pub mod proxy;
mod restore;
#[cfg(unix)]
mod resurrect;
pub mod server;
mod session;
mod settings;
pub mod systemd;
mod workspaces;
mod writer;

pub use proxy::proxy;
pub use restore::STATE_VERSION;
pub use server::{Start, serve};
