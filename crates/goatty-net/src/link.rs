//! A byte stream to a peer's goatty port: a plain TCP connection to its
//! tailnet address when Tailscale has a TUN device, else `tailscale nc`
//! (userspace networking, where tailnet addresses aren't routable from
//! ordinary sockets).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use parking_lot::{Condvar, Mutex};

use crate::tailscale::Tailscale;

/// How to reach tailnet addresses from this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A TCP connection (Tailscale has a TUN device).
    Direct,
    /// `tailscale nc` (userspace networking).
    Nc,
}

impl Mode {
    pub fn for_tun(tun: bool) -> Self {
        if tun { Mode::Direct } else { Mode::Nc }
    }
}

pub struct Link {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub closer: Closer,
    /// What `tailscale nc` said on stderr (empty for TCP).
    pub stderr: Arc<Mutex<Vec<String>>>,
}

enum Inner {
    Tcp(TcpStream),
    Child(Child),
}

/// Closes a link from any thread, which unblocks its reader.
#[derive(Clone)]
pub struct Closer(Arc<Mutex<Option<Inner>>>);

impl Closer {
    pub fn close(&self) {
        match self.0.lock().take() {
            Some(Inner::Tcp(s)) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            Some(Inner::Child(mut c)) => {
                let _ = c.kill();
                let _ = c.wait();
            }
            None => {}
        }
    }
}

/// Connect to `addr:port` on the tailnet. TCP connects give up after
/// `timeout`; `tailscale nc` reports failures when its stream is read.
pub fn open(ts: &Tailscale, mode: Mode, addr: &str, port: u16, timeout: Duration) -> Result<Link> {
    match mode {
        Mode::Direct => {
            let target = resolve(addr, port)?;
            let s = TcpStream::connect_timeout(&target, timeout).with_context(|| format!("couldn't connect to {addr}:{port}"))?;
            // Keystrokes and echoes are tiny: don't let Nagle hold them back.
            s.set_nodelay(true)?;
            let r = s.try_clone()?;
            let w = s.try_clone()?;
            Ok(Link {
                reader: Box::new(r),
                writer: Box::new(w),
                closer: Closer(Arc::new(Mutex::new(Some(Inner::Tcp(s))))),
                stderr: Arc::default(),
            })
        }
        Mode::Nc => {
            let mut child = ts
                .nc(addr, port)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("couldn't run {} nc", ts.program.display()))?;
            let stdin = child.stdin.take().context("stdin")?;
            let stdout = child.stdout.take().context("stdout")?;
            let stderr = Arc::new(Mutex::new(Vec::new()));
            if let Some(err) = child.stderr.take() {
                let lines = stderr.clone();
                let _ = std::thread::Builder::new().name("tailscale-nc-stderr".into()).spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        log::info!("tailscale nc: {line}");
                        let mut l = lines.lock();
                        if l.len() == 20 {
                            l.remove(0);
                        }
                        l.push(line);
                    }
                });
            }
            Ok(Link {
                reader: Box::new(stdout),
                writer: Box::new(stdin),
                closer: Closer(Arc::new(Mutex::new(Some(Inner::Child(child))))),
                stderr,
            })
        }
    }
}

fn resolve(addr: &str, port: u16) -> Result<SocketAddr> {
    (addr, port)
        .to_socket_addrs()
        .with_context(|| format!("couldn't resolve {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {addr}"))
}

/// Closes the link unless dropped within `timeout`: a deadline for the
/// handshake, which works for pipes (`tailscale nc`) too.
pub struct Deadline {
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl Deadline {
    pub fn start(closer: &Closer, timeout: Duration) -> Self {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let (d, closer) = (done.clone(), closer.clone());
        let _ = std::thread::Builder::new().name("gateway-deadline".into()).spawn(move || {
            let mut finished = d.0.lock();
            if !*finished && d.1.wait_for(&mut finished, timeout).timed_out() && !*finished {
                drop(finished);
                closer.close();
            }
        });
        Self { done }
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        *self.done.0.lock() = true;
        self.done.1.notify_all();
    }
}
