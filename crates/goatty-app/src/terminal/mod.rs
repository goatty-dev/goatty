//! The terminal pane: a view that turns GPUI input into terminal input, and
//! a custom element that paints the grid.

pub(crate) mod element;
mod glyphs;
mod keys;
mod links;
mod stats;
mod view;

pub use links::{cleanup_remote_cache, cleanup_stale_remote_cache};
pub use view::{Activity, PasteRequest, TerminalEvent, TerminalView};
