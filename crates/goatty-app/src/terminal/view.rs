//! TerminalView: one pane. Owns a [`Session`], maps GPUI input to terminal
//! input, hosts the grid element, and tells its parent (the workspace) about
//! title changes and the shell exiting via [`TerminalEvent`].
//!
//! Links (#27): with Ctrl held (or always, per `links.open_on`) the link
//! under the mouse is underlined, its target shown at the bottom of the
//! pane, and a click opens it (see `links`). Pastes go through
//! `goatty_term::paste::check` first (#29); a paste that needs a second
//! look becomes [`TerminalEvent::ConfirmPaste`] and the workspace asks.
//!
//! Find (Ctrl+Shift+F) and "View the last command's output" (Ctrl+Shift+L)
//! cover the grid with an [`OutputView`]; output keeps flowing into the
//! terminal underneath, and Esc brings it back.
//!
//! Right click (#186, `mouse.right_click`): goatty's menu, which the
//! workspace opens on [`TerminalEvent::ContextMenu`] with this pane's part
//! from [`TerminalView::menu_entries`] (the link under the pointer, the
//! clipboard, the command line, the output). Programs that use the mouse get
//! the click instead, and Shift does the other thing. The command line
//! typed at a managed zsh prompt is read and changed through goatty's zle
//! widgets ([`LineEdit`]): its text comes back as `UiEvent::CommandLine`.

use std::cell::{Cell as StdCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use goatty_client::RemotePathInfo;
use goatty_config::{
    ClientSettings, CopyOnSelect, CursorShape, FontSettings, LinkClick, MiddleClick, PasteConfirm, RemoteFileOpen, Renderer, RightClick,
};
use goatty_term::input::{
    Key, KeyKind, LineEdit, Mods, MouseAction, MouseButton, encode_key, encode_key_event, encode_mouse, encode_paste,
};
use goatty_term::paste::{self, PasteRisk};
use goatty_term::{CursorKind, Frame, Link, LinkKind, ProgramStatus, PromptJump, PromptJumped, SelectKind, TermMode, UiEvent};
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, ClipboardItem, Context, CursorStyle, DispatchPhase, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, Font, Global, InteractiveElement, IntoElement, KeyDownEvent, KeyUpEvent, Modifiers, ModifiersChangedEvent,
    MouseButton as GMouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollDelta,
    ScrollEvent, StatefulInteractiveElement, Styled, Subscription, Task, UTF16Selection, WeakEntity, Window, canvas, div, point,
    prelude::FluentBuilder, px,
};

use super::element::{Geom, TerminalElement, hsla};
use super::glyphs::GlyphCache;
use super::keys::{mods, terminal_key};
use super::links::{self, Place, RemoteAction, RemoteLookup, Target};
use super::stats::Stats;
use crate::actions::{
    ClearCommandLine, Copy, CopyCommandLine, CopyOrInterrupt, DecreaseFontSize, EditCommandLine, Find, IncreaseFontSize, NextPrompt,
    OpenContextMenu, Paste, PreviousPrompt, ResetFontSize, SaveCommandLine, ScrollPageDown, ScrollPageUp, TERMINAL_CONTEXT,
    TakeSessionSize, ToggleSudo, ViewLastOutput,
};
use crate::agent::notify::Notice;
use crate::agent::{AgentStatus, Identity};
use crate::output_view::{self, OutputEvent, OutputView};
use crate::session::{LinkStatus, Session};
use crate::settings::{AppSettings, palette};
use crate::toast::{self, Toast};
use crate::ui::context_menu::{Entry, Item};
use crate::ui::icons::Icon;
use crate::ui::style::Ui;

/// Cursor blink half-period (xterm/GTK default is ~530 ms).
const BLINK: Duration = Duration::from_millis(530);
const DEFAULT_TITLE: &str = "goatty";
/// The zoom toast is brief: the text size itself is the feedback.
const ZOOM_TOAST: Duration = Duration::from_millis(1800);
/// How often a selecting drag past the edge scrolls.
const AUTOSCROLL_TICK: Duration = Duration::from_millis(50);
/// How long goatty waits for the shell to report the command line.
const LINE_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long positive remote `StatPath` results stay cached before re-checking.
const STAT_FOUND_TTL: Duration = Duration::from_secs(30);
/// How long negative remote `StatPath` results stay cached before re-checking.
const STAT_MISSING_TTL: Duration = Duration::from_secs(3);
/// Backoff after a failed remote `StatPath` RPC before retrying on hover.
const STAT_ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// What a pane tells its parent.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    /// [`TerminalView::title`], the foreground program or the link state
    /// changed (what tabs and the window title show).
    TitleChanged,
    /// The shell exited; the pane should close.
    Exited,
    /// A paste needs confirming first; send the user's choice back with
    /// [`TerminalView::paste_text`].
    ConfirmPaste(PasteRequest),
    /// Something happened that a background tab can show (#122).
    Activity(Activity),
    /// Open goatty's menu for this pane at `at` (window coordinates):
    /// [`TerminalView::menu_entries`] plus the workspace's own. `keyboard`:
    /// opened with a key (the first item starts out picked).
    ContextMenu { at: Point<Pixels>, keyboard: bool },
    /// What the pane's programs say about themselves changed (a coding
    /// agent working, waiting, done; a progress bar): tabs redraw.
    StatusChanged,
    /// Something to tell the user about (#205): the agent waits for them,
    /// finished or failed, or the program asked for a notification. The
    /// workspace decides whether and how (`notifications.*`).
    Notice(Notice),
}

/// What the command line the shell reports is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineUse {
    Copy,
    Snippet,
}

/// What happened in a pane, for the tab's activity marker. Ordered by how
/// much it deserves attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Activity {
    /// New output.
    Output,
    /// A command finished (OSC 133, needs shell integration).
    Finished,
    /// A command finished with a non-zero exit status.
    Failed,
    /// The program rang the bell.
    Bell,
}

/// A paste waiting for the user: the text and why it was stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteRequest {
    pub text: String,
    pub risks: Vec<PasteRisk>,
}

/// Does a paste with these risks need confirming under `confirm`?
pub fn needs_confirmation(confirm: PasteConfirm, risks: &[PasteRisk]) -> bool {
    match confirm {
        PasteConfirm::Never => false,
        PasteConfirm::Suspicious => risks.iter().any(PasteRisk::is_suspicious),
        PasteConfirm::Multiline => !risks.is_empty(),
    }
}

/// The link under the mouse and what it resolved to (`None`: looks like a
/// link but isn't one here, e.g. a path that doesn't exist). Kept so moving
/// over the same link doesn't check the file system again unless a remote
/// `StatPath` lookup is still pending or its cache entry expired.
type RemoteStatKey = (Option<String>, String);

struct ResolveOutcome {
    target: Option<Target>,
    pending: bool,
    stat_key: Option<RemoteStatKey>,
}

struct HoveredLink {
    link: Link,
    target: Option<Target>,
    pending: bool,
    remote_stat_key: Option<RemoteStatKey>,
}

/// An explicit link click waiting on an in-flight `StatPath` reply.
#[derive(Clone)]
struct PendingOpen {
    link: Link,
    stat_key: RemoteStatKey,
    window: AnyWindowHandle,
}

#[derive(Clone)]
enum CachedStat {
    Found { info: RemotePathInfo, at: Instant },
    Missing(Instant),
    Error(Instant),
}

impl CachedStat {
    fn is_fresh(&self, now: Instant) -> bool {
        match self {
            Self::Found { at, .. } => now.saturating_duration_since(*at) < STAT_FOUND_TTL,
            Self::Missing(at) => now.saturating_duration_since(*at) < STAT_MISSING_TTL,
            Self::Error(at) => now.saturating_duration_since(*at) < STAT_ERROR_BACKOFF,
        }
    }
}

#[derive(Default)]
struct RemoteStatCache {
    entries: HashMap<RemoteStatKey, CachedStat>,
    inflight: HashSet<(RemoteStatKey, u64)>,
    generation: u64,
}

impl RemoteStatCache {
    fn generation(&self) -> u64 {
        self.generation
    }

    fn get_fresh(&self, key: &RemoteStatKey, now: Instant) -> Option<&CachedStat> {
        self.entries.get(key).filter(|c| c.is_fresh(now))
    }

    fn invalidate(&mut self) {
        self.entries.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    fn begin_lookup(&mut self, key: &RemoteStatKey) -> Option<u64> {
        let generation = self.generation;
        self.inflight.insert((key.clone(), generation)).then_some(generation)
    }

    fn abort_lookup(&mut self, key: &RemoteStatKey, generation: u64) {
        self.inflight.remove(&(key.clone(), generation));
    }

    fn finish_lookup(
        &mut self,
        key: RemoteStatKey,
        generation: u64,
        current_cwd_present: bool,
        result: std::result::Result<Option<RemotePathInfo>, ()>,
        now: Instant,
    ) -> bool {
        self.inflight.remove(&(key.clone(), generation));
        if generation != self.generation {
            return false;
        }
        // If `Cwd` arrived while a `cwd == None` query was in flight, the daemon
        // may have resolved against the new directory; do not cache under `None`.
        let cwd_mismatch = key.0.is_none() && current_cwd_present;
        if !cwd_mismatch {
            self.evict_if_full(now);
            let entry = match result {
                Ok(Some(info)) => CachedStat::Found { info, at: now },
                Ok(None) => CachedStat::Missing(now),
                Err(()) => CachedStat::Error(now),
            };
            self.entries.insert(key, entry);
        }
        true
    }

    fn insert_found(&mut self, key: RemoteStatKey, generation: u64, current_cwd_present: bool, info: RemotePathInfo, now: Instant) {
        if generation != self.generation || (key.0.is_none() && current_cwd_present) {
            return;
        }
        self.evict_if_full(now);
        self.entries.insert(key, CachedStat::Found { info, at: now });
    }

    fn evict_if_full(&mut self, now: Instant) {
        if self.entries.len() >= 256 {
            self.entries.retain(|_, v| v.is_fresh(now));
            if self.entries.len() >= 256 {
                self.entries.clear();
            }
        }
    }
}

pub struct TerminalView {
    session: Arc<Session>,
    focus: FocusHandle,
    // Shared with the element, which fills them during prepaint.
    frame: Rc<RefCell<Frame>>,
    geom: Rc<StdCell<Geom>>,
    stats: Rc<RefCell<Stats>>,
    glyphs: Rc<RefCell<GlyphCache>>,
    // Appearance (from the settings; see `apply_settings`).
    font: Font,
    /// Settings size plus `zoom`.
    font_size: Pixels,
    /// Ctrl with +/- on top of the settings size, until reset.
    zoom: f32,
    line_height: f32,
    padding: gpui::Edges<Pixels>,
    /// Background opacity (`terminal.opacity`).
    opacity: f32,
    glyph_mode: bool,
    cursor_blink: bool,
    // Interaction state.
    selecting: bool,
    /// Where the selecting drag last was, in window coordinates.
    drag_pos: Option<Point<Pixels>>,
    /// Scrolls while a selecting drag is past the top or bottom edge (#26).
    autoscroll: Option<Task<()>>,
    /// Drops predicted echo (#16) the host didn't confirm in time; runs
    /// while there are predictions.
    expiring: bool,
    scroll_accum: f32,
    /// A jump to the previous prompt waits for older lines from the daemon.
    prompt_jump_pending: bool,
    cursor_visible: bool,
    last_activity: Instant,
    last_bytes: u64,
    /// `bytes_in` when output was last reported as [`Activity::Output`].
    seen_bytes: u64,
    /// Title set by the application (OSC 0/2).
    app_title: String,
    /// Last decorated title reported via [`TerminalEvent::TitleChanged`].
    title: String,
    /// Last foreground program reported the same way.
    process: Option<String>,
    /// What the pane's programs said about themselves (OSC 7501, 9;4).
    status: ProgramStatus,
    exited: bool,
    // Links.
    link_click: LinkClick,
    /// The cell under the mouse, while it's over the pane.
    mouse_cell: Option<(usize, usize)>,
    modifiers: Modifiers,
    hover: Option<HoveredLink>,
    /// `open_on: click`: the link a plain press landed on; opened on release
    /// unless the press turned into a selection.
    pressed_link: Option<Target>,
    /// `open_on: click`: a press on a link whose `StatPath` lookup was still pending.
    pressed_pending_link: Option<(Link, (Option<String>, String))>,
    /// Explicit click waiting for an in-flight `StatPath` response.
    pending_open: Option<PendingOpen>,
    hostname: Option<String>,
    /// Cached and in-flight remote `StatPath` results keyed by `(remote_cwd, raw_target)`.
    remote_stats: RemoteStatCache,
    /// The output viewer, while it covers the grid.
    viewer: Option<(Entity<OutputView>, Subscription)>,
    /// Asked the shell for the command line (and gives up when this ends).
    line_request: Option<(LineUse, Task<()>)>,
    /// The cell a right click opened the menu on (for its link).
    menu_cell: Option<(usize, usize)>,
    /// Keys whose press went to the program while it asked for key
    /// releases (kitty protocol): only their releases go there too, not
    /// those of keys a shortcut took.
    keys_down: Vec<String>,
    /// A right press went to the program: its release goes there too.
    right_to_program: bool,
    /// Left and middle presses that went to the program (bits 0 and 1):
    /// only their releases go there, not the release of a press the pane
    /// kept (the "Take over" chip, the letterbox).
    buttons_to_program: u8,
    /// Text the input method (IME, dead keys, compose) is composing; it
    /// goes to the program only once committed (#21).
    preedit: String,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    /// The output viewer's while it's open, so focusing the pane focuses it.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.viewer {
            Some((v, _)) => v.focus_handle(cx),
            None => self.focus.clone(),
        }
    }
}

impl TerminalView {
    pub fn new(session: Arc<Session>, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let app = cx.global::<AppSettings>().clone();
        let settings = app.client.clone();
        let initial_status = session.terminal.program_status();
        let focus = cx.focus_handle();
        cx.observe_global::<AppSettings>(|view, cx| {
            let app = cx.global::<AppSettings>().clone();
            view.apply_settings(&app);
            view.sync_viewer_font(cx);
            cx.notify();
        })
        .detach();
        window.focus(&focus, cx);

        // I/O threads wake us; one notify per frame at most.
        let wake_rx = session.wake_rx.clone();
        cx.spawn_in(window, async move |this, cx| {
            while wake_rx.recv_async().await.is_ok() {
                if this.update(cx, |view, cx| view.on_wake(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Cursor blink. Stays solid while typing/output is happening and only
        // ticks (one cheap frame per 530 ms) while the window is focused.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                let alive = this.update_in(cx, |view, window, cx| {
                    let focused = view.focus.is_focused(window);
                    let next =
                        if !focused || !view.cursor_blink || view.last_activity.elapsed() < BLINK { true } else { !view.cursor_visible };
                    if next != view.cursor_visible {
                        view.cursor_visible = next;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        if settings.debug.stats {
            cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let alive = this.update(cx, |view, _| {
                        let bytes = view.session.bytes_in.load(Ordering::Relaxed);
                        view.stats.borrow_mut().report(bytes, &mut view.last_bytes);
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }

        // The attach snapshot is already in; only output after it counts.
        let seen_bytes = session.bytes_in.load(Ordering::Relaxed);
        let mut view = Self {
            session,
            focus,
            frame: Rc::new(RefCell::new(Frame::default())),
            geom: Rc::new(StdCell::new(Geom::default())),
            stats: Rc::new(RefCell::new(Stats::default())),
            glyphs: Rc::new(RefCell::new(GlyphCache::new())),
            font: gpui::font("monospace"),
            font_size: px(settings.font.size),
            zoom: 0.0,
            line_height: settings.font.line_height,
            padding: edges(settings.terminal.padding),
            opacity: settings.terminal.opacity,
            glyph_mode: true,
            cursor_blink: true,
            selecting: false,
            drag_pos: None,
            autoscroll: None,
            expiring: false,
            scroll_accum: 0.0,
            prompt_jump_pending: false,
            cursor_visible: true,
            last_activity: Instant::now(),
            last_bytes: 0,
            seen_bytes,
            app_title: DEFAULT_TITLE.into(),
            title: String::new(),
            process: None,
            status: initial_status,
            exited: false,
            link_click: settings.links.open_on,
            mouse_cell: None,
            modifiers: Modifiers::default(),
            hover: None,
            pressed_link: None,
            pressed_pending_link: None,
            pending_open: None,
            hostname: links::hostname(),
            remote_stats: RemoteStatCache::default(),
            viewer: None,
            line_request: None,
            menu_cell: None,
            keys_down: Vec::new(),
            right_to_program: false,
            buttons_to_program: 0,
            preedit: String::new(),
        };
        view.apply_settings(&app);
        view
    }

    /// Take font, layout, cursor and colors from the settings. Called at
    /// start and whenever the settings change (live, no restart).
    fn apply_settings(&mut self, app: &AppSettings) {
        let s: &ClientSettings = &app.client;
        // Compare the whole font: the initial `monospace` placeholder has the
        // same family as an explicit `monospace` but no bundled fallback.
        let font = crate::fonts::terminal(s.font.family.as_deref().unwrap_or("monospace"));
        if self.font != font {
            self.font = font;
        }
        self.font_size = px((s.font.size + self.zoom).clamp(FontSettings::MIN_SIZE, FontSettings::MAX_SIZE));
        self.line_height = s.font.line_height;
        self.padding = edges(s.terminal.padding);
        self.opacity = s.terminal.opacity;
        self.glyph_mode = s.terminal.renderer == Renderer::Glyphs;
        self.session.set_local_echo(s.ssh.local_echo);
        self.cursor_blink = s.terminal.cursor_blink;
        if !self.cursor_blink {
            self.cursor_visible = true;
        }
        self.session.terminal.set_default_cursor(match s.terminal.cursor_shape {
            CursorShape::Block => CursorKind::Block,
            CursorShape::Bar => CursorKind::Beam,
            CursorShape::Underline => CursorKind::Underline,
        });
        self.session.set_palette(palette(&app.colors, s.theme.bold_is_bright));
        if self.link_click != s.links.open_on {
            self.link_click = s.links.open_on;
            self.hover = None;
            self.pending_open = None;
            self.pressed_pending_link = None;
        }
    }

    /// The viewer draws the pane's font at the pane's (zoomed) size.
    fn sync_viewer_font(&self, cx: &mut Context<'_, Self>) {
        let Some((viewer, _)) = &self.viewer else { return };
        let (font, size, lh) = (self.font.clone(), self.font_size, self.line_height);
        viewer.update(cx, |v, cx| v.set_font(font, size, lh, cx));
    }

    /// Title for a window or tab: the application's title plus host and
    /// link state when remote.
    pub fn title(&self) -> String {
        self.session.decorate_title(&self.app_title)
    }

    /// The title the application set (OSC 0/2), if any.
    pub fn app_title(&self) -> Option<&str> {
        (self.app_title != DEFAULT_TITLE).then_some(self.app_title.as_str())
    }

    /// The foreground program (`zsh`, `nvim`, …), when the host can tell.
    pub fn process(&self) -> Option<&str> {
        self.process.as_deref()
    }

    /// What the pane's programs said about themselves.
    pub fn program_status(&self) -> &ProgramStatus {
        &self.status
    }

    /// The coding agent (or other program reporting its status) running here.
    pub fn agent(&self) -> Option<Identity> {
        Identity::of(&self.status, self.process())
    }

    /// What the pane's agent is doing; `done` and `error` reported at or
    /// before `seen` are hidden (the user saw them).
    pub fn agent_status(&self, seen: u64) -> Option<AgentStatus> {
        AgentStatus::of(&self.status, self.process(), seen)
    }

    /// The shell's directory on the session's host (OSC 7 or daemon `/proc` fallback).
    pub fn cwd(&self) -> Option<PathBuf> {
        self.session.cwd()
    }

    /// The size of one cell as last painted; `None` before the first paint.
    pub fn cell_size(&self) -> Option<gpui::Size<Pixels>> {
        let cell = self.geom.get().cell;
        (cell.width > px(0.) && cell.height > px(0.)).then_some(cell)
    }

    /// The padding around the grid (`terminal.padding`).
    pub fn padding(&self) -> gpui::Edges<Pixels> {
        self.padding
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    fn on_wake(&mut self, cx: &mut Context<'_, Self>) {
        self.session.clear_wake();
        self.poke_cursor();
        // Tabs fall back to the directory when there's no title.
        let mut cwd_changed = self.session.take_cwd_dirty();
        let mut clear_stats = false;
        let mut status_changed = false;
        // Told after the drain: agents' news first, so a program's own
        // notification of the same event is seen as its echo.
        let mut news = Vec::new();
        let mut program_notices = Vec::new();
        while let Ok(ev) = self.session.ui_events.try_recv() {
            match ev {
                UiEvent::Title(t) => self.app_title = t,
                UiEvent::ResetTitle => self.app_title = DEFAULT_TITLE.into(),
                UiEvent::ClipboardStore(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                UiEvent::CwdChanged => cwd_changed = true,
                UiEvent::Bell => cx.emit(TerminalEvent::Activity(Activity::Bell)),
                UiEvent::CommandFinished { exit } => {
                    clear_stats = true;
                    let failed = exit.is_some_and(|code| code != 0);
                    cx.emit(TerminalEvent::Activity(if failed { Activity::Failed } else { Activity::Finished }));
                }
                UiEvent::CommandLine(line) => self.command_line_arrived(line, cx),
                UiEvent::Status { news: record } => {
                    status_changed = true;
                    news.extend(record);
                }
                UiEvent::Notification { title, body } => program_notices.extend(Notice::program(&title, &body)),
                UiEvent::Exit => {}
            }
        }
        if status_changed {
            self.status_changed(news, cx);
        }
        for notice in program_notices {
            cx.emit(TerminalEvent::Notice(notice));
        }
        let bytes = self.session.bytes_in.load(Ordering::Relaxed);
        if bytes != self.seen_bytes {
            self.seen_bytes = bytes;
            cx.emit(TerminalEvent::Activity(Activity::Output));
            if let Some((viewer, _)) = &self.viewer {
                viewer.update(cx, |v, cx| v.output_arrived(cx));
            }
        }
        // Host and link state (reconnecting…) are part of the title, so
        // re-check even without a Title event.
        let title = self.title();
        let process = self.session.process.lock().clone();
        let process_changed = process != self.process;
        if clear_stats || process_changed {
            self.remote_stats.invalidate();
            self.hover = None;
            self.pending_open = None;
        }
        if cwd_changed {
            self.hover = None;
            self.pending_open = None;
        }
        if let Some(pending) = &self.pending_open
            && self.mouse_cell.and_then(|(c, r)| self.session.terminal.link_at(c, r)).as_ref() != Some(&pending.link)
        {
            self.pending_open = None;
        }
        if title != self.title || process_changed || cwd_changed {
            self.title = title;
            self.process = process;
            cx.emit(TerminalEvent::TitleChanged);
        }
        if !self.exited && self.session.exited.load(Ordering::Acquire) {
            self.exited = true;
            cx.emit(TerminalEvent::Exited);
        }
        // Output, CWD changes, or finished commands/process transitions can move or change the hovered link.
        if self.hover.is_some() || ((cwd_changed || clear_stats || process_changed) && self.mouse_cell.is_some()) {
            self.refresh_hover(cx);
        }
        // Older lines may have come in (each fetch wakes the view).
        if self.prompt_jump_pending {
            self.jump_to_prompt(PromptJump::Previous, None, cx);
        }
        cx.notify();
    }

    /// Read the program status again and tell the workspace, with each
    /// record that was news (`goatty_term::status`: blocked, done or
    /// failed, as reported then, in order).
    fn status_changed(&mut self, news: Vec<goatty_term::StatusRecord>, cx: &mut Context<'_, Self>) {
        self.status = self.session.terminal.program_status();
        for record in &news {
            let status = AgentStatus::from_record(record, &self.status, self.process());
            cx.emit(TerminalEvent::Notice(Notice::Agent(status)));
        }
        cx.emit(TerminalEvent::StatusChanged);
    }

    fn send(&mut self, bytes: &[u8], cx: &mut Context<'_, Self>) {
        self.prompt_jump_pending = false;
        self.session.terminal.scroll_to_bottom();
        self.session.write(bytes);
        self.arm_expiry(cx);
    }

    /// Predicted echo (#16) the host doesn't confirm in time disappears:
    /// one task follows the earliest deadline until no prediction is left
    /// (confirming the earliest guess moves the deadline later).
    fn arm_expiry(&mut self, cx: &mut Context<'_, Self>) {
        if self.expiring || self.session.prediction_deadline().is_none() {
            return;
        }
        self.expiring = true;
        cx.spawn(async move |this, cx| {
            while let Ok(Some(at)) = this.update(cx, |view, _| view.session.prediction_deadline()) {
                cx.background_executor().timer(at.saturating_duration_since(Instant::now())).await;
                let alive = this.update(cx, |view, cx| {
                    if view.session.expire_predictions() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            // No await since the last check: nothing was predicted meanwhile.
            let _ = this.update(cx, |view, _| view.expiring = false);
        })
        .detach();
    }

    /// Show the cursor solid and restart the blink timer.
    fn poke_cursor(&mut self) {
        self.last_activity = Instant::now();
        self.cursor_visible = true;
    }

    // ---- actions ------------------------------------------------------------

    fn copy_selection(&mut self, cx: &mut Context<'_, Self>) -> bool {
        match self.session.terminal.selection_text() {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            None => false,
        }
    }

    /// A finished mouse selection goes to the primary selection and/or the
    /// clipboard (`selection.copy_on_select`).
    fn copy_on_select(&mut self, cx: &mut Context<'_, Self>) {
        let mode = cx.global::<AppSettings>().client.selection.copy_on_select;
        if mode == CopyOnSelect::Off {
            return;
        }
        let Some(text) = self.session.terminal.selection_text().filter(|t| !t.is_empty()) else { return };
        let item = ClipboardItem::new_string(text);
        if mode == CopyOnSelect::Clipboard {
            cx.write_to_clipboard(item.clone());
        }
        write_primary(cx, item);
    }

    /// The explicit copy action (menu, palette, Ctrl+Shift+C) says what it
    /// did; Ctrl+C with a selection (`copy_or_interrupt`) stays quiet.
    fn copy(&mut self, _: &Copy, window: &mut Window, cx: &mut Context<'_, Self>) {
        let toast = match self.session.terminal.selection_text() {
            Some(text) => {
                let chars = text.chars().count();
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                Toast::success("Copied to clipboard").body(if chars == 1 { "1 character".into() } else { format!("{chars} characters") })
            }
            None => Toast::info("Nothing to copy").body("Select text with the mouse first."),
        };
        toast::show_in(window, cx, toast.key("terminal.copy"));
    }

    fn copy_or_interrupt(&mut self, _: &CopyOrInterrupt, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.copy_selection(cx) {
            self.session.terminal.selection_clear();
        } else if let Some(bytes) = encode_key(&Key::Text("c"), Mods { ctrl: true, ..Mods::default() }, self.session.terminal.mode()) {
            self.poke_cursor();
            self.send(&bytes, cx);
        }
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<'_, Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text());
        if !self.paste_checked(text, cx) {
            toast::show_in(window, cx, Toast::info("Nothing to paste").body("The clipboard has no text.").key("terminal.paste"));
        }
    }

    /// Middle click: the primary selection or the clipboard
    /// (`paste.middle_click`). Nothing there pastes nothing, quietly.
    fn paste_middle_click(&mut self, cx: &mut Context<'_, Self>) {
        let text = match cx.global::<AppSettings>().client.paste.middle_click {
            MiddleClick::Off => return,
            MiddleClick::Clipboard => cx.read_from_clipboard(),
            MiddleClick::Primary => read_primary(cx),
        };
        self.paste_checked(text.and_then(|c| c.text()), cx);
    }

    /// Paste `text` through the paste check. Returns false if there was no
    /// text. Pastes that need a second look go to the workspace as
    /// [`TerminalEvent::ConfirmPaste`].
    fn paste_checked(&mut self, text: Option<String>, cx: &mut Context<'_, Self>) -> bool {
        let Some(text) = text.filter(|t| !t.is_empty()) else { return false };
        let risks = paste::check(&text, self.session.terminal.mode());
        if needs_confirmation(cx.global::<AppSettings>().client.paste.confirm, &risks) {
            cx.emit(TerminalEvent::ConfirmPaste(PasteRequest { text, risks }));
        } else {
            self.paste_text(&text, cx);
        }
        true
    }

    /// Send `text` to the shell as a paste, no questions asked.
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<'_, Self>) {
        let bytes = encode_paste(text, self.session.terminal.mode());
        self.session.terminal.selection_clear();
        self.poke_cursor();
        self.send(&bytes, cx);
        cx.notify();
    }

    /// Put `line` on the prompt at the cursor, without pressing Enter: a
    /// snippet the user filled in, to read and run. Nothing typed there is
    /// cleared. It goes through the paste check like any paste, so a
    /// `curl … | sh` still asks first.
    pub fn insert_line(&mut self, line: &str, cx: &mut Context<'_, Self>) {
        self.paste_checked(Some(line.to_string()), cx);
    }

    /// Type `line` at the prompt and press Enter: a command the user
    /// picked in the palette (a plugin's). Whatever was typed at the prompt
    /// is cleared first (Ctrl+E, Ctrl+U: zsh's and bash's emacs keys; it
    /// goes to the kill ring, Ctrl+Y brings it back), so it can't join the
    /// command. The line is sent as a paste, so nothing in it acts as a key.
    pub fn run_line(&mut self, line: &str, cx: &mut Context<'_, Self>) {
        self.send(b"\x05\x15", cx);
        self.paste_text(line, cx);
        self.send(b"\r", cx);
    }

    // ---- the command line (#186) ----------------------------------------------

    /// Send `edit`'s keys to the shell, if it's at a prompt with goatty's
    /// widgets; otherwise say why not and return false.
    fn line_edit(&mut self, edit: LineEdit, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        if !self.can_edit_line() {
            let body = match self.process.as_deref() {
                Some(p) if !crate::workspace::icons::is_shell(p) => format!("{p} is running in this pane. Use it at a shell prompt."),
                _ => "This works at a prompt of goatty's shell (with shell integration on).".to_string(),
            };
            toast::show_in(window, cx, Toast::info("No command line here").body(body).key("terminal.command_line"));
            return false;
        }
        self.poke_cursor();
        self.send(edit.keys(), cx);
        cx.notify();
        true
    }

    /// Whether goatty's widgets take keys here: the shell says so, and no
    /// other program is in the foreground (an editor started from the line,
    /// a job the shell integration doesn't know about).
    fn can_edit_line(&self) -> bool {
        self.session.terminal.line_editing() && self.process.as_deref().is_none_or(crate::workspace::icons::is_shell)
    }

    /// Ask the shell for the command line; [`Self::command_line_arrived`]
    /// does the rest. One question at a time: a second one while the first
    /// waits would take over its answer.
    fn request_line(&mut self, what: LineUse, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.line_request.is_some() {
            return;
        }
        if !self.line_edit(LineEdit::Report, window, cx) {
            return;
        }
        let timeout = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LINE_TIMEOUT).await;
            let _ = this.update(cx, |this, cx| {
                if this.line_request.take().is_some() {
                    let toast = Toast::warning("The shell didn't answer").body("It may be busy. Try again at the prompt.");
                    toast::show(cx, toast.key("terminal.command_line"));
                }
            });
        });
        self.line_request = Some((what, timeout));
    }

    /// The shell reported its command line (only what goatty asked for
    /// counts: a program could print the same sequence).
    fn command_line_arrived(&mut self, line: String, cx: &mut Context<'_, Self>) {
        let Some((what, _)) = self.line_request.take() else { return };
        let line = line.trim_end_matches('\n').to_string();
        match what {
            LineUse::Copy if line.trim().is_empty() => {
                toast::show(cx, Toast::info("Nothing typed at the prompt").body("Type a command first.").key("terminal.command_line"));
            }
            LineUse::Copy => {
                cx.write_to_clipboard(ClipboardItem::new_string(line.clone()));
                toast::show(cx, Toast::success("Copied the command").body(line).key("terminal.command_line"));
            }
            LineUse::Snippet if line.trim().is_empty() => match self.last_command_line() {
                Some(last) => crate::settings_window::snippets::new_from_line(last, cx),
                None => {
                    toast::show(
                        cx,
                        Toast::info("No command to save yet").body("Type or run a command first.").key("terminal.command_line"),
                    );
                }
            },
            LineUse::Snippet => crate::settings_window::snippets::new_from_line(line, cx),
        }
    }

    /// The last command run in this pane, from the prompt marks.
    fn last_command_line(&self) -> Option<String> {
        let t = self.session.terminal.transcript();
        let b = t.last_command()?;
        t.command_line(b).map(crate::settings_window::snippets::strip_prompt).filter(|l| !l.is_empty())
    }

    fn copy_command_line(&mut self, _: &CopyCommandLine, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.request_line(LineUse::Copy, window, cx);
    }

    /// The typed command into a new snippet; without one (or outside
    /// goatty's shell), the last command run.
    fn save_command_line(&mut self, _: &SaveCommandLine, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.can_edit_line() {
            self.request_line(LineUse::Snippet, window, cx);
        } else {
            crate::snippets::save_last(window, cx);
        }
    }

    fn toggle_sudo(&mut self, _: &ToggleSudo, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.line_edit(LineEdit::ToggleSudo, window, cx);
    }

    fn edit_command_line(&mut self, _: &EditCommandLine, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.line_edit(LineEdit::OpenEditor, window, cx);
    }

    fn clear_command_line(&mut self, _: &ClearCommandLine, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.line_edit(LineEdit::Clear, window, cx);
    }

    /// Another client sets the session's size: make it this pane's (#12).
    fn take_session_size(&mut self, _: &TakeSessionSize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.session.watching() {
            toast::show_in(window, cx, Toast::success("This pane already sets the session's size").key("terminal.size"));
            return;
        }
        self.request_take_size(window, cx);
    }

    /// Ask the daemon to size the session to this pane, or say why it can't.
    fn request_take_size(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.session.take_size() {
            let why = "The connection to the host is down. Try again once it's back.";
            toast::show_in(window, cx, Toast::error("Can't take over the session's size").body(why).key("terminal.size"));
        }
        cx.notify();
    }

    // ---- the menu (#186) ------------------------------------------------------

    /// Shift+F10 / the Menu key: the menu at the text cursor (or the
    /// pane's top left corner when the cursor is hidden).
    fn open_context_menu(&mut self, _: &OpenContextMenu, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let g = self.geom.get();
        let at = match self.frame.borrow().cursor.as_ref() {
            Some(c) => point(g.origin.x + g.cell.width * (c.col + 1) as f32, g.origin.y + g.cell.height * (c.row + 1) as f32),
            None => point(g.origin.x + px(8.), g.origin.y + px(8.)),
        };
        self.menu_cell = None;
        cx.emit(TerminalEvent::ContextMenu { at, keyboard: true });
    }

    /// The program in this pane uses the mouse (and gets right clicks).
    pub fn program_uses_mouse(&self) -> bool {
        self.session.terminal.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// This pane's part of the menu: the link that was right-clicked, the
    /// clipboard, the command line at a prompt of goatty's shell, and the
    /// output. Items run after the menu closed and this pane has focus.
    pub fn menu_entries(&mut self, cx: &mut Context<'_, Self>) -> Vec<Entry> {
        let me = cx.entity().downgrade();
        let mut out: Vec<Entry> = Vec::new();
        let link = self
            .menu_cell
            .take()
            .and_then(|(c, r)| self.session.terminal.link_at(c, r))
            .and_then(|l| self.resolve_link(&l, true, cx).target.or_else(|| self.fallback_remote_menu_target(&l)));
        if let Some(target) = link {
            match &target {
                Target::Url(_) | Target::Other(_) => {
                    let (open, me2) = (target.clone(), me.clone());
                    out.push(
                        Item::new("Open the link", move |window, cx| {
                            let _ = me2.update(cx, |v, cx| v.open_link(open.clone(), window, cx));
                        })
                        .icon(Icon::ExternalLink)
                        .into(),
                    );
                    let text = match &target {
                        Target::Url(u) | Target::Other(u) => u.clone(),
                        _ => unreachable!(),
                    };
                    out.push(
                        Item::new("Copy the link", move |window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            toast::show_in(window, cx, Toast::success("Copied").body(text.clone()).key("terminal.link"));
                        })
                        .icon(Icon::Link)
                        .into(),
                    );
                }
                Target::File { .. } => {
                    let (open, me2) = (target.clone(), me.clone());
                    out.push(
                        Item::new("Open the file", move |window, cx| {
                            let _ = me2.update(cx, |v, cx| v.open_link(open.clone(), window, cx));
                        })
                        .icon(Icon::FileText)
                        .into(),
                    );
                    let text = target.describe();
                    out.push(
                        Item::new("Copy the path", move |window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            toast::show_in(window, cx, Toast::success("Copied").body(text.clone()).key("terminal.link"));
                        })
                        .icon(Icon::Link)
                        .into(),
                    );
                }
                Target::Remote { path, host, line, column, size, on_session_host, can_fetch, .. } => {
                    let action = self.plan_remote(&target, cx);
                    let pending_opener = (size.is_none()
                        && *on_session_host
                        && !path.starts_with('/')
                        && self.session.connection().is_some_and(|c| c.supports_remote_files()))
                    .then(|| self.unresolved_remote_prefers_opener(cx))
                    .flatten();
                    let (open, me2) = (target.clone(), me.clone());
                    let (label, icon) = if let Some(opener_host) = &pending_opener {
                        (format!("Open on {opener_host}"), Icon::ExternalLink)
                    } else {
                        match &action {
                            RemoteAction::OpenInEditor { host, .. } => (format!("Open on {host}"), Icon::ExternalLink),
                            RemoteAction::Fetch { .. } => ("Open read-only copy".to_string(), Icon::FileText),
                            RemoteAction::Copy => ("Copy the remote path".to_string(), Icon::Copy),
                        }
                    };
                    out.push(
                        Item::new(label, move |window, cx| {
                            let _ = me2.update(cx, |v, cx| v.open_link(open.clone(), window, cx));
                        })
                        .icon(icon)
                        .into(),
                    );
                    if (matches!(action, RemoteAction::OpenInEditor { .. }) || pending_opener.is_some()) && *can_fetch {
                        let (p, h, l, c, sz, me3) = (path.clone(), host.clone(), *line, *column, *size, me.clone());
                        let max_bytes = cx.global::<AppSettings>().client.links.remote_max_bytes();
                        out.push(
                            Item::new("Open local copy (read-only)", move |window, cx| {
                                let _ = me3.update(cx, |v, cx| {
                                    let resolved = if sz.is_none() { v.cached_remote_stat(&p) } else { None };
                                    if let Some(info) = resolved {
                                        if info.is_dir {
                                            let copy_path = info.path.clone();
                                            toast::show_in(
                                                window,
                                                cx,
                                                Toast::info(format!("{} is a folder", info.path))
                                                    .body(format!(
                                                        "{h}:{} is a folder; set \"Open remote files with\" under Settings → Terminal to open folders.",
                                                        info.path
                                                    ))
                                                    .action("Copy path", move |_, cx| {
                                                        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                                                    })
                                                    .key("terminal.link"),
                                            );
                                        } else {
                                            v.open_remote_fetch(info.path, h.clone(), l, c, Some(info.size), max_bytes, true, window, cx);
                                        }
                                    } else {
                                        v.open_remote_fetch(p.clone(), h.clone(), l, c, sz, max_bytes, true, window, cx);
                                    }
                                });
                            })
                            .icon(Icon::FileText)
                            .into(),
                        );
                    }
                    if !matches!(action, RemoteAction::Copy) {
                        let (raw_path, me_copy) = (path.clone(), me.clone());
                        out.push(
                            Item::new("Copy the remote path", move |window, cx| {
                                let copy_text = me_copy
                                    .update(cx, |v, _| v.resolved_remote_path(&raw_path))
                                    .ok()
                                    .flatten()
                                    .unwrap_or_else(|| raw_path.clone());
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                                toast::show_in(window, cx, Toast::success("Copied").body(copy_text).key("terminal.link"));
                            })
                            .icon(Icon::Copy)
                            .into(),
                        );
                    }
                }
            }
            out.push(Entry::Separator);
        }

        let selection = self.session.terminal.has_selection();
        out.push(Item::action("Copy", "copy", cx).icon(Icon::Copy).disabled((!selection).then_some("Select text first")).into());
        let can_paste = cx.read_from_clipboard().and_then(|c| c.text()).is_some_and(|t| !t.is_empty());
        out.push(
            Item::action("Paste", "paste", cx)
                .icon(Icon::ClipboardPaste)
                .disabled((!can_paste).then_some("The clipboard has no text"))
                .into(),
        );

        out.push(Entry::Separator);
        if self.can_edit_line() {
            out.push(Entry::Header("Command line".into()));
            out.push(Item::action("Copy the command", "copy_command_line", cx).icon(Icon::Copy).into());
            out.push(Item::action("Save as a snippet…", "save_command_line", cx).icon(Icon::BookmarkPlus).into());
            out.push(Item::action("Add or remove sudo", "toggle_sudo", cx).icon(Icon::Shield).into());
            out.push(Item::action("Edit in your editor", "edit_command_line", cx).icon(Icon::Pencil).into());
            out.push(Item::action("Clear the command", "clear_command_line", cx).icon(Icon::Eraser).into());
            out.push(Item::action("Find a snippet…", "snippets", cx).icon(Icon::BookOpen).into());
        } else if !self.session.terminal.command_running() && self.process.as_deref().is_none_or(crate::workspace::icons::is_shell) {
            // A shell without goatty's widgets (plain SSH, bash): what works anyway.
            out.push(Item::action("Find a snippet…", "snippets", cx).icon(Icon::BookOpen).into());
            if self.last_command_line().is_some() {
                out.push(Item::action("Save the last command as a snippet…", "save_command_line", cx).icon(Icon::BookmarkPlus).into());
            }
        }

        out.push(Entry::Separator);
        if self.session.watching() {
            let down = *self.session.status.lock() != LinkStatus::Connected;
            out.push(
                Item::action("Size the session to this pane", "take_session_size", cx)
                    .icon(Icon::Maximize2)
                    .disabled(down.then_some("Not connected to goattyd"))
                    .into(),
            );
        }
        out.push(Item::action("Find in the output…", "find", cx).icon(Icon::Search).into());
        out.push(Item::action("View the last command's output", "view_last_output", cx).icon(Icon::FileText).into());
        out
    }

    /// A right click that doesn't open the menu: the program's when it
    /// uses the mouse, else `mouse.right_click`'s own action.
    fn right_click(&mut self, ev: &MouseDownEvent, col: usize, row: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let setting = cx.global::<AppSettings>().client.mouse.right_click;
        let uses_mouse = self.program_uses_mouse();
        if setting.opens_menu(uses_mouse, ev.modifiers.shift) {
            self.menu_cell = Some((col, row));
            cx.emit(TerminalEvent::ContextMenu { at: ev.position, keyboard: false });
            return;
        }
        if uses_mouse {
            self.right_to_program = true;
            self.report_mouse(MouseButton::Right, MouseAction::Press, col, row, &ev.modifiers);
            return;
        }
        if setting == RightClick::Paste {
            if self.copy_selection(cx) {
                self.session.terminal.selection_clear();
                cx.notify();
            } else {
                self.paste(&Paste, window, cx);
            }
        }
    }

    // ---- links --------------------------------------------------------------

    fn hover_is_fresh(&self, h: &HoveredLink) -> bool {
        if h.pending {
            return false;
        }
        if let Some(key) = &h.remote_stat_key {
            return key.0 == self.session.remote_cwd_str() && self.remote_stats.get_fresh(key, Instant::now()).is_some();
        }
        true
    }

    /// Re-detect the link under the mouse. Returns whether what's shown
    /// (underline, hint) changed.
    fn refresh_hover(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let wanted = match self.link_click {
            LinkClick::Off => false,
            LinkClick::Click => true,
            LinkClick::CtrlClick => self.modifiers.secondary(cx),
        };
        // In programs with mouse reporting, Shift hands the mouse back to us.
        let active = wanted && !self.selecting && !self.mouse_mode_active(self.modifiers.shift);
        let link = if active { self.mouse_cell.and_then(|(c, r)| self.session.terminal.link_at(c, r)) } else { None };
        let before = self.hover_shown();
        self.hover = match (link, self.hover.take()) {
            (None, _) => None,
            (Some(link), Some(h)) if h.link == link && self.hover_is_fresh(&h) => Some(h),
            (Some(link), _) => {
                let resolved = self.resolve_link(&link, true, cx);
                Some(HoveredLink { link, target: resolved.target, pending: resolved.pending, remote_stat_key: resolved.stat_key })
            }
        };
        self.hover_shown() != before
    }

    /// What the hover shows: the underlined spans and the target.
    fn hover_shown(&self) -> Option<(Vec<goatty_term::LinkSpan>, Target)> {
        let h = self.hover.as_ref()?;
        Some((h.link.spans.clone(), h.target.clone()?))
    }

    fn hovered_target(&self) -> Option<&Target> {
        self.hover.as_ref()?.target.as_ref()
    }

    fn cached_remote_stat(&self, target: &str) -> Option<RemotePathInfo> {
        let key = (self.session.remote_cwd_str(), target.to_string());
        match self.remote_stats.get_fresh(&key, Instant::now())? {
            CachedStat::Found { info, .. } => Some(info.clone()),
            CachedStat::Missing(_) | CachedStat::Error(_) => None,
        }
    }

    fn resolved_remote_path(&self, target: &str) -> Option<String> {
        self.cached_remote_stat(target).map(|info| info.path)
    }

    fn unresolved_remote_prefers_opener(&self, cx: &App) -> Option<String> {
        let links = &cx.global::<AppSettings>().client.links;
        if matches!(links.remote_files, RemoteFileOpen::Fetch | RemoteFileOpen::Copy) {
            return None;
        }
        let host = self.session.ssh_host.as_deref()?;
        let host_info = self.session.connection().map(|c| c.host());
        let remote_user = host_info.as_ref().map(|h| h.user.as_str()).unwrap_or("");
        let remote_hostname = host_info.as_ref().map(|h| h.hostname.as_str()).unwrap_or("");
        links.remote_file_command(host, remote_user, remote_hostname, "/_", None, None)?;
        Some(host.to_string())
    }

    /// Resolve `link`, reporting whether an asynchronous remote `StatPath`
    /// lookup is still awaited and which stat cache key was consulted.
    fn resolve_link(&mut self, link: &Link, trigger_stat: bool, cx: &mut Context<'_, Self>) -> ResolveOutcome {
        let is_remote = self.session.is_remote();
        let conn = self.session.connection().cloned();
        let supports_remote = is_remote && conn.as_ref().is_some_and(|c| c.supports_remote_files());
        let remote_hostname = if is_remote { conn.as_ref().map(|c| c.host().hostname) } else { self.hostname.clone() };
        let cwd = if is_remote { None } else { self.session.cwd() };
        let home = if is_remote { None } else { std::env::var_os("HOME").map(PathBuf::from) };
        let exists = |p: &Path| p.exists();

        let pending_stat_for: RefCell<Option<RemoteStatKey>> = RefCell::new(None);
        let consulted_stat_key: RefCell<Option<RemoteStatKey>> = RefCell::new(None);
        let saw_pending = StdCell::new(false);
        let cwd_key = supports_remote.then(|| self.session.remote_cwd_str()).flatten();
        let now = Instant::now();
        let remote_stat_fn = |text: &str| -> RemoteLookup {
            let key = (cwd_key.clone(), text.to_string());
            *consulted_stat_key.borrow_mut() = Some(key.clone());
            if let Some(cached) = self.remote_stats.get_fresh(&key, now) {
                match cached {
                    CachedStat::Found { info, .. } => RemoteLookup::Found(info.clone()),
                    CachedStat::Missing(_) | CachedStat::Error(_) => RemoteLookup::Missing,
                }
            } else {
                saw_pending.set(true);
                if trigger_stat {
                    *pending_stat_for.borrow_mut() = Some(key);
                }
                RemoteLookup::Pending
            }
        };
        let target = {
            let place = Place {
                cwd: cwd.as_deref(),
                home: home.as_deref(),
                remote_host: self.session.host_label.as_deref(),
                hostname: remote_hostname.as_deref(),
                exists: &exists,
                remote_stat: supports_remote.then_some(&remote_stat_fn as &dyn Fn(&str) -> RemoteLookup),
                remote_can_fetch: supports_remote,
            };
            links::resolve(link, &place)
        };
        if let Some(key) = pending_stat_for.into_inner()
            && let Some(conn) = conn
        {
            self.request_remote_stat(conn, key, cx);
        }
        ResolveOutcome { target, pending: saw_pending.get(), stat_key: consulted_stat_key.into_inner() }
    }

    /// Fallback for right-click context menus when the user didn't hover first
    /// to warm `remote_stats` (or `StatPath` is still in flight).
    fn fallback_remote_menu_target(&self, link: &Link) -> Option<Target> {
        if !self.session.is_remote() || link.kind != LinkKind::Path || links::is_windows_drive_path(&link.target) {
            return None;
        }
        let supports_remote = self.session.connection().is_some_and(|c| c.supports_remote_files());
        if !supports_remote && !link.target.starts_with('/') && !link.target.starts_with("~/") {
            return None;
        }
        let key = (self.session.remote_cwd_str(), link.target.clone());
        if self.remote_stats.get_fresh(&key, Instant::now()).is_some() {
            return None;
        }
        Some(Target::Remote {
            path: link.target.clone(),
            host: self.session.host_label.clone()?,
            line: link.line,
            column: link.column,
            is_dir: false,
            size: None,
            on_session_host: true,
            can_fetch: supports_remote,
        })
    }

    fn request_remote_stat(&mut self, conn: Arc<goatty_client::Connection>, key: RemoteStatKey, cx: &mut Context<'_, Self>) {
        if *self.session.status.lock() != LinkStatus::Connected || !conn.is_connected() {
            return;
        }
        let Some(generation) = self.remote_stats.begin_lookup(&key) else {
            return;
        };
        let stat_cwd = key.0.clone();
        let candidates = links::remote_candidates(&key.1);
        let session_id = self.session.daemon_id();
        let (tx, rx) = flume::bounded(1);
        if let Err(e) = std::thread::Builder::new().name("remote-stat".into()).spawn(move || {
            let _ = tx.send(conn.stat_path(session_id, stat_cwd, candidates));
        }) {
            self.remote_stats.abort_lookup(&key, generation);
            log::debug!("couldn't spawn remote-stat thread for {}: {e}", key.1);
            return;
        }
        cx.spawn(async move |this, cx| {
            let res = rx.recv_async().await;
            let mut pending_to_open: Option<(AnyWindowHandle, Target)> = None;
            let _ = this.update(cx, |view, cx| {
                let now = Instant::now();
                let result = match res {
                    Ok(Ok(info)) => Ok(info),
                    Ok(Err(e)) => {
                        log::debug!("remote stat_path failed for {}: {e:#}", key.1);
                        Err(())
                    }
                    Err(_) => Err(()),
                };
                if !view.remote_stats.finish_lookup(key.clone(), generation, view.session.remote_cwd_str().is_some(), result, now) {
                    return;
                }
                if let Some(pending) = view.pending_open.as_ref().filter(|p| p.stat_key == key).cloned() {
                    view.pending_open = None;
                    let still_valid = view.viewer.is_none()
                        && !view.selecting
                        && view.link_click != LinkClick::Off
                        && !view.mouse_mode_active(view.modifiers.shift)
                        && view.mouse_cell.and_then(|(c, r)| view.session.terminal.link_at(c, r)).as_ref() == Some(&pending.link);
                    if still_valid && let Some(target) = view.resolve_link(&pending.link, false, cx).target {
                        pending_to_open = Some((pending.window, target));
                    }
                }
                if view.hover.as_ref().is_some_and(|h| h.remote_stat_key.as_ref() == Some(&key) || h.link.target == key.1) {
                    view.hover = None;
                }
                if view.refresh_hover(cx) {
                    cx.notify();
                }
            });
            if let Some((window, target)) = pending_to_open {
                cx.update(|cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        if let Some(entity) = this.upgrade() {
                            entity.update(cx, |view, cx| view.open_link(target, window, cx));
                        }
                    });
                });
            }
        })
        .detach();
    }

    fn plan_remote(&self, target: &Target, cx: &App) -> RemoteAction {
        match target {
            Target::Remote { path, line, column, on_session_host, can_fetch, .. } => {
                self.plan_remote_parts(path, *line, *column, *on_session_host, *can_fetch, cx)
            }
            _ => RemoteAction::Copy,
        }
    }

    fn plan_remote_parts(
        &self,
        path: &str,
        line: Option<u32>,
        column: Option<u32>,
        on_session_host: bool,
        can_fetch: bool,
        cx: &App,
    ) -> RemoteAction {
        let host_info = self.session.connection().map(|c| c.host());
        let remote_user = host_info.as_ref().map(|h| h.user.as_str());
        let remote_hostname = host_info.as_ref().map(|h| h.hostname.as_str());
        links::plan_remote_action(
            &cx.global::<AppSettings>().client.links,
            self.session.ssh_host.as_deref(),
            remote_user,
            remote_hostname,
            path,
            line,
            column,
            on_session_host,
            can_fetch,
        )
    }

    fn open_link(&mut self, target: Target, window: &mut Window, cx: &mut Context<'_, Self>) {
        match target {
            Target::Url(url) => cx.open_url(&url),
            Target::Other(url) => {
                let scheme = url.split(':').next().unwrap_or_default().to_string();
                let (open, copy) = (url.clone(), url.clone());
                let toast = Toast::warning(format!("Open this {scheme}: link?"))
                    .body(format!("{url}\nLinks like this start another application."))
                    .action("Open", move |_, cx| cx.open_url(&open))
                    .action("Copy", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone())))
                    .key("terminal.link");
                toast::show_in(window, cx, toast);
            }
            Target::File { path, line, column } => {
                let command = cx.global::<AppSettings>().client.links.file_command(&path.to_string_lossy(), line, column);
                match command {
                    Some(argv) => {
                        if let Err(e) = links::spawn_opener(&argv) {
                            let toast = Toast::error(format!("Couldn't run {}", argv[0]))
                                .body(format!("{e}. Change the command under Settings → Terminal → Open files with."))
                                .key("terminal.link");
                            toast::show_in(window, cx, toast);
                        }
                    }
                    None => cx.open_with_system(&path),
                }
            }
            Target::Remote { path, host, line, column, is_dir, size, on_session_host, can_fetch } => {
                if size.is_none()
                    && on_session_host
                    && !path.starts_with('/')
                    && self.session.connection().is_some_and(|c| c.supports_remote_files())
                {
                    if let Some(info) = self.cached_remote_stat(&path) {
                        self.open_link(
                            Target::Remote {
                                path: info.path,
                                host,
                                line,
                                column,
                                is_dir: info.is_dir,
                                size: Some(info.size),
                                on_session_host,
                                can_fetch: !info.is_dir && can_fetch,
                            },
                            window,
                            cx,
                        );
                        return;
                    }
                    if cx.global::<AppSettings>().client.links.remote_files != RemoteFileOpen::Copy
                        && self.unresolved_remote_prefers_opener(cx).is_some()
                    {
                        self.open_unresolved_remote(path, host, line, column, window, cx);
                        return;
                    }
                }
                let path = if size.is_none() && cx.global::<AppSettings>().client.links.remote_files == RemoteFileOpen::Copy {
                    self.resolved_remote_path(&path).unwrap_or(path)
                } else {
                    path
                };
                match self.plan_remote_parts(&path, line, column, on_session_host, can_fetch, cx) {
                    RemoteAction::OpenInEditor { argv, .. } => {
                        if let Err(e) = links::spawn_opener(&argv) {
                            let copy_path = path.clone();
                            let toast = Toast::error(format!("Couldn't run {}", argv[0]))
                                .body(format!("{e}. Change the command under Settings → Terminal → Open remote files with."))
                                .action("Copy path", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone())))
                                .key("terminal.link");
                            toast::show_in(window, cx, toast);
                        }
                    }
                    RemoteAction::Fetch { max_bytes, prompt_if_larger } => {
                        self.open_remote_fetch(path, host, line, column, size, max_bytes, prompt_if_larger, window, cx);
                    }
                    RemoteAction::Copy => {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        let body = if !on_session_host || !self.session.is_remote() {
                            format!("The file is on {host}; goatty only fetches files from the active session's host.")
                        } else if !self.session.connection().is_some_and(|c| c.supports_remote_files()) {
                            format!("goattyd on {host} is too old to fetch files (protocol v13 required).")
                        } else if is_dir {
                            format!("{host}:{path} is a folder; set \"Open remote files with\" under Settings → Terminal to open folders.")
                        } else {
                            format!("Copied path on {host}.")
                        };
                        toast::show_in(window, cx, Toast::info(format!("Copied {path}")).body(body).key("terminal.link"));
                    }
                }
            }
        }
    }

    fn open_unresolved_remote(
        &mut self,
        path: String,
        host: String,
        line: Option<u32>,
        column: Option<u32>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(conn) = self.session.connection().cloned() else {
            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
            toast::show_in(window, cx, Toast::info(format!("Copied {path}")).key("terminal.link"));
            return;
        };
        let stat_cwd = self.session.remote_cwd_str();
        let generation = self.remote_stats.generation();
        let session_id = self.session.daemon_id();
        let filename = Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or(&path).to_string();
        let toast_id =
            toast::show_in(window, cx, Toast::loading(format!("Checking {filename}…")).body(format!("{host}:{path}")).key("terminal.link"));
        let window_handle = window.window_handle();
        let req_path = path.clone();
        let req_cwd = stat_cwd.clone();
        let (tx, rx) = flume::bounded(1);
        if let Err(e) = std::thread::Builder::new().name("remote-stat-open".into()).spawn(move || {
            let _ = tx.send(conn.stat_path(session_id, req_cwd, links::remote_candidates(&req_path)));
        }) {
            let copy_path = path.clone();
            toast::update(
                cx,
                toast_id,
                Toast::error("Couldn't check remote path")
                    .body(format!("{e}"))
                    .action("Copy path", move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                    })
                    .key("terminal.link"),
            );
            return;
        }
        cx.spawn(async move |this, cx| {
            let res = rx.recv_async().await.unwrap_or_else(|_| Err(anyhow::anyhow!("stat worker terminated")));
            match res {
                Ok(Some(info)) => {
                    let target = Target::Remote {
                        path: info.path.clone(),
                        host,
                        line,
                        column,
                        is_dir: info.is_dir,
                        size: Some(info.size),
                        on_session_host: true,
                        can_fetch: !info.is_dir,
                    };
                    cx.update(|cx| {
                        toast::dismiss(cx, toast_id);
                        let _ = window_handle.update(cx, |_, window, cx| {
                            if let Some(entity) = this.upgrade() {
                                entity.update(cx, |view, cx| {
                                    view.remote_stats.insert_found(
                                        (stat_cwd, path),
                                        generation,
                                        view.session.remote_cwd_str().is_some(),
                                        info,
                                        Instant::now(),
                                    );
                                    view.open_link(target, window, cx);
                                });
                            }
                        });
                    });
                }
                Ok(None) => {
                    let copy_path = path.clone();
                    cx.update(|cx| {
                        toast::update(
                            cx,
                            toast_id,
                            Toast::info(format!("Couldn't find {filename}"))
                                .body(format!("{path} does not exist on {host}."))
                                .action("Copy path", move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                                })
                                .key("terminal.link"),
                        );
                    });
                }
                Err(e) => {
                    let copy_path = path.clone();
                    cx.update(|cx| {
                        toast::update(
                            cx,
                            toast_id,
                            Toast::error("Couldn't check remote path")
                                .body(format!("{e:#}"))
                                .action("Copy path", move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                                })
                                .key("terminal.link"),
                        );
                    });
                }
            }
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn open_remote_fetch(
        &mut self,
        path: String,
        host: String,
        line: Option<u32>,
        column: Option<u32>,
        size: Option<u64>,
        max_bytes: u64,
        prompt_if_larger: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(sz) = size {
            if sz > goatty_proto::HARD_MAX_FILE_BYTES {
                let size_str = links::format_bytes(sz);
                let hard_str = links::format_bytes(goatty_proto::HARD_MAX_FILE_BYTES);
                let copy_path = path.clone();
                let toast = Toast::error(format!("Remote file is {size_str}"))
                    .body(format!("{path}\nToo large to fetch over the session link (limit is {hard_str})."))
                    .action("Copy path", move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                    })
                    .key("terminal.link");
                toast::show_in(window, cx, toast);
                return;
            }
            if prompt_if_larger && sz > max_bytes {
                let size_str = links::format_bytes(sz);
                let limit_str = links::format_bytes(max_bytes);
                let me = cx.entity().downgrade();
                let p2 = path.clone();
                let h2 = host.clone();
                let copy_path = path.clone();
                let toast = Toast::warning(format!("Remote file is {size_str}"))
                    .body(format!("{path}\nLarger than your {limit_str} download limit."))
                    .action("Fetch anyway", move |window, cx| {
                        let _ = me.update(cx, |v, cx| {
                            v.open_remote_fetch(p2.clone(), h2.clone(), line, column, Some(sz), sz.max(max_bytes), false, window, cx);
                        });
                    })
                    .action("Copy path", move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                    })
                    .key("terminal.link");
                toast::show_in(window, cx, toast);
                return;
            }
        }
        let Some(conn) = self.session.connection().cloned() else {
            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
            toast::show_in(window, cx, Toast::info(format!("Copied {path}")).key("terminal.link"));
            return;
        };
        let filename = Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or(&path).to_string();
        let toast_id =
            toast::show_in(window, cx, Toast::loading(format!("Fetching {filename}…")).body(format!("{host}:{path}")).key("terminal.link"));
        let req_path = path.clone();
        let stat_cwd = self.session.remote_cwd_str();
        let cache_endpoint_key = conn.endpoint().cache_key();
        let session_id = self.session.daemon_id();
        let need_prestat = size.is_none();

        enum FetchOutcome {
            Opened { resolved_path: String, local_path: PathBuf },
            NotFound,
            IsDir { resolved_path: String },
            TooLargeHard { resolved_path: String, size: u64 },
            ConfirmLarge { resolved_path: String, size: u64 },
        }

        let (tx, rx) = flume::bounded(1);
        if let Err(e) = std::thread::Builder::new().name("remote-fetch".into()).spawn(move || {
            let res = (|| -> anyhow::Result<FetchOutcome> {
                let mut fetch_path = req_path.clone();
                if need_prestat && conn.supports_remote_files() {
                    match conn.stat_path(session_id, stat_cwd, links::remote_candidates(&req_path)) {
                        Ok(Some(info)) => {
                            if info.is_dir {
                                return Ok(FetchOutcome::IsDir { resolved_path: info.path });
                            }
                            if info.size > goatty_proto::HARD_MAX_FILE_BYTES {
                                return Ok(FetchOutcome::TooLargeHard { resolved_path: info.path, size: info.size });
                            }
                            if prompt_if_larger && info.size > max_bytes {
                                return Ok(FetchOutcome::ConfirmLarge { resolved_path: info.path, size: info.size });
                            }
                            fetch_path = info.path;
                        }
                        Ok(None) => return Ok(FetchOutcome::NotFound),
                        Err(_) => {}
                    }
                }
                let (resolved_path, data) = conn.read_file(&fetch_path, max_bytes)?;
                let local_path = links::save_remote_copy(&cache_endpoint_key, &resolved_path, &data)?;
                Ok(FetchOutcome::Opened { resolved_path, local_path })
            })();
            let _ = tx.send(res);
        }) {
            let copy_path = path.clone();
            toast::update(
                cx,
                toast_id,
                Toast::error("Couldn't fetch remote file")
                    .body(format!("{e}"))
                    .action("Copy path", move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                    })
                    .key("terminal.link"),
            );
            return;
        }

        cx.spawn(async move |this, cx| {
            let res = rx.recv_async().await.unwrap_or_else(|_| Err(anyhow::anyhow!("fetch worker terminated")));
            cx.update(|cx| match res {
                Ok(FetchOutcome::Opened { resolved_path, local_path }) => {
                    let command = cx.global::<AppSettings>().client.links.file_command(&local_path.to_string_lossy(), line, column);
                    match command {
                        Some(argv) => {
                            if let Err(e) = links::spawn_opener(&argv) {
                                toast::update(
                                    cx,
                                    toast_id,
                                    Toast::error(format!("Couldn't run {}", argv[0]))
                                        .body(format!("{e}. Change the command under Settings → Terminal → Open files with."))
                                        .key("terminal.link"),
                                );
                                return;
                            }
                        }
                        None => cx.open_with_system(&local_path),
                    }
                    toast::update(
                        cx,
                        toast_id,
                        Toast::success(format!("Opened {filename}"))
                            .body(format!("Read-only copy from {host}:{resolved_path}"))
                            .key("terminal.link"),
                    );
                }
                Ok(FetchOutcome::NotFound) => {
                    let copy_path = path.clone();
                    toast::update(
                        cx,
                        toast_id,
                        Toast::info(format!("Couldn't find {filename}"))
                            .body(format!("{path} does not exist on {host}."))
                            .action("Copy path", move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                            })
                            .key("terminal.link"),
                    );
                }
                Ok(FetchOutcome::IsDir { resolved_path }) => {
                    let copy_path = resolved_path.clone();
                    toast::update(
                        cx,
                        toast_id,
                        Toast::info(format!("{filename} is a folder"))
                            .body(format!(
                                "{host}:{resolved_path} is a folder; set \"Open remote files with\" under Settings → Terminal to open folders."
                            ))
                            .action("Copy path", move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                            })
                            .key("terminal.link"),
                    );
                }
                Ok(FetchOutcome::TooLargeHard { resolved_path, size: sz }) => {
                    let size_str = links::format_bytes(sz);
                    let hard_str = links::format_bytes(goatty_proto::HARD_MAX_FILE_BYTES);
                    let copy_path = resolved_path.clone();
                    toast::update(
                        cx,
                        toast_id,
                        Toast::error(format!("Remote file is {size_str}"))
                            .body(format!("{resolved_path}\nToo large to fetch over the session link (limit is {hard_str})."))
                            .action("Copy path", move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                            })
                            .key("terminal.link"),
                    );
                }
                Ok(FetchOutcome::ConfirmLarge { resolved_path, size: sz }) => {
                    let size_str = links::format_bytes(sz);
                    let limit_str = links::format_bytes(max_bytes);
                    let p2 = resolved_path.clone();
                    let h2 = host.clone();
                    let copy_path = resolved_path.clone();
                    toast::update(
                        cx,
                        toast_id,
                        Toast::warning(format!("Remote file is {size_str}"))
                            .body(format!("{resolved_path}\nLarger than your {limit_str} download limit."))
                            .action("Fetch anyway", move |window, cx| {
                                let _ = this.update(cx, |v, cx| {
                                    v.open_remote_fetch(p2.clone(), h2.clone(), line, column, Some(sz), sz.max(max_bytes), false, window, cx);
                                });
                            })
                            .action("Copy path", move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                            })
                            .key("terminal.link"),
                    );
                }
                Err(e) => {
                    let copy_path = path.clone();
                    toast::update(
                        cx,
                        toast_id,
                        Toast::error("Couldn't fetch remote file")
                            .body(format!("{e:#}"))
                            .action("Copy path", move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                            })
                            .key("terminal.link"),
                    );
                }
            });
        })
        .detach();
    }

    fn on_modifiers_changed(&mut self, ev: &ModifiersChangedEvent, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.modifiers = ev.modifiers;
        if self.refresh_hover(cx) {
            cx.notify();
        }
    }

    fn on_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if !*hovered {
            self.mouse_cell = None;
            self.pending_open = None;
            if self.refresh_hover(cx) {
                cx.notify();
            }
        }
    }

    // ---- output viewer ----------------------------------------------------------

    /// Open the viewer on `source`, or switch the open one to it.
    fn open_viewer(&mut self, source: output_view::Source, window: &mut Window, cx: &mut Context<'_, Self>) -> Entity<OutputView> {
        if let Some((viewer, _)) = &self.viewer {
            let viewer = viewer.clone();
            viewer.update(cx, |v, cx| {
                if v.source() != source {
                    v.show(source, cx);
                }
            });
            return viewer;
        }
        // A selecting drag ends here (the grid's mouse handlers go away with
        // it, so its auto-scroll would never stop); nothing gets copied.
        self.selecting = false;
        self.drag_pos = None;
        self.autoscroll = None;
        self.session.terminal.selection_clear();
        self.hover = None;
        self.pending_open = None;
        self.pressed_pending_link = None;
        let session = self.session.clone();
        let (font, size, lh) = (self.font.clone(), self.font_size, self.line_height);
        let viewer = cx.new(|cx| OutputView::new(session, source, font, size, lh, window, cx));
        let sub = cx.subscribe_in(&viewer, window, |this, _, ev: &OutputEvent, window, cx| match ev {
            OutputEvent::Close => this.close_viewer(window, cx),
        });
        self.viewer = Some((viewer.clone(), sub));
        cx.notify();
        viewer
    }

    fn close_viewer(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.viewer.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// Find: all output with the search field focused (again: back to it).
    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<'_, Self>) {
        let source = match &self.viewer {
            Some((v, _)) => v.read(cx).source(),
            None => output_view::Source::All,
        };
        let viewer = self.open_viewer(source, window, cx);
        viewer.update(cx, |v, cx| v.focus_search(window, cx));
    }

    fn view_last_output(&mut self, _: &ViewLastOutput, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.open_viewer(output_view::Source::Last, window, cx);
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.session.terminal.scroll(self.geom.get().rows as i32);
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.session.terminal.scroll(-(self.geom.get().rows as i32));
        cx.notify();
    }

    fn previous_prompt(&mut self, _: &PreviousPrompt, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.jump_to_prompt(PromptJump::Previous, Some(window), cx);
    }

    fn next_prompt(&mut self, _: &NextPrompt, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.jump_to_prompt(PromptJump::Next, Some(window), cx);
    }

    /// Without prompts to jump between (a full-screen program, a shell
    /// without integration) the key goes to the program instead. While the
    /// daemon sends older lines the jump waits for them: `on_wake` resumes
    /// it (`window` is `None` then).
    fn jump_to_prompt(&mut self, dir: PromptJump, window: Option<&Window>, cx: &mut Context<'_, Self>) {
        self.prompt_jump_pending = false;
        match self.session.terminal.scroll_to_prompt(dir) {
            PromptJumped::NoPrompts => {
                if window.is_some() {
                    cx.propagate();
                }
            }
            PromptJumped::Moved => cx.notify(),
            PromptJumped::Fetching => {
                self.prompt_jump_pending = true;
                // The redraw fetches the lines near the view.
                cx.notify();
            }
            PromptJumped::Boundary => {
                let toast = match dir {
                    PromptJump::Previous => Toast::info("No earlier command").body("This is the oldest command in the scrollback."),
                    PromptJump::Next => Toast::info("No later command").body("This is the newest output."),
                }
                .key("terminal.prompt_jump");
                match window {
                    Some(window) => toast::show_in(window, cx, toast),
                    None => toast::show(cx, toast),
                };
            }
        }
    }

    /// Zoom this pane (0 resets) and show the new size in a toast that
    /// replaces itself while zooming.
    fn zoom_by(&mut self, delta: f32, window: &mut Window, cx: &mut Context<'_, Self>) {
        let base = cx.global::<AppSettings>().client.font.size;
        let before = self.zoom;
        self.zoom =
            if delta == 0.0 { 0.0 } else { (base + self.zoom + delta).clamp(FontSettings::MIN_SIZE, FontSettings::MAX_SIZE) - base };
        self.font_size = px(base + self.zoom);
        self.sync_viewer_font(cx);
        cx.notify();
        let size = base + self.zoom;
        let toast = if delta != 0.0 && self.zoom == before {
            let limit = if delta > 0.0 { "largest" } else { "smallest" };
            Toast::info(format!("Text size {size} px")).body(format!("That's the {limit} size."))
        } else if self.zoom == 0.0 {
            Toast::info(format!("Text size {size} px")).body("The size from your settings.")
        } else {
            Toast::info(format!("Text size {size} px")).action("Reset", |window, cx| window.dispatch_action(Box::new(ResetFontSize), cx))
        };
        toast::show_in(window, cx, toast.key("view.zoom").duration(ZOOM_TOAST));
    }

    fn increase_font_size(&mut self, _: &IncreaseFontSize, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.zoom_by(1.0, window, cx);
    }

    fn decrease_font_size(&mut self, _: &DecreaseFontSize, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.zoom_by(-1.0, window, cx);
    }

    fn reset_font_size(&mut self, _: &ResetFontSize, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.zoom_by(0.0, window, cx);
    }

    // ---- raw input ----------------------------------------------------------

    /// Keys that no binding claimed go to the shell. A key handled here
    /// stops, so the platform doesn't also hand its text to the input
    /// handler; text that comes only from the input method (composed or
    /// committed) arrives through [`EntityInputHandler`].
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.poke_cursor();
        cx.notify();
        let kind = if ev.is_held { KeyKind::Repeat } else { KeyKind::Press };
        let Some(key) = terminal_key(&ev.keystroke, kind) else { return };
        let mode = self.session.terminal.mode();
        if let Some(bytes) = encode_key_event(&key, mode) {
            cx.stop_propagation();
            // A dead key's composition ends with the composed key itself,
            // which goes where the keyboard focus is.
            if let Some(pane) = self.composing_elsewhere(window, cx) {
                pane.update(cx, |v, cx| v.end_composition(cx));
            }
            self.end_composition(cx);
            self.pending_open = None;
            self.session.terminal.selection_clear();
            self.send(&bytes, cx);
            window.invalidate_character_coordinates();
            if mode.contains(TermMode::KITTY_EVENT_TYPES) && !self.keys_down.contains(&ev.keystroke.key) {
                // Bounded: a release can come under another name (Shift let
                // go first turns `!` into `1`) and never match.
                if self.keys_down.len() >= 16 {
                    self.keys_down.clear();
                }
                self.keys_down.push(ev.keystroke.key.clone());
            }
        }
    }

    /// Key releases go to the program if it asked for them (kitty
    /// protocol) and had the press.
    fn on_key_up(&mut self, ev: &KeyUpEvent, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(i) = self.keys_down.iter().position(|k| *k == ev.keystroke.key) else { return };
        self.keys_down.swap_remove(i);
        // The press went to the program, so the release does too, even
        // with Super held now (`terminal_key` leaves Super combos alone).
        let mut ks = ev.keystroke.clone();
        let logo = std::mem::take(&mut ks.modifiers.platform);
        let Some(mut key) = terminal_key(&ks, KeyKind::Release) else { return };
        key.mods.logo = logo;
        if let Some(bytes) = encode_key_event(&key, self.session.terminal.mode()) {
            self.session.write(&bytes);
            cx.notify();
        }
    }

    fn mouse_mode_active(&self, shift: bool) -> bool {
        !shift && self.session.terminal.mode().intersects(TermMode::MOUSE_MODE)
    }

    fn report_mouse(&self, button: MouseButton, action: MouseAction, col: usize, row: usize, m: &gpui::Modifiers) {
        if let Some(b) = encode_mouse(button, action, col, row, mods(m), self.session.terminal.mode()) {
            self.session.write(&b);
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.focus(&self.focus, cx);
        if !self.geom.get().in_grid(ev.position) {
            // The letterbox around a session another client sizes (#12): no
            // cell is there. A right click still opens the pane's menu.
            if ev.button == GMouseButton::Right {
                self.menu_cell = None;
                cx.emit(TerminalEvent::ContextMenu { at: ev.position, keyboard: false });
            }
            return;
        }
        let (col, row, right) = self.geom.get().cell_at(ev.position);
        self.pressed_link = None;
        self.pressed_pending_link = None;
        self.pending_open = None;
        if ev.button == GMouseButton::Left && ev.click_count == 1 {
            self.modifiers = ev.modifiers;
            self.mouse_cell = Some((col, row));
            self.refresh_hover(cx);
            if let Some(target) = self.hovered_target().cloned() {
                if self.link_click == LinkClick::CtrlClick {
                    self.open_link(target, window, cx);
                    return;
                }
                // Plain click: open on release, unless it becomes a selection.
                self.pressed_link = Some(target);
            } else if let Some(h) = self.hover.as_ref().filter(|h| h.pending)
                && let Some(stat_key) = h.remote_stat_key.clone()
            {
                if self.link_click == LinkClick::CtrlClick {
                    self.pending_open = Some(PendingOpen { link: h.link.clone(), stat_key, window: window.window_handle() });
                    return;
                }
                self.pressed_pending_link = Some((h.link.clone(), stat_key));
            }
        }
        if ev.button == GMouseButton::Right {
            self.right_click(ev, col, row, window, cx);
            return;
        }
        if self.mouse_mode_active(ev.modifiers.shift) {
            if let Some(btn) = mouse_button(Some(ev.button)) {
                self.buttons_to_program |= button_bit(ev.button);
                self.report_mouse(btn, MouseAction::Press, col, row, &ev.modifiers);
            }
            return;
        }
        match ev.button {
            GMouseButton::Left => {
                let kind = match ev.click_count {
                    1 => SelectKind::Simple,
                    2 => SelectKind::Word,
                    _ => SelectKind::Line,
                };
                self.session.terminal.selection_start(col, row, kind, right);
                if kind != SelectKind::Simple {
                    self.session.terminal.selection_update(col, row, right);
                }
                self.selecting = true;
                cx.notify();
            }
            GMouseButton::Middle => self.paste_middle_click(cx),
            _ => {}
        }
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let (col, row, _) = self.geom.get().cell_at(ev.position);
        if ev.button == GMouseButton::Right {
            if std::mem::take(&mut self.right_to_program) {
                self.report_mouse(MouseButton::Right, MouseAction::Release, col, row, &ev.modifiers);
            }
            return;
        }
        let pressed_here = self.buttons_to_program & button_bit(ev.button) != 0;
        self.buttons_to_program &= !button_bit(ev.button);
        if pressed_here
            && self.mouse_mode_active(ev.modifiers.shift)
            && let Some(btn) = mouse_button(Some(ev.button))
        {
            self.report_mouse(btn, MouseAction::Release, col, row, &ev.modifiers);
        }
        if ev.button == GMouseButton::Middle {
            return; // selections and links are the left button's
        }
        self.end_selection(cx);
        let pressed_link = self.pressed_link.take();
        let pressed_pending = self.pressed_pending_link.take();
        if ev.button == GMouseButton::Left && !self.session.terminal.has_selection() {
            if let Some(target) = pressed_link {
                self.mouse_cell = Some((col, row));
                self.refresh_hover(cx);
                if self.hovered_target() == Some(&target) {
                    self.open_link(target, window, cx);
                }
            } else if let Some((link, stat_key)) = pressed_pending {
                self.mouse_cell = Some((col, row));
                self.refresh_hover(cx);
                if self.hover.as_ref().is_some_and(|h| h.link == link) {
                    if let Some(target) = self.hovered_target().cloned() {
                        self.open_link(target, window, cx);
                    } else if self.hover.as_ref().is_some_and(|h| h.pending) {
                        self.pending_open = Some(PendingOpen { link, stat_key, window: window.window_handle() });
                    }
                }
            }
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let geom = self.geom.get();
        // Over the letterbox there's no cell: no hover, no motion report
        // (unless a button pressed on the grid drags out there).
        let inside = geom.in_grid(ev.position);
        let (col, row, _) = geom.cell_at(ev.position);
        self.mouse_cell = inside.then_some((col, row));
        self.modifiers = ev.modifiers;
        if let Some(pending) = &self.pending_open
            && self.mouse_cell.and_then(|(c, r)| self.session.terminal.link_at(c, r)).as_ref() != Some(&pending.link)
        {
            self.pending_open = None;
        }
        if self.selecting {
            // The window-wide listener (see `render`) extends the selection,
            // also when the drag leaves the pane.
            return;
        }
        if self.refresh_hover(cx) {
            cx.notify();
        }
        if self.mouse_mode_active(ev.modifiers.shift) && (inside || self.buttons_to_program != 0) {
            let btn = mouse_button(ev.pressed_button).unwrap_or(MouseButton::None);
            self.report_mouse(btn, MouseAction::Motion, col, row, &ev.modifiers);
        }
    }

    /// A selecting drag moved to `pos` (anywhere in the window). Past the
    /// top or bottom edge, the view scrolls until the drag comes back.
    fn drag_to(&mut self, pos: Point<Pixels>, cx: &mut Context<'_, Self>) {
        let geom = self.geom.get();
        let (col, row, right) = geom.cell_at(pos);
        self.session.terminal.selection_update(col, row, right);
        // A press that turns into a selection doesn't open its link
        // (checked on release), and no link shows while selecting.
        self.hover = None;
        self.pending_open = None;
        self.pressed_pending_link = None;
        self.drag_pos = Some(pos);
        if geom.edge_scroll(pos.y) != 0 && self.autoscroll.is_none() {
            self.autoscroll = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(AUTOSCROLL_TICK).await;
                    match this.update(cx, |view, cx| view.autoscroll_tick(cx)) {
                        Ok(true) => {}
                        _ => break,
                    }
                }
            }));
        }
        cx.notify();
    }

    /// One auto-scroll step; false once the drag is over or back on the grid.
    fn autoscroll_tick(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let geom = self.geom.get();
        let lines = match self.drag_pos {
            Some(pos) if self.selecting => geom.edge_scroll(pos.y),
            _ => 0,
        };
        let Some(pos) = self.drag_pos.filter(|_| lines != 0) else {
            self.autoscroll = None;
            return false;
        };
        self.session.terminal.scroll(lines);
        let (col, row, right) = geom.cell_at(pos);
        self.session.terminal.selection_update(col, row, right);
        cx.notify();
        true
    }

    /// The selecting drag ended: copy what it selected, if anything.
    fn end_selection(&mut self, cx: &mut Context<'_, Self>) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        self.drag_pos = None;
        self.autoscroll = None;
        if self.session.terminal.has_selection() {
            self.copy_on_select(cx);
        } else {
            self.session.terminal.selection_clear();
        }
        cx.notify();
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let geom = self.geom.get();
        let lines = match ev.delta {
            ScrollDelta::Lines(p) => p.y * 3.0,
            ScrollDelta::Pixels(p) => p.y / geom.cell.height.max(px(1.0)),
        };
        self.prompt_jump_pending = false;
        self.scroll_accum += lines;
        let whole = self.scroll_accum.trunc() as i32;
        if whole == 0 {
            return;
        }
        self.scroll_accum -= whole as f32;
        let mode = self.session.terminal.mode();
        let (col, row, _) = geom.cell_at(ev.position);
        if self.mouse_mode_active(ev.modifiers.shift) {
            self.pending_open = None;
            if !geom.in_grid(ev.position) {
                return; // the letterbox has no cell to report
            }
            let btn = if whole > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            for _ in 0..whole.unsigned_abs() {
                self.report_mouse(btn, MouseAction::Press, col, row, &ev.modifiers);
            }
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            self.pending_open = None;
            // less/man/etc. in the alt screen: wheel → arrow keys.
            let key = if whole > 0 { Key::Up } else { Key::Down };
            if let Some(b) = encode_key(&key, Mods::default(), mode) {
                for _ in 0..whole.unsigned_abs() {
                    self.session.write(&b);
                }
            }
        } else {
            self.session.terminal.scroll(whole);
            if let Some(pending) = &self.pending_open
                && self.mouse_cell.and_then(|(c, r)| self.session.terminal.link_at(c, r)).as_ref() != Some(&pending.link)
            {
                self.pending_open = None;
            }
            self.refresh_hover(cx);
            cx.notify();
        }
    }
}

/// The primary selection; the clipboard where there's none.
fn read_primary(cx: &App) -> Option<ClipboardItem> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    return cx.read_from_primary();
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    return cx.read_from_clipboard();
}

/// Set the primary selection where there is one.
fn write_primary(cx: &App, item: ClipboardItem) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    cx.write_to_primary(item);
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = (cx, item);
}

/// `buttons_to_program`'s bit for a left or middle press.
fn button_bit(b: GMouseButton) -> u8 {
    match b {
        GMouseButton::Left => 1,
        GMouseButton::Middle => 2,
        _ => 0,
    }
}

fn mouse_button(b: Option<GMouseButton>) -> Option<MouseButton> {
    match b? {
        GMouseButton::Left => Some(MouseButton::Left),
        GMouseButton::Middle => Some(MouseButton::Middle),
        GMouseButton::Right => Some(MouseButton::Right),
        _ => None,
    }
}

/// Text from the input method (#21): IME composition and commits, dead
/// keys and the compose key. The terminal has no editable document, so the
/// only text the input method sees is its own composition, kept at the
/// cursor.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<'_, Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<'_, Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> Option<Range<usize>> {
        let preedit = |v: &Self| (!v.preedit.is_empty()).then(|| 0..v.preedit.encode_utf16().count());
        match self.composing_elsewhere(window, cx) {
            Some(pane) => preedit(pane.read(cx)),
            None => preedit(self),
        }
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        match self.composing_elsewhere(window, cx) {
            Some(pane) => pane.update(cx, |v, cx| v.end_composition(cx)),
            None => self.end_composition(cx),
        }
    }

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, window: &mut Window, cx: &mut Context<'_, Self>) {
        match self.composing_elsewhere(window, cx) {
            Some(pane) => pane.update(cx, |v, cx| v.commit(text, window, cx)),
            None => self.commit(text, window, cx),
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match self.composing_elsewhere(window, cx) {
            Some(pane) => pane.update(cx, |v, cx| v.compose(text, window, cx)),
            None => self.compose(text, window, cx),
        }
    }

    /// The input method's popup goes under the composition (or the cursor).
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Bounds<Pixels>> {
        match self.composing_elsewhere(window, cx) {
            Some(pane) => pane.read(cx).ime_bounds(range),
            None => self.ime_bounds(range),
        }
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<'_, Self>) -> Option<usize> {
        None
    }
}

/// The pane an input-method composition is going on in. gpui doesn't reset
/// the input method when focus moves to another pane (both take text), so
/// the rest of a composition, and its commit, would land in the newly
/// focused pane: it finishes in the pane it started in instead.
#[derive(Default)]
struct Composing(Option<(AnyWindowHandle, WeakEntity<TerminalView>)>);

impl Global for Composing {}

impl TerminalView {
    /// Another pane in this window that has a composition going on.
    fn composing_elsewhere(&self, window: &Window, cx: &Context<'_, Self>) -> Option<Entity<Self>> {
        let (handle, pane) = cx.try_global::<Composing>()?.0.as_ref()?;
        let pane = pane.upgrade().filter(|p| *handle == window.window_handle() && p.entity_id() != cx.entity_id())?;
        (!pane.read(cx).preedit.is_empty()).then_some(pane)
    }

    /// Show `text` as the composition; empty ends it.
    fn compose(&mut self, text: &str, window: &mut Window, cx: &mut Context<'_, Self>) {
        if text.is_empty() {
            self.end_composition(cx);
        } else {
            text.clone_into(&mut self.preedit);
            let me = cx.entity().downgrade();
            cx.set_global(Composing(Some((window.window_handle(), me))));
        }
        self.poke_cursor();
        cx.notify();
        window.invalidate_character_coordinates();
    }

    /// The input method's text, sent as typed (no key encoding).
    fn commit(&mut self, text: &str, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.end_composition(cx);
        self.poke_cursor();
        if text.is_empty() || self.exited {
            return;
        }
        self.session.terminal.selection_clear();
        self.send(text.as_bytes(), cx);
        window.invalidate_character_coordinates();
    }

    fn end_composition(&mut self, cx: &mut Context<'_, Self>) {
        self.preedit.clear();
        let mine = cx.try_global::<Composing>().and_then(|c| c.0.as_ref()).is_some_and(|(_, p)| p.entity_id() == cx.entity_id());
        if mine {
            cx.set_global(Composing(None));
        }
        cx.notify();
    }

    fn ime_bounds(&self, range: Range<usize>) -> Option<Bounds<Pixels>> {
        let geom = self.geom.get();
        if geom.cols == 0 {
            return None;
        }
        let cells = |s: &str| unicode_width::UnicodeWidthStr::width(s);
        let start = utf16_to_byte(&self.preedit, range.start);
        let origin = geom.preedit_origin(cells(&self.preedit));
        let x = origin.x + geom.cell.width * cells(&self.preedit[..start]) as f32;
        Some(Bounds::new(point(x, origin.y), geom.cell))
    }
}

/// The byte offset of UTF-16 offset `utf16` in `s` (clamped to its end).
fn utf16_to_byte(s: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    s.len()
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Only the default background is see-through; colored cells and
        // text stay opaque (the window is transparent, see `Workspace`).
        let bg = hsla(self.session.terminal.palette().background).opacity(self.opacity);
        let focused = self.focus.is_focused(window);
        let hint = self.hovered_target().map(|t| {
            let how = match self.link_click {
                LinkClick::Click => "Click",
                _ if cfg!(target_os = "macos") => "Cmd+click",
                _ => "Ctrl+click",
            };
            let what: std::borrow::Cow<'static, str> = match t {
                Target::Remote { .. } => match self.plan_remote(t, cx) {
                    RemoteAction::OpenInEditor { host, .. } => format!("open on {host}").into(),
                    RemoteAction::Fetch { .. } => "open read-only copy".into(),
                    RemoteAction::Copy => "copy".into(),
                },
                _ => "open".into(),
            };
            (t.describe(), format!("{how} to {what}"))
        });
        let link_spans = self.hover_shown().map(|(spans, _)| spans).unwrap_or_default();
        let ui = Ui::get(cx);
        if let Some((viewer, _)) = &self.viewer {
            // The viewer covers the grid; the pane's shortcuts that make
            // sense there stay (the workspace's bubble up as usual).
            return div()
                .id("terminal-pane")
                .size_full()
                .key_context(TERMINAL_CONTEXT)
                .track_focus(&self.focus)
                .on_action(cx.listener(Self::find))
                .on_action(cx.listener(Self::view_last_output))
                .on_action(cx.listener(Self::increase_font_size))
                .on_action(cx.listener(Self::decrease_font_size))
                .on_action(cx.listener(Self::reset_font_size))
                // Keys pressed before the viewer opened are let go here.
                .on_key_up(cx.listener(Self::on_key_up))
                .child(viewer.clone())
                .into_any_element();
        }
        div()
            .id("terminal-pane")
            .size_full()
            .relative()
            .bg(bg)
            .cursor(if link_spans.is_empty() { CursorStyle::IBeam } else { CursorStyle::PointingHand })
            .key_context(TERMINAL_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::copy_or_interrupt))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::scroll_page_up))
            .on_action(cx.listener(Self::scroll_page_down))
            .on_action(cx.listener(Self::previous_prompt))
            .on_action(cx.listener(Self::next_prompt))
            .on_action(cx.listener(Self::increase_font_size))
            .on_action(cx.listener(Self::decrease_font_size))
            .on_action(cx.listener(Self::reset_font_size))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::view_last_output))
            .on_action(cx.listener(Self::open_context_menu))
            .on_action(cx.listener(Self::copy_command_line))
            .on_action(cx.listener(Self::save_command_line))
            .on_action(cx.listener(Self::toggle_sudo))
            .on_action(cx.listener(Self::edit_command_line))
            .on_action(cx.listener(Self::clear_command_line))
            .on_action(cx.listener(Self::take_session_size))
            .on_key_down(cx.listener(Self::on_key))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_mouse_down(GMouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(GMouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(GMouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_up(GMouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(GMouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(GMouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(GMouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(GMouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(GMouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_hover(cx.listener(Self::on_hover))
            .on_scroll(cx.listener(Self::on_scroll))
            .child(TerminalElement {
                session: self.session.clone(),
                frame: self.frame.clone(),
                geom: self.geom.clone(),
                stats: self.stats.clone(),
                font: self.font.clone(),
                font_size: self.font_size,
                line_height_factor: self.line_height,
                padding: self.padding,
                view: cx.entity(),
                focus: self.focus.clone(),
                preedit: self.preedit.clone().into(),
                preedit_fg: self.session.terminal.palette().foreground,
                focused,
                glyph_mode: self.glyph_mode,
                glyphs: self.glyphs.clone(),
                cursor_visible: self.cursor_visible,
                link_spans,
            })
            .when(self.selecting, |d| {
                // While selecting, follow the drag across the whole window:
                // the pane's own move handler only fires over the pane.
                let view = cx.entity().downgrade();
                d.child(canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }
                            let _ = view.update(cx, |view, cx| {
                                if ev.pressed_button == Some(GMouseButton::Left) {
                                    if view.selecting {
                                        view.drag_to(ev.position, cx);
                                    }
                                } else {
                                    // Released where we didn't see it (outside the window).
                                    view.end_selection(cx);
                                }
                            });
                        });
                    },
                ))
            })
            .when(self.session.watching(), |d| {
                // Another client sets the size: say so, and offer to take over.
                let font = cx.global::<AppSettings>().ui_font.clone();
                d.child(
                    div()
                        .absolute()
                        .top(px(4.))
                        .right(px(4.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py(px(2.))
                        .rounded_md()
                        .bg(ui.surface)
                        .border_1()
                        .border_color(ui.border)
                        .text_xs()
                        .font_family(font)
                        .child(div().text_color(ui.muted).child("Sized by another window"))
                        .child(
                            div()
                                .id("take-session-size")
                                .cursor_pointer()
                                .text_color(ui.text)
                                .hover(|s| s.underline())
                                .child("Take over")
                                .on_mouse_down(GMouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|this, _, window, cx| this.request_take_size(window, cx))),
                        ),
                )
            })
            .when_some(hint, |d, (target, action)| {
                // The target, since an OSC 8 link's text can say anything.
                d.child(
                    div()
                        .absolute()
                        .bottom(px(4.))
                        .left(px(4.))
                        .max_w(gpui::relative(0.9))
                        .flex()
                        .gap_2()
                        .px_2()
                        .py(px(2.))
                        .rounded_md()
                        .bg(ui.surface)
                        .border_1()
                        .border_color(ui.border)
                        .text_xs()
                        .font_family(cx.global::<AppSettings>().ui_font.clone())
                        .child(div().text_color(ui.text).truncate().child(target))
                        .child(div().flex_shrink_0().text_color(ui.muted).child(action)),
                )
            })
            .into_any_element()
    }
}

/// The padding setting as GPUI edges.
fn edges(p: goatty_config::Padding) -> gpui::Edges<Pixels> {
    gpui::Edges { top: px(p.top), right: px(p.right), bottom: px(p.bottom), left: px(p.left) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_map_to_bytes() {
        assert_eq!(utf16_to_byte("", 3), 0);
        assert_eq!(utf16_to_byte("abc", 2), 2);
        assert_eq!(utf16_to_byte("日本語", 1), 3);
        assert_eq!(utf16_to_byte("a😀b", 3), 5);
        assert_eq!(utf16_to_byte("日本", 9), 6);
    }

    #[test]
    fn confirmation_follows_the_setting() {
        let multiline = [PasteRisk::Multiline { lines: 3 }];
        let runs = [PasteRisk::RunsCommands { commands: 1 }];
        assert!(!needs_confirmation(PasteConfirm::Suspicious, &[]));
        assert!(!needs_confirmation(PasteConfirm::Suspicious, &multiline));
        assert!(needs_confirmation(PasteConfirm::Suspicious, &runs));
        assert!(needs_confirmation(PasteConfirm::Multiline, &multiline));
        assert!(!needs_confirmation(PasteConfirm::Never, &runs));
    }

    #[test]
    fn cached_stat_respects_ttls() {
        let t0 = Instant::now();
        let found = CachedStat::Found { info: RemotePathInfo { path: "/a".into(), is_dir: false, size: 1 }, at: t0 };
        let missing = CachedStat::Missing(t0);
        let err = CachedStat::Error(t0);

        assert!(found.is_fresh(t0 + Duration::from_secs(29)));
        assert!(!found.is_fresh(t0 + STAT_FOUND_TTL));

        assert!(missing.is_fresh(t0 + Duration::from_secs(2)));
        assert!(!missing.is_fresh(t0 + STAT_MISSING_TTL));

        assert!(err.is_fresh(t0 + Duration::from_secs(4)));
        assert!(!err.is_fresh(t0 + STAT_ERROR_BACKOFF));
    }

    #[test]
    fn remote_stat_cache_rejects_pre_invalidation_replies_and_allows_replacement() {
        let mut cache = RemoteStatCache::default();
        let key: RemoteStatKey = (Some("/repo".into()), "generated.rs".into());
        let t0 = Instant::now();

        // Start lookup in generation 0; duplicate in same generation is deduped.
        let gen0 = cache.begin_lookup(&key).expect("gen0 lookup starts");
        assert_eq!(cache.begin_lookup(&key), None);

        // Invalidate cache before gen0 completes: replacement lookup in gen1 must start immediately.
        cache.invalidate();
        let gen1 = cache.begin_lookup(&key).expect("replacement lookup starts after invalidation");
        assert_ne!(gen0, gen1);

        // Stale gen0 missing reply arrives first: rejected, not cached, and gen1 remains in flight.
        assert!(!cache.finish_lookup(key.clone(), gen0, true, Ok(None), t0));
        assert!(cache.get_fresh(&key, t0).is_none());
        assert_eq!(cache.begin_lookup(&key), None);

        // Fresh gen1 found reply arrives: accepted and cached.
        let info = RemotePathInfo { path: "/repo/generated.rs".into(), is_dir: false, size: 42 };
        assert!(cache.finish_lookup(key.clone(), gen1, true, Ok(Some(info.clone())), t0));
        assert!(matches!(cache.get_fresh(&key, t0), Some(CachedStat::Found { info: got, .. }) if got == &info));

        // Even if an earlier-gen reply arrives AFTER gen1 already populated the cache, it is discarded.
        let stale_gen = cache.generation() - 1;
        assert!(!cache.finish_lookup(key.clone(), stale_gen, true, Ok(None), t0));
        assert!(matches!(cache.get_fresh(&key, t0), Some(CachedStat::Found { info: got, .. }) if got == &info));

        // Direct insert_found from an earlier generation is also ignored.
        cache.insert_found(key.clone(), stale_gen, true, RemotePathInfo { path: "/wrong".into(), is_dir: false, size: 0 }, t0);
        assert!(matches!(cache.get_fresh(&key, t0), Some(CachedStat::Found { info: got, .. }) if got == &info));
    }
}
