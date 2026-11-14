//! A [`Transcript`]: the scrollback and screen as *logical* lines (rows the
//! terminal soft-wrapped are joined again), with their colors, split into
//! commands by the OSC 133 prompt marks (see `marks`). The output viewer
//! shows it unwrapped, searches it and copies from it.
//!
//! Rows carry their mark (rio-vt's `Row::semantic_prompt`) into the
//! scrollback and through reflow: a row that starts a logical line keeps it.
//! `Terminal::feed` sets `Prompt` on the row a prompt starts on and
//! `PromptContinuation` on the rows between it and the command's output, so
//! a command's output is the lines after its prompt lines, up to the next
//! prompt.

use std::ops::Range;

use rio_vt::crosswords::grid::row::SemanticPrompt;
use rio_vt::crosswords::pos::{Column, Line, Pos};
use rio_vt::crosswords::style::StyleFlags;

use crate::marks::ShellState;
use crate::terminal::Term;
use crate::{CellFlags, Palette, Rgb, TermMode, Terminal};

/// A logical line: its text (trailing blanks trimmed) and how it looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub text: String,
    /// Style runs: each covers `text[span.start..next span's start]`
    /// (the last one to the end). Empty for an empty line.
    pub spans: Vec<Span>,
    /// Part of a prompt (with the command line typed at it), not output.
    pub prompt: bool,
}

/// How a run of text looks. Colors are resolved against the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte offset into the line's text.
    pub start: usize,
    pub fg: Rgb,
    /// `None`: the default background.
    pub bg: Option<Rgb>,
    /// Only the style bits (bold, italic, underline, strike, dim).
    pub flags: CellFlags,
}

/// One command: its prompt lines and the output after them, as line
/// indices into [`Transcript::lines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandBlock {
    /// Empty for output above the first prompt (before shell integration
    /// started, or after the scrollback dropped the prompt).
    pub prompt: Range<usize>,
    pub output: Range<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    pub lines: Vec<TranscriptLine>,
    /// In order; together they cover every line.
    pub commands: Vec<CommandBlock>,
    /// The shell reported a command that hasn't finished: the last block.
    pub running: bool,
    /// The last block's command finished but no prompt followed yet (the
    /// moment between the two, or a shell that exited after it).
    pub awaiting_prompt: bool,
    /// How many commands this terminal saw start: changes exactly when a new
    /// command starts, unlike indices into `commands` (which shift when the
    /// scrollback drops old commands) or the text of a command line.
    pub commands_started: u64,
    /// A full-screen program (alternate screen) is showing: the lines are
    /// its screen, not the scrollback.
    pub alt_screen: bool,
}

impl Transcript {
    /// Whether any prompt was marked (shell integration is on).
    pub fn has_marks(&self) -> bool {
        self.commands.iter().any(|c| !c.prompt.is_empty())
    }

    /// The command whose output "the last command" means: the running one,
    /// else the newest one that ran (the block at the current prompt, still
    /// being typed, doesn't count). `None` without shell integration.
    pub fn last_command(&self) -> Option<usize> {
        let n = self.commands.len();
        let last = n.checked_sub(1)?;
        let candidate = if self.running || self.awaiting_prompt { last } else { last.checked_sub(1)? };
        (!self.commands[candidate].prompt.is_empty()).then_some(candidate)
    }

    /// The command line of a block: its last prompt line (the one typed at,
    /// with a multi-line prompt), trimmed.
    pub fn command_line(&self, block: usize) -> Option<&str> {
        let prompt = &self.commands.get(block)?.prompt;
        let line = self.lines.get(prompt.end.checked_sub(1)?)?;
        Some(line.text.trim())
    }

    #[cfg(test)]
    fn command_line_of(&self, c: &CommandBlock) -> Option<&str> {
        let line = self.lines.get(c.prompt.end.checked_sub(1)?)?;
        Some(line.text.trim())
    }

    /// The plain text of `lines`, one per line.
    pub fn text(&self, lines: Range<usize>) -> String {
        let mut out = String::new();
        for (i, l) in self.lines[lines].iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&l.text);
        }
        out
    }
}

impl Terminal {
    /// The whole scrollback and screen as a [`Transcript`]. Copies every
    /// cell with the terminal locked: call it when the user asks, not per
    /// frame (about 12 ms for a full 10,000-row scrollback 200 columns
    /// wide, see `vtbench`).
    pub fn transcript(&self) -> Transcript {
        let palette = self.palette();
        // The state changes with the terminal locked, so read it locked too.
        self.with_term(|t| {
            let mut tr = build(t, &palette, self.shell_state());
            tr.commands_started = self.inner.started.load(std::sync::atomic::Ordering::Relaxed);
            tr
        })
    }
}

fn build(t: &Term, pal: &Palette, shell: ShellState) -> Transcript {
    let colors = *t.colors();
    let default_bg = pal.background;
    let cols = t.columns();
    let top = -(t.history_size() as i32);
    // Down to the cursor, or further if something was drawn below it.
    let cursor = t.grid.cursor.pos.row.0;
    let screen_last = t.screen_lines() as i32 - 1;
    let bottom = (cursor + 1..=screen_last).rev().find(|&r| !t.grid[Line(r)].is_clear()).unwrap_or(cursor);

    let mut lines: Vec<TranscriptLine> = Vec::new();
    // Per line: a prompt starts on it (`Prompt`, not `PromptContinuation`).
    let mut starts: Vec<bool> = Vec::new();
    let mut wrapped = false;
    let mut resolved = None;
    for r in top..=bottom {
        let row = &t.grid[Line(r)];
        let mark = row.semantic_prompt;
        // A marked row starts a line even after a wrapped row: output
        // without a final newline leaves zsh's PROMPT_SP mark wrapping into
        // the prompt.
        if !wrapped || mark != SemanticPrompt::None || lines.is_empty() {
            lines.push(TranscriptLine { text: String::new(), spans: Vec::new(), prompt: mark != SemanticPrompt::None });
            starts.push(mark == SemanticPrompt::Prompt);
        }
        let line = lines.last_mut().expect("pushed above");
        for c in 0..cols {
            let sq = row[Column(c)];
            if sq.is_spacer() || sq.is_leading_spacer() {
                continue;
            }
            // Resolving colors is most of the cost; runs share a style.
            let style = t.grid.style_of(&sq);
            let (fg, bg, flags, hidden) = match &resolved {
                Some((s, r)) if *s == style => *r,
                _ => {
                    let mut fg = pal.resolve_fg(style.fg, style.flags, &colors);
                    let mut bg = pal.resolve(style.bg, &colors);
                    if style.flags.contains(StyleFlags::INVERSE) {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                    let r = (fg, (bg != default_bg).then_some(bg), style_flags(style.flags), style.flags.contains(StyleFlags::HIDDEN));
                    resolved = Some((style, r));
                    r
                }
            };
            if line.spans.last().is_none_or(|s| (s.fg, s.bg, s.flags) != (fg, bg, flags)) {
                line.spans.push(Span { start: line.text.len(), fg, bg, flags });
            }
            if hidden {
                line.text.push(' ');
            } else if sq.has_grapheme() {
                for (i, ch) in t.grid.cell_text(Pos::new(Line(r), Column(c))).enumerate() {
                    line.text.push(if i == 0 && ch == '\0' { ' ' } else { ch });
                }
            } else {
                let ch = sq.c();
                line.text.push(if ch == '\0' { ' ' } else { ch });
            }
        }
        wrapped = row[Column(cols - 1)].wrapline();
    }
    for line in &mut lines {
        trim_end(line);
    }
    // Blank rows under the last line (an empty screen below a cleared prompt).
    while lines.len() > 1 && lines.last().is_some_and(|l| l.text.is_empty() && !l.prompt) {
        lines.pop();
    }
    let commands = blocks(&lines, &starts);
    Transcript {
        lines,
        commands,
        running: shell == ShellState::Running,
        awaiting_prompt: shell == ShellState::Finished,
        commands_started: 0,
        alt_screen: t.mode().contains(TermMode::ALT_SCREEN),
    }
}

/// Drop trailing blanks that look like nothing (no background, underline or
/// strike-through), and the spans that started in them.
fn trim_end(line: &mut TranscriptLine) {
    let visible = |s: &Span| s.bg.is_some() || s.flags.contains(CellFlags::UNDERLINE) || s.flags.contains(CellFlags::STRIKE);
    let mut end = line.text.len();
    while end > 0 && line.text.as_bytes()[end - 1] == b' ' {
        let span = line.spans.iter().rev().find(|s| s.start < end).expect("text has spans");
        if visible(span) {
            break;
        }
        end -= 1;
    }
    line.text.truncate(end);
    line.spans.retain(|s| s.start < end);
}

/// Split lines into commands: a prompt's first line starts one, its other
/// lines follow, then the output.
fn blocks(lines: &[TranscriptLine], starts: &[bool]) -> Vec<CommandBlock> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = i;
        if lines[i].prompt {
            i += 1;
            while i < lines.len() && lines[i].prompt && !starts[i] {
                i += 1;
            }
        }
        let prompt = start..i;
        while i < lines.len() && !lines[i].prompt {
            i += 1;
        }
        out.push(CommandBlock { prompt: prompt.clone(), output: prompt.end..i });
    }
    out
}

fn style_flags(sf: StyleFlags) -> CellFlags {
    let mut f = CellFlags::empty();
    for (s, c) in [
        (StyleFlags::BOLD, CellFlags::BOLD),
        (StyleFlags::ITALIC, CellFlags::ITALIC),
        (StyleFlags::STRIKEOUT, CellFlags::STRIKE),
        (StyleFlags::DIM, CellFlags::DIM),
        (StyleFlags::UNDERCURL, CellFlags::UNDERCURL),
    ] {
        if sf.contains(s) {
            f |= c;
        }
    }
    if sf.intersects(StyleFlags::ALL_UNDERLINES) {
        f |= CellFlags::UNDERLINE;
    }
    f
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::TermSize;

    fn term(cols: u16, rows: u16) -> Terminal {
        Terminal::new(TermSize::new(cols, rows), false, Arc::new(Palette::default())).0
    }

    /// One prompt cycle the way goatty's zsh integration prints it.
    fn command(t: &Terminal, cmd: &str, output: &str) {
        t.feed(format!("\x1b]133;A\x07~ $ \x1b]133;B\x07{cmd}\r\n\x1b]133;C\x07{output}\x1b]133;D;0\x07").as_bytes());
    }

    fn texts(tr: &Transcript, r: Range<usize>) -> Vec<&str> {
        tr.lines[r].iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn joins_wrapped_rows_into_lines() {
        let t = term(10, 5);
        t.feed(b"0123456789abcdefghijXYZ\r\nshort\r\n");
        let tr = t.transcript();
        assert_eq!(texts(&tr, 0..tr.lines.len()), ["0123456789abcdefghijXYZ", "short"]);
        assert!(!tr.has_marks());
        assert_eq!(tr.last_command(), None);
    }

    #[test]
    fn lines_survive_scrolling_into_history_and_resizing() {
        let t = term(10, 3);
        let long = "x".repeat(25);
        command(&t, "cat", &format!("{long}\r\nend\r\n"));
        for _ in 0..5 {
            t.feed(b"filler\r\n");
        }
        t.resize(TermSize::new(7, 3));
        let tr = t.transcript();
        assert_eq!(tr.lines[1].text, long);
        t.resize(TermSize::new(40, 3));
        let tr = t.transcript();
        assert_eq!(tr.lines[0].text, "~ $ cat");
        assert!(tr.lines[0].prompt);
        assert_eq!(tr.lines[1].text, long);
    }

    #[test]
    fn splits_commands_by_their_prompts() {
        let t = term(40, 10);
        t.feed(b"before integration\r\n");
        command(&t, "ls", "a\r\nb\r\n");
        command(&t, "true", "");
        command(&t, "echo hi", "hi\r\n");
        t.feed(b"\x1b]133;A\x07~ $ \x1b]133;B\x07");
        let tr = t.transcript();
        assert!(tr.has_marks());
        let blocks: Vec<(Vec<&str>, Vec<&str>)> =
            tr.commands.iter().map(|c| (texts(&tr, c.prompt.clone()), texts(&tr, c.output.clone()))).collect();
        assert_eq!(
            blocks,
            [
                (vec![], vec!["before integration"]),
                (vec!["~ $ ls"], vec!["a", "b"]),
                (vec!["~ $ true"], vec![]),
                (vec!["~ $ echo hi"], vec!["hi"]),
                (vec!["~ $"], vec![]),
            ]
        );
        // At a fresh prompt, the last command is the one before it.
        assert!(!tr.running);
        assert_eq!(tr.last_command(), Some(3));
        assert_eq!(tr.command_line(3), Some("~ $ echo hi"));
    }

    #[test]
    fn a_finished_command_before_its_next_prompt_is_the_last_one() {
        let t = term(40, 10);
        command(&t, "ls", "a\r\n");
        let tr = t.transcript();
        assert!(tr.awaiting_prompt && !tr.running);
        assert_eq!(tr.last_command(), Some(0));
        command(&t, "pwd", "/\r\n");
        assert_eq!(t.transcript().last_command(), Some(1));
    }

    #[test]
    fn commands_started_survives_the_scrollback_dropping_old_commands() {
        let t = term(20, 3);
        command(&t, "ls", "a\r\n");
        t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07tail -f\r\n\x1b]133;C\x07");
        let before = t.transcript();
        assert_eq!(before.commands_started, 2);
        // Enough output to push `ls` out of the scrollback.
        for i in 0..(crate::DEFAULT_SCROLLBACK + 10) {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        let after = t.transcript();
        assert!(!after.commands.iter().any(|c| after.command_line_of(c) == Some("$ ls")));
        assert_ne!(before.last_command(), after.last_command(), "the index shifted");
        assert_eq!(after.commands_started, 2, "but it's the same command");
        t.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07pwd\r\n\x1b]133;C\x07");
        assert_eq!(t.transcript().commands_started, 3);
    }

    #[test]
    fn a_running_command_is_the_last_one() {
        let t = term(40, 10);
        command(&t, "ls", "a\r\n");
        t.feed(b"\x1b]133;A\x07~ $ \x1b]133;B\x07tail -f log\r\n\x1b]133;C\x07line 1\r\nline 2");
        let tr = t.transcript();
        assert!(tr.running);
        let last = tr.last_command().unwrap();
        assert_eq!(texts(&tr, tr.commands[last].output.clone()), ["line 1", "line 2"]);
    }

    #[test]
    fn multi_line_prompts_and_commands_are_prompt_lines() {
        let t = term(40, 10);
        // A two-line prompt (Starship's default) and a command continued
        // with a backslash.
        t.feed(b"\x1b]133;A\x07~/src on main\r\n\xe2\x9d\xaf \x1b]133;B\x07make \\\r\n> all\r\n\x1b]133;C\x07built\r\n\x1b]133;D;0\x07");
        t.feed(b"\x1b]133;A\x07~ $ ");
        let tr = t.transcript();
        let c = &tr.commands[tr.last_command().unwrap()];
        assert_eq!(texts(&tr, c.prompt.clone()), ["~/src on main", "\u{276f} make \\", "> all"]);
        assert_eq!(texts(&tr, c.output.clone()), ["built"]);
        assert_eq!(tr.command_line(0), Some("> all"));
    }

    #[test]
    fn output_without_a_final_newline_stays_output() {
        let t = term(10, 6);
        // zsh's PROMPT_SP: a % and a screen width of spaces, which wraps
        // onto a new row when the cursor wasn't at the start of one.
        t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07printf x\r\n\x1b]133;C\x07x\x1b]133;D;0\x07\x1b[7m%\x1b[0m         \r");
        t.feed(b"\x1b]133;A\x07$ ");
        let tr = t.transcript();
        let c = &tr.commands[tr.last_command().unwrap()];
        assert_eq!(texts(&tr, c.output.clone())[0].trim_end(), "x%");
        assert_eq!(tr.lines.last().unwrap().text, "$");
    }

    #[test]
    fn keeps_colors_and_trims_blank_ends() {
        let t = term(20, 3);
        t.feed(b"\x1b[31mred\x1b[0m plain\x1b[1m bold \x1b[0m   \r\n");
        let tr = t.transcript();
        let l = &tr.lines[0];
        assert_eq!(l.text, "red plain bold");
        let pal = Palette::default();
        assert_eq!(l.spans[0], Span { start: 0, fg: pal.ansi[1], bg: None, flags: CellFlags::empty() });
        assert_eq!(l.spans[1].start, 3);
        assert_eq!(l.spans[2].start, 9);
        assert!(l.spans[2].flags.contains(CellFlags::BOLD));
        assert_eq!(l.spans.len(), 3);
    }

    #[test]
    fn wide_characters_are_one_char() {
        let t = term(6, 3);
        t.feed("日本語です\r\n".as_bytes());
        let tr = t.transcript();
        assert_eq!(tr.lines[0].text, "日本語です");
    }
}
