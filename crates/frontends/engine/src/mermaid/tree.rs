//! The indented-tree kinds: mindmap and timeline, sharing one tree
//! renderer.

use super::starts_with_keyword;
use super::truncate_width;
use crate::width;

// ---------------------------------------------------------------------------
// mindmap / timeline — an indented tree renderer
// ---------------------------------------------------------------------------

/// A tree node: display text plus children.
pub(super) struct TreeNode {
    text: String,
    children: Vec<TreeNode>,
}

/// Parse a mindmap: indentation (two spaces per level) defines the
/// tree; the first content line is the root. `::icon(...)` metadata
/// lines are skipped.
pub(super) fn parse_mindmap(source: &str) -> Option<TreeNode> {
    let mut stack: Vec<(usize, TreeNode)> = Vec::new();
    let mut count = 0usize;
    for raw in source.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim().starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line.trim(), "mindmap") {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("::") || trimmed.starts_with("%%%") {
            continue;
        }
        if count >= 200 {
            return None;
        }
        let indent = line.len() - line.trim_start().len();
        let depth = indent / 2;
        let node = TreeNode {
            text: truncate_width(&peel_shape(trimmed), 48),
            children: Vec::new(),
        };
        count += 1;
        // Attach deeper-or-equal nodes to their parent, then descend.
        if stack.is_empty() {
            stack.push((0, node));
            continue;
        }
        // Pop first, restore on a shallower top: the depth guard and
        // the pop share one `while let`, so no unwrap pairs the guard
        // with a second stack access.
        while let Some((top_depth, popped)) = stack.pop() {
            if top_depth < depth {
                stack.push((top_depth, popped));
                break;
            }
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(popped),
                None => {
                    stack.push((0, popped));
                    break;
                }
            }
        }
        stack.push((depth, node));
    }
    // Drain the stack: the last popped node is the root.
    let mut root: Option<TreeNode> = None;
    while let Some((_, node)) = stack.pop() {
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(node),
            None => root = Some(node),
        }
    }
    root
}

/// Peel wrapping shape delimiters from a mindmap node:
/// `root((mind))` → `mind`, `[item]` → `item`. A leading identifier
/// strips only when the remainder is fully wrapped (`root((mind))`),
/// so plain text with a stray bracket stays intact.
fn peel_shape(text: &str) -> String {
    let mut text = text.trim();
    if let Some(pos) = text.find(['(', '[', '{'])
        && pos > 0
    {
        let opener = text.as_bytes()[pos];
        let closer = match opener {
            b'(' => b')',
            b'[' => b']',
            _ => b'}',
        };
        if text.as_bytes()[text.len() - 1] == closer {
            text = &text[pos..];
        }
    }
    loop {
        let bytes = text.as_bytes();
        if bytes.len() < 2 {
            break;
        }
        let close = match bytes[0] {
            b'(' => b')',
            b'[' => b']',
            b'{' => b'}',
            _ => break,
        };
        if bytes[bytes.len() - 1] != close {
            break;
        }
        text = text[1..text.len() - 1].trim();
        if text.is_empty() {
            break;
        }
    }
    text.to_string()
}

/// Render a tree as box-drawing connector lines; `None` when a row
/// exceeds `columns`.
pub(super) fn render_tree(root: &TreeNode, columns: usize) -> Option<Vec<String>> {
    let mut lines = Vec::new();
    if !root.text.is_empty() {
        lines.push(root.text.clone());
    }
    walk_tree(root, "", &mut lines);
    if lines.is_empty() || lines.iter().any(|l| width::width(l) > columns) {
        return None;
    }
    Some(lines)
}

/// Recursive tree walk: `├─`/`└─` connectors with `│` continuations.
fn walk_tree(node: &TreeNode, prefix: &str, out: &mut Vec<String>) {
    let last = node.children.len().saturating_sub(1);
    for (index, child) in node.children.iter().enumerate() {
        let connector = if index == last { "└─ " } else { "├─ " };
        out.push(format!("{prefix}{connector}{}", child.text));
        let continuation = if index == last { "   " } else { "│  " };
        walk_tree(child, &format!("{prefix}{continuation}"), out);
    }
}

/// Parse a timeline: `period : event : event` rows, with `: event`
/// continuations appending to the previous period.
pub(super) fn parse_timeline(source: &str) -> Option<TreeNode> {
    let mut title = String::new();
    let mut periods: Vec<TreeNode> = Vec::new();
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "timeline") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = line["title".len()..].trim().to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix(':') {
            let event = rest.trim();
            if event.is_empty() {
                return None;
            }
            periods.last_mut()?.children.push(TreeNode {
                text: truncate_width(event, 48),
                children: Vec::new(),
            });
            continue;
        }
        let (period, events) = match line.split_once(':') {
            Some((period, events)) => (period.trim(), events),
            None => (line, ""),
        };
        if period.is_empty() {
            return None;
        }
        let mut node = TreeNode {
            text: truncate_width(period, 48),
            children: Vec::new(),
        };
        for event in events.split(':') {
            let event = event.trim();
            if !event.is_empty() {
                node.children.push(TreeNode {
                    text: truncate_width(event, 48),
                    children: Vec::new(),
                });
            }
        }
        periods.push(node);
        if periods.len() > 60 {
            return None;
        }
    }
    if periods.is_empty() {
        return None;
    }
    Some(TreeNode {
        text: title,
        children: periods,
    })
}
