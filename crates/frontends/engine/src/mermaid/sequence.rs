//! sequenceDiagram: participant lanes with one label-plus-arrow row
//! pair per message, boxed notes across the lanes, and tolerated
//! activation directives.

use std::collections::HashMap;

use super::layout::RowCanvas;
use super::truncate_width;
use crate::width;

// ---------------------------------------------------------------------------
// sequenceDiagram (unchanged geometry), pie, gantt
// ---------------------------------------------------------------------------

/// One interleaved row-pair of a sequence diagram: a message arrow or
/// a boxed note spanning participant lanes.
pub(super) enum SequenceItem {
    Message {
        from: usize,
        to: usize,
        label: String,
        dotted: bool,
    },
    Note {
        over: Vec<usize>,
        side: NoteSide,
        text: String,
    },
}

/// Placement of a note relative to its anchor participants.
#[derive(Debug, Clone, Copy)]
pub(super) enum NoteSide {
    /// Spanning the anchor lanes.
    Over,
    /// Hanging left of the anchor lane.
    Left,
    /// Hanging right of the anchor lane.
    Right,
}

/// A parsed sequence diagram: participant display names and the
/// interleaved message/note items by participant index.
pub(super) struct Sequence {
    names: Vec<String>,
    items: Vec<SequenceItem>,
}

/// Upper bound on rendered messages: each message draws two canvas
/// rows, so the cap keeps oversized sources on the source view like
/// the other layouts' row budgets.
const MAX_SEQUENCE_MESSAGES: usize = 200;

/// Parse a sequence diagram; `None` for unsupported constructs
/// (alt/loop frames, participant boxes) and malformed messages.
/// Activation bookkeeping (`activate`/`deactivate`, the `+`/`-`
/// markers inside message heads) and `auton` are presentation-only
/// and tolerated as no-ops.
pub(super) fn parse_sequence(source: &str) -> Option<Sequence> {
    let mut names: Vec<String> = Vec::new();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut items = Vec::new();
    // Register an implicit participant (Mermaid creates participants
    // from messages and notes on first use).
    let intern =
        |ids: &mut HashMap<String, usize>, names: &mut Vec<String>, id: &str| -> Option<usize> {
            if let Some(&index) = ids.get(id) {
                return Some(index);
            }
            if id.is_empty() {
                return None;
            }
            let index = names.len();
            ids.insert(id.to_string(), index);
            names.push(id.to_string());
            Some(index)
        };
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
                // Re-declaration: the first name wins.
                if !ids.contains_key(id) {
                    ids.insert(id.to_string(), names.len());
                    names.push(name.to_string());
                }
            }
            "auton" | "autonumber" | "activate" | "deactivate" => {}
            "note" => {
                let rest = line["note".len()..].trim();
                let lower = rest.to_ascii_lowercase();
                let (side, head) = if lower.starts_with("over") {
                    (NoteSide::Over, &rest["over".len()..])
                } else if lower.starts_with("left of") {
                    (NoteSide::Left, &rest["left of".len()..])
                } else if lower.starts_with("right of") {
                    (NoteSide::Right, &rest["right of".len()..])
                } else {
                    return None;
                };
                let (target_list, text) = head.trim().split_once(':')?;
                let text = text.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                let mut over = Vec::new();
                for id in target_list.split(',') {
                    over.push(intern(&mut ids, &mut names, id.trim())?);
                }
                if over.is_empty() {
                    return None;
                }
                items.push(SequenceItem::Note {
                    over,
                    side,
                    text: truncate_width(&text, 48),
                });
            }
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
                // Activation markers ride the target (`A->>+B`): strip
                // them so the participant id stays canonical.
                let to = to.trim().trim_start_matches(['+', '-']);
                let from = from.trim();
                let from = intern(&mut ids, &mut names, from)?;
                let to = intern(&mut ids, &mut names, to)?;
                items.push(SequenceItem::Message {
                    from,
                    to,
                    label,
                    dotted,
                });
                if items.len() > MAX_SEQUENCE_MESSAGES {
                    return None;
                }
            }
        }
    }
    if names.is_empty() || items.len() > MAX_SEQUENCE_MESSAGES {
        return None;
    }
    Some(Sequence { names, items })
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

    for item in &s.items {
        match item {
            SequenceItem::Message {
                from,
                to,
                label,
                dotted,
            } => {
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
                        // Label wider than the span: place at the left edge. This
                        // arm is unreachable in practice — a label that overflows
                        // the frame fails the layout's fit check first, and the
                        // whole diagram falls back rather than rendering clipped
                        // rows (pinned by overflowing_labels_fall_back_to_none).
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
            SequenceItem::Note { over, side, text } => {
                // The note box hangs beside the anchor lane (Left /
                // Right) or spans the anchor lanes (Over), sized to its
                // text: two rows whose lifelines skip the box span.
                let anchor = centers[over[0]];
                let text_w = width::width(text);
                // The box hugs its text with room for the `┌ ` prefix
                // and the closing dash and corner.
                let box_w = text_w + 6;
                let (box_left, box_right) = match side {
                    NoteSide::Over => {
                        let (lo, hi) = over
                            .iter()
                            .map(|&p| centers[p])
                            .fold((usize::MAX, 0), |(a, b), c| (a.min(c), b.max(c)));
                        let mid = (lo + hi) / 2;
                        let half = (box_w.max(hi - lo + 1)) / 2;
                        (mid.saturating_sub(half), mid + half)
                    }
                    NoteSide::Left => (anchor.saturating_sub(2 + box_w), anchor.saturating_sub(2)),
                    NoteSide::Right => (
                        anchor + 2,
                        (anchor + 2 + box_w).min(total.saturating_sub(1)),
                    ),
                };
                if box_right <= box_left + 3 || box_right >= total {
                    return None; // no room to draw the box
                }
                let inner = box_right - box_left - 1;
                // The text centers on the top rule between a dash and
                // a space on each side.
                let shown = truncate_width(text, inner.saturating_sub(4));
                let side = (inner.saturating_sub(2) - width::width(&shown)) / 2;
                let mut top = String::from("┌");
                top.push_str(&"─".repeat(side));
                top.push(' ');
                top.push_str(&shown);
                top.push(' ');
                while width::width(&top) < box_right - box_left {
                    top.push('─');
                }
                top.push('┐');
                let mut label_row = RowCanvas::new(total);
                let mut bottom_row = RowCanvas::new(total);
                lifeline(&mut label_row, Some((box_left, box_right)));
                lifeline(&mut bottom_row, Some((box_left, box_right)));
                label_row.put(box_left, top);
                bottom_row.put(
                    box_left,
                    format!("└{}┘", "─".repeat(inner.saturating_sub(1))),
                );
                lines.push(label_row.render());
                lines.push(bottom_row.render());
            }
        }
    }
    Some(lines)
}
