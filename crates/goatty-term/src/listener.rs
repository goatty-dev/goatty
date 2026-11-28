//! Bridges rio-vt's event callbacks to goatty: query replies go back to the
//! PTY (only on the terminal that answers queries), everything else becomes
//! a [`UiEvent`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::Mutex;
use rio_vt::event::{EventListener, ProgressState as RioProgress, RioEvent, WindowId};

use crate::status::{Progress, ProgressState, Store};
use crate::terminal::SharedPalette;

/// Events the terminal emits that the *UI* cares about.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Title(String),
    ResetTitle,
    Bell,
    /// OSC 52 clipboard write from the application (untrusted: any program,
    /// on any host, can send it). `primary`: it asked for the primary
    /// selection (`p` or `s`) rather than the clipboard (`c`).
    ClipboardStore {
        primary: bool,
        text: String,
    },
    CwdChanged,
    /// The shell reported that a command finished (OSC 133;D), with its
    /// exit status when known. Needs shell integration.
    CommandFinished {
        exit: Option<i32>,
    },
    /// The shell reported the command line typed at its prompt (goatty
    /// asked for it with `Terminal::LINE_KEYS`; `OSC 7337;line=`).
    CommandLine(String),
    /// What a program says about itself changed (OSC 7501, OSC 9;4); read
    /// it with `Terminal::program_status`. `news`: the report that made the
    /// record deserving the most attention blocked, done or failed (not
    /// what it already was; never a lifecycle cleanup or a snapshot's
    /// replay), as it was then: one per report, in stream order, so none
    /// is lost when several arrive between two reads.
    Status {
        news: Option<crate::status::Record>,
    },
    /// A program asked for a desktop notification (OSC 9 or OSC 777;
    /// untrusted text, show it as plain text).
    Notification {
        title: String,
        body: String,
    },
    Exit,
}

/// Listener plugged into `Crosswords`. It splits events into two channels:
/// *replies* (bytes that must go back to the PTY, e.g. DA/DSR answers) and
/// *UI events*.
///
/// When the same byte stream is parsed by two terminals (daemon + client),
/// only one of them may answer queries, or the shell sees every reply twice.
/// `answer_queries` selects which.
#[derive(Clone)]
pub(crate) struct Listener {
    pub(crate) replies: flume::Sender<Vec<u8>>,
    pub(crate) ui: flume::Sender<UiEvent>,
    pub(crate) answer_queries: bool,
    pub(crate) palette: SharedPalette,
    pub(crate) size: Arc<AtomicU32>, // packed (cols << 16 | rows)
    pub(crate) cell: Arc<AtomicU32>, // packed (width << 16 | height), pixels
    /// Progress reports (OSC 9;4) land here, next to the OSC 7501 records.
    pub(crate) status: Arc<Mutex<Store>>,
    /// rio-vt's image uploads land here (see `graphics`).
    pub(crate) images: Arc<Mutex<crate::graphics::Store>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: RioEvent, _id: WindowId) {
        match event {
            RioEvent::PtyWrite(_, text) => {
                if self.answer_queries {
                    let _ = self.replies.send(text.into_bytes());
                }
            }
            RioEvent::ColorRequest(_, index, fmt) => {
                if self.answer_queries {
                    let c = self.palette.read().by_index(index);
                    let reply = fmt(rio_vt::config::colors::ColorRgb { r: c.r, g: c.g, b: c.b });
                    let _ = self.replies.send(reply.into_bytes());
                }
            }
            RioEvent::TextAreaSizeRequest(_, fmt) => {
                if self.answer_queries {
                    let packed = self.size.load(Ordering::Relaxed);
                    let (cols, rows) = ((packed >> 16) as u16, (packed & 0xffff) as u16);
                    let cell = self.cell.load(Ordering::Relaxed);
                    // The text area in pixels (CSI 14 t, sixel geometry).
                    let width = cols.saturating_mul((cell >> 16) as u16);
                    let height = rows.saturating_mul((cell & 0xffff) as u16);
                    let ws = rio_vt::event::WindowSize { cols, rows, width, height };
                    let _ = self.replies.send(fmt(ws).into_bytes());
                }
            }
            // OSC 52 reads are a data-exfiltration vector; deny by default.
            RioEvent::ClipboardLoad(..) => {}
            RioEvent::ClipboardStore(kind, text) => {
                let primary = matches!(kind, rio_vt::clipboard::ClipboardType::Selection);
                let _ = self.ui.send(UiEvent::ClipboardStore { primary, text });
            }
            RioEvent::Title(_, t) => {
                let _ = self.ui.send(UiEvent::Title(t));
            }
            RioEvent::ResetTitle => {
                let _ = self.ui.send(UiEvent::ResetTitle);
            }
            RioEvent::Bell(_) => {
                let _ = self.ui.send(UiEvent::Bell);
            }
            RioEvent::CurrentDirectoryChanged(_) => {
                let _ = self.ui.send(UiEvent::CwdChanged);
            }
            RioEvent::ProgressReport(report) => {
                let state = match report.state {
                    RioProgress::Remove => None,
                    RioProgress::Set => Some(ProgressState::Normal),
                    RioProgress::Error => Some(ProgressState::Error),
                    RioProgress::Indeterminate => Some(ProgressState::Indeterminate),
                    RioProgress::Pause => Some(ProgressState::Paused),
                };
                let percent = report.progress.map(|p| p.min(100)).filter(|_| state != Some(ProgressState::Indeterminate));
                if self.status.lock().set_progress(state.map(|state| Progress { state, percent })) {
                    let _ = self.ui.send(UiEvent::Status { news: None });
                }
            }
            RioEvent::DesktopNotification { title, body } => {
                // rio-vt drops multi-param ConEmu OSC 9 subcommands (`9;1;ms`, …),
                // but bare numeric ones without a second `;` (like `9;12` prompt
                // marks) and empty `9;` still arrive here.
                if !(title.is_empty() && body.bytes().all(|b| b.is_ascii_digit())) {
                    let _ = self.ui.send(UiEvent::Notification { title, body });
                }
            }
            RioEvent::UpdateGraphics { queues, .. } => self.images.lock().apply(queues),
            RioEvent::Exit | RioEvent::CloseTerminal(_) => {
                let _ = self.ui.send(UiEvent::Exit);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::UiEvent;

    fn stores(answer_queries: bool, bytes: &[u8]) -> (Vec<(bool, String)>, usize) {
        let (t, ch) = crate::Terminal::new(crate::TermSize::new(40, 5), answer_queries, std::sync::Arc::new(crate::Palette::default()));
        t.feed(bytes);
        let got = ch
            .ui
            .try_iter()
            .filter_map(|e| match e {
                UiEvent::ClipboardStore { primary, text } => Some((primary, text)),
                _ => None,
            })
            .collect();
        (got, ch.replies.try_iter().count())
    }

    /// The GUI's mirror (which answers no queries) still sees OSC 52 writes:
    /// that's how `copy` on a remote host reaches the local clipboard.
    #[test]
    fn osc52_writes_reach_the_mirror_with_their_kind() {
        let (got, _) = stores(false, b"\x1b]52;c;aGVsbG8=\x07\x1b]52;p;d29ybGQ=\x1b\\\x1b]52;s;eA==\x07");
        assert_eq!(got, [(false, "hello".into()), (true, "world".into()), (true, "x".into())]);
    }

    #[test]
    fn osc52_reads_are_denied() {
        let (got, replies) = stores(true, b"\x1b]52;c;?\x07");
        assert!(got.is_empty());
        assert_eq!(replies, 0, "no clipboard contents go back to the program");
    }
}
