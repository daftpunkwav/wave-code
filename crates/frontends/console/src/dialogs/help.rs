//! Scrollable help panel: keybindings plus every command with its
//! description.

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};

/// The scrollable help panel: keybindings plus every command with its
/// description. Esc / q / Enter close it.
pub struct HelpPanel {
    pub(super) title: String,
    lines: Vec<String>,
    scroll: usize,
}

/// Help content rows shown at once.
const HELP_WINDOW: usize = 20;

impl HelpPanel {
    /// Build the panel from pre-wrapped plain lines.
    pub fn new(lines: Vec<String>) -> Self {
        Self {
            title: "Help".to_string(),
            lines,
            scroll: 0,
        }
    }

    /// Deepest scroll offset: the window pins to the last `HELP_WINDOW`
    /// rows, so scrolling stops once the footer reads `N/N`.
    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(HELP_WINDOW)
    }

    fn page_down(&mut self) {
        self.scroll = (self.scroll + HELP_WINDOW).min(self.max_scroll());
    }

    fn page_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(HELP_WINDOW);
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc | Key::Enter | Key::Char('q') => Some(Answer::Dismissed),
            Key::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            Key::Down => {
                self.scroll = (self.scroll + 1).min(self.max_scroll());
                None
            }
            Key::PageUp => {
                self.page_up();
                None
            }
            Key::PageDown => {
                self.page_down();
                None
            }
            _ => None,
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        let end = (self.scroll + HELP_WINDOW).min(self.lines.len());
        for line in &self.lines[self.scroll..end] {
            body.push(theme.paint(Token::Text, line));
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            &format!(
                "↑/↓ scroll · PgUp/PgDn page · esc closes  ({}/{} lines)",
                end,
                self.lines.len()
            ),
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
