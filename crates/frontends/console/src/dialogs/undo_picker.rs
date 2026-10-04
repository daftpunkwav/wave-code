//! Rewind picker (double-Esc): the most recent user turns, newest
//! first.

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};
use tui_engine::width;

/// Rewind points offered in the picker.
pub const MAX_UNDO_ROWS: usize = 8;

/// One rewind point: how many turns to drop and the user message that
/// started the turn.
pub struct UndoRow {
    /// Whole turns dropped when this point is picked.
    pub turns: u32,
    /// First line of the user message that started the turn.
    pub label: String,
}

/// The rewind picker (double-Esc): the most recent user turns, newest
/// first. Picking a row drops that turn and everything after it.
pub struct UndoPickerDialog {
    pub(super) title: String,
    pub(super) rows: Vec<UndoRow>,
    selected: usize,
}

impl UndoPickerDialog {
    /// Build over the untrimmed dialogue; rows are newest-first and
    /// capped at [`MAX_UNDO_ROWS`]. Turn counts span every user turn —
    /// matching the kernel, which rewinds all of them — while only the
    /// label decides whether a row is shown, so a turn whose first line
    /// is blank never shifts the older rows' distances.
    pub fn new(history: &[crate::state::DialogueEntry]) -> Self {
        let rows: Vec<UndoRow> = history
            .iter()
            .filter(|entry| entry.from_user)
            .rev()
            .enumerate()
            .filter_map(|(back, entry)| {
                let label = entry.text.lines().next()?.trim();
                (!label.is_empty()).then(|| UndoRow {
                    turns: back as u32 + 1,
                    label: label.to_string(),
                })
            })
            .take(MAX_UNDO_ROWS)
            .collect();
        Self {
            title: "Rewind".to_string(),
            rows,
            selected: 0,
        }
    }

    /// True when there is nothing to offer.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up if !self.rows.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.rows.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down if !self.rows.is_empty() => {
                self.selected = (self.selected + 1) % self.rows.len();
                None
            }
            Key::Enter => {
                let row = self.rows.get(self.selected)?;
                Some(Answer::RewindTurns { turns: row.turns })
            }
            _ => None,
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        body.push(theme.paint(
            Token::TextDim,
            "rewind to before this turn · ↑/↓ navigate · ↵ rewind · Esc cancel",
        ));
        body.push(String::new());
        for (row, point) in self.rows.iter().enumerate() {
            let marker = if row == self.selected { "❯ " } else { "  " };
            let turns = match point.turns {
                1 => "1 turn".to_string(),
                n => format!("{n} turns"),
            };
            let head = format!(
                "{marker}{}  ({})",
                width::truncate_to_width(&point.label, columns.saturating_sub(16)),
                turns
            );
            if row == self.selected {
                body.push(theme.bold(Token::TextStrong, &head));
            } else {
                body.push(theme.paint(Token::Text, &head));
            }
        }
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
