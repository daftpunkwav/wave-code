//! Theme system: semantic tokens, palettes, and resolution.

pub mod active;
pub mod colors;
pub mod detect;

pub use active::{Theme, current, set};
pub use colors::{Palette, Token};
