//! OSC 133 shell-integration marks, which rio-vt parses but doesn't act on.
//! [`MarkScanner`] watches the same bytes the parser gets and reports the
//! marks goatty uses, each with where it ends in the chunk, so
//! [`Terminal::feed`](crate::Terminal::feed) can apply it at the cursor
//! position the mark was printed at:
//!   * `A` (prompt starts) and `P` (prompt, `k=c`/`k=s`: a continuation line)
//!     mark grid rows, which the [transcript](crate::transcript) uses to find
//!     each command's output,
//!   * `C` (command output starts) and `D [; status]` (command finished): the
//!     shell's state, and activity markers on tabs (#122),
//!   * goatty's own `OSC 7337` (#186): `zle=1|0` (the managed zsh's
//!     command-line widgets are there: it says so at each prompt) and
//!     `line=<percent-encoded>` (the unsent command line, which the shell
//!     reports when goatty asks for it). Only `line=` payloads are kept past
//!     the first few bytes, up to [`MAX_LINE`].
//!   * screen switches (`CSI ? 1049/1047/47 h|l`), reported *before* the
//!     final byte: the primary screen's kitty keyboard flags are noted there,
//!     since rio-vt keeps the inactive screen's flags private and snapshots
//!     need them.
//!   * OSC 7501 program status reports (see [`crate::status`]), kept whole
//!     up to the protocol's limit, and full resets (`ESC c`), which drop
//!     them.
//!
//! The scanner keeps its state between chunks, so a mark split across two
//! reads is still found. Outside escape sequences it only runs `memchr` for
//! ESC. Inside an OSC it jumps to the terminator and keeps just the first
//! few bytes, so a large OSC (an inline image, a clipboard write) costs
//! about as much as plain text.

use memchr::{memchr, memchr2};

use crate::status::{self, Report};

/// A shell-integration mark found in the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mark {
    /// `OSC 133 ; A`, or `P` without `k=c`/`k=s`: a prompt starts here.
    PromptStart,
    /// `OSC 133 ; P ; k=c` or `k=s`: another line of the same prompt.
    PromptContinuation,
    /// `OSC 133 ; C`: the command runs; its output starts here.
    CommandStart,
    /// `OSC 133 ; D`: the command ended, with its exit status if the shell
    /// reported one.
    CommandFinished(Option<i32>),
    /// `OSC 133 ; C ; goatty=prompt|run|done [; n=<started>]`: goatty's own
    /// reattach/resync snapshot restoring the shell's state and how many
    /// commands started, without marking rows or reporting a finished
    /// command.
    Restore(ShellState, Option<u64>),
    /// `OSC 7337 ; zle=1|0`: whether goatty's command-line widgets are
    /// bound in the shell at this prompt.
    Zle(bool),
    /// `OSC 7337 ; line=<percent-encoded bytes>`: the command line typed at
    /// the prompt and not sent yet, reported on request.
    Line(String),
    /// `CSI ? 1049` (or `1047`, `47`) before its `h` or `l`: the screen is
    /// about to switch (or stay).
    ScreenSwitch,
    /// `OSC 7501 ; …`: a program's status report, or the probe.
    Status(Report),
    /// `ESC c` (RIS): a full reset.
    Reset,
}

/// What the shell last said about its command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellState {
    /// At a prompt (or nothing said yet).
    #[default]
    Prompt,
    /// A command runs (`C`).
    Running,
    /// The command finished (`D`) and no prompt followed yet.
    Finished,
}

impl ShellState {
    pub(crate) fn to_u8(self) -> u8 {
        self as u8
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Running,
            2 => Self::Finished,
            _ => Self::Prompt,
        }
    }
}

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// The longest OSC prefix we look at: `133;C;goatty=prompt;n=<u64::MAX>`.
const KEEP: usize = 48;
/// goatty's command-line report, kept whole.
const LINE: &[u8] = b"7337;line=";
/// The longest command line report kept (percent-encoded); a longer one
/// is dropped.
pub(crate) const MAX_LINE: usize = 256 * 1024;
/// Program status reports, kept whole up to [`status::MAX_PAYLOAD`].
const STATUS: &[u8] = b"7501;";

/// The OSCs kept past [`KEEP`] bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Long {
    Line,
    Status,
}

impl Long {
    fn prefix(self) -> &'static [u8] {
        match self {
            Long::Line => LINE,
            Long::Status => STATUS,
        }
    }

    fn max(self) -> usize {
        match self {
            Long::Line => MAX_LINE,
            Long::Status => status::MAX_PAYLOAD,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// After ESC.
    Esc,
    /// Inside `ESC ]`, collecting the payload.
    Osc,
    /// ESC inside an OSC: `\` ends it (ST), anything else aborts it.
    OscEsc,
    /// After `ESC [`.
    Csi,
    /// Inside `ESC [ ?`, with the parameter read so far.
    DecPrivate(u16),
}

#[derive(Debug)]
pub(crate) struct MarkScanner {
    state: State,
    /// The OSC payload's first [`KEEP`] bytes.
    buf: [u8; KEEP],
    len: usize,
    /// A [`Long`] OSC's payload after its prefix, while one is read
    /// (`None` for every other OSC); `overflow` once it passed its limit.
    long: Option<(Long, Vec<u8>)>,
    overflow: bool,
}

impl Default for MarkScanner {
    fn default() -> Self {
        Self { state: State::Ground, buf: [0; KEEP], len: 0, long: None, overflow: false }
    }
}

impl MarkScanner {
    /// Scan the next chunk of output and call `on` for each mark, with the
    /// index in `bytes` just past its terminator.
    pub(crate) fn scan(&mut self, bytes: &[u8], mut on: impl FnMut(usize, Mark)) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match memchr(ESC, &bytes[i..]) {
                    Some(p) => {
                        i += p + 1;
                        self.state = State::Esc;
                    }
                    None => return,
                },
                State::Esc => {
                    self.state = match bytes[i] {
                        b']' => {
                            self.len = 0;
                            self.long = None;
                            self.overflow = false;
                            State::Osc
                        }
                        b'[' => State::Csi,
                        b'c' => {
                            on(i + 1, Mark::Reset);
                            State::Ground
                        }
                        ESC => State::Esc,
                        _ => State::Ground,
                    };
                    i += 1;
                }
                State::Csi => {
                    // Anything but a DEC private mode: back to looking for ESC.
                    if bytes[i] == b'?' {
                        self.state = State::DecPrivate(0);
                        i += 1;
                    } else {
                        self.state = State::Ground;
                    }
                }
                State::DecPrivate(n) => match bytes[i] {
                    d @ b'0'..=b'9' => {
                        self.state = State::DecPrivate(n.saturating_mul(10).saturating_add(u16::from(d - b'0')));
                        i += 1;
                    }
                    b @ (b';' | 0x40..=0x7e) => {
                        if matches!(n, 47 | 1047 | 1049) {
                            on(i, Mark::ScreenSwitch);
                        }
                        self.state = if b == b';' { State::DecPrivate(0) } else { State::Ground };
                        i += 1;
                    }
                    _ => self.state = State::Ground,
                },
                State::Osc => {
                    let rest = &bytes[i..];
                    let end = memchr2(BEL, ESC, rest).unwrap_or(rest.len());
                    let take = end.min(KEEP - self.len);
                    self.buf[self.len..self.len + take].copy_from_slice(&rest[..take]);
                    self.len += take;
                    self.collect_long(&rest[..end], take);
                    i += end;
                    if let Some(&b) = bytes.get(i) {
                        i += 1;
                        if b == BEL {
                            self.finish(i, &mut on);
                        } else {
                            self.state = State::OscEsc;
                        }
                    }
                }
                State::OscEsc => {
                    if bytes[i] == b'\\' {
                        i += 1;
                        self.finish(i, &mut on);
                    } else {
                        // The OSC was cut off; this ESC starts something new.
                        self.state = State::Esc;
                    }
                }
            }
        }
    }

    /// Keep a [`Long`] OSC's payload past [`KEEP`]: `seg` is this chunk's
    /// part of the OSC, of which the first `took` bytes also went into
    /// `buf`.
    fn collect_long(&mut self, seg: &[u8], took: usize) {
        if self.overflow {
            return;
        }
        if let Some((_, v)) = self.long.as_mut() {
            v.extend_from_slice(seg);
        } else {
            let Some(kind) =
                [Long::Line, Long::Status].into_iter().find(|k| self.len >= k.prefix().len() && self.buf.starts_with(k.prefix()))
            else {
                return;
            };
            // Everything so far: `buf` after the prefix, then what didn't fit.
            let mut v = self.buf[kind.prefix().len()..self.len].to_vec();
            v.extend_from_slice(&seg[took..]);
            self.long = Some((kind, v));
        }
        if let Some((kind, v)) = self.long.as_mut()
            && v.len() > kind.max()
        {
            *v = Vec::new();
            self.overflow = true;
        }
    }

    fn finish(&mut self, end: usize, on: &mut impl FnMut(usize, Mark)) {
        self.state = State::Ground;
        if let Some((kind, payload)) = self.long.take() {
            if self.overflow {
                return;
            }
            match kind {
                Long::Line => on(end, Mark::Line(percent_decode(&payload))),
                Long::Status => {
                    if let Some(report) = status::parse(&payload) {
                        on(end, Mark::Status(report));
                    }
                }
            }
            return;
        }
        if let Some(mark) = parse(&self.buf[..self.len]) {
            on(end, mark);
        }
    }
}

/// `133;<letter>[;params…]`, or goatty's `7337;zle=…`.
fn parse(payload: &[u8]) -> Option<Mark> {
    match payload {
        b"7337;zle=1" => return Some(Mark::Zle(true)),
        b"7337;zle=0" => return Some(Mark::Zle(false)),
        _ => {}
    }
    let rest = payload.strip_prefix(b"133;")?;
    let (&kind, rest) = rest.split_first()?;
    if !rest.is_empty() && rest[0] != b';' {
        return None;
    }
    let mut params = rest.split(|&b| b == b';').skip(1);
    match kind {
        b'A' => Some(Mark::PromptStart),
        b'P' => Some(if params.any(|p| p == b"k=c" || p == b"k=s") { Mark::PromptContinuation } else { Mark::PromptStart }),
        b'C' => {
            let state = match params.next() {
                Some(b"goatty=prompt") => ShellState::Prompt,
                Some(b"goatty=run") => ShellState::Running,
                Some(b"goatty=done") => ShellState::Finished,
                _ => return Some(Mark::CommandStart),
            };
            let started = params.find_map(|p| p.strip_prefix(b"n=")).and_then(|n| std::str::from_utf8(n).ok()).and_then(|n| n.parse().ok());
            Some(Mark::Restore(state, started))
        }
        b'D' => Some(finished(rest)),
        _ => None,
    }
}

/// `%XX` → the byte; anything else as it is. Invalid UTF-8 is replaced.
fn percent_decode(s: &[u8]) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && let (Some(h), Some(l)) = (s.get(i + 1).copied().and_then(hex), s.get(i + 2).copied().and_then(hex))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The rest of `133;D`: nothing, or `;<status>[;more…]`.
fn finished(rest: &[u8]) -> Mark {
    let status = match rest {
        [] => None,
        [b';', status @ ..] => {
            let status = status.split(|&b| b == b';').next().unwrap_or_default();
            std::str::from_utf8(status).ok().and_then(|s| s.parse().ok())
        }
        _ => None,
    };
    Mark::CommandFinished(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(chunks: &[&[u8]]) -> Vec<Mark> {
        let mut s = MarkScanner::default();
        let mut out = Vec::new();
        for c in chunks {
            s.scan(c, |_, m| out.push(m));
        }
        out.retain(|m| matches!(m, Mark::CommandFinished(_)));
        out
    }

    fn all_marks(bytes: &[u8]) -> Vec<(usize, Mark)> {
        let mut s = MarkScanner::default();
        let mut out = Vec::new();
        s.scan(bytes, |end, m| out.push((end, m)));
        out
    }

    #[test]
    fn screen_switches_are_reported_before_their_final_byte() {
        let stream: &[u8] = b"\x1b[?25;1049hx\x1b[?1047l\x1b[?47h\x1b[1049h\x1b[?10490h\x1b[?2004h\x1b]133;A\x07";
        let found = all_marks(stream);
        assert_eq!(found.iter().filter(|(_, m)| *m == Mark::ScreenSwitch).count(), 3);
        for (at, m) in &found {
            if *m == Mark::ScreenSwitch {
                assert!(matches!(stream[*at], b'h' | b'l'), "{at}");
            }
        }
        assert!(found.iter().any(|(_, m)| *m == Mark::PromptStart), "OSC after CSI still found");
    }

    #[test]
    fn finds_every_mark_and_where_it_ends() {
        let stream: &[u8] = b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x1b\\out\r\n\x1b]133;D;0\x07\x1b]133;P;k=s\x07";
        let found = all_marks(stream);
        let kinds: Vec<Mark> = found.iter().map(|(_, m)| m.clone()).collect();
        assert_eq!(kinds, [Mark::PromptStart, Mark::CommandStart, Mark::CommandFinished(Some(0)), Mark::PromptContinuation]);
        for (end, _) in &found {
            let before = &stream[..*end];
            assert!(before.ends_with(b"\x07") || before.ends_with(b"\x1b\\"), "{end}");
        }
        assert_eq!(all_marks(b"\x1b]133;P;k=i\x07"), [(12, Mark::PromptStart)]);
        let restore: Vec<Mark> = all_marks(b"\x1b]133;C;goatty=run\x07\x1b]133;C;goatty=done\x07").into_iter().map(|(_, m)| m).collect();
        assert_eq!(restore, [Mark::Restore(ShellState::Running, None), Mark::Restore(ShellState::Finished, None)]);
        let max = format!("\x1b]133;C;goatty=prompt;n={}\x07", u64::MAX);
        assert_eq!(all_marks(max.as_bytes()), [(max.len(), Mark::Restore(ShellState::Prompt, Some(u64::MAX)))]);
        assert!(all_marks(b"\x1b]133;Ab\x07\x1b]133;\x07").is_empty());
    }

    #[test]
    fn finds_command_finished_with_either_terminator() {
        assert_eq!(marks(&[b"out\x1b]133;D;0\x07\x1b]133;A\x07$ "]), [Mark::CommandFinished(Some(0))]);
        assert_eq!(marks(&[b"\x1b]133;D;127\x1b\\"]), [Mark::CommandFinished(Some(127))]);
        assert_eq!(marks(&[b"\x1b]133;D\x07"]), [Mark::CommandFinished(None)]);
        assert_eq!(marks(&[b"\x1b]133;D;1;aid=42\x07"]), [Mark::CommandFinished(Some(1))]);
        assert_eq!(marks(&[b"\x1b]133;D;oops\x07"]), [Mark::CommandFinished(None)]);
    }

    #[test]
    fn ignores_other_oscs() {
        assert!(all_marks(b"\x1b]133;B\x07\x1b]133;E;ls\x07").is_empty());
        assert!(marks(&[b"\x1b]0;133;D\x07\x1b]1337;D\x07\x1b[31m133;D\x07"]).is_empty());
        assert!(marks(&[b"\x1b]133;Done\x07"]).is_empty());
    }

    #[test]
    fn finds_marks_split_across_chunks() {
        let stream: &[u8] = b"make\r\n\x1b]133;D;2\x1b\\\x1b]133;A\x07";
        for cut in 0..=stream.len() {
            let (a, b) = stream.split_at(cut);
            assert_eq!(marks(&[a, b]), [Mark::CommandFinished(Some(2))], "cut at {cut}");
        }
    }

    #[test]
    fn long_and_aborted_oscs_dont_swallow_the_next_mark() {
        let image = [b"\x1b]1337;File=inline=1:".as_slice(), &[b'A'; 100_000], b"\x07"].concat();
        assert_eq!(marks(&[&image, b"\x1b]133;D;0\x07"]), [Mark::CommandFinished(Some(0))]);
        // An OSC cut short by another escape sequence.
        assert_eq!(marks(&[b"\x1b]2;title\x1b[0m\x1b]133;D;3\x07"]), [Mark::CommandFinished(Some(3))]);
        assert_eq!(marks(&[b"\x1b\x1b]133;D;4\x07"]), [Mark::CommandFinished(Some(4))]);
    }

    #[test]
    fn reads_goattys_own_marks() {
        let found: Vec<Mark> =
            all_marks(b"\x1b]7337;zle=1\x07\x1b]7337;zle=0\x1b\\\x1b]7337;zle=2\x07").into_iter().map(|(_, m)| m).collect();
        assert_eq!(found, [Mark::Zle(true), Mark::Zle(false)]);
        let found = all_marks(b"\x1b]7337;line=sudo%20ls%0a%e2%9c%93%zz\x07");
        assert_eq!(found, [(37, Mark::Line("sudo ls\n\u{2713}%zz".into()))]);
        assert_eq!(all_marks(b"\x1b]7337;line=\x07").into_iter().map(|(_, m)| m).collect::<Vec<_>>(), [Mark::Line(String::new())]);
    }

    #[test]
    fn long_command_lines_survive_any_split() {
        let line: String = (0..300).map(|i| format!("w{i}%20")).collect();
        let stream = format!("\x1b]7337;line={line}\x1b\\\x1b]133;D;0\x07");
        let want = line.replace("%20", " ");
        for cut in [0, 3, 9, 12, 15, 47, 48, 49, 60, stream.len() - 1] {
            let (a, b) = stream.as_bytes().split_at(cut);
            let mut s = MarkScanner::default();
            let mut out = Vec::new();
            s.scan(a, |_, m| out.push(m));
            s.scan(b, |_, m| out.push(m));
            assert_eq!(out, [Mark::Line(want.clone()), Mark::CommandFinished(Some(0))], "cut at {cut}");
        }
        // Too long: dropped, and the next mark is still found.
        let huge = [b"\x1b]7337;line=".as_slice(), &vec![b'a'; MAX_LINE + 10], b"\x07\x1b]133;D;1\x07"].concat();
        assert_eq!(marks(&[&huge]), [Mark::CommandFinished(Some(1))]);
        assert_eq!(all_marks(&huge).len(), 1);
    }

    #[test]
    fn terminal_reports_command_finished() {
        use std::sync::Arc;

        use crate::{Palette, TermSize, Terminal, UiEvent};
        let (t, ch) = Terminal::new(TermSize::new(40, 5), false, Arc::new(Palette::default()));
        t.feed(b"\x1b]133;C\x07building\r\n\x1b]133;D;");
        t.feed(b"1\x07\x1b]133;A\x07$ \x07");
        let events: Vec<_> = ch.ui.try_iter().collect();
        assert!(matches!(events.as_slice(), [UiEvent::CommandFinished { exit: Some(1) }, UiEvent::Bell]), "{events:?}");
    }
}
