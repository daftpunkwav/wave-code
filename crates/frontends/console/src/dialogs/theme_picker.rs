//! Theme selector: built-in themes plus user themes, the live theme
//! marked.

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};
use tui_engine::width;

/// One selectable row of the theme picker: the theme name plus a
/// one-line description.
struct ThemeRow {
    name: String,
    description: String,
}

/// The theme selector (`/theme` with no args): the built-in themes
/// first, then the user themes from `~/.wavecode/themes/`, the live
/// theme marked. Enter applies, Esc cancels.
pub struct ThemePickerDialog {
    pub(super) title: String,
    rows: Vec<ThemeRow>,
    /// Name of the live theme (drives the `current` marker).
    current: String,
    pub(super) selected: usize,
}

impl ThemePickerDialog {
    /// Build the picker over the built-ins plus `custom` names paired
    /// with their file descriptions; the live theme is marked when its
    /// name matches a row.
    pub fn new(current: &str, custom: Vec<(String, Option<String>)>) -> Self {
        let builtin_row = |name: &str, fallback: &str| ThemeRow {
            name: name.to_string(),
            description: crate::theme::builtin::description(name)
                .unwrap_or(fallback)
                .to_string(),
        };
        let mut rows = vec![
            ThemeRow {
                name: "auto".to_string(),
                description: "follow the terminal background".to_string(),
            },
            builtin_row("dark", "the default dark identity"),
            builtin_row("deepwave", "the teal ocean identity"),
            builtin_row("light", "the light identity"),
        ];
        rows.extend(custom.into_iter().map(|(name, description)| ThemeRow {
            name,
            description: description.unwrap_or_else(|| "custom theme".to_string()),
        }));
        let selected = rows.iter().position(|row| row.name == current).unwrap_or(0);
        Self {
            title: "Select a theme".to_string(),
            rows,
            current: current.to_string(),
            selected,
        }
    }

    fn resolve(&self) -> Answer {
        Answer::ThemeSelected {
            name: self.rows[self.selected].name.clone(),
        }
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up => {
                self.selected = if self.selected == 0 {
                    self.rows.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.rows.len();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                if let Ok(digit) = c.to_string().parse::<usize>()
                    && digit >= 1
                    && digit <= self.rows.len()
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
        for (index, row) in self.rows.iter().enumerate() {
            let number = index + 1;
            let marker = if row.name == self.current {
                theme.paint(Token::Success, "  ← current")
            } else {
                String::new()
            };
            if index == self.selected {
                body.push(format!(
                    "{} {}{marker}",
                    theme.bold(Token::Accent, &format!("▶ {number}.")),
                    theme.bold(Token::TextStrong, &row.name)
                ));
                body.push(theme.paint(
                    Token::TextDim,
                    &format!(
                        "     {}",
                        width::truncate_to_width(&row.description, columns.saturating_sub(10))
                    ),
                ));
            } else {
                body.push(format!(
                    "  {} {}{marker}",
                    theme.paint(Token::TextDim, &format!("{number}.")),
                    theme.paint(Token::Text, &row.name)
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
