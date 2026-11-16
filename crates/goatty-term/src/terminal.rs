//! [`Terminal`]: one VT state machine (rio-vt `Crosswords` + parser) behind
//! a cheap, thread-safe handle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;
use rio_vt::ansi::CursorShape;
use rio_vt::crosswords::grid::Scroll;
use rio_vt::crosswords::grid::row::SemanticPrompt;
use rio_vt::crosswords::pos::{Column, Line, Pos, Side};
use rio_vt::crosswords::{Crosswords, CrosswordsSize};
use rio_vt::event::WindowId;
use rio_vt::performer::handler::Processor;
use rio_vt::selection::{Selection, SelectionType};

use crate::listener::Listener;
use crate::marks::{Mark, MarkScanner, ShellState};
use crate::{Palette, TermMode, UiEvent};

/// Grid size in cells plus the pixel size (for TIOCSWINSZ / XTWINOPS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl TermSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows, cell_width: 8, cell_height: 16 }
    }
    fn to_crosswords(self) -> CrosswordsSize {
        let mut s = CrosswordsSize::new(self.cols.max(1) as usize, self.rows.max(1) as usize);
        s.square_width = self.cell_width as u32;
        s.square_height = self.cell_height as u32;
        s.width = self.cols as u32 * self.cell_width as u32;
        s.height = self.rows as u32 * self.cell_height as u32;
        s
    }
}

pub(crate) type Term = Crosswords<Listener>;

pub(crate) struct Inner {
    pub(crate) term: Mutex<Term>,
    parser: Mutex<Parser>,
    size: Arc<AtomicU32>,
    /// For the marks rio-vt doesn't report (see `marks`).
    ui: flume::Sender<UiEvent>,
    /// What the shell last said ([`ShellState`] as a byte). Changed with
    /// the terminal locked (in `feed`), so it matches the grid under the lock.
    pub(crate) shell: AtomicU8,
    /// Commands started (OSC 133;C) since this terminal was created: tells
    /// commands apart where indices shift (the scrollback drops old ones).
    pub(crate) started: AtomicU64,
    /// The shell said at its prompt that goatty's command-line widgets are
    /// bound (`OSC 7337;zle=1`); cleared when a command starts.
    pub(crate) zle: AtomicBool,
}

/// rio-vt's parser plus the scanner for the OSC 133 marks it ignores; both
/// see every byte, in order.
#[derive(Default)]
struct Parser {
    vt: Processor,
    marks: MarkScanner,
    /// The marks found in the chunk being fed (kept for its allocation).
    found: Vec<(usize, Mark)>,
}

/// How far up from where a command starts (OSC 133;C) to look for its
/// prompt's first row. A prompt plus a long multi-line command fits.
const MAX_PROMPT_ROWS: i32 = 200;

/// Shared handle to one terminal state machine. Cheap to clone.
#[derive(Clone)]
pub struct Terminal {
    pub(crate) inner: Arc<Inner>,
    /// Shared with the listener, which answers color queries from it.
    palette: SharedPalette,
}

/// A palette that can be swapped while the terminal runs (theme changes).
pub(crate) type SharedPalette = Arc<parking_lot::RwLock<Arc<Palette>>>;

/// Receivers returned by [`Terminal::new`].
pub struct TerminalChannels {
    pub replies: flume::Receiver<Vec<u8>>,
    pub ui: flume::Receiver<UiEvent>,
}

pub const DEFAULT_SCROLLBACK: usize = 10_000;

impl Terminal {
    pub fn new(size: TermSize, answer_queries: bool, palette: Arc<Palette>) -> (Self, TerminalChannels) {
        let (rtx, rrx) = flume::unbounded();
        let (utx, urx) = flume::unbounded();
        let packed = Arc::new(AtomicU32::new(pack(size)));
        let palette: SharedPalette = Arc::new(parking_lot::RwLock::new(palette));
        let listener = Listener { replies: rtx, ui: utx.clone(), answer_queries, palette: palette.clone(), size: packed.clone() };
        let term = Crosswords::new(size.to_crosswords(), CursorShape::Block, listener, WindowId::from(0), 0, DEFAULT_SCROLLBACK);
        let inner = Inner {
            term: Mutex::new(term),
            parser: Mutex::new(Parser::default()),
            size: packed,
            ui: utx,
            shell: AtomicU8::new(0),
            started: AtomicU64::new(0),
            zle: AtomicBool::new(false),
        };
        (Self { inner: Arc::new(inner), palette }, TerminalChannels { replies: rrx, ui: urx })
    }

    /// The palette colors resolve against.
    pub fn palette(&self) -> Arc<Palette> {
        self.palette.read().clone()
    }

    /// Switch palettes (theme change). Takes effect on the next frame and
    /// for the next color query (OSC 4/10/11).
    pub fn set_palette(&self, palette: Arc<Palette>) {
        *self.palette.write() = palette;
    }

    /// The cursor shape used until the application asks for another
    /// (DECSCUSR), and after it resets. Applies right away if the cursor is
    /// currently in the old default shape.
    pub fn set_default_cursor(&self, kind: crate::CursorKind) {
        let shape = match kind {
            crate::CursorKind::Block => CursorShape::Block,
            crate::CursorKind::Beam => CursorShape::Beam,
            crate::CursorKind::Underline => CursorShape::Underline,
        };
        let mut t = self.inner.term.lock();
        if t.cursor_shape == t.default_cursor_shape {
            t.cursor_shape = shape;
        }
        t.default_cursor_shape = shape;
    }

    /// Feed raw PTY output. Returns `true` if a redraw should be scheduled
    /// (false while the application holds a synchronized update, mode 2026).
    pub fn feed(&self, bytes: &[u8]) -> bool {
        let mut parser = self.inner.parser.lock();
        let parser = &mut *parser;
        parser.found.clear();
        parser.marks.scan(bytes, |end, mark| parser.found.push((end, mark)));
        let mut term = self.inner.term.lock();
        // Up to each mark, then the mark at the cursor it was printed at.
        let mut at = 0;
        for (end, mark) in parser.found.drain(..) {
            parser.vt.advance(&mut *term, &bytes[at..end]);
            at = end;
            self.apply_mark(&mut term, mark);
        }
        parser.vt.advance(&mut *term, &bytes[at..]);
        if term.synchronized_output() {
            if term.sync_update_expired(rio_vt::time::Instant::now()) {
                term.end_sync_update();
                return true;
            }
            return false;
        }
        true
    }

    /// Mark the cursor's row for a prompt mark, or note the shell's state.
    fn apply_mark(&self, t: &mut Term, mark: Mark) {
        let row = t.grid.cursor.pos.row;
        match mark {
            Mark::PromptStart => {
                t.grid[row].semantic_prompt = SemanticPrompt::Prompt;
                self.set_shell(ShellState::Prompt);
            }
            Mark::PromptContinuation => {
                if t.grid[row].semantic_prompt == SemanticPrompt::None {
                    t.grid[row].semantic_prompt = SemanticPrompt::PromptContinuation;
                }
            }
            Mark::CommandStart => {
                // The rows between the prompt's first row and here are the
                // rest of the prompt and the command line: mark them, so the
                // output is everything after them up to the next prompt.
                let top = -(t.history_size() as i32);
                let stop = (row.0 - MAX_PROMPT_ROWS).max(top);
                let prompt = (stop..row.0).rev().find(|&r| t.grid[Line(r)].semantic_prompt == SemanticPrompt::Prompt);
                if let Some(prompt) = prompt {
                    for r in prompt + 1..row.0 {
                        let line = &mut t.grid[Line(r)];
                        if line.semantic_prompt == SemanticPrompt::None {
                            line.semantic_prompt = SemanticPrompt::PromptContinuation;
                        }
                    }
                }
                self.set_shell(ShellState::Running);
                self.inner.zle.store(false, Ordering::Relaxed);
                self.inner.started.fetch_add(1, Ordering::Relaxed);
            }
            Mark::Restore(state, started) => {
                // The session's own count: after an attach both count the
                // same `C`s, so a resync changes it only for commands this
                // client missed (not for the marks the snapshot replays).
                if let Some(n) = started {
                    self.inner.started.store(n, Ordering::Relaxed);
                }
                self.set_shell(state);
            }
            Mark::CommandFinished(exit) => {
                self.set_shell(ShellState::Finished);
                let _ = self.inner.ui.send(UiEvent::CommandFinished { exit });
            }
            Mark::Zle(on) => self.inner.zle.store(on, Ordering::Relaxed),
            Mark::Line(line) => {
                let _ = self.inner.ui.send(UiEvent::CommandLine(line));
            }
        }
    }

    /// Whether the shell waits at a prompt with goatty's command-line
    /// widgets bound (the managed zsh with shell integration, #186): then
    /// [`crate::input::LineEdit`] keys work.
    pub fn line_editing(&self) -> bool {
        self.inner.zle.load(Ordering::Relaxed) && self.shell_state() != ShellState::Running
    }

    /// What the snapshots carry about the shell.
    pub(crate) fn shell_info(&self) -> crate::snapshot::ShellInfo {
        (self.shell_state(), self.commands_started(), self.inner.zle.load(Ordering::Relaxed))
    }

    /// Whether the shell reported a command that hasn't finished yet (needs
    /// shell integration).
    pub fn command_running(&self) -> bool {
        self.shell_state() == ShellState::Running
    }

    /// Read it with the terminal locked to match the grid.
    pub(crate) fn shell_state(&self) -> ShellState {
        ShellState::from_u8(self.inner.shell.load(Ordering::Relaxed))
    }

    /// Commands started so far (see `Inner::started`).
    pub(crate) fn commands_started(&self) -> u64 {
        self.inner.started.load(Ordering::Relaxed)
    }

    fn set_shell(&self, state: ShellState) {
        self.inner.shell.store(state.to_u8(), Ordering::Relaxed);
    }

    pub fn resize(&self, size: TermSize) {
        self.inner.size.store(pack(size), Ordering::Relaxed);
        self.inner.term.lock().resize(size.to_crosswords());
    }

    pub fn size(&self) -> (u16, u16) {
        let p = self.inner.size.load(Ordering::Relaxed);
        ((p >> 16) as u16, (p & 0xffff) as u16)
    }

    /// Run `f` with the terminal locked. Keep it short: the I/O thread is
    /// blocked meanwhile. Crate-private so rio-vt types never leak out.
    pub(crate) fn with_term<R>(&self, f: impl FnOnce(&mut Term) -> R) -> R {
        f(&mut self.inner.term.lock())
    }

    pub fn mode(&self) -> TermMode {
        self.inner.term.lock().mode()
    }

    /// Scrollback lines currently held (primary screen).
    pub fn history_size(&self) -> usize {
        self.inner.term.lock().history_size()
    }

    pub fn title(&self) -> String {
        self.inner.term.lock().title.clone()
    }

    pub fn scroll(&self, delta_lines: i32) {
        self.inner.term.lock().scroll_display(Scroll::Delta(delta_lines));
    }

    pub fn scroll_to_bottom(&self) {
        let mut t = self.inner.term.lock();
        if t.display_offset() != 0 {
            t.scroll_display(Scroll::Bottom);
        }
    }

    // ---- selection -------------------------------------------------------

    /// Start a selection at a *viewport* cell (row 0 = top of what's visible).
    pub fn selection_start(&self, col: usize, row: usize, kind: SelectKind, right_half: bool) {
        let mut t = self.inner.term.lock();
        let pos = viewport_to_grid(&t, col, row);
        let side = if right_half { Side::Right } else { Side::Left };
        let ty = match kind {
            SelectKind::Simple => SelectionType::Simple,
            SelectKind::Word => SelectionType::Semantic,
            SelectKind::Line => SelectionType::Lines,
        };
        t.selection = Some(Selection::new(ty, pos, side));
        t.mark_fully_damaged();
    }

    pub fn selection_update(&self, col: usize, row: usize, right_half: bool) {
        let mut t = self.inner.term.lock();
        let pos = viewport_to_grid(&t, col, row);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(sel) = t.selection.as_mut() {
            sel.update(pos, side);
        }
        t.mark_fully_damaged();
    }

    pub fn selection_clear(&self) {
        let mut t = self.inner.term.lock();
        if t.selection.take().is_some() {
            t.mark_fully_damaged();
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        let t = self.inner.term.lock();
        t.selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn has_selection(&self) -> bool {
        let t = self.inner.term.lock();
        t.selection.as_ref().is_some_and(|s| !s.is_empty())
    }
}

pub(crate) fn pack(s: TermSize) -> u32 {
    ((s.cols as u32) << 16) | s.rows as u32
}

fn viewport_to_grid(t: &Term, col: usize, row: usize) -> Pos {
    let offset = t.display_offset() as i32;
    let row = row.min(t.screen_lines().saturating_sub(1));
    let col = col.min(t.columns().saturating_sub(1));
    Pos::new(Line(row as i32 - offset), Column(col))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectKind {
    Simple,
    Word,
    Line,
}
