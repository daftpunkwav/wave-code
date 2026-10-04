//! Model selector: provider tabs, type-to-search, and a thinking
//! level row.

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};

/// One selectable model entry shown in the model picker.
///
/// Pure display data; the harness derives it from config (`[models]`
/// aliases plus the configured default model) so this crate never
/// depends on the config layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntryView {
    /// Display label (alias or wire model name).
    pub label: String,
    /// Provider id the entry samples through.
    pub provider: String,
    /// Wire model name sent on `SetModel`.
    pub model: String,
    /// Reasoning-effort default for this entry, when known.
    pub effort: Option<String>,
}

/// Fuzzy-scored, order-stable filter over the model entries.
pub(crate) fn filter_indices(entries: &[ModelEntryView], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut scored: Vec<(i64, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let haystack = format!("{} {}", entry.label, entry.provider);
            tui_engine::fuzzy::score(query, &haystack).map(|score| (score, index))
        })
        .collect();
    // `fuzzy::score` ranks lower scores higher; ties keep entry order.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, index)| index).collect()
}

/// The model selector: provider tabs, type-to-search, and a thinking
/// level row. Enter saves the choice as the default; Alt+S applies it
/// to this session only; Esc cancels.
pub struct ModelPickerDialog {
    pub(super) title: String,
    entries: Vec<ModelEntryView>,
    /// Label of the live model (drives the `current` marker).
    current_label: String,
    /// Thinking levels offered for the current provider kind; empty
    /// hides the thinking row entirely (e.g. budget-driven Anthropic).
    thinking_levels: Vec<String>,
    /// Tab list: `All` first, then providers in entry order.
    tabs: Vec<String>,
    active_tab: usize,
    query: String,
    /// Filtered indices into `entries` under tab + query.
    filtered: Vec<usize>,
    pub(super) selected: usize,
    /// Thinking draft for the highlighted entry (one of the levels).
    thinking_draft: String,
    /// Live effort for the current model (marks the current segment).
    current_effort: Option<String>,
}

/// Visible list rows before the `N more` collapse.
pub(super) const PICKER_PAGE: usize = 8;

impl ModelPickerDialog {
    /// Build the picker over `entries`; `thinking_levels` empty hides
    /// the thinking row.
    pub fn new(
        entries: Vec<ModelEntryView>,
        current_label: String,
        current_effort: Option<String>,
        thinking_levels: Vec<String>,
    ) -> Self {
        let tabs = Self::tabs_of(&entries);
        let mut picker = Self {
            title: "Select a model".to_string(),
            entries,
            current_label,
            current_effort,
            thinking_levels,
            tabs,
            active_tab: 0,
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
            thinking_draft: String::new(),
        };
        picker.refilter();
        picker
    }

    /// `All` plus the unique provider ids in first-seen order.
    fn tabs_of(entries: &[ModelEntryView]) -> Vec<String> {
        let mut tabs = vec!["All".to_string()];
        for entry in entries {
            if !tabs.iter().any(|tab| tab == &entry.provider) {
                tabs.push(entry.provider.clone());
            }
        }
        tabs
    }

    /// Recompute the filtered list; keeps the cursor in range and
    /// re-seeds the thinking draft from the highlighted entry.
    fn refilter(&mut self) {
        self.filtered = filter_indices(&self.entries, &self.query)
            .into_iter()
            .filter(|index| {
                self.active_tab == 0 || self.entries[*index].provider == self.tabs[self.active_tab]
            })
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.thinking_draft = self
            .highlighted()
            .and_then(|entry| entry.effort.clone())
            .or_else(|| self.current_effort.clone())
            .or_else(|| self.thinking_levels.first().cloned())
            .unwrap_or_else(|| "off".to_string());
    }

    fn highlighted(&self) -> Option<&ModelEntryView> {
        self.filtered
            .get(self.selected)
            .map(|index| &self.entries[*index])
    }

    /// True when the highlighted entry is the live model.
    fn is_current(&self, index: usize) -> bool {
        self.filtered
            .get(index)
            .map(|entry_index| self.entries[*entry_index].label == self.current_label)
            .unwrap_or(false)
    }

    fn resolve(&self, session_only: bool) -> Option<Answer> {
        let entry = self.highlighted()?;
        let effort = if self.thinking_levels.is_empty() {
            None
        } else {
            let draft = self.thinking_draft.as_str();
            if draft.eq_ignore_ascii_case("off") {
                None
            } else {
                Some(draft.to_string())
            }
        };
        Some(Answer::ModelSelected {
            label: entry.label.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            effort,
            session_only,
        })
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
                self.refilter();
                None
            }
            Key::Down if !self.filtered.is_empty() => {
                self.selected = (self.selected + 1) % self.filtered.len();
                self.refilter();
                None
            }
            Key::Tab => {
                // BackTab arrives as Tab+shift; both cycle the tabs.
                let count = self.tabs.len();
                let step = if event.mods.shift { count - 1 } else { 1 };
                self.active_tab = (self.active_tab + step) % count;
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Left | Key::Right if !self.thinking_levels.is_empty() => {
                let position = self
                    .thinking_levels
                    .iter()
                    .position(|level| level.eq_ignore_ascii_case(&self.thinking_draft))
                    .unwrap_or(0);
                let count = self.thinking_levels.len();
                let step = if event.key == Key::Right {
                    1
                } else {
                    count - 1
                };
                self.thinking_draft = self.thinking_levels[(position + step) % count].clone();
                None
            }
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Char(c) if event.mods.alt && c == 's' => self.resolve(true),
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                self.query.push(c);
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Enter => self.resolve(false),
            _ => None,
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        // Tab strip: [All] provider-a provider-b …
        let mut strip = String::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            let label = if index == self.active_tab {
                format!("[{tab}]")
            } else {
                tab.clone()
            };
            if index == self.active_tab {
                strip.push_str(&theme.bold(Token::Primary, &label));
            } else {
                strip.push_str(&theme.paint(Token::TextDim, &label));
            }
            strip.push_str("  ");
        }
        body.push(strip.trim_end().to_string());
        body.push(String::new());
        // List window with the current marker and `N more` collapse.
        // The window slides with the selection: a fixed top-anchored
        // window leaves the highlighted row invisible once it moves
        // past the page.
        let visible = self.filtered.len().min(PICKER_PAGE);
        let start = if self.selected >= visible {
            (self.selected + 1 - visible).min(self.filtered.len() - visible)
        } else {
            0
        };
        for offset in 0..visible {
            let index = start + offset;
            let entry_index = self.filtered[index];
            let entry = &self.entries[entry_index];
            let marker = if index == self.selected { "❯ " } else { "  " };
            let mut line = format!("{marker}{}", entry.label);
            if self.is_current(index) {
                line.push_str(&theme.paint(Token::Success, "  ← current"));
            }
            if index == self.selected {
                body.push(theme.bold(Token::TextStrong, &line));
            } else {
                body.push(theme.paint(Token::Text, &line));
            }
        }
        if self.filtered.is_empty() {
            body.push(theme.paint(Token::TextDim, "No matches"));
        } else if start + visible < self.filtered.len() {
            body.push(theme.paint(
                Token::TextDim,
                &format!("  ▼ {} more", self.filtered.len() - start - visible),
            ));
        }
        if !self.query.trim().is_empty() {
            body.push(theme.paint(
                Token::TextDim,
                &format!(
                    "Search: {} · {}/{}",
                    self.query,
                    self.filtered.len(),
                    self.entries.len()
                ),
            ));
        }
        // Thinking row (hidden when the provider offers no levels).
        if !self.thinking_levels.is_empty() {
            body.push(String::new());
            body.push(theme.paint(Token::Text, "Thinking  (←/→ to switch)"));
            let mut row = String::from("   ");
            for level in &self.thinking_levels {
                let active = level.eq_ignore_ascii_case(&self.thinking_draft);
                let label = if active {
                    format!("[ {level} ]")
                } else {
                    format!("{level}  ")
                };
                if active {
                    row.push_str(&theme.bold(Token::Primary, &label));
                } else {
                    row.push_str(&theme.paint(Token::TextDim, &label));
                }
            }
            body.push(row);
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "Tab provider · type to search · ↑/↓ navigate · ↵ save · Alt+S session-only · Esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
