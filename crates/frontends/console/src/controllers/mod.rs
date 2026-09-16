//! Controllers: glue between session events and UI state.

pub mod streaming;

pub use streaming::{FLUSH_INTERVAL, StreamingController};
