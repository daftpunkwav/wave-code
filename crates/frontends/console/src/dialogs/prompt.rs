//! Bare-command free-text prompt: one prefilled editable line
//! (`/title`, `/editor`, `/export`, `/compact`, `/btw`).

use super::{Answer, PromptPurpose};
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};
use tui_engine::width;

/// The bare-command free-text prompt: one prefilled editable line.
/// Enter submits (trimmed; empty allowed where the command treats it
/// as "use the default"), Esc cancels.
pub struct PromptDialog {
    pub(super) title: String,
    /// Where the answer routes back to.
    purpose: PromptPurpose,
    /// The editable line, prefilled by the caller.
    value: String,
    hint: &'static str,
}

impl PromptDialog {
    /// Build the prompt over `initial` text.
    pub fn new(title: &str, purpose: PromptPurpose, initial: &str, hint: &'static str) -> Self {
        Self {
            title: title.to_string(),
            purpose,
            value: initial.to_string(),
            hint,
        }
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Backspace => {
                self.value.pop();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                self.value.push(c);
                None
            }
            Key::Enter => Some(Answer::Prompt {
                purpose: self.purpose.clone(),
                value: self.value.trim().to_string(),
            }),
            _ => None,
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let body = vec![
            theme.paint(
                Token::Text,
                &width::truncate_to_width(&self.value, columns.saturating_sub(6)),
            ),
            String::new(),
            theme.paint(Token::TextDim, self.hint),
        ];
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
