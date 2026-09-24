//! The `/usage` transcript panel: a context-window bar with severity
//! coloring plus the cumulative token split for the session.

use crate::state::{TokenUsage, context_percent, format_tokens};
use crate::theme::{self, Token};
use tui_engine::component::Component;

/// Width of the context bar in cells.
pub const BAR_WIDTH: usize = 20;

/// A `/usage` panel; immutable once built (a snapshot of state).
pub struct UsagePanel {
    context_used: Option<u64>,
    context_window: Option<u64>,
    usage: TokenUsage,
}

impl UsagePanel {
    /// Snapshot the current usage facts.
    pub fn new(context_used: Option<u64>, context_window: Option<u64>, usage: TokenUsage) -> Self {
        Self {
            context_used,
            context_window,
            usage,
        }
    }
}

/// The context bar: `filled` cells proportional to `percent`, severity
/// colored (ok below half, warn from 50%, danger from 85%).
pub fn context_bar(percent: u64, width: usize) -> String {
    let filled = ((percent.min(100) as usize) * width / 100).min(width);
    format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

/// Severity token for a context percentage.
pub fn severity_token(percent: u64) -> Token {
    match percent {
        0..=49 => Token::Success,
        50..=84 => Token::Warning,
        _ => Token::Error,
    }
}

impl Component for UsagePanel {
    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let label_width = 12;
        let mut out = Vec::new();
        out.push(theme.paint(Token::TextDim, "● usage"));
        let row = |out: &mut Vec<String>, label: &str, value: String| {
            out.push(format!(
                "  {}{}",
                theme.paint(Token::TextDim, &format!("{label:<label_width$}")),
                value
            ));
        };
        if let (Some(used), Some(window)) = (self.context_used, self.context_window) {
            let percent = context_percent(used, window);
            let token = severity_token(percent);
            let bar = context_bar(percent, BAR_WIDTH.min(columns.saturating_sub(40)));
            row(
                &mut out,
                "context",
                format!(
                    "{} {}% ({}/{})",
                    theme.paint(token, &bar),
                    theme.paint(Token::Text, &percent.to_string()),
                    theme.paint(Token::Text, &format_tokens(used)),
                    theme.paint(Token::TextDim, &format_tokens(window))
                ),
            );
        } else {
            row(
                &mut out,
                "context",
                theme.paint(Token::TextDim, "no samples yet"),
            );
        }
        row(
            &mut out,
            "input",
            theme.paint(Token::Text, &format_tokens(self.usage.input)),
        );
        row(
            &mut out,
            "output",
            theme.paint(Token::Text, &format_tokens(self.usage.output)),
        );
        row(
            &mut out,
            "cache",
            theme.paint(
                Token::Text,
                &format!(
                    "read {} · write {}",
                    format_tokens(self.usage.cache_read),
                    format_tokens(self.usage.cache_creation)
                ),
            ),
        );
        row(
            &mut out,
            "total",
            theme.paint(Token::Text, &format_tokens(self.usage.total())),
        );
        out.push(String::new());
        out
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TokenUsage;
    use tui_engine::width::strip_ansi;

    #[test]
    fn bar_fills_proportionally() {
        assert_eq!(context_bar(0, 10), "[░░░░░░░░░░]");
        assert_eq!(context_bar(50, 10), "[█████░░░░░]");
        assert_eq!(context_bar(100, 10), "[██████████]");
        assert_eq!(context_bar(120, 10), "[██████████]", "clamped");
    }

    #[test]
    fn severity_escalates() {
        assert_eq!(severity_token(10), Token::Success);
        assert_eq!(severity_token(50), Token::Warning);
        assert_eq!(severity_token(85), Token::Error);
    }

    #[test]
    fn panel_renders_rows_and_total() {
        theme::set(theme::Theme::synthwave());
        let mut panel = UsagePanel::new(
            Some(84_000),
            Some(200_000),
            TokenUsage {
                input: 12_000,
                output: 3_000,
                cache_read: 100_000,
                cache_creation: 2_000,
            },
        );
        let lines: Vec<String> = panel
            .render(80)
            .into_iter()
            .map(|l| strip_ansi(&l))
            .collect();
        let joined = lines.join("\n");
        assert!(joined.contains("42% (82.0k/195k)"), "{joined}");
        assert!(
            joined.contains("input") && joined.contains("11.7k"),
            "{joined}"
        );
        assert!(
            joined.contains("output") && joined.contains("2.9k"),
            "{joined}"
        );
        assert!(
            joined.contains("total") && joined.contains("14.6k"),
            "{joined}"
        );
        assert!(joined.contains("read 97.7k"), "{joined}");
    }

    #[test]
    fn panel_without_samples_says_so() {
        theme::set(theme::Theme::synthwave());
        let mut panel = UsagePanel::new(None, None, TokenUsage::default());
        let lines: Vec<String> = panel
            .render(80)
            .into_iter()
            .map(|l| strip_ansi(&l))
            .collect();
        assert!(lines.join("\n").contains("no samples yet"));
    }
}
