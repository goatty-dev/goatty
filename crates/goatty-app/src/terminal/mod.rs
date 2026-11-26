//! The terminal pane: a view that turns GPUI input into terminal input, and
//! a custom element that paints the grid.

pub(crate) mod box_drawing;
pub(crate) mod element;
mod glyphs;
mod images;
pub(crate) mod keys;
mod links;
mod stats;
mod view;
pub(crate) mod wheel;

pub use links::{cleanup_remote_cache, cleanup_stale_remote_cache};
pub use view::{Activity, PasteRequest, TerminalEvent, TerminalView};
