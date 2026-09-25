//! The glyph family: the single source of every semantic symbol the
//! console renders. User input rides a chevron, assistant speech a
//! sine, thinking a triangle — fine-lined to match the braille
//! oscilloscope banner; result marks stay neutral.
//!
//! Adding a symbol here is a design-system change: keep the family
//! consistent (thin strokes, no filled blocks) and note the category.

/// User input — keyed-in strokes: the editor prompt, the user message
/// bullet, the queue marker. The thin pulse variant rides beside it.
pub const USER_PROMPT: &str = "❯";

/// The thin pulse of [`USER_PROMPT`]: the busy-tick and queue-wait
/// marker (bold↔thin alternation reads as a pulse).
pub const USER_PROMPT_PULSE: &str = "›";

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The message bullets compose the family constants: a symbol plus
    /// one space. Composing elsewhere would fork the single source.
    #[test]
    fn message_bullets_derive_from_the_family() {
        use crate::messages::simple::{ASSISTANT_BULLET, USER_BULLET};
        assert_eq!(USER_BULLET.trim_end(), USER_PROMPT);
        assert_eq!(USER_BULLET.len(), USER_PROMPT.len() + 1, "one space");
        assert_eq!(ASSISTANT_BULLET.trim_end(), DONE);
        assert_eq!(ASSISTANT_BULLET.len(), DONE.len() + 1, "one space");
    }
}
