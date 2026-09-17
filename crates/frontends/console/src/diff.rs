//! Clustered LCS diff rendering for file edits.
//!
//! Produces the reference-style diff card: a `+N -M <path>` header
//! (strong colors, bold), clusters of changed lines with three context
//! lines between, `… N unchanged lines …` elision rows, and gutter
//! numbering. `incomplete` suppresses trailing deletions so a
//! streaming diff never flashes red before new text arrives.

use crate::theme::{self, Token};

/// Context lines shown around each change cluster.
pub const CONTEXT_LINES: usize = 3;
/// Per-side line budget for the O(n*m) LCS table; larger inputs render
/// as a truncated summary instead of computing a diff.
pub const MAX_DIFF_LINES: usize = 2000;
/// Lines kept per cluster before elision (collapsed preview).
pub const MAX_CLUSTER_LINES: usize = 10;

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

/// Render the clustered, colored diff card (header row included).
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

    let mut body: Vec<String> = Vec::new();
    let mut old_number = 0usize;
    let mut new_number = 0usize;
    for (index, row) in rows.iter().enumerate() {
        let keep = marked[index];
        match row {
            DiffRow::Context(_) => {
                old_number += 1;
                new_number += 1;
                if keep {
                    body.push(format!(
                        "{}  {}",
                        gutter(old_number, new_number),
                        theme.paint(Token::Text, row_text(row))
                    ));
                }
            }
            DiffRow::Added(_) => {
                new_number += 1;
                if keep {
                    body.push(format!(
                        "{}  {}",
                        gutter(old_number, new_number),
                        theme.paint(Token::DiffAdded, &format!("+ {}", row_text(row)))
                    ));
                }
            }
            DiffRow::Removed(_) => {
                old_number += 1;
                if keep {
                    body.push(format!(
                        "{}  {}",
                        gutter(old_number, new_number),
                        theme.paint(Token::DiffRemoved, &format!("- {}", row_text(row)))
                    ));
                }
            }
        }
    }
    let gap_count = count_gaps(&marked);
    let mut result: Vec<String> = vec![out.remove(0)];
    let total_body = body.len();
    if total_body <= max_rows {
        result.extend(body);
    } else {
        result.extend(body[..max_rows].iter().cloned());
        result.push(theme.paint(
            Token::DiffMeta,
            &format!(
                "… {} more changed lines (ctrl+o to expand)",
                total_body - max_rows
            ),
        ));
    }
    if gap_count > 0 {
        // One elision row after the card body summarizing unchanged spans.
        result.push(theme.paint(
            Token::DiffMeta,
            &format!(
                "… {} unchanged segment{} collapsed",
                gap_count,
                if gap_count == 1 { "" } else { "s" }
            ),
        ));
    }
    result
}

fn row_text(row: &DiffRow) -> &str {
    match row {
        DiffRow::Context(text) | DiffRow::Added(text) | DiffRow::Removed(text) => text,
    }
}

/// Line-number gutter: dim, old/new numbers separated.
fn gutter(old: usize, new: usize) -> String {
    let theme = theme::current();
    theme.paint(Token::DiffGutter, &format!("{old:>4} {new:>4}"))
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

    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| tui_engine::width::strip_ansi(l))
            .collect()
    }

    #[test]
    fn simple_replacement_diff() {
        theme::set(theme::Theme::dark());
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
        theme::set(theme::Theme::dark());
        let lines = render("a\nb\nc", "a\nX\nc", Some("src/lib.rs"), false, 20);
        let plain = plain(&lines);
        assert!(plain[0].contains("+1"), "{plain:?}");
        assert!(plain[0].contains("-1"), "{plain:?}");
        assert!(plain[0].contains("src/lib.rs"), "{plain:?}");
        assert!(plain.iter().any(|l| l.contains("+ X")));
        assert!(plain.iter().any(|l| l.contains("- b")));
    }

    #[test]
    fn render_elides_unchanged_spans() {
        theme::set(theme::Theme::dark());
        let old = format!(
            "{}\nOLD\n{}",
            "ctx\n".repeat(10).trim(),
            "ctx\n".repeat(10).trim()
        );
        let new = old.replace("OLD", "NEW");
        let lines = render(&old, &new, None, false, 40);
        let plain = plain(&lines);
        assert!(
            plain.iter().any(|l| l.contains("unchanged segment")),
            "{plain:?}"
        );
    }

    #[test]
    fn render_caps_body_rows() {
        theme::set(theme::Theme::dark());
        let old = "a\nb\nc";
        let new = "1\n2\n3";
        let lines = render(old, new, None, false, 4);
        let plain = plain(&lines);
        assert!(
            plain.iter().any(|l| l.contains("more changed lines")),
            "{plain:?}"
        );
    }
}
