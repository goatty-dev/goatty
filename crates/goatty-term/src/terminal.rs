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

use crate::graphics;
use crate::listener::Listener;
use crate::marks::{Mark, MarkScanner, ShellState};
use crate::status::{self, Applied, ProgramStatus, Report};
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
    /// Lock order: `parser`, `term`, `lines`.
    pub(crate) parser: Mutex<Parser>,
    size: Arc<AtomicU32>,
    /// The cell size in pixels, packed (width << 16 | height).
    cell: Arc<AtomicU32>,
    /// For the marks rio-vt doesn't report (see `marks`).
    ui: flume::Sender<UiEvent>,
    /// The answer to the OSC 7501 probe goes here, in stream order with
    /// rio-vt's replies (only when this terminal answers queries).
    replies: flume::Sender<Vec<u8>>,
    answer_queries: bool,
    /// What programs said about themselves (OSC 7501, OSC 9;4); shared
    /// with the listener, which gets the progress reports. Lock order:
    /// after `term`.
    pub(crate) status: Arc<Mutex<status::Store>>,
    /// What the shell last said ([`ShellState`] as a byte). Changed with
    /// the terminal locked (in `feed`), so it matches the grid under the lock.
    pub(crate) shell: AtomicU8,
    /// Commands started (OSC 133;C) since this terminal was created: tells
    /// commands apart where indices shift (the scrollback drops old ones).
    pub(crate) started: AtomicU64,
    /// The shell said at its prompt that goatty's command-line widgets are
    /// bound (`OSC 7337;zle=1`); cleared when a command starts.
    pub(crate) zle: AtomicBool,
    /// How many times the shell said `zle=1`: once per prompt (local echo
    /// prediction tells a fresh prompt from the one input went to).
    pub(crate) prompts: AtomicU64,
    /// How many prompts the shell marked (`OSC 133 ; A`), any shell.
    pub(crate) marked_prompts: AtomicU64,
    /// Which host line each row shows (clients; see `history`).
    pub(crate) lines: Mutex<crate::history::LineMap>,
    /// The primary screen's kitty keyboard flags when it last switched to
    /// the alternate screen: what a snapshot taken in the alternate screen
    /// restores for it (rio-vt keeps the inactive screen's flags private).
    pub(crate) primary_kitty: AtomicU8,
    /// ConPTY / Windows win32-input-mode (`CSI ? 9001 h` / `l`), tracked
    /// alongside rio-vt because rio-vt does not implement mode 9001.
    pub(crate) win32_input: AtomicBool,
    /// The pixels of the images placed (see `graphics`); shared with the
    /// listener, which gets rio-vt's uploads. Lock order: after `term`.
    pub(crate) images: Arc<Mutex<graphics::Store>>,
    /// Image transmissions already encoded for snapshots.
    pub(crate) encoded: Mutex<graphics::EncodeCache>,
}

/// rio-vt's parser plus the scanner for the OSC 133 marks it ignores; both
/// see every byte, in order.
#[derive(Default)]
pub(crate) struct Parser {
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
        let status = Arc::new(Mutex::new(status::Store::default()));
        let images = Arc::new(Mutex::new(graphics::Store::default()));
        let cell = Arc::new(AtomicU32::new(pack_cell(size)));
        let listener = Listener {
            replies: rtx.clone(),
            ui: utx.clone(),
            answer_queries,
            palette: palette.clone(),
            size: packed.clone(),
            cell: cell.clone(),
            status: status.clone(),
            images: images.clone(),
        };
        let term = Crosswords::new(size.to_crosswords(), CursorShape::Block, listener, WindowId::from(0), 0, DEFAULT_SCROLLBACK);
        let inner = Inner {
            term: Mutex::new(term),
            parser: Mutex::new(Parser::default()),
            size: packed,
            cell,
            ui: utx,
            replies: rtx,
            answer_queries,
            status,
            shell: AtomicU8::new(0),
            started: AtomicU64::new(0),
            zle: AtomicBool::new(false),
            prompts: AtomicU64::new(0),
            marked_prompts: AtomicU64::new(0),
            lines: Mutex::new(Default::default()),
            primary_kitty: AtomicU8::new(0),
            win32_input: AtomicBool::new(false),
            images,
            encoded: Mutex::default(),
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
                self.inner.marked_prompts.fetch_add(1, Ordering::Relaxed);
                // The program that reported is gone.
                if self.inner.status.lock().prompt() {
                    let _ = self.inner.ui.send(UiEvent::Status { news: None });
                }
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
            Mark::Zle(on) => {
                self.inner.zle.store(on, Ordering::Relaxed);
                if on {
                    self.inner.prompts.fetch_add(1, Ordering::Relaxed);
                }
            }
            Mark::Line(line) => {
                let _ = self.inner.ui.send(UiEvent::CommandLine(line));
            }
            Mark::ScreenSwitch => {
                let mode = TermMode::of(t);
                if !mode.contains(TermMode::ALT_SCREEN) {
                    self.inner.primary_kitty.store(mode.kitty_flags(), Ordering::Relaxed);
                }
            }
            Mark::Win32InputMode(on) => {
                self.inner.win32_input.store(on, Ordering::Relaxed);
            }
            Mark::Status(Report::Probe) => {
                if self.inner.answer_queries {
                    let _ = self.inner.replies.send(status::PROBE_REPLY.to_vec());
                }
            }
            Mark::Status(report) => {
                let news = match self.inner.status.lock().apply(report) {
                    Applied::Unchanged => return,
                    Applied::Changed => None,
                    Applied::News(record) => Some(record),
                };
                let _ = self.inner.ui.send(UiEvent::Status { news });
            }
            Mark::Reset => {
                self.inner.win32_input.store(false, Ordering::Relaxed);
                if self.inner.status.lock().reset() {
                    let _ = self.inner.ui.send(UiEvent::Status { news: None });
                }
            }
        }
    }

    /// What the programs running here said about themselves: OSC 7501
    /// records (coding agents working, waiting for the user, done) and the
    /// last OSC 9;4 progress report. [`UiEvent::Status`] says when it changed.
    pub fn program_status(&self) -> ProgramStatus {
        self.inner.status.lock().status()
    }

    /// Whether the shell waits at a prompt with goatty's command-line
    /// widgets bound (the managed zsh with shell integration, #186): then
    /// [`crate::input::LineEdit`] keys work.
    pub fn line_editing(&self) -> bool {
        self.inner.zle.load(Ordering::Relaxed) && self.shell_state() != ShellState::Running
    }

    /// Counts the prompts any shell with shell integration marked so far
    /// (`OSC 133 ; A`): once it's above zero, the shell reads what's typed.
    pub fn prompts_marked(&self) -> u64 {
        self.inner.marked_prompts.load(Ordering::Relaxed)
    }

    /// Counts the prompts with line editing (`zle=1`) so far.
    pub(crate) fn prompt_generation(&self) -> u64 {
        self.inner.prompts.load(Ordering::Relaxed)
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
        let old = self.inner.size.swap(pack(size), Ordering::Relaxed);
        self.inner.cell.store(pack_cell(size), Ordering::Relaxed);
        self.inner.term.lock().resize(size.to_crosswords());
        self.line_map_resized(old >> 16 != size.cols as u32);
    }

    /// The cell size in pixels last given to [`Self::resize`] (0 when
    /// unknown): what images are sized with.
    pub fn cell_size(&self) -> (u16, u16) {
        let p = self.inner.cell.load(Ordering::Relaxed);
        ((p >> 16) as u16, (p & 0xffff) as u16)
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
        let mut mode = TermMode::of(&self.inner.term.lock());
        if self.inner.win32_input.load(Ordering::Relaxed) {
            mode |= TermMode::WIN32_INPUT_MODE;
        }
        mode
    }

    pub(crate) fn win32_input(&self) -> bool {
        self.inner.win32_input.load(Ordering::Relaxed)
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

pub(crate) fn pack_cell(s: TermSize) -> u32 {
    ((s.cell_width as u32) << 16) | s.cell_height as u32
}

/// Run `f` with the primary screen in `t.grid`, even while the alternate
/// screen is active (rio-vt keeps the inactive grid private). Switching back
/// to the alternate screen clears it, so its grid and selection are saved and
/// restored around `f`. (`swap_alt` also resets the primary grid's
/// `saved_cursor` to `cursor`, which `1049h` already did on entry; `inactive_grid`
/// cannot be accessed without it.)
pub(crate) fn with_primary<R>(t: &mut Term, f: impl FnOnce(&mut Term) -> R) -> R {
    if !TermMode::from_rio(t.mode()).contains(TermMode::ALT_SCREEN) {
        return f(t);
    }
    let alt = t.grid.clone();
    let selection = t.selection.take();
    t.swap_alt();
    let r = f(t);
    // Entering the alternate screen drops its sixel/iTerm2 images (it
    // starts blank); keep them, they're still on it.
    let inactive = &mut t.graphics.kitty_inactive_screen;
    let atlas = (std::mem::take(&mut inactive.atlas_placements), std::mem::take(&mut inactive.atlas_key_refs));
    t.swap_alt();
    (t.graphics.atlas_placements, t.graphics.atlas_key_refs) = atlas;
    t.grid = alt;
    t.selection = selection;
    r
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
