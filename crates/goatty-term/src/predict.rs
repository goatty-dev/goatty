//! Local echo prediction (mosh-style, #16): show what the user types
//! before the host echoes it, so typing feels instant on slow links.
//!
//! The [`Predictor`] guesses where typed characters land on the client's
//! mirror [`Terminal`] and overlays them, underlined, on the [`Frame`] the
//! renderer draws. When the host's output arrives, each guess is checked
//! against the real grid: a match is dropped (the real cell now shows it),
//! a mismatch drops every guess. Nothing is ever written to the terminal
//! itself, so a wrong guess can't corrupt it.
//!
//! Guesses are grouped in *epochs*. Any input we can't predict (Enter,
//! arrows, control keys, escape sequences) starts a new one. A new epoch is
//! *tentative*: its guesses stay hidden until the host echoes one of them.
//! So at a password prompt (echo off) nothing typed is ever shown. An epoch
//! is trusted right away only when the shell says it's at a prompt with its
//! line editor running (goatty's managed zsh, `OSC 7337;zle=1`), that prompt
//! is newer than the last unpredictable input (a key bound to a widget may
//! read a secret without echo while zle stays on), nothing we sent earlier
//! still waits for a reaction, and no shown guess was wrong since the host
//! last confirmed one.
//!
//! Guesses typed after unpredictable input can't be confirmed before the
//! host reacted to it: until then the cursor they were placed at is stale,
//! and an echo meant for an earlier guess could seem to confirm them.
//!
//! The caller decides when to predict at all (a slow link, not in the alt
//! screen) and feeds every input chunk and every output chunk through here.

use std::time::{Duration, Instant};

use rio_vt::crosswords::pos::{Column, Line};

use crate::{CellFlags, Frame, Terminal};

/// One guessed cell.
#[derive(Debug, Clone, Copy)]
struct Guess {
    /// Screen row (0 = top of the screen, independent of scrolling).
    row: usize,
    col: usize,
    ch: char,
    epoch: u64,
    /// Order of creation, for the fence below.
    seq: u64,
    sent: Instant,
    deadline: Instant,
}

/// Waiting for the host to react to input we couldn't predict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fence {
    /// Nothing in flight.
    Clear,
    /// Guesses up to `seq` must be resolved first (their echo comes
    /// before the reaction to the unpredictable input).
    Guesses(u64),
    /// Then any further output counts as the reaction.
    Output,
}

impl Fence {
    fn clear(self) -> bool {
        self == Fence::Clear
    }
}

#[derive(Debug, Clone)]
pub struct Predictor {
    guesses: Vec<Guess>,
    epoch: u64,
    /// Epochs up to here are shown (one of their guesses was confirmed, or
    /// they were trusted when they started).
    shown_epoch: u64,
    /// The epoch was decided to be tentative or trusted already.
    decided: bool,
    /// The next guess goes at the real cursor, not after the last guess.
    from_cursor: bool,
    fence: Fence,
    /// The last guess made before the last unpredictable input: later
    /// ones wait for the fence.
    barrier: u64,
    /// The prompt (`Terminal::prompt_generation`) that last got
    /// unpredictable input: it isn't trusted any more.
    spoiled_prompt: Option<u64>,
    /// A shown guess was wrong: trust nothing until the host confirms one.
    glitch: bool,
    seq: u64,
    /// Guesses are drawn. Off, they are still checked, which measures
    /// [`Self::echo_delay`].
    visible: bool,
    /// Smoothed time from typing a character to its echo.
    echo: Option<Duration>,
    /// Guesses the host confirmed / that turned out wrong (for tests and
    /// statistics).
    pub confirmed: u64,
    pub wrong: u64,
}

impl Default for Predictor {
    fn default() -> Self {
        Self::new()
    }
}

/// What a chunk of input means to the predictor.
enum Typed {
    /// Printable, single-width characters.
    Chars,
    /// One Backspace (DEL or BS).
    Backspace,
    /// Anything else.
    Other,
}

fn classify(bytes: &[u8]) -> Typed {
    if bytes == b"\x7f" || bytes == b"\x08" {
        return Typed::Backspace;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.is_empty() && s.chars().all(predictable) => Typed::Chars,
        _ => Typed::Other,
    }
}

/// Characters we know take exactly one cell and combine with nothing:
/// ASCII, Latin-1, Latin Extended, Greek and Cyrillic letters.
fn predictable(c: char) -> bool {
    matches!(c, ' '..='~' | '\u{a0}'..='\u{24f}' | '\u{370}'..='\u{4ff}') && c != '\u{ad}'
}

impl Predictor {
    pub fn new() -> Self {
        Self {
            guesses: Vec::new(),
            epoch: 1,
            shown_epoch: 0,
            decided: false,
            from_cursor: true,
            fence: Fence::Clear,
            barrier: 0,
            spoiled_prompt: None,
            glitch: false,
            seq: 0,
            visible: true,
            echo: None,
            confirmed: 0,
            wrong: 0,
        }
    }

    /// Forget every guess (prediction got turned off, the screen was
    /// resized, the link dropped). The next input starts a tentative epoch.
    /// Input still waiting for the host's reaction keeps waiting: only
    /// output resolves the fence.
    pub fn reset(&mut self) {
        self.guesses.clear();
        self.new_epoch();
    }

    fn new_epoch(&mut self) {
        self.epoch += 1;
        self.decided = false;
        self.from_cursor = true;
    }

    /// Draw guesses or only check them. Switching forgets the guesses
    /// made so far (they were made under the other choice).
    pub fn set_visible(&mut self, on: bool) {
        if on != self.visible {
            self.visible = on;
            self.reset();
            // Wrong guesses nobody saw don't count.
            self.glitch = false;
        }
    }

    /// Whether any guess is shown.
    pub fn showing(&self) -> bool {
        self.visible && self.guesses.iter().any(|g| g.epoch <= self.shown_epoch)
    }

    /// How long the host takes to echo a typed character (smoothed over
    /// the last few), once one was echoed. Unlike the link's round trip it
    /// includes a slow host and an `ssh` running inside the session.
    pub fn echo_delay(&self) -> Option<Duration> {
        self.echo
    }

    fn sample_echo(&mut self, d: Duration) {
        // One stall shouldn't count for more than a second.
        let d = d.min(Duration::from_secs(1));
        self.echo = Some(match self.echo {
            None => d,
            Some(e) => (e * 7 + d) / 8,
        });
    }

    /// When the oldest guess gives up waiting for its echo; call
    /// [`Self::expire`] then.
    pub fn deadline(&self) -> Option<Instant> {
        self.guesses.iter().map(|g| g.deadline).min()
    }

    /// Input sent to the host (`bytes` as written, one key or paste per
    /// call). `timeout`: how long to wait for an echo (a few round trips).
    pub fn input(&mut self, term: &Terminal, bytes: &[u8], now: Instant, timeout: Duration) {
        match classify(bytes) {
            Typed::Chars => {
                if !self.decided {
                    self.decided = true;
                    if !self.glitch && self.fence.clear() && term.line_editing() && self.spoiled_prompt != Some(term.prompt_generation()) {
                        self.shown_epoch = self.epoch;
                    }
                }
                let (row, mut col, cols) = match (self.from_cursor, self.guesses.last()) {
                    (false, Some(g)) => (g.row, g.col + 1, term.size().0 as usize),
                    _ => term.with_term(|t| (t.grid.cursor.pos.row.0.max(0) as usize, t.grid.cursor.pos.col.0, t.columns())),
                };
                self.from_cursor = false;
                for ch in std::str::from_utf8(bytes).unwrap_or_default().chars() {
                    // Wrapping depends on the application: stop at the
                    // last column and wait for the host.
                    if col + 1 >= cols {
                        self.unpredictable(term);
                        return;
                    }
                    self.seq += 1;
                    self.guesses.push(Guess { row, col, ch, epoch: self.epoch, seq: self.seq, sent: now, deadline: now + timeout });
                    col += 1;
                }
            }
            Typed::Backspace => match self.guesses.last() {
                // Taking back a guess the host hasn't echoed yet: it will
                // echo both, and the cell ends up as it is now.
                Some(g) if !self.from_cursor && g.epoch == self.epoch => {
                    self.guesses.pop();
                }
                _ => self.unpredictable(term),
            },
            Typed::Other => self.unpredictable(term),
        }
    }

    /// Input whose effect we can't guess: wait for the host's reaction.
    fn unpredictable(&mut self, term: &Terminal) {
        self.new_epoch();
        self.barrier = self.seq;
        self.fence = if self.guesses.iter().any(|g| g.seq <= self.barrier) { Fence::Guesses(self.seq) } else { Fence::Output };
        self.spoiled_prompt = Some(term.prompt_generation());
    }

    /// The host's output was fed into `term`: check the guesses against it.
    pub fn output(&mut self, term: &Terminal, now: Instant) {
        // Checked before this chunk resolves guesses: the reaction comes in
        // a later chunk than the last echo before it.
        if self.fence == Fence::Output {
            self.fence = Fence::Clear;
        }
        if !self.guesses.is_empty() {
            let shown = self.shown_epoch;
            // Before the host reacted to unpredictable input, guesses typed
            // after it can't be checked.
            let (fenced, barrier) = (!self.fence.clear(), self.barrier);
            let mut failed = None;
            let mut confirmed_epoch = None;
            let mut echoed = None;
            term.with_term(|t| {
                let cur = t.grid.cursor.pos;
                let (crow, ccol) = (cur.row.0, cur.col.0);
                let rows = t.screen_lines();
                self.guesses.retain(|g| {
                    if failed.is_some() {
                        return true;
                    }
                    if g.row >= rows || crow != g.row as i32 {
                        // The cursor left the line (a new prompt, output,
                        // a redraw): what's left can't be checked.
                        failed = Some(false);
                        return true;
                    }
                    if ccol <= g.col || (fenced && g.seq > barrier) {
                        return true;
                    }
                    let ch = t.grid[Line(g.row as i32)][Column(g.col)].c();
                    if ch == g.ch {
                        self.confirmed += 1;
                        echoed = Some(g.sent);
                        confirmed_epoch = Some(confirmed_epoch.unwrap_or(0).max(g.epoch));
                        false
                    } else {
                        failed = Some(g.epoch <= shown);
                        true
                    }
                });
            });
            if let Some(sent) = echoed {
                self.sample_echo(now.saturating_duration_since(sent));
            }
            if let Some(e) = confirmed_epoch
                && e > self.shown_epoch
            {
                // The host echoes this epoch's typing: show the rest, and
                // trust it again.
                self.shown_epoch = e;
                self.glitch = false;
            }
            if let Some(was_shown) = failed {
                if was_shown {
                    self.wrong += 1;
                    self.glitch = true;
                }
                self.guesses.clear();
                self.new_epoch();
            }
        }
        if let Fence::Guesses(seq) = self.fence
            && self.guesses.first().is_none_or(|g| g.seq > seq)
        {
            self.fence = Fence::Output;
        }
    }

    /// Drop guesses whose echo didn't come in time. Returns whether
    /// anything shown disappeared (redraw).
    pub fn expire(&mut self, now: Instant) -> bool {
        if !self.guesses.iter().any(|g| g.deadline <= now) {
            return false;
        }
        let shown = self.showing();
        if self.visible && self.guesses.iter().any(|g| g.deadline <= now && g.epoch <= self.shown_epoch) {
            // A shown guess never got its echo: the host does something
            // else with what we type.
            self.wrong += 1;
            self.glitch = true;
        }
        self.guesses.clear();
        self.new_epoch();
        if matches!(self.fence, Fence::Guesses(_)) {
            self.fence = Fence::Output;
        }
        shown
    }

    /// Draw the shown guesses (underlined) and the cursor after them.
    pub fn overlay(&self, frame: &mut Frame) {
        if !self.visible {
            return;
        }
        let mut cursor = None;
        for g in self.guesses.iter().filter(|g| g.epoch <= self.shown_epoch) {
            let row = g.row + frame.display_offset;
            if row >= frame.rows || g.col >= frame.cols {
                continue;
            }
            let cell = &mut frame.cells[row * frame.cols + g.col];
            cell.ch = g.ch;
            cell.extra = 0;
            let mut flags = CellFlags::UNDERLINE;
            for keep in [CellFlags::BOLD, CellFlags::ITALIC, CellFlags::DIM] {
                if cell.flags.contains(keep) {
                    flags |= keep;
                }
            }
            cell.flags = flags;
            cursor = Some((row, g.col + 1));
        }
        if let (Some((row, col)), Some(cur)) = (cursor, frame.cursor.as_mut()) {
            cur.row = row;
            cur.col = col.min(frame.cols - 1);
            cur.wide = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Palette, TermSize};

    const T: Duration = Duration::from_secs(1);

    fn term() -> Terminal {
        Terminal::new(TermSize::new(20, 5), false, Arc::new(Palette::default())).0
    }

    /// A prompt from goatty's managed zsh: line editor bound.
    fn zle_prompt(t: &Terminal) {
        t.feed(b"\x1b]133;A\x07$ \x1b]7337;zle=1\x07");
    }

    fn shown(p: &Predictor, t: &Terminal) -> String {
        let mut f = Frame::default();
        t.snapshot_into(&mut f);
        p.overlay(&mut f);
        f.row(0).iter().filter(|c| c.flags.contains(CellFlags::UNDERLINE)).map(|c| c.ch).collect()
    }

    fn cursor(p: &Predictor, t: &Terminal) -> (usize, usize) {
        let mut f = Frame::default();
        t.snapshot_into(&mut f);
        p.overlay(&mut f);
        let c = f.cursor.unwrap();
        (c.row, c.col)
    }

    fn feed(p: &mut Predictor, t: &Terminal, bytes: &[u8]) {
        t.feed(bytes);
        p.output(t, Instant::now());
    }

    #[test]
    fn trusted_at_a_zle_prompt_and_confirmed_by_the_echo() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        p.input(&t, b"l", now, T);
        p.input(&t, b"s", now, T);
        assert_eq!(shown(&p, &t), "ls");
        assert_eq!(cursor(&p, &t), (0, 4));
        feed(&mut p, &t, b"l");
        assert_eq!(shown(&p, &t), "s");
        feed(&mut p, &t, b"s");
        assert_eq!(shown(&p, &t), "");
        assert_eq!((p.confirmed, p.wrong), (2, 0));
    }

    #[test]
    fn hidden_until_the_host_echoes_without_shell_integration() {
        let t = term();
        t.feed(b"$ ");
        let mut p = Predictor::new();
        let now = Instant::now();
        p.input(&t, b"a", now, T);
        p.input(&t, b"b", now, T);
        assert_eq!(shown(&p, &t), "");
        feed(&mut p, &t, b"a");
        // The host echoes: the rest of the epoch shows.
        assert_eq!(shown(&p, &t), "b");
        p.input(&t, b"c", now, T);
        assert_eq!(shown(&p, &t), "bc");
    }

    #[test]
    fn password_prompts_never_show_what_is_typed() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        for b in [&b"s"[..], b"u", b"d", b"o"] {
            p.input(&t, b, now, T);
        }
        feed(&mut p, &t, b"sudo");
        p.input(&t, b"\r", now, T);
        // The command runs and asks for a password, echo off.
        feed(&mut p, &t, b"\r\n\x1b]133;C\x07[sudo] password: ");
        for b in [&b"h"[..], b"u", b"n", b"t", b"e", b"r"] {
            p.input(&t, b, now, T);
            assert_eq!(shown(&p, &t), "");
        }
        // No echo ever comes; the guesses time out unseen.
        assert!(!p.expire(now + T));
        assert_eq!(p.wrong, 0);
    }

    #[test]
    fn a_wrong_guess_drops_everything_and_stops_trusting() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        p.input(&t, b"j", now, T);
        p.input(&t, b"k", now, T);
        assert_eq!(shown(&p, &t), "jk");
        // The application did something else with the keys.
        feed(&mut p, &t, b"xy");
        assert_eq!(shown(&p, &t), "");
        assert_eq!(p.wrong, 1);
        // Not trusted any more, even at a zle prompt…
        p.input(&t, b"a", now, T);
        assert_eq!(shown(&p, &t), "");
        // …until the host confirms a guess.
        feed(&mut p, &t, b"a");
        p.input(&t, b"b", now, T);
        assert_eq!(shown(&p, &t), "b");
    }

    #[test]
    fn unpredictable_input_waits_for_the_hosts_reaction() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        p.input(&t, b"a", now, T);
        p.input(&t, b"\x1b[D", now, T); // Left
        p.input(&t, b"b", now, T);
        // `b`'s place depends on how the host handles Left: hidden.
        assert_eq!(shown(&p, &t), "a");
        feed(&mut p, &t, b"a");
        // The echo of `a` isn't the reaction to Left yet.
        let mut early = p.clone();
        early.input(&t, b"c", now, T);
        assert_eq!(shown(&early, &t), "");
        // zsh moves left, then inserts `b` before `a`.
        feed(&mut p, &t, b"\x08");
        feed(&mut p, &t, b"ba\x08");
        assert_eq!(p.wrong, 0);
        // `b` waited for the reaction to Left, then its echo confirmed it:
        // the rest of its epoch shows, at the real cursor.
        p.input(&t, b"c", now, T);
        assert_eq!(shown(&p, &t), "c");
        assert_eq!(cursor(&p, &t), (0, 4));
    }

    #[test]
    fn backspace_takes_back_an_unechoed_guess() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        p.input(&t, b"a", now, T);
        p.input(&t, b"b", now, T);
        p.input(&t, b"\x7f", now, T);
        assert_eq!(shown(&p, &t), "a");
        assert_eq!(cursor(&p, &t), (0, 3));
        feed(&mut p, &t, b"ab\x08 \x08");
        assert_eq!(shown(&p, &t), "");
        assert_eq!(p.wrong, 0);
    }

    #[test]
    fn stops_before_the_last_column() {
        let t = term();
        t.feed(&[b'x'; 17]);
        t.feed(b"\x1b]7337;zle=1\x07");
        let mut p = Predictor::new();
        let now = Instant::now();
        p.input(&t, b"ab", now, T);
        p.input(&t, b"c", now, T);
        let mut f = Frame::default();
        t.snapshot_into(&mut f);
        p.overlay(&mut f);
        let row: String = f.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(row.trim_end(), "xxxxxxxxxxxxxxxxxab");
    }

    #[test]
    fn a_missing_echo_times_out() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        p.input(&t, b"a", now, T);
        assert!(!p.expire(now));
        assert!(p.expire(now + T));
        assert_eq!(shown(&p, &t), "");
        assert_eq!(p.wrong, 1);
    }

    #[test]
    fn a_new_line_drops_what_cannot_be_checked() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        p.input(&t, b"a", now, T);
        feed(&mut p, &t, b"\r\noutput\r\n");
        assert_eq!(shown(&p, &t), "");
        assert_eq!(p.wrong, 0);
    }

    #[test]
    fn hidden_guesses_measure_the_echo_delay() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        p.set_visible(false);
        let now = Instant::now();
        p.input(&t, b"a", now, T);
        assert_eq!(shown(&p, &t), "");
        t.feed(b"a");
        p.output(&t, now + Duration::from_millis(80));
        assert_eq!(p.echo_delay(), Some(Duration::from_millis(80)));
        p.input(&t, b"b", now, T);
        t.feed(b"b");
        p.output(&t, now + Duration::from_millis(160));
        assert_eq!(p.echo_delay(), Some(Duration::from_millis(90)));
        // Shown from the next guess on.
        p.set_visible(true);
        p.input(&t, b"c", now, T);
        assert_eq!(shown(&p, &t), "c");
    }

    fn type_each(p: &mut Predictor, t: &Terminal, keys: &[u8], now: Instant) {
        for k in keys {
            p.input(t, std::slice::from_ref(k), now, T);
        }
    }

    #[test]
    fn password_typeahead_is_not_confirmed_by_the_commands_echo() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        // All typed before the host echoes anything.
        type_each(&mut p, &t, b"sudo\rsecret", now);
        assert_eq!(shown(&p, &t), "sudo");
        // The echo of the command's `s` lands where the password's `s` was
        // guessed too; it must not confirm it.
        feed(&mut p, &t, b"s");
        assert_eq!(shown(&p, &t), "udo");
        feed(&mut p, &t, b"udo");
        feed(&mut p, &t, b"\r\n\x1b]133;C\x07[sudo] password: ");
        assert_eq!(shown(&p, &t), "");
        p.expire(now + T);
        assert_eq!(shown(&p, &t), "");
    }

    #[test]
    fn a_widget_reading_without_echo_at_the_prompt_is_not_trusted() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        let now = Instant::now();
        // A key bound to a widget that asks for a passphrase (`read -s`):
        // zle stays on, no new prompt.
        p.input(&t, b"\x1bp", now, T);
        feed(&mut p, &t, b"Passphrase: ");
        type_each(&mut p, &t, b"hunter2", now);
        assert_eq!(shown(&p, &t), "");
        // The next prompt is trusted again.
        p.input(&t, b"\r", now, T);
        feed(&mut p, &t, b"\r\n");
        t.feed(b"\x1b[H\x1b[2J");
        zle_prompt(&t);
        p.output(&t, now);
        p.input(&t, b"l", now, T);
        assert_eq!(shown(&p, &t), "l");
    }

    #[test]
    fn turning_drawing_on_keeps_waiting_for_the_hosts_reaction() {
        let t = term();
        zle_prompt(&t);
        let mut p = Predictor::new();
        p.set_visible(false);
        let now = Instant::now();
        type_each(&mut p, &t, b"sudo\r", now);
        // The echo delay crossed the threshold: drawing turns on before
        // the host reacted to Enter (zle is still on).
        p.set_visible(true);
        p.input(&t, b"x", now, T);
        assert_eq!(shown(&p, &t), "");
    }
}
