//! Window title and tab progress reporting (OSC 0 / OSC 9;4).
//!
//! The title names the session pane (`WaveCode · model · title`); the
//! OSC 9;4 indeterminate progress marks a running turn in terminals
//! that render it (Windows Terminal, WezTerm, kitty) and is invisible
//! elsewhere. Progress sequences are re-emitted roughly once a second
//! while busy: some terminals clear the progress state on their own.

/// Build the OSC 0 window-title sequence. Control characters are
/// stripped: the title crosses the terminal as plain text.
pub fn set_title(title: &str) -> String {
    let clean: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("\x1b]0;{clean}\x07")
}

/// Start the indeterminate tab progress (`OSC 9;4` state 2).
pub fn progress_start() -> String {
    "\x1b]9;4;2;0\x07".to_string()
}

/// Clear the tab progress (`OSC 9;4` state 0).
pub fn progress_clear() -> String {
    "\x1b]9;4;0;0\x07".to_string()
}

/// How often the busy progress is re-emitted while unchanged.
pub const PROGRESS_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(1);

/// The window title for a session: app name, model, then the session
/// title (falling back to the short session id when unset or blank).
pub fn session_title(model: &str, title: Option<&str>, session_id: &str) -> String {
    let subject = match title {
        Some(title) if !title.trim().is_empty() => title.to_string(),
        _ => crate::ui::short_id(session_id),
    };
    format!("WaveCode · {model} · {subject}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_sequence_carries_text() {
        assert_eq!(set_title("wave"), "\x1b]0;wave\x07");
    }

    #[test]
    fn title_strips_control_characters() {
        // Each control character maps to one space, matching notify.
        assert_eq!(set_title("a\r\nb"), "\x1b]0;a  b\x07");
    }

    #[test]
    fn progress_sequences_are_locked() {
        assert_eq!(progress_start(), "\x1b]9;4;2;0\x07");
        assert_eq!(progress_clear(), "\x1b]9;4;0;0\x07");
    }

    #[test]
    fn session_title_prefers_title_over_id() {
        assert_eq!(
            session_title("m1", Some("fix the bug"), "1234567890"),
            "WaveCode · m1 · fix the bug"
        );
        assert_eq!(
            session_title("m1", None, "1234567890"),
            "WaveCode · m1 · 12345678"
        );
        assert_eq!(
            session_title("m1", Some("  "), "1234567890"),
            "WaveCode · m1 · 12345678"
        );
    }
}
