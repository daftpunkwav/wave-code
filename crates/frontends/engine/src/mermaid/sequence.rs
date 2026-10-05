//! sequenceDiagram: participant lanes with one label-plus-arrow row
//! pair per message.

use std::collections::HashMap;

use super::layout::RowCanvas;
use crate::width;

// ---------------------------------------------------------------------------
// sequenceDiagram (unchanged geometry), pie, gantt
// ---------------------------------------------------------------------------

/// A parsed sequence diagram: participant display names and messages
/// `(from, to, label, dotted)` by participant index.
pub(super) struct Sequence {
    names: Vec<String>,
    messages: Vec<(usize, usize, String, bool)>,
}

/// Upper bound on rendered messages: each message draws two canvas
/// rows, so the cap keeps oversized sources on the source view like
/// the other layouts' row budgets.
const MAX_SEQUENCE_MESSAGES: usize = 200;

/// Parse a sequence diagram; `None` for unsupported constructs
/// (activations, notes, loops) and malformed messages.
pub(super) fn parse_sequence(source: &str) -> Option<Sequence> {
    let mut names: Vec<String> = Vec::new();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut messages = Vec::new();
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let keyword = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match keyword.as_str() {
            "sequencediagram" => {}
            "participant" | "actor" => {
                let rest = line[line.find(char::is_whitespace)?..].trim();
                let (id, name) = match rest.split_once(" as ") {
                    Some((id, name)) => (id.trim(), name.trim()),
                    None => (rest, rest),
                };
                if id.is_empty() || name.is_empty() {
                    return None;
                }
                if let Some(&index) = ids.get(id) {
                    // Re-declaration: first name wins; nothing to do.
                    let _ = index;
                } else {
                    ids.insert(id.to_string(), names.len());
                    names.push(name.to_string());
                }
            }
            "auton" | "activate" | "deactivate" | "note" | "loop" | "alt" | "else" | "end"
            | "par" | "critical" | "box" => return None,
            _ => {
                // Message: `A->>B: text`, `A-->>B: text`, `A->B: text`.
                let (heads, label) = line.split_once(':')?;
                let label = label.trim().to_string();
                let (from, to, dotted) = if let Some((from, to)) = heads.split_once("-->>") {
                    (from, to, true)
                } else if let Some((from, to)) = heads.split_once("->>") {
                    (from, to, false)
                } else if let Some((from, to)) = heads.split_once("-->") {
                    (from, to, true)
                } else {
                    let (from, to) = heads.split_once("->")?;
                    (from, to, false)
                };
                // Mermaid creates participants implicitly from messages:
                // `A->>B: hi` with no participant lines renders both ends.
                for id in [from.trim(), to.trim()] {
                    if !ids.contains_key(id) {
                        ids.insert(id.to_string(), names.len());
                        names.push(id.to_string());
                    }
                }
                let from = ids[from.trim()];
                let to = ids[to.trim()];
                messages.push((from, to, label, dotted));
            }
        }
    }
    if names.is_empty() || messages.len() > MAX_SEQUENCE_MESSAGES {
        return None;
    }
    Some(Sequence { names, messages })
}

/// Lay a sequence diagram out: participant boxes in a row, one
/// label-plus-arrow row pair per message, lifelines at the boxes.
pub(super) fn layout_sequence(s: &Sequence, columns: usize) -> Option<Vec<String>> {
    if s.names.len() > 12 {
        return None;
    }
    let gap = 8usize;
    let widths: Vec<usize> = s
        .names
        .iter()
        .map(|name| width::width(name).max(4) + 2)
        .collect();
    let mut centers = Vec::with_capacity(s.names.len());
    let mut cursor = 0usize;
    for (index, w) in widths.iter().enumerate() {
        centers.push(cursor + w / 2);
        cursor += w;
        if index + 1 < widths.len() {
            cursor += gap;
        }
    }
    let total = cursor;
    if total > columns {
        return None;
    }

    let mut lines = Vec::new();
    // Participant boxes.
    let mut top = RowCanvas::new(total);
    let mut mid = RowCanvas::new(total);
    let mut bottom = RowCanvas::new(total);
    let mut offset = 0usize;
    for (index, name) in s.names.iter().enumerate() {
        let w = widths[index];
        top.put(offset, format!("┌{}┐", "─".repeat(w - 2)));
        bottom.put(offset, format!("└{}┘", "─".repeat(w - 2)));
        let label_w = width::width(name);
        let pad = w - 2 - label_w;
        mid.put(
            offset,
            format!(
                "│{}{name}{}│",
                " ".repeat(pad / 2),
                " ".repeat(pad - pad / 2)
            ),
        );
        offset += w + gap;
    }
    lines.push(top.render());
    lines.push(mid.render());
    lines.push(bottom.render());

    // Lifelines skip the span an arrow occupies.
    let lifeline = |row: &mut RowCanvas, skip: Option<(usize, usize)>| {
        for center in &centers {
            let in_span = skip.is_some_and(|(a, b)| *center > a && *center < b);
            if !in_span {
                row.put(*center, "│");
            }
        }
    };
    let mut row = RowCanvas::new(total);
    lifeline(&mut row, None);
    lines.push(row.render());

    for (from, to, label, dotted) in &s.messages {
        let (a, b) = (centers[*from], centers[*to]);
        let (left, right) = if a < b { (a, b) } else { (b, a) };
        // Label row: centered over the span between the two centers.
        let mut label_row = RowCanvas::new(total);
        lifeline(&mut label_row, Some((left, right)));
        if left + 1 < right {
            let label_w = width::width(label);
            let room = right - left - 1;
            if label_w <= room {
                let col = left + 1 + (room - label_w) / 2;
                label_row.put(col, label.clone());
            } else {
                // Label wider than the span: place at the left edge and
                // let the canvas clip what follows. An overflowing label
                // never reaches this path — the parse-level fit contract
                // falls the whole diagram back instead.
                label_row.put(left + 1, label.clone());
            }
        } else {
            label_row.put(right + 1, label.clone());
        }
        lines.push(label_row.render());
        // Arrow row.
        let mut arrow_row = RowCanvas::new(total);
        lifeline(&mut arrow_row, Some((left, right)));
        if left == right {
            // Self message: a loop marker instead of a zero-width arrow.
            arrow_row.put(left + 1, "↻");
        } else {
            let fill = if *dotted { "┄" } else { "─" };
            for col in left..right {
                arrow_row.put(col, fill);
            }
            // The arrowhead sits on the receiver's lifeline: `A->>B` points
            // right, `B-->>A` points left.
            if a < b {
                arrow_row.put(right, "▶");
            } else {
                arrow_row.put(left, "◀");
            }
        }
        lines.push(arrow_row.render());
    }
    Some(lines)
}
