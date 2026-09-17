//! The todo panel: session task list rendered from `todowrite` calls.
//!
//! Mounted between the transcript and the queue pane. Collapsed it
//! shows at most [`COLLAPSED_ROWS`] rows — every in-progress entry plus
//! the earliest pending and latest done — folding the rest into a
//! distribution summary row; Ctrl+T expands when the list overflows.

use crate::state::{TodoEntry, TodoStatus};
use crate::theme::{self, Token};
use tui_engine::width;

/// Visible rows when collapsed (before the summary row).
pub const COLLAPSED_ROWS: usize = 5;

/// Render the todo panel for `todos`; empty input renders nothing (the
/// panel disappears until the next non-empty `todowrite`).
pub fn render_todos(todos: &[TodoEntry], expanded: bool, columns: usize) -> Vec<String> {
    if todos.is_empty() {
        return Vec::new();
    }
    let theme = theme::current();
    let mut out = Vec::new();
    out.push(theme.paint(Token::Border, &"─".repeat(columns.min(80))));
    out.push(format!("  {}", theme.bold(Token::Primary, "Todo")));

    let rows: Vec<String> = todos
        .iter()
        .map(|entry| todo_row(&theme, entry, columns))
        .collect();
    if expanded || rows.len() <= COLLAPSED_ROWS {
        let total = rows.len();
        out.extend(rows);
        if total > COLLAPSED_ROWS {
            out.push(format!(
                "  {}",
                theme.paint(
                    Token::TextDim,
                    &format!("all {total} items · ctrl+t to collapse")
                )
            ));
        }
        return out;
    }

    // Collapsed overflow: keep every in-progress entry, fill the rest
    // with the earliest pending and the latest done.
    let mut keep = Vec::with_capacity(COLLAPSED_ROWS);
    let mut kept = vec![false; todos.len()];
    for (index, entry) in todos.iter().enumerate() {
        if entry.status == TodoStatus::InProgress && keep.len() < COLLAPSED_ROWS {
            keep.push(index);
            kept[index] = true;
        }
    }
    for index in 0..todos.len() {
        if todos[index].status == TodoStatus::Pending && keep.len() < COLLAPSED_ROWS {
            keep.push(index);
            kept[index] = true;
        }
    }
    for index in (0..todos.len()).rev() {
        if todos[index].status == TodoStatus::Completed && keep.len() < COLLAPSED_ROWS {
            keep.push(index);
            kept[index] = true;
        }
    }
    keep.sort_unstable();
    for index in keep {
        out.push(rows[index].clone());
    }
    let hidden = todos.len() - COLLAPSED_ROWS;
    let done = todos
        .iter()
        .filter(|entry| entry.status == TodoStatus::Completed)
        .count();
    let pending = todos
        .iter()
        .filter(|entry| entry.status == TodoStatus::Pending)
        .count();
    out.push(format!(
        "  {}",
        theme.paint(
            Token::TextDim,
            &format!("… +{hidden} more ({done} done · {pending} pending) · ctrl+t to expand")
        )
    ));
    out
}

/// One todo row: `● in-progress` (bold primary), `✓ done` (success,
/// struck through), `○ pending` (dim).
fn todo_row(theme: &crate::theme::Theme, entry: &TodoEntry, columns: usize) -> String {
    let (marker, marker_token, text_style) = match entry.status {
        TodoStatus::InProgress => (
            crate::chrome::symbols::IN_PROGRESS,
            Token::Primary,
            theme.style(Token::Text).bold(),
        ),
        TodoStatus::Completed => (
            crate::chrome::symbols::CHECK,
            Token::Success,
            theme.style(Token::TextDim).strikethrough(),
        ),
        TodoStatus::Pending => (
            crate::chrome::symbols::PENDING,
            Token::TextDim,
            theme.style(Token::Text),
        ),
    };
    let body_width = columns.saturating_sub(6);
    let first = width::wrap_line(&entry.content, body_width)
        .into_iter()
        .next()
        .unwrap_or_default();
    format!(
        "  {} {}",
        theme.paint(marker_token, marker),
        text_style.paint(&first)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(content: &str, status: TodoStatus) -> TodoEntry {
        TodoEntry {
            content: content.to_string(),
            status,
        }
    }

    fn sample() -> Vec<TodoEntry> {
        vec![
            entry("first", TodoStatus::Completed),
            entry("second", TodoStatus::InProgress),
            entry("third", TodoStatus::Pending),
        ]
    }

    #[test]
    fn empty_list_renders_nothing() {
        theme::set(theme::Theme::dark());
        assert!(render_todos(&[], false, 60).is_empty());
    }

    #[test]
    fn rows_carry_status_markers() {
        theme::set(theme::Theme::dark());
        let lines: Vec<String> = render_todos(&sample(), false, 60)
            .into_iter()
            .map(|l| width::strip_ansi(&l))
            .collect();
        assert!(lines.iter().any(|l| l.contains("Todo")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("✓ first")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("● second")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("○ third")), "{lines:?}");
    }

    #[test]
    fn overflow_collapses_with_summary() {
        theme::set(theme::Theme::dark());
        let todos: Vec<TodoEntry> = (0..8)
            .map(|i| entry(&format!("task {i}"), TodoStatus::Pending))
            .chain(std::iter::once(entry("active", TodoStatus::InProgress)))
            .collect();
        let lines: Vec<String> = render_todos(&todos, false, 60)
            .into_iter()
            .map(|l| width::strip_ansi(&l))
            .collect();
        assert!(lines.iter().any(|l| l.contains("● active")), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("… +4 more (0 done · 8 pending) · ctrl+t to expand")),
            "{lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("task ")).count(),
            4,
            "earliest pending fill: {lines:?}"
        );
    }

    #[test]
    fn expansion_shows_everything() {
        theme::set(theme::Theme::dark());
        let todos: Vec<TodoEntry> = (0..8)
            .map(|i| entry(&format!("task {i}"), TodoStatus::Pending))
            .collect();
        let lines: Vec<String> = render_todos(&todos, true, 60)
            .into_iter()
            .map(|l| width::strip_ansi(&l))
            .collect();
        assert!(lines.iter().any(|l| l.contains("all 8 items")), "{lines:?}");
        assert_eq!(
            lines.iter().filter(|l| l.contains("task ")).count(),
            8,
            "{lines:?}"
        );
    }
}
