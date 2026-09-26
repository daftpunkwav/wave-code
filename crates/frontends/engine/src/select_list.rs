//! Select list component: the popup used by slash commands, file
//! mentions, model pickers, and dialogs.
//!
//! Layout: a primary column (labels), a gap, then a description column;
//! the selected row is highlighted with a pointer prefix. A footer shows
//! `(selected/total)` when the list scrolls.

use crate::color::Style;
use crate::component::{Component, Segment};
use crate::width;

/// One selectable entry.
#[derive(Debug, Clone)]
pub struct SelectItem {
    /// Primary label (command name, file path, option text).
    pub label: String,
    /// Dimmed description shown in the second column.
    pub description: Option<String>,
}

/// Visual styles for a select list, supplied by the themed app layer.
#[derive(Debug, Clone, Copy)]
pub struct SelectListStyle {
    /// Style applied to the selected row (pointer + label).
    pub selected: Style,
    /// Style applied to unselected labels.
    pub label: Style,
    /// Style applied to descriptions.
    pub description: Style,
}

/// A vertical list with keyboard-driven selection.
pub struct SelectList {
    items: Vec<SelectItem>,
    selected: usize,
    max_visible: usize,
    min_primary_column: usize,
    max_primary_column: usize,
    style: SelectListStyle,
}

impl SelectList {
    /// A list with at most `max_visible` rows shown at once (clamped to
    /// a sane range, matching the reference: 3..=20).
    pub fn new(items: Vec<SelectItem>, style: SelectListStyle) -> Self {
        Self {
            items,
            selected: 0,
            max_visible: 5,
            min_primary_column: 12,
            max_primary_column: 32,
            style,
        }
    }

    /// Swap the styling (theme switches rebuild it).
    pub fn set_style(&mut self, style: SelectListStyle) {
        self.style = style;
    }

    /// Replace the item set, preserving the selection when possible.
    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.selected = self.selected.min(items.len().saturating_sub(1));
        self.items = items;
    }

    /// Set the visible row budget (clamped 3..=20).
    pub fn set_max_visible(&mut self, max: usize) {
        self.max_visible = max.clamp(3, 20);
    }

    /// Override the primary column bounds (used by narrower popups).
    pub fn set_primary_column_bounds(&mut self, min: usize, max: usize) {
        self.min_primary_column = min;
        self.max_primary_column = max;
    }

    /// The selected index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Borrow the selected item.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.items.get(self.selected)
    }

    /// Move the selection up with wrap-around.
    pub fn select_previous(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Move the selection down with wrap-around.
    pub fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    /// Item count.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when there are no items.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn primary_column(&self) -> usize {
        let widest = self
            .items
            .iter()
            .map(|item| width::width(&item.label))
            .max()
            .unwrap_or(0);
        widest.clamp(self.min_primary_column, self.max_primary_column)
    }
}

impl Component for SelectList {
    fn render(&mut self, width_budget: usize) -> Segment {
        if self.items.is_empty() {
            return Segment::new(vec![self.style.description.paint("  No matching commands")]);
        }
        let primary_column = self.primary_column();
        let total = self.items.len();
        let start = if self.selected >= self.max_visible {
            self.selected + 1 - self.max_visible
        } else {
            0
        };
        let end = (start + self.max_visible).min(total);
        let mut out = Vec::new();
        for index in start..end {
            let item = &self.items[index];
            let selected = index == self.selected;
            let mut row = String::new();
            if selected {
                row.push_str(&self.style.selected.paint("→ "));
                row.push_str(&self.style.selected.paint(&item.label));
            } else {
                row.push_str("  ");
                row.push_str(&self.style.label.paint(&item.label));
            }
            if let Some(description) = &item.description {
                let used = width::width(&row);
                let column = primary_column + 2;
                if column < width_budget {
                    row.push_str(&" ".repeat(column.saturating_sub(used)));
                    let budget = width_budget - column;
                    let description = width::truncate_to_width(description, budget);
                    row.push_str(&self.style.description.paint(&description));
                }
            }
            out.push(row);
        }
        if total > self.max_visible {
            out.push(
                self.style
                    .description
                    .paint(&format!("  ({}/{total})", self.selected + 1)),
            );
        }
        Segment::new(out)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::strip_ansi;

    fn plain_style() -> SelectListStyle {
        SelectListStyle {
            selected: Style::new(),
            label: Style::new(),
            description: Style::new(),
        }
    }

    #[test]
    fn selection_moves_with_wraparound() {
        let mut list = SelectList::new(
            vec![
                SelectItem {
                    label: "a".into(),
                    description: None,
                },
                SelectItem {
                    label: "b".into(),
                    description: None,
                },
            ],
            plain_style(),
        );
        list.select_next();
        assert_eq!(list.selected(), 1);
        list.select_next();
        assert_eq!(list.selected(), 0, "wraps to top");
        list.select_previous();
        assert_eq!(list.selected(), 1, "wraps to bottom");
    }

    #[test]
    fn renders_pointer_and_description_column() {
        let mut list = SelectList::new(
            vec![
                SelectItem {
                    label: "help".into(),
                    description: Some("show help".into()),
                },
                SelectItem {
                    label: "model".into(),
                    description: None,
                },
            ],
            plain_style(),
        );
        let rows = list.render(80);
        // Description column opens at primary_column + 2 (min 12).
        assert_eq!(strip_ansi(&rows[0]), "→ help        show help");
        assert_eq!(strip_ansi(&rows[1]), "  model");
    }

    #[test]
    fn scrolls_with_selection_and_shows_footer() {
        let items: Vec<SelectItem> = (0..6)
            .map(|i| SelectItem {
                label: format!("item{i}"),
                description: None,
            })
            .collect();
        let mut list = SelectList::new(items, plain_style());
        for _ in 0..5 {
            list.select_next();
        }
        let rows = list.render(80);
        assert_eq!(rows.len(), 6, "5 visible rows + footer");
        assert_eq!(strip_ansi(&rows[0]), "  item1", "window slides past item0");
        assert_eq!(strip_ansi(&rows[4]), "→ item5");
        assert_eq!(strip_ansi(&rows[5]), "  (6/6)");
    }

    #[test]
    fn empty_list_shows_placeholder() {
        let mut list = SelectList::new(Vec::new(), plain_style());
        assert_eq!(strip_ansi(&list.render(80)[0]), "  No matching commands");
    }
}
