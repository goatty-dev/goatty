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
    /// OSC 52 clipboard write from the application.
    ClipboardStore(String),
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
    /// it with `Terminal::program_status`. `notify`: the program reported
    /// something new (not a lifecycle cleanup, not a snapshot replaying
    /// what was already there), so a change of state is news.
    Status {
        notify: bool,
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
    /// Progress reports (OSC 9;4) land here, next to the OSC 7501 records.
    pub(crate) status: Arc<Mutex<Store>>,
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
                    let ws = rio_vt::event::WindowSize { cols: (packed >> 16) as u16, rows: (packed & 0xffff) as u16, width: 0, height: 0 };
                    let _ = self.replies.send(fmt(ws).into_bytes());
                }
            }
            // OSC 52 reads are a data-exfiltration vector; deny by default.
            RioEvent::ClipboardLoad(..) => {}
            RioEvent::ClipboardStore(_, text) => {
                let _ = self.ui.send(UiEvent::ClipboardStore(text));
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
                    let _ = self.ui.send(UiEvent::Status { notify: false });
                }
            }
            RioEvent::DesktopNotification { title, body } => {
                // ConEmu's other numbered OSC 9 commands (`9;1;ms` sleeps,
                // `9;12` marks a prompt…) aren't notifications.
                if !(title.is_empty() && body.bytes().all(|b| b.is_ascii_digit())) {
                    let _ = self.ui.send(UiEvent::Notification { title, body });
                }
            }
            RioEvent::Exit | RioEvent::CloseTerminal(_) => {
                let _ = self.ui.send(UiEvent::Exit);
            }
            _ => {}
        }
    }
}
