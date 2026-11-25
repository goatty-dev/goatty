//! Connecting to a goattyd, shown in the tab that waits for it (#37).
//!
//! Every terminal window opens right away. A tab whose session isn't there
//! yet shows a [`ConnectView`]: it connects off the UI thread (to the local
//! daemon, or over SSH, which can take seconds and ask questions) and, when
//! that works, hands the workspace the session or the restored layout
//! ([`ConnectEvent::Ready`]), which replaces the view in the tab. When it
//! doesn't, it explains what's wrong and offers what goatty can do about it:
//!
//! * goattyd missing → install it (`goatty_client::remote::install`),
//! * goattyd too old → install this build's goattyd and restart the daemon
//!   (a daemon from protocol v10 on keeps its sessions; an older one's end,
//!   and the button says so),
//! * either way, or when goattyd can't run there → **plain SSH**: `ssh host`
//!   in a session of the *local* goattyd. Nothing is installed on the host,
//!   and nothing of goatty runs there; the session survives closing the
//!   window, but not the network dropping.
//!
//! The local daemon gets the same treatment: one that speaks no protocol
//! this goatty does, or that runs an older build than the installed goattyd
//! (after an update or a `cargo build`), is restarted with the installed
//! one. Right away when that keeps its sessions (goattyd re-executes itself
//! with them, protocol v10 on) or it holds none; else only after asking,
//! since they'd end. "Keep it running" is remembered for that daemon
//! (`goatty_client::local`).
//!
//! ssh's own questions (passphrases, host keys) come up as separate windows
//! from `crate::askpass`. Connections come from `crate::hosts::pool`, so a
//! second tab on a host reuses the first one's link.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use goatty_client::local::{self, LocalProblem, Outdated, Restarted};
use goatty_client::remote::{self, Bundled, Platform, Probe};
use goatty_client::{Connection, Endpoint, RemoteProblem, SshTarget};
use goatty_shell::HostService;
use goatty_term::TermSize;
use gpui::{
    Animation, AnimationExt as _, App, AsyncWindowContext, Context, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent, Render,
    SharedString, Transformation, WeakEntity, Window, div, percentage, prelude::*, px,
};

use crate::hosts::pool;
use crate::session::{AttachMode, Session, Spawner, plain_ssh_argv};
use crate::settings::AppSettings;
use crate::toast::{self, Toast};
use crate::ui::controls::{ButtonKind, button};
use crate::ui::icons::Icon;
use crate::ui::style::Ui;
use crate::workspace::saved::{self, Startup};

/// What `goatty` was asked to connect to.
#[derive(Clone)]
pub struct ConnectPlan {
    pub endpoint: Endpoint,
    pub mode: AttachMode,
    /// `--settings[=PAGE]`, opened with the terminal window.
    pub open_settings: Option<String>,
    /// Plain SSH right away (the host is set to it): `ssh host` in a local
    /// session instead of goattyd on the host.
    pub plain: bool,
    /// A connection to the endpoint made already (the session picker's).
    pub conn: Option<Arc<Connection>>,
    /// How the window joins the host's workspace (#215); set by the window
    /// (`Workspace::open_connect_tab`). `None`: no joining (a plain SSH tab
    /// in a window that shows this computer's workspace already).
    pub join: Option<saved::Join>,
    /// The tab to show once there (a reopened window's, a new window's).
    pub active_tab: Option<u64>,
}

/// Join the workspace `mode` asks for on `conn` (or, without `join`, just
/// make a session there).
fn start(conn: &Arc<Connection>, mode: AttachMode, join: Option<&saved::Join>) -> Result<Startup> {
    match join {
        Some(join) => saved::startup(conn, initial_size(), mode, join, None),
        None => Ok(Startup::Session(Session::daemon(conn.clone(), initial_size(), None, None)?)),
    }
}

/// The terminal window's first pane size; it resizes on first layout.
fn initial_size() -> TermSize {
    TermSize::new(100, 30)
}

enum Step {
    Working(String),
    Ask(Ask),
}

/// What the user can do about a failed connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Upload goattyd, then connect.
    Install,
    /// Upload goattyd if needed, stop the running (incompatible or outdated)
    /// daemon, connect.
    Restart,
    /// Use the outdated local daemon anyway (and don't ask again for it).
    Keep,
    Plain,
    Retry,
    Cancel,
}

/// A question for the user: what happened and the possible ways on.
#[derive(Debug, Clone)]
pub struct Ask {
    pub title: String,
    pub body: Vec<String>,
    /// Shown highlighted: what a choice costs.
    pub warning: Option<String>,
    /// In button order; the last one is the default (Enter).
    pub choices: Vec<(Choice, String)>,
    /// What [`Choice::Install`]/[`Choice::Restart`] upload (`None`: nothing,
    /// the right goattyd is installed already).
    pub upload: Option<Bundled>,
}

const PLAIN_NOTE: &str = "Plain SSH opens a normal ssh session in a local goatty session instead. Nothing is installed on the host, \
                          and goatty's shell setup, settings and plugins don't reach it there.";

const SETUP_NOTE: &str = "Your shell setup comes along: this computer's prompt, aliases, plugins and their settings, and goatty's \
                          tools, so the shell there feels like the one here (Settings → Hosts can turn that off).";

fn mib(path: &std::path::Path) -> String {
    std::fs::metadata(path).map(|m| format!("{:.1} MiB", m.len() as f64 / (1024. * 1024.))).unwrap_or_else(|_| "? MiB".into())
}

/// Turn a connect problem into a question. `find` looks up a goattyd to
/// upload for a target (`remote::find_bundled` outside tests).
pub fn describe(problem: &RemoteProblem, find: impl Fn(&str) -> Option<Bundled>) -> Ask {
    let ver = env!("CARGO_PKG_VERSION");
    let dest = format!("~/{}", remote::BIN_DIR);
    let cancel = (Choice::Cancel, "Cancel".to_string());
    let plain = (Choice::Plain, "Plain SSH".to_string());
    match problem {
        RemoteProblem::NotInstalled { host, platform } => match platform.target() {
            None => Ask {
                title: format!("goattyd doesn't run on {host}"),
                body: vec![
                    format!("{host} is {platform}. goattyd, goatty's session daemon, is available for Linux on x86_64 and aarch64 so far."),
                    PLAIN_NOTE.into(),
                ],
                warning: None,
                choices: vec![cancel, plain],
                upload: None,
            },
            Some(target) => match find(target) {
                Some(bundled) => {
                    let mut body = vec![
                        format!(
                            "goatty keeps your shells on {host} in goattyd, its session daemon: they survive disconnects and closing \
                             the window, and get goatty's shell setup. It isn't installed there yet."
                        ),
                        format!(
                            "goatty can copy goattyd {ver} for {platform} ({}) to {dest} on {host}. Nothing else changes: no sudo, \
                             no PATH or dotfile edits.",
                            mib(&bundled.path)
                        ),
                    ];
                    body.push(SETUP_NOTE.into());
                    if bundled.this_build {
                        body.push(
                            "This development build copies its own goattyd, which may not run on a host with an older C library. \
                             goatty checks that it runs before using it."
                                .into(),
                        );
                    }
                    body.push(PLAIN_NOTE.into());
                    Ask {
                        title: format!("Install goattyd on {host}?"),
                        body,
                        warning: None,
                        choices: vec![cancel, plain, (Choice::Install, "Install goattyd".into())],
                        upload: Some(bundled),
                    }
                }
                None => Ask {
                    title: format!("No goattyd to install on {host}"),
                    body: vec![
                        format!(
                            "{host} needs goattyd for {platform}, and this goatty doesn't include one (release archives carry them in \
                             remote/). Install goattyd there yourself (`cargo install --path crates/goatty-daemon`), then retry."
                        ),
                        PLAIN_NOTE.into(),
                    ],
                    warning: None,
                    choices: vec![cancel, plain, (Choice::Retry, "Retry".into())],
                    upload: None,
                },
            },
        },
        RemoteProblem::Incompatible { host, mismatch, probe } => {
            if mismatch.daemon_is_newer {
                return Ask {
                    title: format!("goattyd on {host} is newer than this goatty"),
                    body: vec![format!("{mismatch}."), "Plain SSH works in the meantime.".into()],
                    warning: None,
                    choices: vec![cancel, plain],
                    upload: None,
                };
            }
            let warning = (!remote_keeps_sessions(mismatch)).then(|| format!("Restarting goattyd ends the sessions running on {host}."));
            if probe.as_ref().is_some_and(Probe::runs_ours) {
                return Ask {
                    title: format!("Restart goattyd on {host}?"),
                    body: vec![format!("{mismatch}."), format!("goattyd {ver} is installed there, but an older daemon is still running.")],
                    warning,
                    choices: vec![cancel, plain, (Choice::Restart, "Restart goattyd".into())],
                    upload: None,
                };
            }
            let platform: Option<&Platform> = probe.as_ref().map(|p| &p.platform);
            match platform.and_then(|p| Some((p, find(p.target()?)?))) {
                Some((platform, bundled)) => Ask {
                    title: format!("Update goattyd on {host}?"),
                    body: vec![
                        format!("{mismatch}."),
                        format!(
                            "goatty can copy goattyd {ver} for {platform} ({}) to {dest} on {host} and restart the daemon with it.",
                            mib(&bundled.path)
                        ),
                    ],
                    warning,
                    choices: vec![cancel, plain, (Choice::Restart, "Update and restart".into())],
                    upload: Some(bundled),
                },
                None => Ask {
                    title: format!("goattyd on {host} is too old"),
                    body: vec![format!("{mismatch}."), "Update goattyd there and retry, or use plain SSH.".into()],
                    warning: None,
                    choices: vec![cancel, plain, (Choice::Retry, "Retry".into())],
                    upload: None,
                },
            }
        }
    }
}

/// An older local daemon that a restart replaces with its sessions kept
/// (protocol v10 on Unix; Windows daemons cannot hand off ConPTY handles).
fn keeps_sessions(mismatch: &goatty_client::VersionMismatch) -> bool {
    !mismatch.daemon_is_newer && mismatch.daemon_protocol.is_some_and(local::can_keep_sessions)
}

/// An older remote SSH daemon that a restart replaces with its sessions kept.
/// Remote SSH daemons run on Unix (`goatty_client::remote`), so protocol
/// support does not depend on the local client OS.
fn remote_keeps_sessions(mismatch: &goatty_client::VersionMismatch) -> bool {
    !mismatch.daemon_is_newer && mismatch.daemon_protocol.is_some_and(|p| p >= goatty_proto::RESTART_SINCE)
}

/// The local daemon speaks no protocol this goatty does but is old enough
/// to restart with its sessions: restart it with the installed build right
/// away (no question: nothing is lost) and connect.
fn open_local(restarted: &Mutex<Option<Restart>>) -> Result<Arc<Connection>> {
    match pool::open(Endpoint::Local) {
        Err(e) => {
            let keeps = matches!(e.downcast_ref::<LocalProblem>(), Some(LocalProblem::Incompatible(m)) if keeps_sessions(m));
            if !keeps {
                return Err(e);
            }
            let _one = one_local_restart();
            if let Ok(conn) = pool::open(Endpoint::Local) {
                // Another window restarted it meanwhile.
                return Ok(conn);
            }
            log::info!("restarting the local goattyd, which is older than this goatty speaks: {e:#}");
            // Not forced: one that turns out unable to keep its sessions
            // isn't stopped (the error asks instead).
            let (conn, kept) = restart_local(None, false)?;
            *restarted.lock().unwrap_or_else(|p| p.into_inner()) = Some(if kept { Restart::Kept } else { Restart::Empty });
            Ok(conn)
        }
        ok => ok,
    }
}

/// A connect failure goatty can't fix.
fn failed(label: &str, error: &anyhow::Error, ssh: bool) -> Ask {
    let mut choices = vec![(Choice::Cancel, "Close".to_string())];
    if ssh {
        choices.push((Choice::Plain, "Plain SSH".into()));
    }
    choices.push((Choice::Retry, "Retry".into()));
    Ask { title: format!("Couldn't connect to {label}"), body: vec![format!("{error:#}")], warning: None, choices, upload: None }
}

/// Turn a problem with the local daemon into a question. `installed`: the
/// goattyd a restart starts.
pub fn describe_local(problem: &LocalProblem, installed: Option<&std::path::Path>) -> Ask {
    let close = (Choice::Cancel, "Close".to_string());
    let restart = (Choice::Restart, "Restart goattyd".to_string());
    let starts = match installed {
        Some(p) => format!("Restarting stops it and starts {}.", p.display()),
        None => "Restarting stops it and starts goattyd again.".into(),
    };
    match problem {
        LocalProblem::Outdated { outdated, sessions } => {
            let n = match sessions {
                1 => "1 session".to_string(),
                n => format!("{n} sessions"),
            };
            Ask {
                title: "Restart goattyd with the new build?".into(),
                body: vec![
                    format!(
                        "goattyd, which keeps your sessions, is still running an older build ({}, pid {}). {starts}",
                        outdated.running.display(),
                        outdated.pid
                    ),
                    "Keeping it works, but without what changed since. goatty won't ask again until it restarts.".into(),
                ],
                warning: Some(format!("Restarting goattyd ends the {n} running in it.")),
                choices: vec![(Choice::Keep, "Keep it running".into()), restart],
                upload: None,
            }
        }
        LocalProblem::Incompatible(mismatch) => {
            let daemon = match (&mismatch.daemon_version, mismatch.daemon_protocol) {
                (Some(v), Some(p)) => format!("goattyd {v} (protocol v{p})"),
                (None, Some(p)) => format!("a goattyd with protocol v{p}"),
                _ => "a goattyd".into(),
            };
            let (min, max) = (goatty_proto::MIN_PROTOCOL_VERSION, goatty_proto::PROTOCOL_VERSION);
            let age = if mismatch.daemon_is_newer { "newer" } else { "older" };
            let warning = (!keeps_sessions(mismatch)).then(|| "Restarting goattyd ends the sessions running in it.".to_string());
            Ask {
                title: "Restart goattyd?".into(),
                body: vec![format!("The running {daemon} is {age} than this goatty can talk to (protocol v{min}–v{max}). {starts}")],
                warning,
                choices: vec![close, (Choice::Retry, "Retry".into()), restart],
                upload: None,
            }
        }
    }
}

/// What happened to an outdated local daemon on the way (the tab tells
/// the user once it's connected).
enum Restart {
    /// Restarted; it had no sessions.
    Empty,
    /// Restarted with its sessions.
    Kept,
    /// It could have kept its sessions, but didn't restart (why): goatty
    /// goes on with it.
    Refused(String),
}

/// Restart the local daemon with the installed build and connect to it. Its
/// sessions carry over when it can do that, else they end (callers only get
/// here with none, or after the user agreed). A connection to it that's in
/// use (`conn`, else the pooled one) reconnects by itself; wait for that
/// rather than racing it to start one. True when the sessions were kept.
fn restart_local(conn: Option<Arc<Connection>>, force: bool) -> Result<(Arc<Connection>, bool)> {
    let conn = conn.or_else(|| pool::live(&Endpoint::Local));
    let old = conn.as_ref().map(|c| c.daemon_pid());
    let installed = local::installed_daemon();
    let kept = match local::restart_daemon(installed.as_deref(), force).context("couldn't restart the running goattyd")? {
        Restarted::Kept { .. } => true,
        Restarted::NotRunning | Restarted::Stopped(_) => false,
    };
    let Some(conn) = conn else { return Ok((pool::open(Endpoint::Local)?, kept)) };
    conn.reconnect();
    let deadline = Instant::now() + Duration::from_secs(5);
    // Kept: the same pid answers again (the old link is gone, so a reply
    // comes from the new image). Else a new daemon.
    while !(conn.is_connected() && if kept { conn.list_sessions().is_ok() } else { Some(conn.daemon_pid()) != old }) {
        anyhow::ensure!(Instant::now() < deadline, "the new goattyd didn't come up");
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok((conn, kept))
}

/// Held while checking or restarting the local daemon: windows reopened
/// together (each with its own question) restart it once.
fn one_local_restart() -> std::sync::MutexGuard<'static, ()> {
    static RESTARTING: Mutex<()> = Mutex::new(());
    RESTARTING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Check the local daemon is the installed build: restart it right away
/// when that keeps its sessions or it holds none, else ask
/// ([`LocalProblem::Outdated`]). `restarted` says how that went.
fn check_local(conn: Arc<Connection>, restarted: &Mutex<Option<Restart>>) -> Result<Arc<Connection>> {
    // One check at a time: windows reopened together mustn't restart the
    // daemon twice. The second one then finds the restarted daemon current.
    let _one = one_local_restart();
    let conn = match crate::hosts::pool::live(&Endpoint::Local) {
        // Restarted by the check before this one.
        Some(live) if !Arc::ptr_eq(&live, &conn) => live,
        _ => conn,
    };
    let Some(outdated) = local::outdated(conn.daemon_pid()) else { return Ok(conn) };
    // Kept running on purpose, or a restart was refused: until it or the
    // installed build changes.
    if local::declined(&outdated) {
        return Ok(conn);
    }
    let keeps = local::can_keep_sessions(conn.protocol_version());
    let sessions = conn.list_sessions()?.len();
    if sessions > 0 && !keeps {
        return Err(LocalProblem::Outdated { outdated, sessions }.into());
    }
    log::info!("restarting goattyd ({sessions} sessions, kept): {} replaces {}", outdated.installed.display(), outdated.running.display());
    let old = conn.clone();
    // Forced only when nothing would end: with sessions, a daemon that
    // can't keep them (or doesn't answer as one that can) isn't stopped.
    let (conn, kept) = match restart_local(Some(conn), sessions == 0) {
        Ok(done) => done,
        // It refused (e.g. the installed build can't take over) and runs on:
        // keep using it rather than asking to retry the same thing.
        Err(e) if keeps && old.is_connected() => {
            log::warn!("goattyd didn't restart with the new build: {e:#}");
            local::decline(&outdated);
            *restarted.lock().unwrap_or_else(|p| p.into_inner()) = Some(Restart::Refused(format!("{e:#}")));
            return Ok(old);
        }
        Err(e) => return Err(e),
    };
    let how = if kept && sessions > 0 { Restart::Kept } else { Restart::Empty };
    *restarted.lock().unwrap_or_else(|p| p.into_inner()) = Some(how);
    Ok(conn)
}

/// What the connect view tells the tab it sits in.
pub enum ConnectEvent {
    /// The session (or layout) is there: take it with [`ConnectView::take_ready`].
    Ready,
    /// The user gave up: close the tab.
    Cancelled,
    /// What it shows changed (the window title follows it).
    Changed,
}

pub struct ConnectView {
    plan: ConnectPlan,
    step: Step,
    focus: FocusHandle,
    ready: Option<Ready>,
    /// Check that the local daemon is the installed build (off once the
    /// user chose, or it was restarted).
    check_local: bool,
    /// The outdated local daemon being asked about.
    outdated: Option<Outdated>,
    /// What a restart of the local daemon on the way did (tell the user).
    restarted: Arc<Mutex<Option<Restart>>>,
    /// goattyd is being installed on the host (a fresh install, not an update).
    installing: bool,
    /// It was, and the connection works: the shell setup comes along.
    installed: bool,
}

impl EventEmitter<ConnectEvent> for ConnectView {}

impl Focusable for ConnectView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// A connected tab: what it starts with, and where its next sessions come from.
pub type Ready = (Startup, Spawner);

impl ConnectView {
    /// A view connecting for `plan`, started right away.
    pub fn new(plan: ConnectPlan, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let mut view = Self {
            plan,
            step: Step::Working(String::new()),
            focus: cx.focus_handle(),
            ready: None,
            check_local: true,
            outdated: None,
            restarted: Arc::default(),
            installing: false,
            installed: false,
        };
        view.connect(window, cx);
        view
    }

    /// The host, for the tab's label.
    /// Connecting to this computer's goattyd.
    pub fn is_local(&self) -> bool {
        self.plan.endpoint.is_local()
    }

    pub fn host_label(&self) -> &str {
        match &self.plan.endpoint {
            Endpoint::Local => "local",
            e => e.label(),
        }
    }

    /// The window title while this tab is active.
    pub fn title(&self) -> String {
        match &self.step {
            Step::Working(_) => format!("Connecting to {} — goatty", self.host_label()),
            Step::Ask(ask) => format!("{} — goatty", ask.title),
        }
    }

    /// `--settings`, to open once connected.
    pub fn take_open_settings(&mut self) -> Option<String> {
        self.plan.open_settings.take()
    }

    /// goattyd was freshly installed on the host for this connection (then
    /// the shell setup is synced without asking again: the install
    /// question said it comes along).
    pub fn take_installed(&mut self) -> bool {
        std::mem::take(&mut self.installed)
    }

    /// The result after [`ConnectEvent::Ready`].
    pub fn take_ready(&mut self) -> Option<Ready> {
        self.ready.take()
    }

    /// The tab to show once connected.
    pub fn active_tab(&self) -> Option<u64> {
        self.plan.active_tab
    }

    fn ssh_target(&self) -> Option<SshTarget> {
        match &self.plan.endpoint {
            Endpoint::Ssh(t) => Some(t.clone()),
            _ => None,
        }
    }

    /// Run `job` off the UI thread with `label` showing; then open the
    /// terminal or ask what to do.
    fn work(
        &mut self,
        label: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        job: impl FnOnce() -> Result<Ready> + Send + 'static,
    ) {
        self.step = Step::Working(label);
        cx.emit(ConnectEvent::Changed);
        cx.notify();
        let view_id = self.plan.join.as_ref().map(|j| j.view);
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
            let result = cx.background_executor().spawn(async move { job() }).await;
            let mut result = Some(result);
            let delivered = this.update_in(cx, |view, window, cx| {
                if let Some(result) = result.take() {
                    view.finish(result, window, cx);
                }
            });
            // The tab (or its window) went away meanwhile: let go of what
            // it brought, so nothing stays subscribed or joined for it.
            if delivered.is_err()
                && let Some(Ok(ready)) = result
            {
                discard(ready, view_id);
            }
        })
        .detach();
    }

    fn finish(&mut self, result: Result<Ready>, _window: &mut Window, cx: &mut Context<'_, Self>) {
        match result {
            Ok(ready) => {
                let restarted = self.restarted.lock().unwrap_or_else(|p| p.into_inner()).take();
                let toast = match restarted {
                    Some(Restart::Empty) => Some(Toast::success("Restarted goattyd").body("It was an older build and had no sessions.")),
                    Some(Restart::Kept) => {
                        Some(Toast::success("Restarted goattyd").body("It was an older build. Your sessions kept running."))
                    }
                    Some(Restart::Refused(why)) => Some(
                        Toast::warning("goattyd still runs the older build")
                            .body(format!("It couldn't restart with the new one: {why}. Everything works, without what changed since.")),
                    ),
                    None => None,
                };
                if let Some(t) = toast {
                    toast::show(cx, t.key("goattyd-restarted"));
                }
                self.installed = std::mem::take(&mut self.installing);
                self.ready = Some(ready);
                cx.emit(ConnectEvent::Ready);
            }
            Err(e) => {
                log::warn!("connecting to {}: {e:#}", self.plan.endpoint.label());
                self.outdated = None;
                self.installing = false;
                let ask = if let Some(problem) = e.downcast_ref::<RemoteProblem>() {
                    describe(problem, remote::find_bundled)
                } else if let Some(problem) = e.downcast_ref::<LocalProblem>() {
                    if let LocalProblem::Outdated { outdated, .. } = problem {
                        self.outdated = Some(outdated.clone());
                    }
                    describe_local(problem, local::installed_daemon().as_deref())
                } else {
                    failed(self.plan.endpoint.label(), &e, self.ssh_target().is_some())
                };
                self.step = Step::Ask(ask);
                cx.emit(ConnectEvent::Changed);
                cx.notify();
            }
        }
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.plan.plain
            && let Some(target) = self.ssh_target()
        {
            let join = self.plan.join.clone();
            self.work(format!("Opening ssh {}…", target.host), window, cx, move || plain_ssh(&target.host, join.as_ref()));
            return;
        }
        let (endpoint, mode, conn, join) = (self.plan.endpoint.clone(), self.plan.mode, self.plan.conn.take(), self.plan.join.clone());
        let restoring = mode.restores();
        // A workspace this window switches to: the host is there already.
        let switching = conn.is_some() && matches!(mode, AttachMode::Join(Some(_)));
        let label = match (endpoint.is_local(), restoring) {
            _ if switching => "Opening the workspace…".to_string(),
            (true, false) => "Starting a shell…".to_string(),
            (true, true) => "Restoring your tabs…".to_string(),
            (false, false) => format!("Connecting to {}…", endpoint.label()),
            (false, true) => format!("Restoring your tabs on {}…", endpoint.label()),
        };
        let check = endpoint.is_local() && self.check_local;
        let restarted = self.restarted.clone();
        self.work(label, window, cx, move || {
            let mut conn = match conn {
                Some(c) => c,
                None if check => open_local(&restarted)?,
                None => pool::open(endpoint)?,
            };
            if check {
                conn = check_local(conn, &restarted)?;
            }
            let startup = start(&conn, mode, join.as_ref())?;
            Ok((startup, Spawner::Daemon(conn)))
        });
    }

    fn choose(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<'_, Self>) {
        let upload = match &self.step {
            Step::Ask(ask) => ask.upload.clone(),
            Step::Working(_) => return,
        };
        match choice {
            Choice::Cancel => cx.emit(ConnectEvent::Cancelled),
            Choice::Retry => self.connect(window, cx),
            Choice::Keep => {
                self.check_local = false;
                if let Some(o) = self.outdated.take() {
                    cx.background_executor().spawn(async move { local::decline(&o) }).detach();
                }
                self.connect(window, cx);
            }
            Choice::Restart if self.plan.endpoint.is_local() => {
                self.check_local = false;
                self.outdated = None;
                let (mode, join) = (self.plan.mode, self.plan.join.clone());
                self.work("Restarting goattyd…".into(), window, cx, move || {
                    let _one = one_local_restart();
                    // Another window's answer may have replaced it meanwhile:
                    // then this one runs the installed build and speaks our protocol.
                    let current = pool::open(Endpoint::Local).ok().filter(|c| local::outdated(c.daemon_pid()).is_none());
                    let conn = match current {
                        Some(conn) => conn,
                        None => restart_local(None, true)?.0,
                    };
                    let startup = start(&conn, mode, join.as_ref())?;
                    Ok((startup, Spawner::Daemon(conn)))
                });
            }
            Choice::Plain => {
                let Some(target) = self.ssh_target() else { return };
                let join = self.plan.join.clone();
                self.work(format!("Opening ssh {}…", target.host), window, cx, move || plain_ssh(&target.host, join.as_ref()));
            }
            Choice::Install | Choice::Restart => {
                let Some(target) = self.ssh_target() else { return };
                let (endpoint, mode, join) = (self.plan.endpoint.clone(), self.plan.mode, self.plan.join.clone());
                let restart = choice == Choice::Restart;
                self.installing = choice == Choice::Install;
                let label = if upload.is_some() {
                    format!("Installing goattyd on {}…", target.host)
                } else {
                    format!("Restarting goattyd on {}…", target.host)
                };
                self.work(label, window, cx, move || {
                    if let Some(bundled) = upload {
                        remote::install(&target, &bundled.path)?;
                    }
                    if restart {
                        remote::restart_daemon(&target)?;
                    }
                    let conn = Connection::open(endpoint, crate::CLIENT)?;
                    pool::add(&conn);
                    let startup = start(&conn, mode, join.as_ref())?;
                    Ok((startup, Spawner::Daemon(conn)))
                });
            }
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Step::Ask(ask) = &self.step else { return };
        let choice = match ev.keystroke.key.as_str() {
            "enter" => ask.choices.last().map(|c| c.0),
            "escape" => Some(Choice::Cancel),
            _ => None,
        };
        if let Some(c) = choice {
            cx.stop_propagation();
            self.choose(c, window, cx);
        }
    }
}

/// What a connecting tab brought when nobody's there to take it: its
/// sessions are let go (they keep running) and the window `view` leaves the
/// workspace it joined.
fn discard((startup, spawner): Ready, view: Option<goatty_proto::ViewId>) {
    match startup {
        Startup::Session(s) => {
            s.release_view();
        }
        Startup::Joined(j) => {
            for s in j.sessions.values().chain(j.extra.iter().map(|(s, _)| s)) {
                s.release_view();
            }
            if let (Some(conn), Some(view)) = (spawner.connection(), view) {
                conn.leave_workspace(view);
            }
        }
    }
}

/// `ssh host` in a new session of the local goattyd (it survives closing the
/// window), in this computer's workspace (`join`: the window shows none
/// yet, so it joins one). If the local daemon can't (too old, won't
/// start), in a PTY of this window instead. New tabs from it run `ssh host`
/// again.
pub fn plain_ssh(host: &str, join: Option<&saved::Join>) -> Result<Ready> {
    let argv = plain_ssh_argv(host);
    match pool::open(Endpoint::Local) {
        Ok(conn) => match Session::daemon_run(conn.clone(), initial_size(), argv.clone(), None) {
            Ok(session) => {
                let spawner = Spawner::Plain { host: host.to_string(), base: Box::new(Spawner::Daemon(conn.clone())) };
                let startup = match join {
                    Some(join) => {
                        let id = session.daemon_id();
                        match saved::startup(&conn, initial_size(), AttachMode::Join(None), join, Some((session, host.to_string()))) {
                            Ok(s) => s,
                            Err(e) => {
                                // Nothing would show that ssh: a retry starts another.
                                if let Some(id) = id
                                    && let Err(k) = conn.kill(id)
                                {
                                    log::info!("ending the unused ssh session: {k:#}");
                                }
                                return Err(e);
                            }
                        }
                    }
                    None => Startup::Session(session),
                };
                return Ok((startup, spawner));
            }
            Err(e) => log::warn!("the local goattyd can't run ssh ({e:#}); running it in this window"),
        },
        Err(e) => log::warn!("no local goattyd ({e:#}); running ssh in this window"),
    }
    let session = Session::local_command(initial_size(), None, argv, None).context("couldn't start ssh")?;
    let daemon =
        std::env::current_exe().ok().map(|e| e.with_file_name(format!("goattyd{}", std::env::consts::EXE_SUFFIX))).filter(|d| d.exists());
    let base = Spawner::Local(Arc::new(HostService::from_env(daemon)));
    Ok((Startup::Session(session), Spawner::Plain { host: host.to_string(), base: Box::new(base) }))
}

impl Render for ConnectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let ui_font = cx.global::<AppSettings>().ui_font.clone();
        let outer = div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .bg(ui.bg)
            .font_family(ui_font)
            .text_sm()
            .text_color(ui.text);
        let card = div().w(px(560.)).max_w_full().flex().flex_col().gap_3();
        let card = match &self.step {
            Step::Working(label) => card.items_center().child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::Loader.svg().size(px(18.)).text_color(ui.accent).with_animation(
                        ("connect-spin", cx.entity_id().as_u64() as usize),
                        Animation::new(Duration::from_millis(900)).repeat(),
                        |s, t| s.with_transformation(Transformation::rotate(percentage(t))),
                    ))
                    .child(div().text_base().child(SharedString::from(label.clone()))),
            ),
            Step::Ask(ask) => {
                let last = ask.choices.len().saturating_sub(1);
                let buttons = ask.choices.iter().enumerate().map(|(i, (choice, label))| {
                    let kind = match choice {
                        Choice::Restart => ButtonKind::Danger,
                        _ if i == last => ButtonKind::Primary,
                        _ => ButtonKind::Normal,
                    };
                    let (choice, view) = (*choice, cx.entity().downgrade());
                    button(SharedString::from(format!("connect-{i}")), label.clone(), kind, ui, move |window, cx| {
                        let _ = view.update(cx, |v, cx| v.choose(choice, window, cx));
                    })
                });
                let icon = if ask.warning.is_some() { (Icon::TriangleAlert, ui.warning) } else { (Icon::Info, ui.accent) };
                card.p_5()
                    .border_1()
                    .border_color(ui.border)
                    .rounded_lg()
                    .bg(ui.surface)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icon.0.svg().size(px(18.)).text_color(icon.1))
                            .child(div().text_base().font_weight(FontWeight::BOLD).child(SharedString::from(ask.title.clone()))),
                    )
                    .children(ask.body.iter().map(|p| div().text_color(ui.text).child(SharedString::from(p.clone()))))
                    .when_some(ask.warning.clone(), |d, w| d.child(div().text_color(ui.warning).font_weight(FontWeight::SEMIBOLD).child(w)))
                    .child(div().flex().justify_end().gap_2().pt_2().children(buttons))
            }
        };
        outer.child(card)
    }
}

#[cfg(test)]
mod tests {
    use goatty_client::VersionMismatch;

    use super::*;

    fn linux(arch: &str) -> Platform {
        Platform { os: "Linux".into(), arch: arch.into() }
    }

    fn bundle(this_build: bool) -> impl Fn(&str) -> Option<Bundled> {
        move |t: &str| Some(Bundled { path: format!("/nonexistent/goattyd-{t}").into(), this_build })
    }

    fn choices(a: &Ask) -> Vec<Choice> {
        a.choices.iter().map(|c| c.0).collect()
    }

    #[test]
    fn missing_goattyd_offers_install_or_plain_ssh() {
        let p = RemoteProblem::NotInstalled { host: "box".into(), platform: linux("x86_64") };
        let a = describe(&p, bundle(false));
        assert_eq!(choices(&a), [Choice::Cancel, Choice::Plain, Choice::Install]);
        assert!(a.upload.is_some() && a.warning.is_none());
        assert!(a.body.iter().any(|b| b.contains("no sudo")));
        assert!(describe(&p, bundle(true)).body.iter().any(|b| b.contains("older C library")));
        // Nothing to upload: retry after installing by hand.
        assert_eq!(choices(&describe(&p, |_| None)), [Choice::Cancel, Choice::Plain, Choice::Retry]);
        // No goattyd for that platform at all.
        let mac = RemoteProblem::NotInstalled { host: "mac".into(), platform: Platform { os: "Darwin".into(), arch: "arm64".into() } };
        assert_eq!(choices(&describe(&mac, bundle(false))), [Choice::Cancel, Choice::Plain]);
    }

    #[test]
    fn old_goattyd_offers_a_restart_that_says_what_it_costs() {
        let older = VersionMismatch { daemon_protocol: Some(3), daemon_version: Some("0.0.0".into()), daemon_is_newer: false };
        let probe = |daemon: &str| Some(Probe { platform: linux("aarch64"), daemon: Some(daemon.into()) });
        let p = RemoteProblem::Incompatible { host: "box".into(), mismatch: older.clone(), probe: probe("/usr/bin/goattyd") };
        let a = describe(&p, bundle(false));
        assert_eq!(choices(&a), [Choice::Cancel, Choice::Plain, Choice::Restart]);
        assert!(a.upload.as_ref().unwrap().path.ends_with("goattyd-aarch64-linux"));
        assert!(a.warning.as_ref().unwrap().contains("ends the sessions"));
        // This build's goattyd is installed; only the running daemon is old.
        let ours = format!("/home/u/{}/{}", remote::BIN_DIR, remote::binary_name());
        let p = RemoteProblem::Incompatible { host: "box".into(), mismatch: older, probe: probe(&ours) };
        let a = describe(&p, bundle(false));
        assert!(a.upload.is_none());
        assert_eq!(a.choices.last().unwrap().1, "Restart goattyd");
        // A newer daemon: never offer to replace it from here.
        let newer = VersionMismatch { daemon_protocol: Some(99), daemon_version: None, daemon_is_newer: true };
        let p = RemoteProblem::Incompatible { host: "box".into(), mismatch: newer, probe: None };
        assert_eq!(choices(&describe(&p, bundle(false))), [Choice::Cancel, Choice::Plain]);
        // A daemon from v10 on keeps its sessions over the restart: no warning.
        let v16 = VersionMismatch { daemon_protocol: Some(16), daemon_version: Some("0.3.0".into()), daemon_is_newer: false };
        let p = RemoteProblem::Incompatible { host: "box".into(), mismatch: v16, probe: probe("/usr/bin/goattyd") };
        assert!(describe(&p, bundle(false)).warning.is_none());
    }

    #[test]
    fn local_daemon_problems_offer_a_restart() {
        let older = VersionMismatch { daemon_protocol: Some(3), daemon_version: Some("0.0.0".into()), daemon_is_newer: false };
        let a = describe_local(&LocalProblem::Incompatible(older), Some(std::path::Path::new("/x/goattyd")));
        assert_eq!(choices(&a), [Choice::Cancel, Choice::Retry, Choice::Restart]);
        assert!(a.body[0].contains("older") && a.body[0].contains("/x/goattyd"), "{:?}", a.body);
        assert!(a.warning.unwrap().contains("ends the sessions"));
        let v16 = VersionMismatch { daemon_protocol: Some(16), daemon_version: Some("0.3.0".into()), daemon_is_newer: false };
        if cfg!(windows) {
            assert!(describe_local(&LocalProblem::Incompatible(v16), None).warning.unwrap().contains("ends the sessions"));
        } else {
            assert!(describe_local(&LocalProblem::Incompatible(v16), None).warning.is_none());
        }
    }
}
