//! The queue pane: user messages waiting for the current turn to end.

use crate::theme::{self, Token};
use std::time::Instant;
use tui_engine::width;

/// Square-wave phase flip (the queued pulses tick while waiting).
const QUEUE_FLIP_INTERVAL_MS: u128 = 400;

/// Render the queued-message pane lines (empty when nothing queued).
pub fn render(queued: &[String], columns: usize, now: Instant) -> Vec<String> {
    if queued.is_empty() {
        return Vec::new();
    }
    let theme = theme::current();
    let marker = if (now.elapsed().as_millis() / QUEUE_FLIP_INTERVAL_MS).is_multiple_of(2) {
        "⊓⊔"
    } else {
        "⊔⊓"
    };
    let mut out = Vec::new();
    // Top rule separates the pane from the transcript above.
    out.push(theme.paint(Token::Border, &"─".repeat(columns)));
    for item in queued {
        let text = width::truncate_to_width(item, columns.saturating_sub(5));
        out.push(format!(
            "  {} {}",
            theme.paint(Token::Accent, marker),
            theme.paint(Token::Text, &text)
        ));
    }
    out.push(theme.paint(
        Token::TextDim,
        "  ↑ to edit · will send after current task · ctrl-s to steer immediately",
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    #[test]
    fn empty_queue_renders_nothing() {
        theme::set(theme::Theme::synthwave());
        assert!(render(&[], 80, Instant::now()).is_empty());
    }

    #[test]
    fn queued_items_render_with_pointer_and_hint() {
        theme::set(theme::Theme::synthwave());
        let lines = render(
            &["first".to_string(), "second".to_string()],
            80,
            Instant::now(),
        );
        let plain: Vec<String> = lines.iter().map(|l| width::strip_ansi(l)).collect();
        assert_eq!(plain.len(), 4, "rule + 2 items + hint");
        assert!(plain[1].contains("⊓⊔ first") || plain[1].contains("⊔⊓ first"));
        assert!(plain[2].contains("second"));
        assert!(plain[3].contains("ctrl-s to steer"));
    }

    #[test]
    fn long_items_truncate() {
        theme::set(theme::Theme::synthwave());
        let lines = render(&["x".repeat(200)], 40, Instant::now());
        assert!(width::width(&lines[1]) <= 40);
    }
}
