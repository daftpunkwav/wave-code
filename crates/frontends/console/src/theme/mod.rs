//! Theme system: semantic tokens, palettes, and resolution.

pub mod active;
pub mod colors;
pub mod custom;
pub mod detect;
pub mod syntax;

pub use active::{Theme, current, set};
pub use colors::{Palette, Token};
pub use syntax::SyntaxTheme;
