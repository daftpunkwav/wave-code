//! Permission-mode selector: plan / auto / wave with descriptions.

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};

/// The permission-mode selector: three modes with descriptions, Enter
/// applies, Esc cancels.
pub struct PermissionPickerDialog {
    pub(super) title: String,
    modes: Vec<(&'static str, &'static str)>,
    selected: usize,
}

impl PermissionPickerDialog {
    /// Build the selector over plan/auto/wave.
    pub fn new(current: &str) -> Self {
        let modes = vec![
            (
                "plan",
                "Plan Mode — read-only exploration; changes wait for approval",
            ),
            (
                "auto",
                "Auto Mode — asks before exec and destructive actions",
            ),
            (
                "wave",
                "Wave Mode — fully automatic; the denylist still applies",
            ),
        ];
        let selected = modes
            .iter()
            .position(|(name, _)| *name == current)
            .unwrap_or(1);
        Self {
            title: "Select a permission mode".to_string(),
            modes,
            selected,
        }
    }

    fn resolve(&self) -> Answer {
        Answer::PermissionSelected {
            mode: self.modes[self.selected].0.to_string(),
        }
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up => {
                self.selected = if self.selected == 0 {
                    self.modes.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.modes.len();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                if let Ok(digit) = c.to_string().parse::<usize>()
                    && digit >= 1
                    && digit <= self.modes.len()
                {
                    self.selected = digit - 1;
                    return Some(self.resolve());
                }
                None
            }
            Key::Enter => Some(self.resolve()),
            _ => None,
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        for (index, (name, description)) in self.modes.iter().enumerate() {
            let number = index + 1;
            if index == self.selected {
                body.push(format!(
                    "{} {}",
                    theme.bold(Token::Accent, &format!("▶ {number}.")),
                    theme.bold(Token::TextStrong, name)
                ));
                body.push(theme.paint(Token::TextDim, &format!("     {description}")));
            } else {
                body.push(format!(
                    "  {} {}",
                    theme.paint(Token::TextDim, &format!("{number}.")),
                    theme.paint(Token::Text, name)
                ));
            }
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "↑/↓ select · 1/2/3 choose · ↵ apply · esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
