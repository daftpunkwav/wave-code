//! Completion plumbing: providers, completion values, and trigger
//! detection for slash commands and `@` file mentions.
//!
//! Providers are synchronous by contract: the application layer feeds
//! them precomputed or lazily refreshed candidate sets, keeping this
//! crate free of any async runtime.

use crate::fuzzy;
use crate::select_list::{SelectItem, SelectList, SelectListStyle};

/// One completion the editor can insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Primary label shown in the popup.
    pub label: String,
    /// Optional dimmed description.
    pub description: Option<String>,
    /// Text inserted when accepted (replacing the active token).
    pub insert: String,
}

/// Source of completions for the editor.
pub trait CompletionProvider {
    /// Completions for the active token `token` (the text from the
    /// trigger character up to the cursor, without the trigger itself).
    /// Returns best matches first.
    fn complete(&self, token: &str) -> Vec<Completion>;
}

/// A provider backed by a static candidate list, ranked by fuzzy score.
pub struct FuzzyProvider {
    candidates: Vec<Completion>,
}

impl FuzzyProvider {
    /// A provider over fixed candidates (slash commands, skill names).
    pub fn new(candidates: Vec<Completion>) -> Self {
        Self { candidates }
    }
}

impl CompletionProvider for FuzzyProvider {
    fn complete(&self, token: &str) -> Vec<Completion> {
        let mut scored: Vec<(i64, &Completion)> = self
            .candidates
            .iter()
            .filter_map(|c| fuzzy::score(token, &c.label).map(|s| (s, c)))
            .collect();
        scored.sort_by_key(|(s, _)| *s);
        scored.into_iter().map(|(_, c)| c.clone()).collect()
    }
}

/// The active completion context for an input line: which trigger fired
/// and the token being completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    /// Byte offset of the trigger character in the input line.
    pub start: usize,
    /// Token text after the trigger character, up to the cursor.
    pub token: String,
}

/// Which trigger character opened the popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// Slash command at start of input (or after whitespace).
    Slash,
    /// File mention via `@`.
    Mention,
}

/// Detect an active trigger for `line` with the cursor at byte offset
/// `cursor`. A slash fires at start-of-line or after whitespace; a
/// mention fires at a token boundary (start or whitespace).
pub fn detect_trigger(line: &str, cursor: usize, kind: TriggerKind) -> Option<Trigger> {
    let trigger_char = match kind {
        TriggerKind::Slash => '/',
        TriggerKind::Mention => '@',
    };
    let bytes = line.as_bytes();
    let cursor = cursor.min(bytes.len());
    // Find the trigger character at or before the cursor, without
    // crossing a whitespace boundary.
    let mut start = None;
    let mut i = cursor;
    while i > 0 {
        let b = bytes[i - 1];
        if b == trigger_char as u8 {
            let boundary = i == 1 || bytes[i - 2].is_ascii_whitespace();
            if boundary {
                start = Some(i - 1);
            }
            break;
        }
        if b.is_ascii_whitespace() {
            break;
        }
        i -= 1;
    }
    let start = start?;
    if start == cursor {
        return None; // trigger char sits at the cursor: no token yet
    }
    let token = line[start + 1..cursor].to_string();
    if token.contains(' ') {
        return None;
    }
    Some(Trigger { start, token })
}

/// Drive a popup list from a trigger + provider: compute items and bind
/// them to a styled [`SelectList`].
pub struct AutocompletePopup {
    list: SelectList,
    completions: Vec<Completion>,
}

impl AutocompletePopup {
    /// A popup with themed list styles and a visible-row budget.
    pub fn new(style: SelectListStyle, max_visible: usize) -> Self {
        let mut list = SelectList::new(Vec::new(), style);
        list.set_max_visible(max_visible);
        Self {
            list,
            completions: Vec::new(),
        }
    }

    /// Recompute the popup for `trigger` against `provider`.
    pub fn update(&mut self, provider: &dyn CompletionProvider, trigger: Option<Trigger>) {
        self.completions = trigger
            .map(|t| provider.complete(&t.token))
            .unwrap_or_default();
        let items = self
            .completions
            .iter()
            .map(|c| SelectItem {
                label: c.label.clone(),
                description: c.description.clone(),
            })
            .collect();
        self.list.set_items(items);
    }

    /// Override the visible-row budget of the underlying list.
    pub fn set_max_visible(&mut self, max: usize) {
        self.list.set_max_visible(max);
    }

    /// Borrow the underlying list (rendering + selection movement).
    pub fn list(&self) -> &SelectList {
        &self.list
    }

    /// Mutably borrow the underlying list.
    pub fn list_mut(&mut self) -> &mut SelectList {
        &mut self.list
    }

    /// True when the popup has nothing to show.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The completion to apply for the current selection, if any.
    pub fn selected_completion(&self) -> Option<&Completion> {
        self.completions.get(self.list.selected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Style;
    use crate::component::Component;
    use crate::width::strip_ansi;

    fn provider() -> FuzzyProvider {
        FuzzyProvider::new(vec![
            Completion {
                label: "help".into(),
                description: Some("show help".into()),
                insert: "/help ".into(),
            },
            Completion {
                label: "model".into(),
                description: None,
                insert: "/model ".into(),
            },
        ])
    }

    #[test]
    fn slash_triggers_at_start_only() {
        assert!(detect_trigger("/he", 3, TriggerKind::Slash).is_some());
        assert!(detect_trigger("say /he", 7, TriggerKind::Slash).is_some());
        assert!(detect_trigger("abc/he", 6, TriggerKind::Slash).is_none());
        assert!(detect_trigger("/he llo", 7, TriggerKind::Slash).is_none());
    }

    #[test]
    fn mention_triggers_mid_text() {
        let trigger = detect_trigger("look @src/ma", 12, TriggerKind::Mention).unwrap();
        assert_eq!(trigger.token, "src/ma");
        assert!(detect_trigger("email a@b", 9, TriggerKind::Mention).is_none());
    }

    #[test]
    fn popup_ranks_and_selects() {
        let mut popup = AutocompletePopup::new(
            SelectListStyle {
                selected: Style::new(),
                label: Style::new(),
                description: Style::new(),
            },
            5,
        );
        popup.update(&provider(), detect_trigger("/m", 2, TriggerKind::Slash));
        assert!(!popup.is_empty());
        assert_eq!(popup.selected_completion().unwrap().label, "model");
        popup.update(&provider(), detect_trigger("/zz", 3, TriggerKind::Slash));
        assert!(popup.is_empty());
    }

    #[test]
    fn popup_renders_rows_from_provider() {
        let mut popup = AutocompletePopup::new(
            SelectListStyle {
                selected: Style::new(),
                label: Style::new(),
                description: Style::new(),
            },
            5,
        );
        popup.update(&provider(), detect_trigger("/", 1, TriggerKind::Slash));
        let rows = popup.list_mut().render(80);
        assert_eq!(strip_ansi(&rows[0]), "→ help        show help");
    }
}
