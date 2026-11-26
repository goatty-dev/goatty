//! goatty's terminal engine.
//!
//! This crate is the *only* place that talks to `rio-vt` directly. Everything
//! above it (the GPUI renderer, the daemon) goes through [`Terminal`] and the
//! plain data types in this crate, so swapping the VT core (e.g. for
//! `wezterm-term` or `alacritty_terminal`) only touches this crate.
//!
//! Threading model: bytes are fed from a single I/O thread via
//! [`Terminal::feed`]; renderers take the lock briefly once per frame via
//! [`Terminal::with_term`] / [`Terminal::snapshot_into`] and copy out what
//! they need.

pub mod colors;
mod frame;
pub mod graphics;
pub mod history;
pub mod icat;
pub mod input;
mod links;
mod listener;
mod marks;
pub mod media;
mod mode;
pub mod paste;
pub mod predict;
pub mod process;
pub mod pty;
pub mod snapshot;
pub mod status;
mod terminal;
pub mod transcript;

pub use colors::{Palette, Rgb};
pub use frame::{Cell, CellFlags, Cursor, CursorKind, Frame};
pub use graphics::{FrameImage, Image, ImageLayer};
pub use history::{HistoryRows, HistoryWanted, TopLineCause};
pub use links::{Found, Link, LinkKind, LinkSpan, find_links};
pub use listener::UiEvent;
pub use media::MediaRewriter;
pub use mode::TermMode;
pub use status::{BlockedKind, ProgramStatus, Progress, ProgressState, Record as StatusRecord, State as StatusState};
pub use terminal::{DEFAULT_SCROLLBACK, SelectKind, TermSize, Terminal, TerminalChannels};
pub use transcript::{CommandBlock, PromptJump, PromptJumped, Span, Transcript, TranscriptLine};
