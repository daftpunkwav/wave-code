//! The glyph family: the single source of every semantic symbol the
//! console renders. One wave kind per activity category, fine-lined to
//! match the braille oscilloscope banner; result marks stay neutral.
//!
//! Adding a symbol here is a design-system change: keep the family
//! consistent (thin strokes, no filled blocks) and note the category.

/// Square wave — user input (keyed-in pulses): the editor prompt, the
/// user message bullet, the queue marker.
pub const SQUARE_WAVE: &str = "⊓⊔";

/// Sine wave — assistant speech: the message bullet.
pub const SINE_WAVE: &str = "∿";

/// Triangle wave — thinking: the finalized thinking-block bullet.
pub const TRIANGLE_WAVE: &str = "△";

/// Neutral result: completed successfully.
pub const DONE: &str = "●";

/// Checked off (todo completion, struck through).
pub const CHECK: &str = "✓";

/// Neutral result: failed.
pub const FAILED: &str = "✗";

/// Neutral result: pending.
pub const PENDING: &str = "○";

/// In progress (mirrors the todo panel's dot).
pub const IN_PROGRESS: &str = "●";

/// Git branch marker in the info grid and footer.
pub const BRANCH: &str = "⎇";
