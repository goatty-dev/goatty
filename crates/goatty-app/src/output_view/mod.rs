//! The output viewer: a pane's output as text you can search, filter,
//! select and copy, without the terminal's grid in the way. It covers the
//! pane (Esc goes back) and shows either one command's output or all of it:
//!
//! * **Command**: the output of the last command (live while it runs), or
//!   an earlier one (Ctrl+Up/Down step through them). The terminal finds
//!   where it starts and ends from the shell integration's OSC 133 marks
//!   (`goatty_term::transcript`), so nobody has to pipe anything.
//! * **All output**: the whole scrollback, prompts on a band.
//!
//! Lines are the program's lines: what the terminal soft-wrapped is joined
//! again, and the view scrolls sideways (wrapping is a toggle, Alt+Z). The
//! search field filters as you type (plain text or a regular expression,
//! case-insensitive unless asked); "Only matching lines" turns it into grep
//! for people who don't know grep, and Copy copies what's shown.
//!
//! [`TerminalView`](crate::terminal::view::TerminalView) owns the viewer,
//! forwards output to it (`output_arrived`) and drops it on
//! [`OutputEvent::Close`].

mod element;
pub mod layout;
pub mod search;

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use goatty_term::{CellFlags, Transcript};
use gpui::{
    App, Bounds, ClipboardItem, Context, DispatchPhase, Entity, EventEmitter, FocusHandle, Focusable, Font, FontStyle, FontWeight, Hsla,
    KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Role, ScrollDelta, ScrollEvent,
    ShapedLine, SharedString, StrikethroughStyle, Subscription, Task, TextRun, UnderlineStyle, Window, actions, canvas, div, point,
    prelude::*, px, size,
};

use self::element::{Paint, TextArea};
use self::layout::{Rows, column_window, display_width, row_count, wrap_points};
use self::search::{Match, Matcher, Query, find_all};
use crate::actions::{Copy, CopyOrInterrupt};
use crate::session::Session;
use crate::terminal::element::hsla;
use crate::terminal::wheel::wheel_lines;
use crate::toast::{self, Toast};
use crate::ui::controls::{ButtonKind, Tone, badge, button_with, icon_button, icon_button_tinted, segmented, tooltip};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

pub const CONTEXT: &str = "OutputView";

/// How often a live view (a running command, or all output at the bottom)
/// reads the terminal again while output keeps coming: at most this often,
/// and never more than a tenth of the time (a full scrollback takes ~12 ms
/// with the terminal locked).
const REFRESH: Duration = Duration::from_millis(250);
const SCROLLBAR: f32 = 10.;
const MIN_THUMB: f32 = 28.;
const PAD: f32 = 8.;

actions!(
    output_view,
    [
        /// Back to the terminal.
        Close,
        NextMatch,
        PreviousMatch,
        FocusSearch,
        ToggleCaseSensitive,
        ToggleRegex,
        ToggleOnlyMatching,
        ToggleWrap,
        PreviousCommand,
        NextCommand,
        ScrollUp,
        ScrollDown,
        ScrollLeft,
        ScrollRight,
        PageUp,
        PageDown,
        ScrollToTop,
        ScrollToBottom,
        SelectAll,
        /// The next control (Tab): toolbar, search field, text.
        FocusNextControl,
        FocusPreviousControl,
    ]
);

pub fn bindings() -> Vec<KeyBinding> {
    let c = Some(CONTEXT);
    vec![
        KeyBinding::new("escape", Close, c),
        KeyBinding::new("enter", NextMatch, c),
        KeyBinding::new("f3", NextMatch, c),
        KeyBinding::new("ctrl-g", NextMatch, c),
        KeyBinding::new("shift-enter", PreviousMatch, c),
        KeyBinding::new("shift-f3", PreviousMatch, c),
        KeyBinding::new("ctrl-shift-g", PreviousMatch, c),
        KeyBinding::new("ctrl-f", FocusSearch, c),
        KeyBinding::new("alt-c", ToggleCaseSensitive, c),
        KeyBinding::new("alt-r", ToggleRegex, c),
        KeyBinding::new("alt-l", ToggleOnlyMatching, c),
        KeyBinding::new("alt-z", ToggleWrap, c),
        KeyBinding::new("ctrl-up", PreviousCommand, c),
        KeyBinding::new("ctrl-down", NextCommand, c),
        KeyBinding::new("up", ScrollUp, c),
        KeyBinding::new("down", ScrollDown, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("ctrl-home", ScrollToTop, c),
        KeyBinding::new("ctrl-end", ScrollToBottom, c),
        // Only reach the view when the search field doesn't take them.
        KeyBinding::new("left", ScrollLeft, c),
        KeyBinding::new("right", ScrollRight, c),
        KeyBinding::new("home", ScrollToTop, c),
        KeyBinding::new("end", ScrollToBottom, c),
        KeyBinding::new("ctrl-a", SelectAll, c),
        KeyBinding::new("tab", FocusNextControl, c),
        KeyBinding::new("shift-tab", FocusPreviousControl, c),
    ]
}

/// The viewer's tab stops in order. The terminal window has no shared focus
/// pool (that's the settings window's, `ui::focus`), so the viewer keeps its
/// own handles and moves between them itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The Command / All output switch: one stop, Left/Right pick.
    Source,
    PrevCommand,
    NextCommand,
    Wrap,
    Refresh,
    Close,
    /// The search field (its own handle).
    Search,
    PrevMatch,
    NextMatch,
    Case,
    Regex,
    OnlyMatching,
    Copy,
    /// The text (the view's own handle).
    Text,
}

const STOPS: [Stop; 14] = [
    Stop::Source,
    Stop::PrevCommand,
    Stop::NextCommand,
    Stop::Wrap,
    Stop::Refresh,
    Stop::Close,
    Stop::Search,
    Stop::PrevMatch,
    Stop::NextMatch,
    Stop::Case,
    Stop::Regex,
    Stop::OnlyMatching,
    Stop::Copy,
    Stop::Text,
];

pub enum OutputEvent {
    Close,
}

/// Which lines the viewer shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The running command, else the newest one that ran; follows new
    /// commands while the view is open.
    Last,
    /// One command, by its index in the transcript.
    Command(usize),
    All,
}

/// A place in the text: a transcript line and a byte in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    line: usize,
    byte: usize,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor: Pos,
    head: Pos,
}

impl Selection {
    fn range(&self) -> (Pos, Pos) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    Select,
    /// Dragging a scrollbar's thumb, grabbed this far from its start.
    VThumb(Pixels),
    HThumb(Pixels),
}

/// Where to scroll once the layout is known.
#[derive(Debug, Clone, Copy)]
enum Reveal {
    Top,
    /// Put a line's first row at the top (keeps the place when wrapping changes).
    Line(usize),
    /// Center a place (a match) and bring it into view sideways.
    Pos(Pos),
}

/// A row as painted, for mapping the mouse to text.
struct PaintedRow {
    top: Pixels,
    line: usize,
    seg: Range<usize>,
    origin_x: Pixels,
    shaped: ShapedLine,
}

/// The last frame's geometry.
#[derive(Default)]
struct Geometry {
    text: Bounds<Pixels>,
    cell: gpui::Size<Pixels>,
    cols: usize,
    visible_rows: f32,
    vbar: Option<Bar>,
    hbar: Option<Bar>,
    rows: Vec<PaintedRow>,
}

#[derive(Clone, Copy)]
struct Bar {
    track: Bounds<Pixels>,
    thumb: Bounds<Pixels>,
}

pub struct OutputView {
    session: Arc<Session>,
    transcript: Arc<Transcript>,
    source: Source,
    /// The source's lines (transcript indices).
    range: Range<usize>,
    /// The lines shown: all of `range`, or the ones that match.
    shown: Vec<usize>,
    query: Query,
    matcher: Option<Matcher>,
    query_error: Option<String>,
    matches: Vec<Match>,
    /// More matches than [`search::MAX_MATCHES`].
    capped: bool,
    current: Option<usize>,
    only_matching: bool,
    wrap: bool,
    // Layout.
    rows: Rows,
    /// What `rows` was computed for: columns (0 unwrapped) and `version`.
    rows_key: Option<(usize, u64)>,
    /// Bumped whenever `shown` changes.
    version: u64,
    /// The widest shown line, in columns.
    max_width: usize,
    /// The first visible row (fractional while scrolling by pixels).
    top: f32,
    scroll_x: Pixels,
    /// Stick to the bottom as lines arrive.
    follow: bool,
    reveal: Option<Reveal>,
    geom: Geometry,
    selection: Option<Selection>,
    drag: Option<Drag>,
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// One handle per [`Stop`] (the search field's and the text's unused).
    stops: Vec<FocusHandle>,
    /// The text got focus from Tab (it shows a ring), not from a click.
    text_focus_visible: bool,
    font: Font,
    font_size: Pixels,
    line_height: f32,
    last_refresh: Instant,
    /// How long reading the terminal took last time.
    refresh_cost: Duration,
    refresh_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<OutputEvent> for OutputView {}

impl Focusable for OutputView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl OutputView {
    pub fn new(
        session: Arc<Session>,
        source: Source,
        font: Font,
        font_size: Pixels,
        line_height: f32,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("Search the output…", "", cx).label("Search the output").bare());
        let typed = cx.subscribe_in(&input, window, |this, _, ev: &TextInputEvent, window, cx| match ev {
            TextInputEvent::Changed(q) => {
                this.query.text = q.clone();
                this.search_changed(cx);
            }
            TextInputEvent::Submitted(_) => this.step_match(1, window, cx),
        });
        input.focus_handle(cx).focus(window, cx);
        let transcript = Arc::new(session.terminal.transcript());
        let follow = follows(source, &transcript);
        let mut this = Self {
            session,
            transcript,
            source,
            range: 0..0,
            shown: Vec::new(),
            query: Query::default(),
            matcher: None,
            query_error: None,
            matches: Vec::new(),
            capped: false,
            current: None,
            only_matching: false,
            wrap: false,
            rows: Rows::default(),
            rows_key: None,
            version: 0,
            max_width: 0,
            top: 0.,
            scroll_x: px(0.),
            follow,
            reveal: (!follow).then_some(Reveal::Top),
            geom: Geometry::default(),
            selection: None,
            drag: None,
            input,
            focus: cx.focus_handle(),
            stops: STOPS.iter().map(|_| cx.focus_handle()).collect(),
            text_focus_visible: false,
            font,
            font_size,
            line_height,
            last_refresh: Instant::now(),
            refresh_cost: Duration::ZERO,
            refresh_task: None,
            _subscriptions: vec![typed],
        };
        this.rebuild();
        this
    }

    /// Follow the pane's font, size and line height.
    pub fn set_font(&mut self, font: Font, font_size: Pixels, line_height: f32, cx: &mut Context<'_, Self>) {
        if (&self.font, self.font_size, self.line_height) != (&font, font_size, line_height) {
            (self.font, self.font_size, self.line_height) = (font, font_size, line_height);
            self.rows_key = None;
            cx.notify();
        }
    }

    /// Show another source (the toolbar, or the terminal's shortcuts while
    /// the viewer is open).
    pub fn show(&mut self, source: Source, cx: &mut Context<'_, Self>) {
        // A command's index is into the transcript it was picked from.
        if !matches!(source, Source::Command(_)) {
            self.transcript = Arc::new(self.session.terminal.transcript());
        }
        self.source = source;
        self.selection = None;
        // A refresh scheduled for the old source would read a new transcript under a pinned index.
        self.refresh_task = None;
        self.follow = follows(source, &self.transcript);
        self.reveal = (!self.follow).then_some(Reveal::Top);
        self.rebuild();
        cx.notify();
    }

    pub fn source(&self) -> Source {
        self.source
    }

    /// Put the cursor in the search field with its text selected.
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.input.focus_handle(cx).focus(window, cx);
        self.input.update(cx, |i, cx| i.select_all_text(cx));
    }

    // ---- data -----------------------------------------------------------------

    /// The pane got output. A live view reads the terminal again, at most
    /// every [`REFRESH`].
    pub fn output_arrived(&mut self, cx: &mut Context<'_, Self>) {
        if !self.live() || self.refresh_task.is_some() {
            return;
        }
        let wait = REFRESH.max(self.refresh_cost * 10).saturating_sub(self.last_refresh.elapsed());
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |view, cx| {
                view.refresh_task = None;
                view.refresh(cx);
            });
        }));
    }

    /// Whether new output changes what's shown: the last command (output
    /// can also be a short command that started and finished between two
    /// wakes, so "running" isn't enough), or all output while scrolled to
    /// the bottom. A pinned command never changes.
    fn live(&self) -> bool {
        match self.source {
            Source::Last => true,
            Source::All => self.follow,
            Source::Command(_) => false,
        }
    }

    /// Read the terminal again, keeping the place and the current match. A
    /// pinned command keeps the transcript its index is into: once the
    /// scrollback is full, old commands drop out and indices shift.
    fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        self.last_refresh = Instant::now();
        let before = self.last_identity();
        if !matches!(self.source, Source::Command(_)) {
            self.transcript = Arc::new(self.session.terminal.transcript());
        }
        self.refresh_cost = self.last_refresh.elapsed();
        let current = self.current.and_then(|i| self.matches.get(i).cloned());
        self.rebuild();
        // "Last" moved on to another command (one started meanwhile): treat
        // it like opening it, so a running one follows. A pause within the
        // same command stays.
        if self.source == Source::Last
            && let Some(follow) = last_moved(before, &self.transcript)
        {
            self.follow = follow;
            self.reveal = (!follow).then_some(Reveal::Top);
            self.selection = None;
        }
        if let Some(m) = current {
            self.current = self.matches.iter().position(|x| *x == m).or(self.current.filter(|&i| i < self.matches.len()));
        }
        cx.notify();
    }

    /// Which command "Last" shows, by how many commands had started: the
    /// index shifts when the scrollback drops old commands, and two runs of
    /// the same command line look alike.
    fn last_identity(&self) -> Option<u64> {
        self.transcript.last_command_id()
    }

    /// The source's block, for a command source.
    fn block(&self) -> Option<usize> {
        match self.source {
            Source::Last => self.transcript.last_command(),
            Source::Command(i) => (i < self.transcript.commands.len()).then_some(i),
            Source::All => None,
        }
    }

    /// Recompute the lines, matches and what's shown from the transcript.
    fn rebuild(&mut self) {
        let t = &self.transcript;
        self.range = match self.source {
            Source::All => 0..t.lines.len(),
            _ => self.block().map(|b| t.commands[b].output.clone()).unwrap_or(0..0),
        };
        self.capped = match &self.matcher {
            Some(m) => find_all(m, self.range.clone().map(|i| (i, t.lines[i].text.as_str())), &mut self.matches),
            None => {
                self.matches.clear();
                false
            }
        };
        self.shown = match (&self.matcher, self.only_matching) {
            (Some(m), true) => self.range.clone().filter(|&i| m.is_match(&t.lines[i].text)).collect(),
            _ => self.range.clone().collect(),
        };
        self.max_width = self.shown.iter().map(|&i| display_width(&t.lines[i].text)).max().unwrap_or(0);
        self.version += 1;
        // A refresh can rewrite a line in place (a running command's last
        // line): drop a selection whose ends aren't on that text any more.
        let valid = |p: Pos| self.range.contains(&p.line) && t.lines[p.line].text.is_char_boundary(p.byte);
        if let Some(sel) = self.selection
            && !(valid(sel.anchor) && valid(sel.head))
        {
            self.selection = None;
        }
        self.current = match self.current {
            _ if self.matches.is_empty() => None,
            Some(i) => Some(i.min(self.matches.len() - 1)),
            None => None,
        };
    }

    fn search_changed(&mut self, cx: &mut Context<'_, Self>) {
        match Matcher::new(&self.query) {
            Ok(m) => {
                self.matcher = m;
                self.query_error = None;
            }
            Err(e) => {
                self.matcher = None;
                self.query_error = Some(e);
            }
        }
        // Where the view is: where it's about to scroll (typing outruns
        // frames), else the top line as last painted.
        let first_visible = match self.reveal {
            Some(Reveal::Pos(p)) => Some(p.line),
            Some(Reveal::Line(line)) => Some(line),
            Some(Reveal::Top) => self.shown.first().copied(),
            None => self.rows.locate(self.top.max(0.) as usize).and_then(|(i, _)| self.shown.get(i).copied()),
        };
        let current = self.current.and_then(|i| self.matches.get(i)).map(|m| (m.line, m.range.start));
        let place = current.map(|(line, _)| line).or(first_visible);
        self.current = None;
        self.rebuild();
        if self.matches.is_empty() {
            // Back to every line: stay where the last match was.
            self.reveal = match place {
                Some(line) if !self.follow && self.shown.contains(&line) => Some(Reveal::Pos(Pos { line, byte: 0 })),
                _ => self.reveal,
            };
        } else {
            // Typing on stays on the current match while it still matches.
            // At the end (all output, a running command): the newest match.
            // Else the first one at or after what's on screen, or the last
            // before it: typing finds what you're looking at first.
            let i = match (current, first_visible) {
                (Some(at), _) => self.matches.partition_point(|m| (m.line, m.range.start) < at),
                _ if self.follow => self.matches.len() - 1,
                (None, Some(line)) => self.matches.partition_point(|m| m.line < line),
                (None, None) => 0,
            };
            let i = if i < self.matches.len() { i } else { self.matches.len() - 1 };
            self.current = Some(i);
            self.reveal_match(i);
        }
        cx.notify();
    }

    fn reveal_match(&mut self, i: usize) {
        if let Some(m) = self.matches.get(i) {
            self.follow = false;
            self.reveal = Some(Reveal::Pos(Pos { line: m.line, byte: m.range.start }));
        }
    }

    fn step_match(&mut self, step: isize, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        let i = match self.current {
            Some(i) => (i as isize + step).rem_euclid(n as isize) as usize,
            None if step > 0 => 0,
            None => n - 1,
        };
        self.current = Some(i);
        self.reveal_match(i);
        cx.notify();
    }

    /// Enter / Shift+Enter: the next or previous match, except on a focused
    /// toolbar control, where the key goes on to press it (GPUI clicks a
    /// focused element with `on_click` when no action took the key).
    fn match_key(&mut self, step: isize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let on_control =
            STOPS.iter().filter(|s| !matches!(s, Stop::Search | Stop::Text)).any(|&s| self.stops[s as usize].is_focused(window));
        if on_control {
            cx.propagate();
        } else {
            self.step_match(step, window, cx);
        }
    }

    /// Wrap long lines or not, keeping the current match or the top line in view.
    fn toggle_wrap(&mut self, cx: &mut Context<'_, Self>) {
        let first_visible = self.rows.locate(self.top as usize).and_then(|(i, _)| self.shown.get(i).copied());
        self.wrap = !self.wrap;
        self.reveal = match self.current.and_then(|i| self.matches.get(i)) {
            Some(m) => Some(Reveal::Pos(Pos { line: m.line, byte: m.range.start })),
            None if self.follow => None,
            None => first_visible.map(Reveal::Line),
        };
        cx.notify();
    }

    fn toggle(&mut self, what: fn(&mut Self) -> &mut bool, cx: &mut Context<'_, Self>) {
        let flag = what(self);
        *flag = !*flag;
        self.search_changed(cx);
    }

    /// Step to the previous (-1) or next (+1) command.
    fn step_command(&mut self, step: isize, cx: &mut Context<'_, Self>) {
        let t = &self.transcript;
        let last = t.last_command();
        let current = match self.source {
            Source::All => None,
            _ => self.block(),
        };
        let target = match (current, step) {
            (None, s) if s < 0 => last,
            (None, _) => return,
            (Some(b), s) if s < 0 => (0..b).rev().find(|&i| !t.commands[i].prompt.is_empty() || i == 0),
            (Some(b), _) => (b + 1..=last.unwrap_or(0)).next(),
        };
        let Some(target) = target else { return };
        self.show(if Some(target) == last { Source::Last } else { Source::Command(target) }, cx);
    }

    // ---- copying ----------------------------------------------------------------

    fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let (a, b) = sel.range();
        if a == b {
            return None;
        }
        let lines = &self.transcript.lines;
        let mut out = String::new();
        let mut first = true;
        for &i in self.shown.iter().filter(|&&i| i >= a.line && i <= b.line) {
            let text = &lines[i].text;
            let from = if i == a.line { a.byte.min(text.len()) } else { 0 };
            let to = if i == b.line { b.byte.min(text.len()) } else { text.len() };
            if !first {
                out.push('\n');
            }
            first = false;
            out.push_str(&text[from..to.max(from)]);
        }
        Some(out)
    }

    /// The selection, or every line shown (all of them, or the matching
    /// ones), to the clipboard.
    fn copy(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let (text, what) = match self.selected_text() {
            Some(text) => {
                let chars = text.chars().count();
                (text, if chars == 1 { "1 character".to_string() } else { format!("{} characters", group(chars)) })
            }
            None => {
                let lines = &self.transcript.lines;
                let text = self.shown.iter().map(|&i| lines[i].text.as_str()).collect::<Vec<_>>().join("\n");
                let n = self.shown.len();
                let which = if self.only_matching && self.matcher.is_some() { "matching " } else { "" };
                (text, if n == 1 { format!("1 {which}line") } else { format!("{} {which}lines", group(n)) })
            }
        };
        let toast = if text.is_empty() {
            Toast::info("Nothing to copy")
        } else {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            Toast::success("Copied to clipboard").body(what)
        };
        toast::show_in(window, cx, toast.key("output.copy"));
    }

    // ---- scrolling ----------------------------------------------------------------

    fn max_top(&self) -> f32 {
        (self.rows.total() as f32 - self.geom.visible_rows.floor()).max(0.)
    }

    fn scroll_rows(&mut self, delta: f32, cx: &mut Context<'_, Self>) {
        self.top = (self.top + delta).clamp(0., self.max_top());
        self.set_follow(self.top >= self.max_top(), cx);
        self.reveal = None;
        cx.notify();
    }

    /// Back at the end of all output: read what arrived while scrolled up,
    /// which wasn't read then.
    fn set_follow(&mut self, follow: bool, cx: &mut Context<'_, Self>) {
        let resumed = follow && !self.follow;
        self.follow = follow;
        if resumed && self.source == Source::All {
            self.refresh(cx);
        }
    }

    fn scroll_cols(&mut self, delta: Pixels, cx: &mut Context<'_, Self>) {
        if self.wrap {
            return;
        }
        self.scroll_x = (self.scroll_x + delta).clamp(px(0.), self.max_scroll_x());
        cx.notify();
    }

    fn max_scroll_x(&self) -> Pixels {
        let content = self.geom.cell.width * (self.max_width + 1) as f32;
        (content - self.geom.text.size.width).max(px(0.))
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let lh = self.geom.cell.height.max(px(1.));
        let (dx, dy) = match ev.delta {
            ScrollDelta::Lines(p) => {
                let p = wheel_lines(p, cx.global::<crate::settings::AppSettings>().client.mouse.scroll_lines);
                (self.geom.cell.width * p.x, p.y)
            }
            ScrollDelta::Pixels(p) => (p.x, p.y / lh),
        };
        // Shift turns the wheel sideways.
        let (dx, dy) = if ev.modifiers.shift && dx == px(0.) { (lh * dy, 0.) } else { (dx, dy) };
        if dy != 0. {
            self.scroll_rows(-dy, cx);
        }
        if dx != px(0.) {
            self.scroll_cols(-dx, cx);
        }
    }

    // ---- mouse ----------------------------------------------------------------

    /// The text position under a point (clamped to the painted rows).
    fn pos_at(&self, p: Point<Pixels>) -> Option<Pos> {
        let rows = &self.geom.rows;
        let lh = self.geom.cell.height;
        let row = rows.iter().find(|r| p.y < r.top + lh).or(rows.last())?;
        let row = if p.y < rows.first()?.top { rows.first()? } else { row };
        let x = p.x - row.origin_x;
        let byte = if x <= px(0.) { row.seg.start } else { row.seg.start + row.shaped.closest_index_for_x(x) };
        Some(Pos { line: row.line, byte: byte.min(row.seg.end) })
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.focus.focus(window, cx);
        self.text_focus_visible = false;
        for (bar, vertical) in [(self.geom.vbar, true), (self.geom.hbar, false)] {
            let Some(bar) = bar else { continue };
            if !bar.track.contains(&ev.position) {
                continue;
            }
            let grab = if bar.thumb.contains(&ev.position) {
                if vertical { ev.position.y - bar.thumb.origin.y } else { ev.position.x - bar.thumb.origin.x }
            } else if vertical {
                bar.thumb.size.height / 2.
            } else {
                bar.thumb.size.width / 2.
            };
            self.drag = Some(if vertical { Drag::VThumb(grab) } else { Drag::HThumb(grab) });
            self.drag_to(ev.position, cx);
            return;
        }
        let Some(pos) = self.pos_at(ev.position) else { return };
        self.selection = Some(match ev.click_count {
            1 if ev.modifiers.shift => {
                let anchor = self.selection.map_or(pos, |s| s.anchor);
                Selection { anchor, head: pos }
            }
            1 => Selection { anchor: pos, head: pos },
            2 => {
                let text = &self.transcript.lines[pos.line].text;
                let (a, b) = word_at(text, pos.byte);
                Selection { anchor: Pos { line: pos.line, byte: a }, head: Pos { line: pos.line, byte: b } }
            }
            _ => {
                let len = self.transcript.lines[pos.line].text.len();
                Selection { anchor: Pos { line: pos.line, byte: 0 }, head: Pos { line: pos.line, byte: len } }
            }
        });
        self.drag = (ev.click_count == 1).then_some(Drag::Select);
        cx.notify();
    }

    fn drag_to(&mut self, p: Point<Pixels>, cx: &mut Context<'_, Self>) {
        match self.drag {
            Some(Drag::Select) => {
                let text = self.geom.text;
                // Past the top or bottom edge: scroll along.
                if p.y < text.origin.y {
                    self.scroll_rows(-1., cx);
                } else if p.y > text.origin.y + text.size.height {
                    self.scroll_rows(1., cx);
                }
                if let (Some(pos), Some(sel)) = (self.pos_at(p), self.selection.as_mut()) {
                    sel.head = pos;
                }
            }
            Some(Drag::VThumb(grab)) => {
                let Some(bar) = self.geom.vbar else { return };
                let room = bar.track.size.height - bar.thumb.size.height;
                if room > px(0.) {
                    let t = ((p.y - grab - bar.track.origin.y) / room).clamp(0., 1.);
                    self.top = t * self.max_top();
                    self.set_follow(t >= 1., cx);
                    self.reveal = None;
                }
            }
            Some(Drag::HThumb(grab)) => {
                let Some(bar) = self.geom.hbar else { return };
                let room = bar.track.size.width - bar.thumb.size.width;
                if room > px(0.) {
                    let t = ((p.x - grab - bar.track.origin.x) / room).clamp(0., 1.);
                    self.scroll_x = self.max_scroll_x() * t;
                }
            }
            None => return,
        }
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<'_, Self>) {
        if self.drag.take() == Some(Drag::Select)
            && let Some(sel) = self.selection
            && sel.anchor == sel.head
        {
            self.selection = None;
        }
        cx.notify();
    }

    // ---- keys ----------------------------------------------------------------

    /// Typing in the text area goes to the search field.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.focus.is_focused(window) {
            return;
        }
        let k = &ev.keystroke;
        if (k.modifiers.control || k.modifiers.alt || k.modifiers.platform) && !crate::terminal::keys::is_windows_altgr(k) {
            return;
        }
        let Some(ch) = k.key_char.as_ref().filter(|c| !c.chars().any(char::is_control)) else { return };
        let text = format!("{}{ch}", self.input.read(cx).text());
        // `set_text` doesn't emit `Changed`: search here.
        self.input.update(cx, |i, cx| i.set_text(text.clone(), cx));
        self.input.focus_handle(cx).focus(window, cx);
        self.query.text = text;
        self.search_changed(cx);
        cx.stop_propagation();
    }

    // ---- layout (called by the element) -----------------------------------------

    fn run_font(&self, flags: CellFlags) -> Font {
        let mut f = self.font.clone();
        if flags.contains(CellFlags::BOLD) {
            f.weight = FontWeight::BOLD;
        }
        if flags.contains(CellFlags::ITALIC) {
            f.style = FontStyle::Italic;
        }
        f
    }

    /// Lay out the visible rows in `bounds` and say what to paint.
    fn prepaint_text(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<'_, Self>) -> Paint {
        let ui = Ui::get(cx);
        let palette = self.session.terminal.palette();
        let ts = window.text_system().clone();
        let font_id = ts.resolve_font(&self.font);
        let cell_w = ts.advance(font_id, self.font_size, 'm').map(|s| s.width).unwrap_or(self.font_size * 0.6);
        let lh = (self.font_size * self.line_height).round();
        let fg = hsla(palette.foreground);

        // Gutter with line numbers (counted from the source's first line).
        let digits = self.range.len().max(1).to_string().len();
        let gutter = cell_w * (digits as f32 + 2.);
        let bar = px(SCROLLBAR);
        let mut text = Bounds::new(
            point(bounds.origin.x + gutter, bounds.origin.y + px(PAD / 2.)),
            size(bounds.size.width - gutter - bar - px(PAD), bounds.size.height - px(PAD / 2.)),
        );
        let cols = ((text.size.width / cell_w).floor() as usize).max(8);
        let wide = !self.wrap && cell_w * self.max_width as f32 > text.size.width;
        if wide {
            text.size.height -= bar;
        }
        self.geom.text = text;
        self.geom.cell = size(cell_w, lh);
        self.geom.cols = cols;
        self.geom.visible_rows = (text.size.height / lh).max(1.);

        let key = (if self.wrap { cols } else { 0 }, self.version);
        if self.rows_key != Some(key) {
            let lines = &self.transcript.lines;
            self.rows = if self.wrap {
                Rows::from_counts(self.shown.iter().map(|&i| row_count(&lines[i].text, cols)))
            } else {
                Rows::unwrapped(self.shown.len())
            };
            self.rows_key = Some(key);
        }

        let max_top = self.max_top();
        match self.reveal.take() {
            Some(Reveal::Top) => self.top = 0.,
            Some(Reveal::Line(line)) => {
                if let Some(si) = self.shown.iter().position(|&i| i == line) {
                    self.top = self.rows.first_row(si) as f32;
                }
            }
            Some(Reveal::Pos(pos)) => self.reveal_pos(pos, cols),
            None if self.follow => self.top = max_top,
            None => {}
        }
        self.top = self.top.clamp(0., max_top);
        self.scroll_x = if self.wrap { px(0.) } else { self.scroll_x.clamp(px(0.), self.max_scroll_x()) };

        let mut paint = Paint {
            background: Some((bounds, hsla(palette.background))),
            text_clip: Bounds::new(point(text.origin.x - px(2.), text.origin.y), size(text.size.width + px(4.), text.size.height)),
            gutter_clip: Bounds::new(point(bounds.origin.x, text.origin.y), size(gutter, text.size.height)),
            line_height: lh,
            ..Paint::default()
        };
        self.geom.rows.clear();
        let first = self.top.floor() as usize;
        let mut y = text.origin.y - lh * self.top.fract();
        let first_col = if self.wrap { 0 } else { (self.scroll_x / cell_w).floor() as usize };
        let sel = self.selection.map(|s| s.range());
        // Solid highlights with the text in the background color, like `less`.
        let (match_bg, current_bg, match_fg) = (ui.warning, ui.accent, hsla(palette.background));
        let band = fg.opacity(0.07);
        let number_color = fg.opacity(0.35);
        let mut points: Option<(usize, Vec<usize>)> = None;
        let mut row = first;
        while y < text.origin.y + text.size.height {
            let Some((si, sub)) = self.rows.locate(row) else { break };
            let li = self.shown[si];
            let line = &self.transcript.lines[li];
            let (seg, seg_col) = if self.wrap {
                if points.as_ref().is_none_or(|(l, _)| *l != li) {
                    points = Some((li, wrap_points(&line.text, cols)));
                }
                let p = &points.as_ref().expect("set above").1;
                (p[sub]..p.get(sub + 1).copied().unwrap_or(line.text.len()), 0)
            } else {
                column_window(&line.text, first_col, first_col + cols + 2)
            };
            let origin_x = text.origin.x + cell_w * seg_col as f32 - self.scroll_x;
            let row_bounds = Bounds::new(point(text.origin.x - px(2.), y), size(text.size.width + px(4.), lh));
            if line.prompt {
                paint.rects.push((row_bounds, band));
            }

            let slice = &line.text[seg.clone()];
            let from = self.matches.partition_point(|m| m.line < li);
            let line_matches = &self.matches[from..self.matches[from..].partition_point(|m| m.line == li) + from];
            let runs = self.runs(line, &seg, fg, line_matches, match_fg);
            let shaped = ts.shape_line(SharedString::from(slice.to_string()), self.font_size, &runs, None);
            let x_of = |byte: usize| origin_x + shaped.x_for_index(byte.clamp(seg.start, seg.end) - seg.start);
            let line_len = line.text.len();
            let rect = |from: usize, to: usize, color: Hsla, paint: &mut Paint| {
                let (a, b) = (from.max(seg.start), to.min(seg.end));
                // A selection past the end of the line covers its newline.
                let newline = to > line_len && seg.end == line_len;
                if a > b || (a == b && !newline) {
                    return;
                }
                let xb = if newline { x_of(b) + cell_w * 0.5 } else { x_of(b) };
                let xa = x_of(a);
                paint.rects.push((Bounds::new(point(xa, y), size(xb - xa, lh)), color));
            };
            // Colored backgrounds from the terminal.
            for (k, span) in line.spans.iter().enumerate() {
                if let Some(bg) = span.bg {
                    let end = line.spans.get(k + 1).map_or(line.text.len(), |s| s.start);
                    rect(span.start, end, hsla(bg), &mut paint);
                }
            }
            // Matches on this line.
            for (k, m) in line_matches.iter().enumerate() {
                let color = if self.current == Some(from + k) { current_bg } else { match_bg };
                rect(m.range.start, m.range.end, color, &mut paint);
            }
            // The selection (past the end of a line it covers the newline).
            if let Some((a, b)) = sel
                && li >= a.line
                && li <= b.line
            {
                let from = if li == a.line { a.byte } else { 0 };
                let to = if li == b.line { b.byte } else { usize::MAX };
                rect(from, to, hsla(palette.selection_bg).opacity(0.9), &mut paint);
            }
            if sub == 0 {
                let n = SharedString::from(format!("{:>digits$}", li - self.range.start + 1));
                let run = TextRun {
                    len: n.len(),
                    font: self.font.clone(),
                    color: number_color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = ts.shape_line(n, self.font_size, &[run], None);
                paint.numbers.push((point(bounds.origin.x + cell_w, y), shaped));
            }
            paint.lines.push((point(origin_x, y), shaped.clone()));
            self.geom.rows.push(PaintedRow { top: y, line: li, seg, origin_x, shaped });
            y += lh;
            row += 1;
        }

        // Scrollbars: a quiet track, a thumb that darkens while dragged.
        let thumb_color = |dragging: bool| if dragging { ui.muted } else { ui.faint.opacity(0.7) };
        self.geom.vbar = None;
        self.geom.hbar = None;
        let total = self.rows.total() as f32;
        let visible = self.geom.visible_rows.floor();
        if total > visible {
            let track = Bounds::new(point(bounds.origin.x + bounds.size.width - bar - px(2.), text.origin.y), size(bar, text.size.height));
            let h = (track.size.height * (visible / total)).max(px(MIN_THUMB)).min(track.size.height);
            let t = if max_top > 0. { self.top / max_top } else { 0. };
            let thumb = Bounds::new(point(track.origin.x + px(2.), track.origin.y + (track.size.height - h) * t), size(bar - px(4.), h));
            paint.bars.push((thumb, thumb_color(matches!(self.drag, Some(Drag::VThumb(_)))), px(4.)));
            self.geom.vbar = Some(Bar { track, thumb });
        }
        if wide {
            let track = Bounds::new(point(text.origin.x, text.origin.y + text.size.height), size(text.size.width, bar));
            let content = cell_w * (self.max_width + 1) as f32;
            let w = (track.size.width * (text.size.width / content)).max(px(MIN_THUMB)).min(track.size.width);
            let max_x = self.max_scroll_x();
            let t = if max_x > px(0.) { self.scroll_x / max_x } else { 0. };
            let thumb = Bounds::new(point(track.origin.x + (track.size.width - w) * t, track.origin.y + px(2.)), size(w, bar - px(4.)));
            paint.bars.push((thumb, thumb_color(matches!(self.drag, Some(Drag::HThumb(_)))), px(4.)));
            self.geom.hbar = Some(Bar { track, thumb });
        }
        paint
    }

    /// Scroll so `pos` is in view: centered vertically if it isn't on
    /// screen, and sideways when unwrapped.
    fn reveal_pos(&mut self, pos: Pos, cols: usize) {
        let Some(si) = self.shown.iter().position(|&i| i == pos.line) else { return };
        let text = &self.transcript.lines[pos.line].text;
        // Live output can rewrite the line after the position was taken.
        let mut byte = pos.byte.min(text.len());
        while !text.is_char_boundary(byte) {
            byte -= 1;
        }
        let sub = if self.wrap { wrap_points(text, cols).partition_point(|&p| p <= byte).saturating_sub(1) } else { 0 };
        let row = (self.rows.first_row(si) + sub) as f32;
        let visible = self.geom.visible_rows.floor();
        if row < self.top || row >= self.top + visible {
            self.top = row - (visible / 2.).floor();
        }
        if !self.wrap {
            let col = display_width(&text[..byte]) as f32;
            let x = self.geom.cell.width * col;
            let w = self.geom.text.size.width;
            if x < self.scroll_x || x > self.scroll_x + w - self.geom.cell.width * 4. {
                self.scroll_x = (x - w / 3.).max(px(0.));
            }
        }
    }

    /// Text runs for the bytes `seg` of a line.
    /// Text runs for the bytes `seg` of a line; text in `marked` (the
    /// line's matches) is drawn in `marked_fg`.
    fn runs(
        &self,
        line: &goatty_term::TranscriptLine,
        seg: &Range<usize>,
        default_fg: Hsla,
        marked: &[Match],
        marked_fg: Hsla,
    ) -> Vec<TextRun> {
        let mut runs = Vec::new();
        if seg.is_empty() {
            return runs;
        }
        // Cut the spans where matches start and end.
        let ends = line.spans.iter().skip(1).map(|s| s.start).chain(std::iter::once(line.text.len()));
        let mut pieces = Vec::new();
        for (span, end) in line.spans.iter().zip(ends) {
            let (mut a, b) = (span.start.max(seg.start), end.min(seg.end));
            while a < b {
                let inside = marked.iter().find(|m| m.range.start <= a && a < m.range.end);
                let next = match inside {
                    Some(m) => m.range.end.min(b),
                    None => marked.iter().map(|m| m.range.start).filter(|&s| s > a).min().unwrap_or(b).min(b),
                };
                pieces.push((span, a, next, inside.is_some()));
                a = next;
            }
        }
        for (span, a, b, is_marked) in pieces {
            let mut color = if is_marked { marked_fg } else { hsla(span.fg) };
            if span.flags.contains(CellFlags::DIM) && !is_marked {
                color = color.opacity(0.6);
            }
            let underline = span.flags.contains(CellFlags::UNDERLINE).then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(color),
                wavy: span.flags.contains(CellFlags::UNDERCURL),
            });
            let strikethrough =
                span.flags.contains(CellFlags::STRIKE).then_some(StrikethroughStyle { thickness: px(1.), color: Some(color) });
            runs.push(TextRun { len: b - a, font: self.run_font(span.flags), color, background_color: None, underline, strikethrough });
        }
        let covered: usize = runs.iter().map(|r| r.len).sum();
        if covered < seg.len() {
            // Text without spans (shouldn't happen): plain.
            runs.push(TextRun {
                len: seg.len() - covered,
                font: self.font.clone(),
                color: default_fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
        runs
    }

    // ---- toolbar ----------------------------------------------------------------

    fn match_label(&self) -> Option<(SharedString, bool)> {
        if let Some(e) = &self.query_error {
            return Some((e.clone().into(), true));
        }
        self.matcher.as_ref()?;
        let n = self.matches.len();
        let total = if self.capped { format!("{}+", group(n)) } else { group(n) };
        Some(match self.current {
            _ if n == 0 => ("No matches".into(), true),
            Some(i) => (format!("{} of {total}", group(i + 1)).into(), false),
            None => (format!("{total} matches").into(), false),
        })
    }

    /// What the viewer shows, above the text: the command line, or a note.
    fn caption(&self) -> (SharedString, Option<(&'static str, Tone)>) {
        let t = &self.transcript;
        let lines = |n: usize| if n == 1 { "1 line".to_string() } else { format!("{} lines", group(n)) };
        match (self.source, self.block()) {
            (Source::All, _) if t.alt_screen => ("The screen of the full-screen program".into(), Some(("full screen", Tone::Neutral))),
            (Source::All, _) => (format!("All output · {}", lines(self.range.len())).into(), None),
            (_, Some(b)) => {
                let running = t.running && Some(b) == t.commands.len().checked_sub(1);
                let fallback = if t.prompt_evicted(b) { "Its command line left the scrollback" } else { "Output before the first prompt" };
                let cmd = t.command_line(b).filter(|c| !c.is_empty()).unwrap_or(fallback);
                let badge = running.then_some(("running", Tone::Accent));
                (format!("{cmd} · {}", lines(self.range.len())).into(), badge)
            }
            (_, None) => ("".into(), None),
        }
    }

    /// Why there's nothing to show, if so, with a way out.
    fn empty_state(&self) -> Option<(&'static str, &'static str, bool)> {
        let t = &self.transcript;
        if self.source != Source::All && self.block().is_none() {
            return Some(if t.has_marks() {
                ("No command has run yet", "Run a command, then open this again.", true)
            } else {
                (
                    "goatty can't tell where the last command's output starts",
                    "That needs goatty's shell integration (on in goatty's managed zsh). All output works anywhere.",
                    true,
                )
            });
        }
        if self.range.is_empty() {
            return Some(("The command printed nothing", "", self.source != Source::All));
        }
        if self.shown.is_empty() {
            return Some(("No lines match", "Change the search, or show every line again.", false));
        }
        None
    }

    fn has_prev_command(&self) -> bool {
        match self.source {
            Source::All => self.transcript.last_command().is_some(),
            _ => self.block().is_some_and(|b| b > 0),
        }
    }

    fn has_next_command(&self) -> bool {
        self.source != Source::All && self.block().is_some_and(|b| Some(b) < self.transcript.last_command())
    }

    fn stop_handle(&self, stop: Stop, cx: &App) -> FocusHandle {
        match stop {
            Stop::Search => self.input.focus_handle(cx),
            Stop::Text => self.focus.clone(),
            _ => self.stops[stop as usize].clone(),
        }
    }

    /// Disabled controls (no previous command, no matches) are skipped.
    fn stop_enabled(&self, stop: Stop) -> bool {
        match stop {
            Stop::PrevCommand => self.has_prev_command(),
            Stop::NextCommand => self.has_next_command(),
            Stop::PrevMatch | Stop::NextMatch => !self.matches.is_empty(),
            _ => true,
        }
    }

    /// Tab / Shift+Tab: the next or previous enabled control, around.
    fn move_focus(&mut self, step: isize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let order: Vec<Stop> = STOPS.iter().copied().filter(|&s| self.stop_enabled(s)).collect();
        let n = order.len() as isize;
        let next = match order.iter().position(|&s| self.stop_handle(s, cx).is_focused(window)) {
            Some(i) => (i as isize + step).rem_euclid(n),
            None if step > 0 => 0,
            None => n - 1,
        };
        let stop = order[next as usize];
        self.text_focus_visible = stop == Stop::Text;
        self.stop_handle(stop, cx).focus(window, cx);
        cx.notify();
    }

    /// Make `el` the tab stop `stop`, with a ring while focused from the
    /// keyboard. A border, not the settings window's spread shadow: on the
    /// toolbar's see-through buttons a shadow fills the whole button and
    /// looks like "on".
    fn stop<E: InteractiveElement + Styled>(&self, el: E, stop: Stop, ui: Ui) -> E {
        el.track_focus(&self.stops[stop as usize])
            .border_2()
            .border_color(ui.accent.opacity(0.))
            .focus_visible(move |s| s.border_color(ui.accent))
    }

    /// Left/Right on the source switch pick the other source.
    fn on_source_switch(&self, window: &Window) -> bool {
        self.stops[Stop::Source as usize].is_focused(window)
    }

    fn toolbar(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let view = cx.entity().downgrade();
        let on = |f: fn(&mut Self, &mut Window, &mut Context<'_, Self>)| {
            let view = view.clone();
            move |window: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |v, cx| f(v, window, cx));
            }
        };
        let is_command = self.source != Source::All;
        let (caption, caption_badge) = self.caption();
        let (has_prev, has_next) = (self.has_prev_command(), self.has_next_command());
        let tinted = |id: &'static str,
                      stop: Stop,
                      icon: Icon,
                      tip: &'static str,
                      active: bool,
                      f: fn(&mut Self, &mut Window, &mut Context<'_, Self>)| {
            self.stop(icon_button_tinted(id, icon, tip, active.then_some(ui.accent_text), ui, on(f)), stop, ui)
                .aria_toggled(if active { gpui::Toggled::True } else { gpui::Toggled::False })
                .when(active, |d| d.bg(ui.accent_soft))
        };
        let nav = |id: &'static str,
                   stop: Stop,
                   icon: Icon,
                   tip: &'static str,
                   enabled: bool,
                   f: fn(&mut Self, &mut Window, &mut Context<'_, Self>)| {
            self.stop(icon_button(id, icon, tip, ui, on(f)), stop, ui).when(!enabled, |d| d.opacity(0.4))
        };
        let (count, count_bad) = self.match_label().map_or((None, false), |(l, bad)| (Some(l), bad));
        let row = || div().flex().flex_wrap().items_center().gap(px(6.)).min_w_0();
        let _ = window;

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(10.))
            .py(px(8.))
            .bg(ui.surface)
            .border_b_1()
            .border_color(ui.border)
            .text_color(ui.text)
            .font_family(cx.global::<crate::settings::AppSettings>().ui_font.clone())
            .child(
                row()
                    .child(
                        self.stop(
                            div()
                                .id("output-source-switch")
                                .role(Role::RadioGroup)
                                .aria_label("Show")
                                .rounded_full()
                                .tooltip(tooltip("What to show (Left/Right to switch)")),
                            Stop::Source,
                            ui,
                        )
                        .child(segmented(
                            "output-source",
                            &[(true, "Command"), (false, "All output")],
                            &is_command,
                            ui,
                            {
                                let view = view.clone();
                                move |command, _, cx| {
                                    let _ = view.update(cx, |v, cx| v.show(if command { Source::Last } else { Source::All }, cx));
                                }
                            },
                        )),
                    )
                    .child(nav(
                        "output-prev-command",
                        Stop::PrevCommand,
                        Icon::ChevronUp,
                        "Previous command (Ctrl+Up)",
                        has_prev,
                        |v, _, cx| v.step_command(-1, cx),
                    ))
                    .when(is_command, |d| {
                        d.child(nav(
                            "output-next-command",
                            Stop::NextCommand,
                            Icon::ChevronDown,
                            "Next command (Ctrl+Down)",
                            has_next,
                            |v, _, cx| v.step_command(1, cx),
                        ))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(text::SMALL))
                            .text_color(ui.muted)
                            .font_family(self.font.family.clone())
                            .child(caption),
                    )
                    .when_some(caption_badge, |d, (label, tone)| d.child(badge(label, tone, ui)))
                    .child(tinted("output-wrap", Stop::Wrap, Icon::WrapText, "Wrap long lines (Alt+Z)", self.wrap, |v, _, cx| {
                        v.toggle_wrap(cx)
                    }))
                    .child(self.stop(
                        icon_button("output-refresh", Icon::Refresh, "Read the terminal again", ui, on(|v, _, cx| v.refresh(cx))),
                        Stop::Refresh,
                        ui,
                    ))
                    .child(self.stop(
                        icon_button(
                            "output-close",
                            Icon::Close,
                            "Back to the terminal (Esc)",
                            ui,
                            on(|_, _, cx| cx.emit(OutputEvent::Close)),
                        ),
                        Stop::Close,
                        ui,
                    )),
            )
            .child(
                row()
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(200.))
                            .items_center()
                            .gap(px(8.))
                            .h(px(32.))
                            .px(px(10.))
                            .rounded(px(radius::CONTROL))
                            .bg(ui.input_bg)
                            .border_1()
                            .border_color(if count_bad && self.query_error.is_some() { ui.danger } else { ui.border })
                            .child(Icon::Search.svg().size(px(15.)).text_color(ui.muted))
                            .child(div().flex_1().min_w_0().child(self.input.clone()))
                            .when_some(count, |d, label| {
                                d.child(
                                    div()
                                        .id("output-count")
                                        .flex_shrink_0()
                                        .role(Role::Status)
                                        .text_size(px(text::CAPTION))
                                        .text_color(if count_bad { ui.danger } else { ui.muted })
                                        .child(label),
                                )
                            }),
                    )
                    .child(nav(
                        "output-prev-match",
                        Stop::PrevMatch,
                        Icon::ChevronUp,
                        "Previous match (Shift+Enter)",
                        !self.matches.is_empty(),
                        |v, w, cx| v.step_match(-1, w, cx),
                    ))
                    .child(nav(
                        "output-next-match",
                        Stop::NextMatch,
                        Icon::ChevronDown,
                        "Next match (Enter)",
                        !self.matches.is_empty(),
                        |v, w, cx| v.step_match(1, w, cx),
                    ))
                    .child(tinted(
                        "output-case",
                        Stop::Case,
                        Icon::CaseSensitive,
                        "Match case (Alt+C)",
                        self.query.case_sensitive,
                        |v, _, cx| v.toggle(|v| &mut v.query.case_sensitive, cx),
                    ))
                    .child(tinted("output-regex", Stop::Regex, Icon::Regex, "Regular expression (Alt+R)", self.query.regex, |v, _, cx| {
                        v.toggle(|v| &mut v.query.regex, cx)
                    }))
                    .child(
                        self.stop(
                            button_with(
                                "output-only-matching",
                                Some(Icon::ListFilter),
                                "Only matching lines",
                                if self.only_matching { ButtonKind::Primary } else { ButtonKind::Ghost },
                                ui,
                                on(|v, _, cx| v.toggle(|v| &mut v.only_matching, cx)),
                            ),
                            Stop::OnlyMatching,
                            ui,
                        )
                        .aria_toggled(if self.only_matching { gpui::Toggled::True } else { gpui::Toggled::False })
                        .tooltip(tooltip("Hide the lines without a match, like grep (Alt+L)")),
                    )
                    .child(
                        self.stop(
                            button_with("output-copy", Some(Icon::Copy), "Copy", ButtonKind::Normal, ui, on(|v, w, cx| v.copy(w, cx))),
                            Stop::Copy,
                            ui,
                        )
                        .tooltip(tooltip("Copy the selection, or every line shown")),
                    ),
            )
    }
}

impl Render for OutputView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let empty = self.empty_state();
        let view = cx.entity();
        div()
            .id("output-view")
            .size_full()
            .flex()
            .flex_col()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &Close, _, cx| cx.emit(OutputEvent::Close)))
            .on_action(cx.listener(|v, _: &NextMatch, w, cx| v.match_key(1, w, cx)))
            .on_action(cx.listener(|v, _: &PreviousMatch, w, cx| v.match_key(-1, w, cx)))
            .on_action(cx.listener(|v, _: &FocusSearch, w, cx| v.focus_search(w, cx)))
            .on_action(cx.listener(|v, _: &ToggleCaseSensitive, _, cx| v.toggle(|v| &mut v.query.case_sensitive, cx)))
            .on_action(cx.listener(|v, _: &ToggleRegex, _, cx| v.toggle(|v| &mut v.query.regex, cx)))
            .on_action(cx.listener(|v, _: &ToggleOnlyMatching, _, cx| v.toggle(|v| &mut v.only_matching, cx)))
            .on_action(cx.listener(|v, _: &ToggleWrap, _, cx| v.toggle_wrap(cx)))
            .on_action(cx.listener(|v, _: &PreviousCommand, _, cx| v.step_command(-1, cx)))
            .on_action(cx.listener(|v, _: &NextCommand, _, cx| v.step_command(1, cx)))
            .on_action(cx.listener(|v, _: &ScrollUp, _, cx| v.scroll_rows(-1., cx)))
            .on_action(cx.listener(|v, _: &ScrollDown, _, cx| v.scroll_rows(1., cx)))
            .on_action(cx.listener(|v, _: &PageUp, _, cx| v.scroll_rows(-(v.geom.visible_rows.floor() - 1.).max(1.), cx)))
            .on_action(cx.listener(|v, _: &PageDown, _, cx| v.scroll_rows((v.geom.visible_rows.floor() - 1.).max(1.), cx)))
            .on_action(cx.listener(|v, _: &ScrollToTop, _, cx| v.scroll_rows(-f32::MAX / 4., cx)))
            .on_action(cx.listener(|v, _: &ScrollToBottom, _, cx| v.scroll_rows(f32::MAX / 4., cx)))
            .on_action(cx.listener(|v, _: &ScrollLeft, w, cx| {
                if v.on_source_switch(w) {
                    v.show(Source::Last, cx);
                } else {
                    v.scroll_cols(-v.geom.cell.width * 4., cx);
                }
            }))
            .on_action(cx.listener(|v, _: &ScrollRight, w, cx| {
                if v.on_source_switch(w) {
                    v.show(Source::All, cx);
                } else {
                    v.scroll_cols(v.geom.cell.width * 4., cx);
                }
            }))
            .on_action(cx.listener(|v, _: &FocusNextControl, w, cx| v.move_focus(1, w, cx)))
            .on_action(cx.listener(|v, _: &FocusPreviousControl, w, cx| v.move_focus(-1, w, cx)))
            .on_action(cx.listener(|v, _: &SelectAll, _, cx| {
                if let (Some(&a), Some(&b)) = (v.shown.first(), v.shown.last()) {
                    let len = v.transcript.lines[b].text.len();
                    v.selection = Some(Selection { anchor: Pos { line: a, byte: 0 }, head: Pos { line: b, byte: len } });
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|v, _: &Copy, w, cx| v.copy(w, cx)))
            .on_action(cx.listener(|v, _: &CopyOrInterrupt, w, cx| {
                // Ctrl+C copies a selection; there's no program to interrupt here.
                if v.selection.is_some_and(|s| s.anchor != s.head) {
                    v.copy(w, cx);
                }
            }))
            .on_key_down(cx.listener(Self::on_key))
            .child(self.toolbar(window, cx))
            .child(
                div()
                    .id("output-text")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .role(Role::Document)
                    .aria_label("Output")
                    .cursor(gpui::CursorStyle::IBeam)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(|v, _: &MouseUpEvent, _, cx| v.end_drag(cx)))
                    .on_scroll(cx.listener(Self::on_scroll))
                    .child(TextArea { view: view.clone() })
                    .when(self.text_focus_visible && self.focus.is_focused(window), |d| {
                        // Over the text, which paints its own background.
                        d.child(div().absolute().inset_0().border_2().border_color(ui.accent))
                    })
                    .when_some(empty, |d, (title, body, show_all)| {
                        d.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap(px(8.))
                                .p(px(24.))
                                .font_family(cx.global::<crate::settings::AppSettings>().ui_font.clone())
                                .child(
                                    div().text_size(px(text::HEADING)).font_weight(FontWeight::SEMIBOLD).text_color(ui.text).child(title),
                                )
                                .when(!body.is_empty(), |d| {
                                    d.child(div().max_w(px(420.)).text_center().text_size(px(text::SMALL)).text_color(ui.muted).child(body))
                                })
                                .when(show_all, |d| {
                                    d.child(button_with("output-show-all", None, "Show all output", ButtonKind::Normal, ui, {
                                        let view = view.downgrade();
                                        move |_, cx| {
                                            let _ = view.update(cx, |v, cx| v.show(Source::All, cx));
                                        }
                                    }))
                                }),
                        )
                    })
                    .when(self.drag.is_some(), |d| {
                        // Follow the drag across the whole window, like the terminal does.
                        let view = view.downgrade();
                        d.child(canvas(
                            |_, _, _| (),
                            move |_, _, window, _| {
                                let v = view.clone();
                                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                                    if phase != DispatchPhase::Bubble {
                                        return;
                                    }
                                    let _ = v.update(cx, |v, cx| {
                                        if ev.pressed_button == Some(MouseButton::Left) {
                                            v.drag_to(ev.position, cx);
                                        } else {
                                            v.end_drag(cx);
                                        }
                                    });
                                });
                                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                                    if phase == DispatchPhase::Bubble {
                                        let _ = view.update(cx, |v, cx| v.end_drag(cx));
                                    }
                                });
                            },
                        ))
                    }),
            )
    }
}

/// After reading the terminal again for "Last": `Some(follow)` when it is
/// another command now (start over as if just opened: a running one
/// follows), `None` when it is the same command, whose place (followed or
/// paused) stays. `before` is [`Transcript::last_command_id`] from before.
fn last_moved(before: Option<u64>, after: &Transcript) -> Option<bool> {
    (after.last_command_id() != before).then(|| follows(Source::Last, after))
}

/// Whether a source starts at its end and follows new lines: all output
/// and a running command do; a finished command reads from its start.
fn follows(source: Source, t: &Transcript) -> bool {
    match source {
        Source::All => true,
        Source::Last => t.running,
        Source::Command(_) => false,
    }
}

/// The word (or run of punctuation) around a byte.
fn word_at(text: &str, byte: usize) -> (usize, usize) {
    let byte = byte.min(text.len());
    let class = |c: char| {
        if c.is_alphanumeric() || "_-./:@~".contains(c) {
            1
        } else if c.is_whitespace() {
            0
        } else {
            2
        }
    };
    let at = text[byte..].chars().next().or_else(|| text[..byte].chars().next_back());
    let Some(at) = at else { return (byte, byte) };
    let k = class(at);
    let start = text[..byte].char_indices().rev().take_while(|(_, c)| class(*c) == k).last().map_or(byte, |(i, _)| i);
    let end = text[byte..].char_indices().find(|(_, c)| class(*c) != k).map_or(text.len(), |(i, _)| byte + i);
    (start, end)
}

/// 12345 → "12,345".
fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_for_double_click() {
        let t = "error at /var/log/app.log: denied";
        let w = |b| {
            let (a, z) = word_at(t, b);
            &t[a..z]
        };
        assert_eq!(w(2), "error");
        assert_eq!(w(12), "/var/log/app.log:");
        assert_eq!(w(5), " ");
        assert_eq!(w(t.len()), "denied");
        assert_eq!(word_at("", 0), (0, 0));
    }

    /// What a paused "Last" view does on refresh, with real terminals: the
    /// same command stays paused through either kind of eviction, and a new
    /// command starts over (following, since it runs).
    #[test]
    fn a_paused_last_command_stays_paused_through_evictions() {
        use goatty_term::{Palette, TermSize, Terminal};
        let new = || Terminal::new(TermSize::new(20, 3), false, std::sync::Arc::new(Palette::default())).0;
        let flood = |t: &Terminal, until: &dyn Fn(&Transcript) -> bool| {
            // Most of the scrollback at once, then line by line to the boundary.
            if t.history_size() < 100 {
                let bulk: String = (0..goatty_term::DEFAULT_SCROLLBACK - 20).map(|i| format!("bulk {i}\r\n")).collect();
                t.feed(bulk.as_bytes());
            }
            for i in 0.. {
                t.feed(format!("line {i}\r\n").as_bytes());
                if until(&t.transcript()) {
                    break;
                }
            }
        };
        let older = new();
        older.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a\r\n\x1b]133;D;0\x07");
        older.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07tail -f\r\n\x1b]133;C\x07");
        let before = older.transcript().last_command_id();
        // Only the older command leaves: the running prompt is still there.
        flood(&older, &|t| t.commands.len() == 1);
        let after = older.transcript();
        assert_eq!(after.command_line(0), Some("$ tail -f"));
        assert_eq!(last_moved(before, &after), None, "still paused");
        // Then its own prompt leaves too.
        flood(&older, &|t| t.commands[0].prompt.is_empty());
        assert_eq!(last_moved(before, &older.transcript()), None, "still paused");
        // A new command is another one: it runs, so the view follows it.
        older.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07");
        assert_eq!(last_moved(before, &older.transcript()), Some(true));
    }

    #[test]
    fn grouped_numbers() {
        assert_eq!(group(7), "7");
        assert_eq!(group(1234), "1,234");
        assert_eq!(group(1234567), "1,234,567");
    }
}
