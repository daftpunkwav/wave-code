//! Reasoning-effort selector: the provider thinking levels (or the
//! standard ramp), the live level marked.

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};

/// The reasoning-effort selector (`/effort` with no args): the
/// provider's thinking levels (or the standard ramp), the live level
/// marked. Enter applies, Esc cancels.
pub struct EffortPickerDialog {
    pub(super) title: String,
    pub(super) levels: Vec<String>,
    /// The live level (`off` when unset; drives the marker).
    current: String,
    pub(super) selected: usize,
}

impl EffortPickerDialog {
    /// Build the picker over `levels`, seeding `off` at the front when
    /// the provider list omits it (off is always available).
    pub fn new(current: Option<&str>, levels: &[String]) -> Self {
        let mut levels = levels.to_vec();
        if !levels.iter().any(|level| level == "off") {
            levels.insert(0, "off".to_string());
        }
        let current = current.unwrap_or("off").to_lowercase();
        let selected = levels
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        Self {
            title: "Select a reasoning effort".to_string(),
            levels,
            current,
            selected,
        }
    }

    fn resolve(&self) -> Answer {
        let level = &self.levels[self.selected];
        Answer::EffortSelected {
            level: (level != "off").then(|| level.clone()),
        }
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up => {
                self.selected = if self.selected == 0 {
                    self.levels.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.levels.len();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                if let Ok(digit) = c.to_string().parse::<usize>()
                    && digit >= 1
                    && digit <= self.levels.len()
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
        for (index, level) in self.levels.iter().enumerate() {
            let number = index + 1;
            let marker = if *level == self.current {
                theme.paint(Token::Success, "  ← current")
            } else {
                String::new()
            };
            if index == self.selected {
                body.push(format!(
                    "{} {}{marker}",
                    theme.bold(Token::Accent, &format!("▶ {number}.")),
                    theme.bold(Token::TextStrong, level)
                ));
            } else {
                body.push(format!(
                    "  {} {}{marker}",
                    theme.paint(Token::TextDim, &format!("{number}.")),
                    theme.paint(Token::Text, level)
                ));
            }
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "↑/↓ select · 1-9 choose · ↵ apply · esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
