//! Clustered LCS diff rendering for file edits.
//!
//! Produces the reference-style diff card: a `+N -M <path>` header
//! (strong colors, bold), then a single-column unified body in the
//! code-frame style — a dim bar, one gutter per line (right-aligned old
//! and new line numbers, fixed width per card), one `+`/`-`/space
//! marker column, and one content column. Gutter cells stay blank on
//! the side a change does not touch; elided context runs render without
//! a gutter. `incomplete` suppresses trailing deletions so a streaming
//! diff never flashes red before new text arrives. Content truncation
//! and all padding are display-width aware (CJK counts as 2 cells), so
//! the gutter grid stays aligned for any input.

use crate::theme::{self, Token};
use tui_engine::width;

/// Context lines shown around each change cluster.
pub const CONTEXT_LINES: usize = 3;
/// Per-side line budget for the O(n*m) LCS table; larger inputs render
/// as a truncated summary instead of computing a diff.
pub const MAX_DIFF_LINES: usize = 2000;

/// One diff row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    /// Unchanged context line.
    Context(String),
    /// Added line.
    Added(String),
    /// Removed line.
    Removed(String),
}

/// Compute the line diff between two texts via LCS dynamic programming.
/// `incomplete` drops trailing removals (streaming guard).
pub fn compute_rows(old: &str, new: &str, incomplete: bool) -> Vec<DiffRow> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // LCS table.
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, ai) in a.iter().enumerate() {
        for (j, bj) in b.iter().enumerate() {
            table[i + 1][j + 1] = if ai == bj {
                table[i][j] + 1
            } else {
                table[i][j + 1].max(table[i + 1][j])
            };
        }
    }
    // Backtrack to rows.
    let mut rows: Vec<DiffRow> = Vec::new();
    let mut i = a.len();
    let mut j = b.len();
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            rows.push(DiffRow::Context(a[i - 1].to_string()));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || table[i][j] == table[i][j - 1]) {
            rows.push(DiffRow::Added(b[j - 1].to_string()));
            j -= 1;
        } else {
            rows.push(DiffRow::Removed(a[i - 1].to_string()));
            i -= 1;
        }
    }
    rows.reverse();
    if incomplete {
        while matches!(rows.last(), Some(DiffRow::Removed(_))) {
            rows.pop();
        }
    }
    rows
}

/// Added/removed counts over the rows.
pub fn counts(rows: &[DiffRow]) -> (usize, usize) {
    let added = rows
        .iter()
        .filter(|r| matches!(r, DiffRow::Added(_)))
        .count();
    let removed = rows
        .iter()
        .filter(|r| matches!(r, DiffRow::Removed(_)))
        .count();
    (added, removed)
}

/// Render the clustered, colored unified diff card (header row included).
/// `max_rows` bounds the body; overflow gains an elision hint row.
/// Inputs over [`MAX_DIFF_LINES`] lines per side return an empty vec —
/// the O(n*m) LCS table must not allocate on the UI thread — and the
/// caller shows a summary instead.
pub fn render(
    old: &str,
    new: &str,
    path: Option<&str>,
    incomplete: bool,
    max_rows: usize,
    columns: usize,
) -> Vec<String> {
    if old.lines().count() > MAX_DIFF_LINES || new.lines().count() > MAX_DIFF_LINES {
        return Vec::new();
    }
    let theme = theme::current();
    let rows = compute_rows(old, new, incomplete);
    let (added, removed) = counts(&rows);
    let mut out = Vec::new();
    let header_path = path.unwrap_or("");
    let header = format!(
        "{} {} {}",
        theme.bold(Token::DiffAddedStrong, &format!("+{added}")),
        theme.bold(Token::DiffRemovedStrong, &format!("-{removed}")),
        theme.paint(Token::Text, header_path)
    );
    out.push(header);

    // Cluster: group changes, keep CONTEXT_LINES context around each,
    // elide long gaps. Track (row index, rendered) for the window.
    let mut marked: Vec<bool> = rows.iter().map(|_| false).collect();
    for (index, row) in rows.iter().enumerate() {
        if matches!(row, DiffRow::Added(_) | DiffRow::Removed(_)) {
            let start = index.saturating_sub(CONTEXT_LINES);
            let end = (index + CONTEXT_LINES + 1).min(rows.len());
            for marked_index in marked.iter_mut().take(end).skip(start) {
                *marked_index = true;
            }
        }
    }

    // Gutter width: the widest line number either side can reach, fixed
    // for the whole card so every body line shares one grid.
    let gutter_width = rows.len().max(1).to_string().len().max(2);
    // Body prefix: bar, two gutter cells, one space, marker, one space.
    let prefix_width = 2 + gutter_width + 1 + gutter_width + 1 + 1 + 1;
    let content_budget = columns.saturating_sub(prefix_width).max(8);

    let bar = theme.paint(Token::DiffGutter, "│ ");
    let total_body = marked.iter().filter(|m| **m).count();
    let mut body: Vec<String> = Vec::new();
    let mut old_number = 0usize;
    let mut new_number = 0usize;
    for (index, row) in rows.iter().enumerate() {
        let keep = marked[index];
        // The fixed grid: bar, old cell, new cell, marker, content.
        let (old_cell, new_cell, marker, token) = match row {
            DiffRow::Context(_) => {
                old_number += 1;
                new_number += 1;
                if !keep {
                    continue;
                }
                (
                    gutter_cell(old_number, gutter_width),
                    gutter_cell(new_number, gutter_width),
                    " ",
                    Token::Text,
                )
            }
            DiffRow::Added(_) => {
                new_number += 1;
                if !keep {
                    continue;
                }
                (
                    blank_cell(gutter_width),
                    gutter_cell(new_number, gutter_width),
                    "+",
                    Token::DiffAdded,
                )
            }
            DiffRow::Removed(_) => {
                old_number += 1;
                if !keep {
                    continue;
                }
                (
                    gutter_cell(old_number, gutter_width),
                    blank_cell(gutter_width),
                    "-",
                    Token::DiffRemoved,
                )
            }
        };
        body.push(format!(
            "{bar}{old_cell} {new_cell} {} {}",
            theme.paint(token, marker),
            theme.paint(token, &truncate_content(row_text(row), content_budget)),
        ));
        if body.len() >= max_rows && total_body > max_rows {
            // One elision row replaces the rest of the body.
            body.push(theme.paint(
                Token::DiffMeta,
                &format!(
                    "│ … {} more changed lines (ctrl+o to expand)",
                    total_body - max_rows
                ),
            ));
            break;
        }
    }
    out.extend(body);

    let gap_count = count_gaps(&marked);
    if gap_count > 0 {
        // One elision row after the card body summarizing unchanged spans.
        out.push(theme.paint(
            Token::DiffMeta,
            &format!(
                "… {} unchanged segment{} collapsed",
                gap_count,
                if gap_count == 1 { "" } else { "s" }
            ),
        ));
    }
    out
}

fn row_text(row: &DiffRow) -> &str {
    match row {
        DiffRow::Context(text) | DiffRow::Added(text) | DiffRow::Removed(text) => text,
    }
}

/// One right-aligned gutter cell, display-width padded.
fn gutter_cell(number: usize, total: usize) -> String {
    let theme = theme::current();
    let digits = number.to_string();
    let pad = total.saturating_sub(width::width(&digits));
    theme.paint(Token::DiffGutter, &format!("{}{digits}", " ".repeat(pad)))
}

/// The blank gutter cell for the side a change does not touch.
fn blank_cell(total: usize) -> String {
    " ".repeat(total)
}

/// Width-truncated content with an ellipsis when anything was cut.
fn truncate_content(text: &str, max: usize) -> String {
    if width::width(text) <= max {
        return text.to_string();
    }
    let cut = width::truncate_to_width(text, max.saturating_sub(1));
    format!("{cut}…")
}

/// Count runs of unmarked rows (gaps between clusters).
fn count_gaps(marked: &[bool]) -> usize {
    let mut gaps = 0;
    let mut in_gap = false;
    for m in marked {
        if !m {
            if !in_gap {
                gaps += 1;
                in_gap = true;
            }
        } else {
            in_gap = false;
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    /// Plain text of the rendered lines.
    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| tui_engine::width::strip_ansi(l))
            .collect()
    }

    #[test]
    fn simple_replacement_diff() {
        theme::set(theme::Theme::synthwave());
        let rows = compute_rows("a\nb\nc", "a\nX\nc", false);
        assert_eq!(
            rows,
            vec![
                DiffRow::Context("a".into()),
                DiffRow::Removed("b".into()),
                DiffRow::Added("X".into()),
                DiffRow::Context("c".into()),
            ]
        );
    }

    #[test]
    fn counts_additions_and_removals() {
        let rows = compute_rows("a\nb", "x\ny\nz", false);
        assert_eq!(counts(&rows), (3, 2));
    }

    #[test]
    fn incomplete_suppresses_trailing_removals() {
        // Mid-stream, the tail of the old text has no counterpart yet:
        // suppress it so previews never flash red.
        let rows = compute_rows("keep\nold-tail", "keep", true);
        assert!(
            !rows.iter().any(|r| matches!(r, DiffRow::Removed(_))),
            "no trailing deletions: {rows:?}"
        );
        let complete = compute_rows("keep\nold-tail", "keep", false);
        assert!(complete.iter().any(|r| matches!(r, DiffRow::Removed(_))));
    }

    #[test]
    fn render_header_shows_counts_and_path() {
        theme::set(theme::Theme::synthwave());
        let lines = render("a\nb\nc", "a\nX\nc", Some("src/lib.rs"), false, 20, 80);
        let plain = plain(&lines);
        assert!(plain[0].contains("+1"), "{plain:?}");
        assert!(plain[0].contains("-1"), "{plain:?}");
        assert!(plain[0].contains("src/lib.rs"), "{plain:?}");
        assert!(plain.iter().any(|l| l.ends_with("+ X")), "{plain:?}");
        assert!(plain.iter().any(|l| l.ends_with("- b")), "{plain:?}");
    }

    #[test]
    fn render_elides_unchanged_spans() {
        theme::set(theme::Theme::synthwave());
        let old = format!(
            "{}\nOLD\n{}",
            "ctx\n".repeat(10).trim(),
            "ctx\n".repeat(10).trim()
        );
        let new = old.replace("OLD", "NEW");
        let lines = render(&old, &new, None, false, 40, 80);
        let plain = plain(&lines);
        assert!(
            plain.iter().any(|l| l.contains("unchanged segment")),
            "{plain:?}"
        );
    }

    #[test]
    fn render_caps_body_rows() {
        theme::set(theme::Theme::synthwave());
        let lines = render("a\nb\nc", "1\n2\n3", None, false, 4, 80);
        let plain = plain(&lines);
        assert!(
            plain.iter().any(|l| l.contains("more changed lines")),
            "{plain:?}"
        );
    }

    /// The screenshot regression: a JS edit with CJK comments must
    /// render as one clean column — no second content column, no `│`
    /// separators beyond the leading frame bar, and every line's
    /// content starting at the same column.
    #[test]
    fn cjk_edit_renders_single_aligned_column() {
        theme::set(theme::Theme::synthwave());
        let old = "// 初始化配置\nconst size = 10;\n// 渲染循环\nfunction draw() {\n  requestAnimationFrame(draw);\n}\n// 结束\nexport default draw;\n";
        let new = "// 初始化配置\nconst size = 24;\n// 高分屏适配\nconst scale = 2;\n// 渲染循环\nfunction draw() {\n  requestAnimationFrame(draw);\n}\n// 结束\nexport default draw;\n";
        let lines = render(old, new, Some("src/render.js"), false, 40, 80);
        let plain = plain(&lines);
        let body: Vec<&String> = plain.iter().filter(|l| l.starts_with("│ ")).collect();
        assert!(!body.is_empty(), "diff body present: {plain:?}");
        // Gutter width per the render contract: digits of the total row
        // count, minimum 2.
        let w = compute_rows(old, new, false)
            .len()
            .max(1)
            .to_string()
            .len()
            .max(2);
        let marker_at = 2 * w + 2;
        for line in &body {
            // Exactly one frame bar, at the very start of the line.
            assert_eq!(
                line.matches('│').count(),
                1,
                "no stray column separators: {line:?}"
            );
            assert!(line.starts_with("│ "), "bar leads: {line:?}");
            // The marker column sits at one shared offset everywhere.
            let rest = line.trim_start_matches("│ ");
            let marker = rest.as_bytes().get(marker_at).copied();
            assert!(
                matches!(marker, Some(b'+') | Some(b'-') | Some(b' ')),
                "marker column at a shared offset: {line:?}"
            );
        }
        // Added and removed CJK content survives intact.
        assert!(plain.iter().any(|l| l.ends_with("+ const size = 24;")));
        assert!(plain.iter().any(|l| l.ends_with("- const size = 10;")));
        assert!(plain.iter().any(|l| l.contains("高分屏适配")));
    }

    /// Long lines truncate to the single content column instead of
    /// pushing a second column of content to a far-right position.
    #[test]
    fn long_lines_truncate_to_the_content_column() {
        theme::set(theme::Theme::synthwave());
        let long = "x".repeat(200);
        let lines = render(&long, &long, None, false, 40, 80);
        for line in plain(&lines) {
            assert!(
                tui_engine::width::width(&line) <= 80,
                "line fits the card: {line:?}"
            );
        }
        let lines = render("keep", &format!("keep\n{long}"), None, false, 40, 80);
        let added = plain(&lines)
            .into_iter()
            .find(|l| l.starts_with("│ ") && l.contains('+'))
            .expect("added row");
        assert!(added.contains('…'), "truncation marked: {added:?}");
    }

    /// Pure-addition and pure-deletion hunks keep the shared grid: the
    /// untouched side's gutter cell stays blank, never renumbered.
    #[test]
    fn pure_additions_and_deletions_share_the_grid() {
        theme::set(theme::Theme::synthwave());
        let lines = render(
            "keep\nend",
            "keep\nnew-1\nnew-2\nnew-3\nend",
            None,
            false,
            40,
            80,
        );
        let added_view = plain(&lines);
        let added_rows: Vec<&String> = added_view.iter().filter(|l| l.contains("+ new")).collect();
        assert_eq!(added_rows.len(), 3, "all additions shown: {added_view:?}");
        for row in &added_rows {
            let rest = row.strip_prefix("│ ").expect("bar leads");
            // Old cell blank, new cell numbered: `  {n} +`.
            assert!(
                rest[..2].chars().all(char::is_whitespace),
                "blank old cell: {row:?}"
            );
            assert!(
                !rest[3..5].chars().all(char::is_whitespace),
                "new number present: {row:?}"
            );
        }
        let lines = render("keep\na\nb\nc\nd\nend", "keep\nend", None, false, 40, 80);
        let removed_view = plain(&lines);
        assert_eq!(
            removed_view
                .iter()
                .filter(|l| l.ends_with("- a")
                    || l.ends_with("- b")
                    || l.ends_with("- c")
                    || l.ends_with("- d"))
                .count(),
            4,
            "all deletions shown: {removed_view:?}"
        );
    }

    /// The gutter is one fixed-width grid per card: every body line's
    /// marker lands at the same display column.
    #[test]
    fn gutter_width_is_uniform_across_rows() {
        theme::set(theme::Theme::synthwave());
        let mut old = String::from("head\n");
        old.extend((0..9).map(|i| format!("line {i}\n")));
        let mut new = String::from("head\n");
        new.extend((0..120).map(|i| format!("row {i}\n")));
        let lines = render(old.trim_end(), new.trim_end(), None, false, 200, 80);
        let w = compute_rows(old.trim_end(), new.trim_end(), false)
            .len()
            .max(1)
            .to_string()
            .len()
            .max(2);
        let mut marker_columns = std::collections::HashSet::new();
        for line in plain(&lines) {
            let Some(rest) = line.strip_prefix("│ ") else {
                continue;
            };
            if let Some(marker) = rest.as_bytes().get(2 * w + 2) {
                marker_columns.insert(*marker);
            }
        }
        assert_eq!(
            marker_columns,
            std::collections::HashSet::from([b'+', b'-', b' ']),
            "markers share one column: {marker_columns:?}"
        );
    }
}
