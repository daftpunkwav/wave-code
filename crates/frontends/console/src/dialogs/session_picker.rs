//! Session picker: fuzzy search over titles, cwd/all scoping,
//! Enter resumes.

use super::Answer;
use super::model_picker::PICKER_PAGE;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};
use tui_engine::width;

/// One row of the session picker: meta plus the rendered last-prompt
/// preview head.
pub struct SessionRow {
    /// Session id (resume target).
    pub id: String,
    /// Display title (or the id when untitled).
    pub title: String,
    /// Working directory of the session.
    pub cwd: String,
    /// Relative timestamp label (e.g. `5m ago`).
    pub age: String,
    /// Recorded turn count.
    pub turns: u32,
}

/// The session picker: fuzzy search over titles, cwd/all scoping,
/// Enter resumes.
pub struct SessionPickerDialog {
    pub(super) title: String,
    rows: Vec<SessionRow>,
    /// True scopes the list to the current cwd, false lists all.
    cwd_scoped: bool,
    current_cwd: String,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
}

impl SessionPickerDialog {
    /// Build the picker over `rows`; sessions from other directories
    /// stay reachable via the Ctrl+A scope toggle.
    pub fn new(rows: Vec<SessionRow>, current_cwd: &str) -> Self {
        let mut picker = Self {
            title: "Sessions".to_string(),
            rows,
            cwd_scoped: true,
            current_cwd: current_cwd.to_string(),
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker
    }

    /// Ctrl+A toggles cwd ↔ all scoping.
    fn toggle_scope(&mut self) {
        self.cwd_scoped = !self.cwd_scoped;
        self.selected = 0;
        self.refilter();
    }

    fn refilter(&mut self) {
        self.filtered = (0..self.rows.len())
            .filter(|index| {
                let row = &self.rows[*index];
                if self.cwd_scoped && row.cwd != self.current_cwd {
                    return false;
                }
                let query = self.query.trim();
                query.is_empty()
                    || tui_engine::fuzzy::score(query, &row.title).is_some()
                    || row.title.to_lowercase().contains(&query.to_lowercase())
            })
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up if !self.filtered.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.filtered.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down if !self.filtered.is_empty() => {
                self.selected = (self.selected + 1) % self.filtered.len();
                None
            }
            Key::Char('a') if event.mods.ctrl => {
                self.toggle_scope();
                None
            }
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                self.query.push(c);
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Enter => {
                let index = self.filtered.get(self.selected)?;
                Some(Answer::ResumeSession {
                    id: self.rows[*index].id.clone(),
                })
            }
            _ => None,
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        let scope = if self.cwd_scoped {
            "Sessions (this directory)"
        } else {
            "All sessions"
        };
        body.push(theme.bold(Token::Primary, scope));
        body.push(theme.paint(
            Token::TextDim,
            "type to search · ↑/↓ navigate · Ctrl+A toggle directory scope · ↵ resume · Esc cancel",
        ));
        body.push(String::new());
        let visible = self.filtered.len().min(PICKER_PAGE);
        // The window slides with the selection (see the model picker):
        // a fixed top-anchored page strands the highlighted row below
        // it, visually unreachable.
        let start = if self.selected >= visible {
            (self.selected + 1 - visible).min(self.filtered.len() - visible)
        } else {
            0
        };
        for offset in 0..visible {
            let index = start + offset;
            let entry = &self.rows[self.filtered[index]];
            let marker = if index == self.selected { "❯ " } else { "  " };
            let head = format!(
                "{marker}{}  {}  {} turns",
                width::truncate_to_width(&entry.title, columns.saturating_sub(24)),
                entry.age,
                entry.turns
            );
            if index == self.selected {
                body.push(theme.bold(Token::TextStrong, &head));
                body.push(theme.paint(Token::TextDim, &format!("    {}", entry.id)));
            } else {
                body.push(theme.paint(Token::Text, &head));
            }
        }
        if self.filtered.is_empty() {
            body.push(theme.paint(Token::TextDim, "No matching sessions"));
        } else if start + visible < self.filtered.len() {
            body.push(theme.paint(
                Token::TextDim,
                &format!("  ▼ {} more", self.filtered.len() - start - visible),
            ));
        }
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
