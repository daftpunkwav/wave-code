//! `/provider` opening view: catalog providers with model counts,
//! plus the new-provider row.

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};

/// The `/provider` opening view: providers already in the catalog
/// (with their model counts), plus the new-provider row. Picking one
/// opens the wizard seeded from it.
pub struct ProviderPickerDialog {
    pub(super) title: String,
    rows: Vec<(String, usize)>,
    selected: usize,
}

impl ProviderPickerDialog {
    /// Build over the catalog's distinct providers and model counts.
    pub fn new(rows: Vec<(String, usize)>) -> Self {
        Self {
            title: "Providers".to_string(),
            rows,
            selected: 0,
        }
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        let count = self.rows.len() + 1; // + the add-new row
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up => {
                self.selected = if self.selected == 0 {
                    count.saturating_sub(1)
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % count.max(1);
                None
            }
            Key::Enter => {
                let name = if self.selected < self.rows.len() {
                    Some(self.rows[self.selected].0.clone())
                } else {
                    None
                };
                Some(Answer::ProviderPicked { name })
            }
            _ => None,
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        for (index, (name, models)) in self.rows.iter().enumerate() {
            let line = format!("{name} · {models} model(s)");
            if index == self.selected {
                body.push(theme.bold(Token::TextStrong, &format!("❯ {line}")));
            } else {
                body.push(theme.paint(Token::Text, &format!("  {line}")));
            }
        }
        let add_row = self.rows.len();
        let line = "＋ add a new provider";
        if add_row == self.selected {
            body.push(theme.bold(Token::Accent, &format!("❯ {line}")));
        } else {
            body.push(theme.paint(Token::Primary, &format!("  {line}")));
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "↑/↓ choose · ↵ configure (an existing provider preseeds the wizard) · esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
