//! The live activity pane above the editor: spinner + phase label.

use crate::state::StreamingPhase;
use crate::theme::{self, Token};
use tui_engine::component::Component;
use tui_engine::loader::{Loader, SpinnerStyle};

/// Random working tips shown next to the composing spinner.
pub const WORKING_TIPS: [&str; 5] = [
    "shift+enter adds a newline",
    "ctrl+o expands tool output",
    "ctrl+s steers a running turn",
    "! runs a shell command",
    "@ mentions a file",
];

/// One animated activity line: `<moon> ` / `⠋ working… · Tip: …`.
pub struct ActivityPane {
    phase: StreamingPhase,
    spinner: Loader,
    tip: Option<String>,
    rendered_phase: StreamingPhase,
}

impl ActivityPane {
    /// A pane starting idle.
    pub fn new() -> Self {
        let theme = theme::current();
        Self {
            phase: StreamingPhase::Idle,
            spinner: Loader::new(
                SpinnerStyle::Moon,
                "waiting",
                theme.style(Token::Primary),
                theme.style(Token::Text),
            ),
            tip: None,
            rendered_phase: StreamingPhase::Idle,
        }
    }

    /// Update the phase; switches spinner style and label.
    pub fn set_phase(&mut self, phase: StreamingPhase) {
        if self.rendered_phase != phase {
            let theme = theme::current();
            self.spinner = match phase {
                StreamingPhase::Composing => Loader::new(
                    SpinnerStyle::Braille,
                    "working…",
                    theme.style(Token::Primary),
                    theme.style(Token::Primary),
                ),
                _ => Loader::new(
                    SpinnerStyle::Moon,
                    "",
                    theme.style(Token::Primary),
                    theme.style(Token::Text),
                ),
            };
            self.rendered_phase = phase;
        }
        self.phase = phase;
        // Rotate the tip each time composing (re)starts.
        if phase == StreamingPhase::Composing && self.tip.is_none() {
            let index = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_millis() as usize)
                .unwrap_or(0);
            self.tip = Some(WORKING_TIPS[index % WORKING_TIPS.len()].to_string());
        }
    }

    /// The label for the current phase (exposed for tests).
    pub fn label(&self) -> Option<String> {
        match self.phase {
            StreamingPhase::Idle => None,
            StreamingPhase::Waiting => Some(String::new()),
            StreamingPhase::Thinking => None, // the live thinking block animates
            StreamingPhase::Composing => Some("working…".to_string()),
            StreamingPhase::Tool => Some(String::new()),
        }
    }
}

impl Default for ActivityPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for ActivityPane {
    fn render(&mut self, _columns: usize) -> Vec<String> {
        let theme = theme::current();
        match self.phase {
            StreamingPhase::Idle | StreamingPhase::Thinking => Vec::new(),
            StreamingPhase::Waiting => {
                vec![format!(
                    "{} {}",
                    self.spinner.current_frame(),
                    theme.paint(Token::Text, "waiting for response…")
                )]
            }
            StreamingPhase::Tool => {
                vec![format!(
                    "{} {}",
                    self.spinner.current_frame(),
                    theme.paint(Token::Text, "running…")
                )]
            }
            StreamingPhase::Composing => {
                let mut line = format!(
                    "{} {}",
                    self.spinner.current_frame(),
                    theme.paint(Token::Primary, "working…")
                );
                if let Some(tip) = &self.tip {
                    line.push_str(&theme.paint(Token::TextDim, &format!(" · Tip: {tip}")));
                }
                vec![line]
            }
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use tui_engine::width::strip_ansi;

    #[test]
    fn idle_and_thinking_render_nothing() {
        theme::set(theme::Theme::dark());
        let mut pane = ActivityPane::new();
        assert!(pane.render(60).is_empty());
        pane.set_phase(StreamingPhase::Thinking);
        assert!(pane.render(60).is_empty());
    }

    #[test]
    fn composing_shows_working_and_tip() {
        theme::set(theme::Theme::dark());
        let mut pane = ActivityPane::new();
        pane.set_phase(StreamingPhase::Composing);
        let line = strip_ansi(&pane.render(80)[0]);
        assert!(line.contains("working…"), "{line}");
        assert!(line.contains("Tip:"), "{line}");
    }

    #[test]
    fn waiting_shows_label() {
        theme::set(theme::Theme::dark());
        let mut pane = ActivityPane::new();
        pane.set_phase(StreamingPhase::Waiting);
        assert!(strip_ansi(&pane.render(80)[0]).contains("waiting for response…"));
    }
}
