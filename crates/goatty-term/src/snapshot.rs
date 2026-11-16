//! Reattach snapshots: serialize the daemon's authoritative terminal state as
//! a VT byte stream that, fed into a *fresh* client terminal, reproduces it.
//!
//! rio-vt's `contents_formatted()` covers the visible screen + cursor. We add
//! what it leaves out and what reconnecting actually needs:
//!   * scrollback (replayed as lines so they scroll into the client history),
//!   * the alternate screen switch (so `vim` exiting restores correctly),
//!   * input-affecting modes (DECCKM, keypad, bracketed paste, mouse, focus),
//!   * the window title,
//!   * the OSC 133 prompt marks on rows and soft wraps in the scrollback,
//!     so the client's [transcript](crate::transcript) finds the same
//!     commands and unwrapped lines,
//!   * OSC 8 hyperlinks.

use std::fmt::Write as _;

use rio_vt::crosswords::formatter::write_sgr;
use rio_vt::crosswords::grid::row::SemanticPrompt;
use rio_vt::crosswords::pos::{Column, Line};
use rio_vt::crosswords::square::Hyperlink;
use rio_vt::crosswords::style::Style;

use crate::marks::ShellState;
use crate::terminal::Term;
use crate::{TermMode, Terminal};

/// The shell's state, how many commands started, and whether goatty's
/// command-line widgets are bound (see `Terminal::line_editing`).
pub(crate) type ShellInfo = (ShellState, u64, bool);

/// Upper bound on replayed history lines; keeps attach latency bounded.
pub const MAX_REPLAY_HISTORY: usize = 5_000;

impl Terminal {
    /// Full snapshot for a *fresh* client terminal: RIS, up to
    /// `max_history` scrollback lines, screen, modes.
    pub fn reattach_snapshot(&self) -> Vec<u8> {
        self.reattach_snapshot_with_history(MAX_REPLAY_HISTORY)
    }

    pub fn reattach_snapshot_with_history(&self, max_history: usize) -> Vec<u8> {
        // The shell state changes with the terminal locked: read it locked.
        self.with_term(|t| serialize(t, max_history.min(MAX_REPLAY_HISTORY), self.shell_info()))
    }

    /// Everything another process needs to rebuild this terminal (goattyd
    /// handing its sessions to its next version): like
    /// [`Self::reattach_snapshot`], plus the primary screen and its
    /// scrollback while the alternate screen is active, which the attach
    /// snapshot leaves out. Leaves the terminal as it was.
    pub fn restore_snapshot(&self) -> Vec<u8> {
        self.with_term(|t| {
            let shell = self.shell_info();
            if !t.mode().contains(TermMode::ALT_SCREEN) {
                return serialize(t, MAX_REPLAY_HISTORY, shell);
            }
            // rio-vt keeps the primary grid private while the alternate one
            // is active. Switch to it, and back: switching to the alternate
            // screen clears it, so put its grid back afterwards.
            let alt = t.grid.clone();
            t.swap_alt();
            let mut out = String::from("\x1bc");
            write_primary(t, MAX_REPLAY_HISTORY, &mut out);
            t.swap_alt();
            t.grid = alt;
            let mode = t.mode();
            out.push_str("\x1b[?1049h");
            write_screen(t, false, &mut out);
            write_shell_state(shell, &mut out);
            write_modes(t, mode, &mut out);
            out.into_bytes()
        })
    }

    /// Catch-up snapshot for a client terminal that already mirrors this
    /// session but missed some output (flow control skipped it). Unlike
    /// [`Self::reattach_snapshot`] it does not reset the client, so the
    /// client keeps its scrollback; the old screen scrolls into history
    /// (rio's `ED 2` on the primary screen pushes the viewport up) and the
    /// skipped lines are simply absent. Small: one screen plus modes.
    pub fn resync_snapshot(&self) -> Vec<u8> {
        self.with_term(|t| resync(t, self.shell_info()))
    }
}

fn resync(t: &mut Term, shell: ShellInfo) -> Vec<u8> {
    let mode = t.mode();
    let mut out = String::new();
    // Undo state the skipped bytes may have left on the client: scroll
    // region, origin mode, and the alt-screen switch in whichever direction.
    // Autowrap on for the replay (soft-wrapped rows rely on it);
    // `write_modes` then sets the session's own state.
    out.push_str("\x1b[0m\x1b[r\x1b[?6l\x1b[?7h");
    out.push_str(if mode.contains(TermMode::ALT_SCREEN) { "\x1b[?1049h" } else { "\x1b[?1049l" });
    write_screen(t, true, &mut out);
    write_shell_state(shell, &mut out);
    // Mouse protocols are one setting in rio: reset, then set what's active.
    out.push_str("\x1b[?1000l\x1b[?1005l\x1b[?1006l");
    write_modes(t, mode, &mut out);
    out.into_bytes()
}

fn serialize(t: &Term, max_history: usize, shell: ShellInfo) -> Vec<u8> {
    let mode = t.mode();
    let mut out = String::new();
    // Full reset of the fresh client, then UTF-8 + autowrap defaults.
    out.push_str("\x1bc");

    let alt = mode.contains(TermMode::ALT_SCREEN);
    // Scrollback only exists for the primary grid. When the alt screen is
    // active, `t.grid` *is* the alt grid (rio swaps them), so history replay
    // is only possible on the primary screen.
    if !alt {
        write_primary(t, max_history, &mut out);
    } else {
        out.push_str("\x1b[?1049h");
        write_screen(t, false, &mut out);
    }
    write_shell_state(shell, &mut out);
    write_modes(t, mode, &mut out);
    out.into_bytes()
}

/// The primary screen with up to `max_history` lines of scrollback, and the
/// cursor. The scrollback and every screen row go in one run: the last
/// screen row lands on the client's bottom row, which scrolls exactly the
/// scrollback into its history, and a line wrapping from the scrollback onto
/// the screen stays one line.
fn write_primary(t: &Term, max_history: usize, out: &mut String) {
    let history = t.history_size().min(max_history) as i32;
    write_rows(t, -history..t.screen_lines() as i32, false, out);
    write_cursor(t, out);
}

/// The visible screen like rio's `contents_formatted` (reset, clear, rows,
/// cursor position and visibility), but with prompt marks and soft wraps.
/// `clear`: erase the client's screen first (on the primary screen rio
/// pushes it into history, which resync wants).
fn write_screen(t: &Term, clear: bool, out: &mut String) {
    out.push_str(if clear { "\x1b[0m\x1b[2J\x1b[H" } else { "\x1b[0m\x1b[H" });
    let cols = t.columns();
    let last = (0..t.screen_lines() as i32)
        .rev()
        .find(|&r| last_content_col(t, Line(r), cols).is_some() || t.grid[Line(r)].semantic_prompt != SemanticPrompt::None);
    if let Some(last) = last {
        write_rows(t, 0..last + 1, false, out);
    }
    write_cursor(t, out);
}

/// Reset the pen, then the cursor's position and visibility.
fn write_cursor(t: &Term, out: &mut String) {
    out.push_str("\x1b[0m");
    let pos = t.grid.cursor.pos;
    let _ = write!(out, "\x1b[{};{}H", pos.row.0.max(0) + 1, pos.col.0 + 1);
    out.push_str(if t.mode().contains(TermMode::SHOW_CURSOR) { "\x1b[?25h" } else { "\x1b[?25l" });
}

/// Write `lines` with their styles and prompt marks. A soft-wrapped row is
/// written in full and without a newline, so the client wraps it too (at
/// whatever width it has); a marked row always starts a line of its own.
/// `newline_after_last`: end with a newline (the scrollback) or not (the
/// screen, which would scroll).
pub(crate) fn write_rows(t: &Term, lines: std::ops::Range<i32>, newline_after_last: bool, out: &mut String) {
    let cols = t.columns();
    let mut current = Style::default();
    let mut wrapping = false;
    for r in lines.clone() {
        let line = Line(r);
        let last = r + 1 == lines.end;
        if !wrapping {
            write_mark(t, line, out);
        }
        let next_marked = !last && t.grid[Line(r + 1)].semantic_prompt != SemanticPrompt::None;
        let wraps = !last && !next_marked && t.grid[line][Column(cols - 1)].wrapline();
        let written = write_row(t, line, cols, wraps, &mut current, out);
        if wrapping && written == 0 {
            // Move past the pending wrap, or the newline stays on the row above.
            out.push(' ');
        }
        wrapping = wraps;
        if !wraps && (!last || newline_after_last) {
            out.push_str("\x1b[0m\r\n");
            current = Style::default();
        }
    }
}

/// The shell's state for the client's viewer: running, finished, or at a prompt.
/// With how many commands started, so the client tells commands apart
/// like the session does (a command whose prompt the replay left out is
/// still one).
/// Then whether the command-line widgets are there (both ways: a resync
/// may have to clear it).
fn write_shell_state((shell, started, zle): ShellInfo, out: &mut String) {
    let state = match shell {
        ShellState::Prompt => "prompt",
        ShellState::Running => "run",
        ShellState::Finished => "done",
    };
    let _ = write!(out, "\x1b]133;C;goatty={state};n={started}\x07\x1b]7337;zle={}\x07", u8::from(zle));
}

/// The OSC 133 mark that gives the row at the cursor `line`'s mark.
fn write_mark(t: &Term, line: Line, out: &mut String) {
    match t.grid[line].semantic_prompt {
        SemanticPrompt::None => {}
        SemanticPrompt::Prompt => out.push_str("\x1b]133;A\x07"),
        SemanticPrompt::PromptContinuation => out.push_str("\x1b]133;P;k=c\x07"),
    }
}

/// Modes that change how the client must encode input, plus the title.
fn write_modes(t: &Term, mode: TermMode, out: &mut String) {
    let set = |out: &mut String, on: bool, n: u16| {
        let _ = write!(out, "\x1b[?{n}{}", if on { 'h' } else { 'l' });
    };
    set(out, mode.contains(TermMode::APP_CURSOR), 1);
    set(out, mode.contains(TermMode::LINE_WRAP), 7);
    set(out, mode.contains(TermMode::BRACKETED_PASTE), 2004);
    set(out, mode.contains(TermMode::FOCUS_IN_OUT), 1004);
    if mode.contains(TermMode::MOUSE_REPORT_X10) {
        set(out, true, 9);
    }
    if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        set(out, true, 1000);
    }
    if mode.contains(TermMode::MOUSE_DRAG) {
        set(out, true, 1002);
    }
    if mode.contains(TermMode::MOUSE_MOTION) {
        set(out, true, 1003);
    }
    if mode.contains(TermMode::UTF8_MOUSE) {
        set(out, true, 1005);
    }
    if mode.contains(TermMode::SGR_MOUSE) {
        set(out, true, 1006);
    }
    out.push_str(if mode.contains(TermMode::APP_KEYPAD) { "\x1b=" } else { "\x1b>" });
    if !t.title.is_empty() {
        let _ = write!(out, "\x1b]2;{}\x07", t.title.replace(['\x07', '\x1b'], ""));
    }
}

/// The last column that shows something: a glyph, or a blank with a
/// visible style (rio-vt's rule: a blank with only a foreground doesn't).
fn last_content_col(t: &Term, line: Line, cols: usize) -> Option<usize> {
    let row = &t.grid[line];
    (0..cols).rev().find(|&c| {
        let sq = row[Column(c)];
        let ch = sq.c();
        (ch != ' ' && ch != '\0') || t.grid.style_of(&sq).renders_on_blank()
    })
}

/// Write a row's text with its styles and hyperlinks; `full` keeps
/// trailing blanks. Returns how many cells it wrote.
fn write_row(t: &Term, line: Line, cols: usize, full: bool, current: &mut Style, out: &mut String) -> usize {
    let row = &t.grid[line];
    let last = if full { Some(cols - 1) } else { last_content_col(t, line, cols) };
    let Some(last) = last else { return 0 };
    let mut link: Option<Hyperlink> = None;
    let mut c = 0;
    while c <= last {
        let sq = row[Column(c)];
        if sq.is_spacer() || sq.is_leading_spacer() {
            c += 1;
            continue;
        }
        let style = t.grid.style_of(&sq);
        if style != *current {
            write_sgr(out, &style);
            *current = style;
        }
        let cell_link = if sq.has_hyperlink() { t.cell_hyperlink(line, Column(c)) } else { None };
        if cell_link != link {
            write_hyperlink(cell_link.as_ref(), out);
            link = cell_link;
        }
        let pos = rio_vt::crosswords::pos::Pos::new(line, Column(c));
        for (i, ch) in t.grid.cell_text(pos).enumerate() {
            out.push(if i == 0 && ch == '\0' { ' ' } else { ch });
        }
        c += if sq.is_wide() { 2 } else { 1 };
    }
    if link.is_some() {
        write_hyperlink(None, out);
    }
    last + 1
}

/// Open `link` (OSC 8 with its id, so the cells stay one link), or close
/// the open one.
fn write_hyperlink(link: Option<&Hyperlink>, out: &mut String) {
    let clean = |s: &str| s.replace(['\x07', '\x1b', ';', ':'], "");
    match link {
        Some(l) => {
            let _ = write!(out, "\x1b]8;id={};{}\x1b\\", clean(l.id()), l.uri().replace(['\x07', '\x1b'], ""));
        }
        None => out.push_str("\x1b]8;;\x1b\\"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{Palette, TermSize, Terminal};

    fn text_of(t: &Terminal) -> Vec<String> {
        let mut f = crate::Frame::default();
        t.snapshot_into(&mut f);
        (0..f.rows).map(|r| f.row(r).iter().map(|c| c.ch).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn roundtrip_screen_and_history() {
        let size = TermSize::new(20, 5);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        for i in 0..12 {
            a.feed(format!("line {i}\r\n").as_bytes());
        }
        a.feed(b"\x1b[31mred\x1b[0m $ ");
        let snap = a.reattach_snapshot();

        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        b.feed(&snap);
        assert_eq!(text_of(&a), text_of(&b));
        let ha = a.history_size();
        let hb = b.history_size();
        assert_eq!(ha, hb, "history should be replayed");
    }

    #[test]
    fn restore_snapshot_keeps_both_screens() {
        let size = TermSize::new(20, 5);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        for i in 0..12 {
            a.feed(format!("line {i}\r\n").as_bytes());
        }
        a.feed(b"$ vim");
        let (primary, history) = (text_of(&a), a.history_size());
        a.feed(b"\x1b[?1049h\x1b[2;3HALT SCREEN");
        let alt = text_of(&a);
        let snap = a.restore_snapshot();
        // Taking it changes nothing.
        assert_eq!(text_of(&a), alt);
        a.feed(b"!");

        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        b.feed(&snap);
        b.feed(b"!");
        assert_eq!(text_of(&b), text_of(&a), "the alternate screen and its cursor");
        // Leaving it shows the primary screen and its history on both.
        for t in [&a, &b] {
            t.feed(b"\x1b[?1049l");
            assert_eq!(text_of(t), primary);
            assert_eq!(t.history_size(), history);
        }
    }

    #[test]
    fn roundtrip_keeps_prompt_marks_and_wrapped_lines() {
        let size = TermSize::new(10, 4);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        let cycle =
            |cmd: &str, out: &str| format!("\x1b]133;A\x07$ \x1b]133;B\x07{cmd}\r\n\x1b]133;C\x07{out}\x1b]133;D;0\x07").into_bytes();
        a.feed(&cycle("ls", "0123456789abcdefghij\r\nshort\r\n"));
        a.feed(&cycle("pwd", "/home\r\n"));
        a.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n\x1b]133;C\x07zz");
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        b.feed(&a.reattach_snapshot());
        let (ta, tb) = (a.transcript(), b.transcript());
        assert_eq!(ta.lines, tb.lines);
        assert_eq!(ta.commands, tb.commands);
        assert!(tb.running);
        assert_eq!(tb.lines[1].text, "0123456789abcdefghij");
        assert_eq!(text_of(&a), text_of(&b));
    }

    fn reattached(a: &Terminal, size: TermSize) -> Terminal {
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        b.feed(&a.reattach_snapshot());
        b
    }

    #[test]
    fn roundtrip_keeps_wrapped_rows_on_the_screen() {
        let size = TermSize::new(10, 5);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        a.feed(b"0123456789abcdefghijX\r\nnext");
        let b = reattached(&a, size);
        assert_eq!(a.transcript().lines, b.transcript().lines);
        assert_eq!(b.transcript().lines[0].text, "0123456789abcdefghijX");
        assert_eq!(text_of(&a), text_of(&b));
    }

    #[test]
    fn roundtrip_keeps_a_line_wrapping_from_history_onto_the_screen() {
        let size = TermSize::new(10, 3);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        a.feed(b"0123456789abcdefghijABCDEFGHIJZ");
        assert_eq!(a.history_size(), 1);
        let b = reattached(&a, size);
        assert_eq!(b.history_size(), 1);
        assert_eq!(b.transcript().lines[0].text, "0123456789abcdefghijABCDEFGHIJZ");
        assert_eq!(text_of(&a), text_of(&b));
    }

    #[test]
    fn roundtrip_of_a_running_command_keeps_its_output() {
        let size = TermSize::new(20, 6);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        a.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07one\r\ntwo\r\nthree");
        let b = reattached(&a, size);
        let tb = b.transcript();
        assert!(tb.running);
        let c = &tb.commands[tb.last_command().unwrap()];
        assert_eq!(tb.text(c.output.clone()), "one\ntwo\nthree");
        assert_eq!(a.transcript().commands, tb.commands);
    }

    #[test]
    fn roundtrip_keeps_a_prompt_after_output_without_a_newline() {
        let size = TermSize::new(10, 3);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        // zsh's PROMPT_SP wraps the partial line into the prompt's row.
        a.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07printf x\r\n\x1b]133;C\x07x\x1b]133;D;0\x07\x1b[7m%\x1b[0m         \r");
        a.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a\r\nb\r\nc\r\nd\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        let b = reattached(&a, size);
        let (ta, tb) = (a.transcript(), b.transcript());
        assert_eq!(ta.commands, tb.commands);
        assert_eq!(tb.command_line(1), Some("$ ls"));
    }

    #[test]
    fn resync_replays_wrapped_rows_even_if_the_client_missed_autowrap_on() {
        let size = TermSize::new(10, 4);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        a.feed(b"\x1b[?7l");
        b.feed(b"\x1b[?7l");
        // The client misses autowrap going back on, and the wrapped line.
        a.feed(b"\x1b[?7h0123456789abcdefghijX");
        b.feed(&a.resync_snapshot());
        assert_eq!(b.transcript().lines.last().unwrap().text, "0123456789abcdefghijX");
        assert_eq!(text_of(&a), text_of(&b));
        assert!(b.mode().contains(crate::TermMode::LINE_WRAP));
    }

    #[test]
    fn reattach_keeps_a_finished_command_whose_prompt_the_replay_left_out() {
        let size = TermSize::new(20, 3);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        let out: String = (0..30).map(|i| format!("out {i}\r\n")).collect();
        a.feed(format!("\x1b]133;A\x07$ \x1b]133;B\x07seq\r\n\x1b]133;C\x07{out}\x1b]133;D;0\x07").as_bytes());
        let ta = a.transcript();
        assert_eq!(ta.last_command(), Some(0));
        // Replay only the last 5 history rows: the prompt is left out.
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        b.feed(&a.reattach_snapshot_with_history(5));
        let tb = b.transcript();
        assert!(tb.commands[0].prompt.is_empty());
        assert_eq!(tb.last_command(), Some(0), "still a command");
        assert_eq!(tb.last_command_id(), ta.last_command_id());
    }

    #[test]
    fn resync_keeps_the_running_command_id() {
        let size = TermSize::new(20, 5);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        let start = b"\x1b]133;A\x07$ \x1b]133;B\x07tail -f\r\n\x1b]133;C\x07one\r\n";
        a.feed(start);
        b.feed(start);
        let before = b.transcript().last_command_id();
        // The client misses some output; the running prompt is still on screen.
        a.feed(b"two\r\nthree\r\n");
        b.feed(&a.resync_snapshot());
        assert!(b.command_running());
        assert_eq!(b.transcript().last_command_id(), before, "the replayed A mark isn't a new command");
        // A command the client missed starting is a new one.
        a.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07");
        b.feed(&a.resync_snapshot());
        assert_ne!(b.transcript().last_command_id(), before);
        assert_eq!(b.transcript().commands_started, a.transcript().commands_started);
    }

    #[test]
    fn resync_restores_the_shell_state() {
        let size = TermSize::new(20, 5);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        let start = b"\x1b]133;A\x07$ \x1b]133;B\x07sleep 9\r\n";
        a.feed(start);
        b.feed(start);
        // The client misses the command starting.
        a.feed(b"\x1b]133;C\x07zz");
        b.feed(&a.resync_snapshot());
        assert!(b.command_running());
        // ...and later it finishing, with the next prompt.
        a.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        b.feed(&a.resync_snapshot());
        assert!(!b.command_running());
        // The old screen is in the client's history now; the last command is still the right one.
        let tb = b.transcript();
        assert_eq!(tb.command_line(tb.last_command().unwrap()), Some("$ sleep 9"));
    }

    #[test]
    fn snapshots_carry_the_line_editing_widgets() {
        let size = TermSize::new(20, 5);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        a.feed(b"\x1b]7337;zle=1\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(a.line_editing());
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        b.feed(&a.reattach_snapshot());
        assert!(b.line_editing());
        // A command runs: no widgets until the next prompt says so.
        a.feed(b"vim\r\n\x1b]133;C\x07");
        assert!(!a.line_editing());
        b.feed(&a.resync_snapshot());
        assert!(!b.line_editing());
    }

    #[test]
    fn roundtrip_alt_screen_and_modes() {
        let size = TermSize::new(20, 5);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        a.feed(b"shell$ vim\r\n\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[Hvim buffer");
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        b.feed(&a.reattach_snapshot());
        assert_eq!(text_of(&a), text_of(&b));
        let relevant = crate::TermMode::ALT_SCREEN
            | crate::TermMode::APP_CURSOR
            | crate::TermMode::BRACKETED_PASTE
            | crate::TermMode::MOUSE_MODE
            | crate::TermMode::SGR_MOUSE;
        assert_eq!((a.mode() & relevant).bits(), (b.mode() & relevant).bits());
    }

    #[test]
    fn resync_catches_up_without_losing_history() {
        let size = TermSize::new(20, 5);
        let (a, _ca) = Terminal::new(size, true, Arc::new(Palette::default()));
        let (b, _cb) = Terminal::new(size, false, Arc::new(Palette::default()));
        // Both see the beginning...
        let start: Vec<u8> = (0..10).flat_map(|i| format!("early {i}\r\n").into_bytes()).collect();
        a.feed(&start);
        b.feed(&start);
        let hb_before = b.history_size();
        // ...then the client misses a burst that also enters the alt screen
        // and turns on mouse reporting and app cursor keys.
        a.feed(b"lots of skipped output\r\n\x1b[?1049h\x1b[?1h\x1b[?1000h\x1b[?1006h\x1b[3;3Hin-app");
        b.feed(&a.resync_snapshot());
        assert_eq!(text_of(&a), text_of(&b));
        let relevant = crate::TermMode::ALT_SCREEN | crate::TermMode::APP_CURSOR | crate::TermMode::MOUSE_MODE | crate::TermMode::SGR_MOUSE;
        assert_eq!((a.mode() & relevant).bits(), (b.mode() & relevant).bits());

        // Leaving the app on the daemon side while the client is still
        // skipping must bring the client back to the primary screen too.
        a.feed(b"\x1b[?1049l\x1b[?1l\x1b[?1000lback at prompt");
        b.feed(&a.resync_snapshot());
        assert_eq!(text_of(&a), text_of(&b));
        assert_eq!((a.mode() & relevant).bits(), (b.mode() & relevant).bits());
        assert!(b.history_size() >= hb_before, "client history kept");
    }
}
