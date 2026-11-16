//! One client connection: decode requests, manage its subscriptions, and
//! write replies and stream frames through a dedicated writer thread.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use goatty_proto::{ClientMsg, Frame, HISTORY_SINCE, PROTOCOL_VERSION, ServerMsg, SessionId, encode, encode_server, read_msg};
use goatty_term::Palette;

use crate::server::Daemon;
use crate::session::{Session, SubFeatures, Subscriber};

/// Serve one client connection until it closes. Its subscriptions end with
/// it; the sessions keep running.
pub(crate) fn handle(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let conn = daemon.next_conn.fetch_add(1, Ordering::Relaxed);
    // Unbounded, but bounded in practice: stream frames are limited by each
    // subscriber's window, control replies are tiny.
    let (tx, rx) = flume::unbounded::<Frame>();
    // An empty frame (real ones never are) asks the writer to flush and say
    // so here: everything queued before it has been sent.
    let (flushed_tx, flushed_rx) = flume::bounded::<()>(1);
    let wstream = stream.try_clone()?;
    let writer = std::thread::Builder::new().name(format!("c{conn}-writer")).spawn(move || {
        let mut w = BufWriter::with_capacity(128 << 10, &wstream);
        while let Ok(f) = rx.recv() {
            if f.is_empty() {
                if w.flush().is_err() {
                    break;
                }
                let _ = flushed_tx.try_send(());
                continue;
            }
            if w.write_all(&f).is_err() {
                break;
            }
            // Batch under load; flush the moment the queue is idle.
            if rx.is_empty() && w.flush().is_err() {
                break;
            }
        }
        let _ = wstream.shutdown(std::net::Shutdown::Both);
    })?;
    let reply = |msg: ServerMsg| {
        if let Ok(f) = encode(&msg) {
            let _ = tx.send(f);
        }
    };

    let mut subs: HashMap<SessionId, (Arc<Session>, Arc<Subscriber>)> = HashMap::new();
    // The protocol version agreed in the handshake.
    let mut agreed_version = 0;
    let mut features = SubFeatures::default();
    let mut rstream = BufReader::with_capacity(64 << 10, stream);
    let result = (|| -> Result<()> {
        loop {
            let msg: ClientMsg = match read_msg(&mut rstream) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            match msg {
                ClientMsg::Hello { version, client } => {
                    let host = crate::server::host_info();
                    let daemon_pid = std::process::id();
                    let Some(agreed) = goatty_proto::negotiate(version) else {
                        // Our newest version tells the client which side is older.
                        log::info!("connection {conn}: {client} speaks protocol v{version}, which this goattyd doesn't");
                        reply(ServerMsg::Hello { version: PROTOCOL_VERSION, daemon_pid, host });
                        return Ok(());
                    };
                    log::debug!("connection {conn}: {client} (protocol v{agreed})");
                    agreed_version = agreed;
                    features.lines = agreed >= HISTORY_SINCE;
                    reply(ServerMsg::Hello { version: agreed, daemon_pid, host });
                }
                ClientMsg::ListSessions { req } => reply(ServerMsg::Sessions { req, list: daemon.list() }),
                ClientMsg::Create { req, size, cwd, opts } => match daemon.create(size, cwd, agreed_version >= goatty_proto::SHELL_SINCE) {
                    Ok(s) => {
                        let sub = s.subscribe(conn, req, opts, features, &tx);
                        subs.insert(s.id, (s, sub));
                    }
                    Err(e) => reply(ServerMsg::Error { req: Some(req), message: format!("{e:#}") }),
                },
                ClientMsg::Run { req, size, cwd, argv, opts } => match daemon.run(size, cwd, argv) {
                    Ok(s) => {
                        let sub = s.subscribe(conn, req, opts, features, &tx);
                        subs.insert(s.id, (s, sub));
                    }
                    Err(e) => reply(ServerMsg::Error { req: Some(req), message: format!("{e:#}") }),
                },
                ClientMsg::Attach { req, session, size, opts } => match daemon.get(session) {
                    Some(s) => {
                        // Resize first so the snapshot already has the
                        // client's geometry (reflowed by the VT core).
                        s.resize(size, None);
                        let sub = s.subscribe(conn, req, opts, features, &tx);
                        subs.insert(s.id, (s, sub));
                    }
                    None => reply(ServerMsg::Error { req: Some(req), message: format!("no session {session}") }),
                },
                ClientMsg::Detach { session } => {
                    if let Some((s, _)) = subs.remove(&session) {
                        s.unsubscribe(conn);
                    }
                }
                ClientMsg::Input { session, data } => {
                    if let Some((s, _)) = subs.get(&session) {
                        let _ = s.pty.lock().write(&data);
                        // Enter may have started a program that prints nothing.
                        s.poke_foreground();
                    }
                }
                ClientMsg::Resize { session, size } => {
                    if let Some((s, _)) = subs.get(&session) {
                        s.resize(size, Some(conn));
                    }
                }
                ClientMsg::Ack { session, consumed } => {
                    if let Some((s, sub)) = subs.get(&session) {
                        s.ack(sub, consumed);
                    }
                }
                ClientMsg::Ping { nonce } => reply(ServerMsg::Pong { nonce }),
                ClientMsg::Kill { req, session } => match daemon.get(session) {
                    Some(s) => {
                        s.pty.lock().kill();
                        reply(ServerMsg::Ok { req });
                    }
                    None => reply(ServerMsg::Error { req: Some(req), message: format!("no session {session}") }),
                },
                ClientMsg::SendInput { req, session, data } => match daemon.get(session) {
                    Some(s) => {
                        let _ = s.pty.lock().write(&data);
                        reply(ServerMsg::Ok { req });
                    }
                    None => reply(ServerMsg::Error { req: Some(req), message: format!("no session {session}") }),
                },
                ClientMsg::Shutdown { req } => {
                    reply(ServerMsg::Ok { req });
                    daemon.shutdown();
                }
                ClientMsg::Restart { req, exe } => match crate::restore::successor(&daemon, exe) {
                    Ok(exe) => {
                        reply(ServerMsg::Ok { req });
                        // The exec closes the connection: let the reply out first.
                        let _ = tx.send(Frame::from(&[][..]));
                        if flushed_rx.recv_timeout(std::time::Duration::from_secs(2)).is_err() {
                            log::warn!("connection {conn}: the restart reply may not have gone out");
                        }
                        if let Err(e) = crate::restore::restart(&daemon, &exe) {
                            log::error!("restart failed, going on as before: {e:#}");
                        }
                    }
                    Err(e) => reply(ServerMsg::Error { req: Some(req), message: format!("{e:#}") }),
                },
                ClientMsg::GetHostSettings { req } => reply(crate::settings::get(&daemon, req)),
                ClientMsg::SetHostSettings { req, json, apply } => reply(crate::settings::set(&daemon, req, &json, apply)),
                ClientMsg::ListGenerations { req } => reply(crate::settings::generations(&daemon, req)),
                ClientMsg::Rollback { req, generation } => reply(crate::settings::rollback(&daemon, req, generation)),
                ClientMsg::ReloadShells { req } => {
                    reply(ServerMsg::ShellsReloaded { req, count: u32::try_from(daemon.host.reload_shells()).unwrap_or(u32::MAX) })
                }
                ClientMsg::Plugin { req, request } => {
                    // Installs download and build a generation: don't hold
                    // up this connection's input (keystrokes) meanwhile.
                    let (daemon, tx) = (daemon.clone(), tx.clone());
                    let spawned = std::thread::Builder::new().name(format!("c{conn}-plugin")).spawn(move || {
                        let msg = crate::settings::plugin(&daemon, req, &request);
                        if let Ok(f) = encode(&msg) {
                            let _ = tx.send(f);
                        }
                    });
                    if let Err(e) = spawned {
                        reply(ServerMsg::Error { req: Some(req), message: format!("couldn't start the plugin operation: {e}") });
                    }
                }
                ClientMsg::Shell { req, request } => {
                    // The status starts a shell, installs download: off the reader thread.
                    let (daemon, tx) = (daemon.clone(), tx.clone());
                    let spawned = std::thread::Builder::new().name(format!("c{conn}-shell")).spawn(move || {
                        let msg = crate::settings::shell(&daemon, req, &request);
                        if let Ok(f) = encode(&msg) {
                            let _ = tx.send(f);
                        }
                    });
                    if let Err(e) = spawned {
                        reply(ServerMsg::Error { req: Some(req), message: format!("couldn't start the shell operation: {e}") });
                    }
                }
                ClientMsg::SaveLayout { id, sessions, json } => {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    daemon.layouts.lock().save(conn, id, sessions, json, now);
                }
                ClientMsg::ClaimLayout { req } => reply(ServerMsg::Layout { req, layout: daemon.claim_layout(conn) }),
                ClientMsg::FetchHistory { req, session, before, count } => match daemon.get(session) {
                    Some(s) => {
                        let rows = s.term.history_rows(before, count as usize);
                        let msg = ServerMsg::History { req, session, before, first: rows.first, oldest: rows.oldest, data: rows.data };
                        if let Ok(f) = encode_server(&msg, features.zstd) {
                            let _ = tx.send(f);
                        }
                    }
                    None => reply(ServerMsg::Error { req: Some(req), message: format!("no session {session}") }),
                },
                ClientMsg::Compress { zstd } => {
                    features.zstd = zstd;
                    for (_, sub) in subs.values() {
                        sub.zstd.store(zstd, Ordering::Relaxed);
                    }
                }
                ClientMsg::SetPalette { session, colors } => {
                    // Any client of the session may set it (last one wins);
                    // it only affects answers to color queries.
                    match (daemon.get(session), Palette::from_rgb_list(&colors, true)) {
                        (Some(s), Some(p)) => s.term.set_palette(Arc::new(p)),
                        (_, None) => log::debug!("connection {conn}: bad palette ({} colors)", colors.len()),
                        (None, _) => {}
                    }
                }
            }
        }
    })();

    daemon.layouts.lock().release(conn);
    for (id, (s, _)) in subs {
        s.unsubscribe(conn);
        log::info!("connection {conn} detached from session {id}");
    }
    drop(tx);
    let _ = writer.join();
    result
}
