//! Chrome: the persistent frame furniture around the transcript.

pub mod footer;
pub mod notify;
pub mod symbols;
pub mod todo;

pub use footer::{TIP_ROTATE_INTERVAL, TIPS, TransientHint, mode_badge, row1, row2};
pub use todo::render_todos;
