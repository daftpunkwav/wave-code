//! The queue pane: user messages waiting for the current turn to end.

use crate::theme::{self, Token};
use tui_engine::width;

/// Render the queued-message pane lines (empty when nothing queued).
pub fn render(queued: &[String], columns: usize) -> Vec<String> {
    if queued.is_empty() {
        return Vec::new();
    }
    let theme = theme::current();
    let mut out = Vec::new();
    // Top rule separates the pane from the transcript above.
    out.push(theme.paint(Token::Border, &"─".repeat(columns)));
    for item in queued {
        let text = width::truncate_to_width(item, columns.saturating_sub(4));
        out.push(format!(
            "  {} {}",
            theme.paint(Token::Accent, "❯"),
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
        theme::set(theme::Theme::dark());
        assert!(render(&[], 80).is_empty());
    }

    #[test]
    fn queued_items_render_with_pointer_and_hint() {
        theme::set(theme::Theme::dark());
        let lines = render(&["first".to_string(), "second".to_string()], 80);
        let plain: Vec<String> = lines.iter().map(|l| width::strip_ansi(l)).collect();
        assert_eq!(plain.len(), 4, "rule + 2 items + hint");
        assert!(plain[1].contains("❯ first"));
        assert!(plain[2].contains("❯ second"));
        assert!(plain[3].contains("ctrl-s to steer"));
    }

    #[test]
    fn long_items_truncate() {
        theme::set(theme::Theme::dark());
        let lines = render(&["x".repeat(200)], 40);
        assert!(width::width(&lines[1]) <= 40);
    }
}
