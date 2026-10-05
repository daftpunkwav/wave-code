//! The shared graph IR plus the flowchart parser and the
//! state-diagram adapter that lowers into it.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// The shared graph IR: nodes with label lines, styled edges, subgraph
// groups. Every node-and-edge kind lowers into this and rides `layout`.
// ---------------------------------------------------------------------------

/// Flow direction of a graph diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// Top-down bands.
    TD,
    /// Left-to-right columns.
    LR,
}

/// Edge rendering style: the vertical fill and the arrowhead glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EdgeStyle {
    /// `│ … ▼`
    Solid,
    /// `┆ … ▼` (dashed)
    Dotted,
    /// `┃ … ▼` (heavy)
    Thick,
    /// `│ … ▽` (hollow head: inheritance, realization)
    Hollow,
}

impl EdgeStyle {
    /// The vertical line glyph running down the band gaps.
    pub(super) fn vertical(self) -> &'static str {
        match self {
            EdgeStyle::Solid | EdgeStyle::Hollow => "│",
            EdgeStyle::Dotted => "┆",
            EdgeStyle::Thick => "┃",
        }
    }

    /// The arrowhead landing on the target band.
    pub(super) fn head(self) -> &'static str {
        match self {
            EdgeStyle::Hollow => "▽",
            _ => "▼",
        }
    }

    /// The horizontal run glyph along elbow rows.
    pub(super) fn horizontal(self) -> &'static str {
        match self {
            EdgeStyle::Dotted => "┄",
            _ => "─",
        }
    }
}

/// One directed edge between node indices.
pub(super) struct GraphEdge {
    pub(super) from: usize,
    pub(super) to: usize,
    pub(super) label: Option<String>,
    pub(super) style: EdgeStyle,
}

/// Upper bound on nodes in one diagram: the band/column layouts refuse
/// more, so refusing at creation keeps parsing (and per-group member
/// dedup) bounded on oversized sources.
const MAX_NODES: usize = 100;

/// The shared graph IR: node label lines in definition order, styled
/// edges between indices, and subgraph groups over member indices.
pub(super) struct Diagram {
    pub(super) direction: Direction,
    pub(super) labels: Vec<Vec<String>>,
    pub(super) edges: Vec<GraphEdge>,
    pub(super) groups: Vec<(String, Vec<usize>)>,
}

impl Diagram {
    pub(super) fn new(direction: Direction) -> Self {
        Self {
            direction,
            labels: Vec::new(),
            edges: Vec::new(),
            groups: Vec::new(),
        }
    }

    /// Register (or look up) a node by id; `lines` are its box body
    /// lines. A later body (a class/entity block after a relation
    /// referenced it bare) fills in a bare single-line node. `None` past
    /// [`MAX_NODES`]: the layouts reject that count anyway, and the cap
    /// bounds subgraph bookkeeping for pathological sources.
    pub(super) fn node(
        &mut self,
        ids: &mut HashMap<String, usize>,
        id: &str,
        lines: Vec<String>,
    ) -> Option<usize> {
        if let Some(&index) = ids.get(id) {
            if self.labels[index].len() <= 1 && lines.len() > 1 {
                self.labels[index] = lines;
            }
            return Some(index);
        }
        if self.labels.len() >= MAX_NODES {
            return None;
        }
        let index = self.labels.len();
        self.labels.push(lines);
        ids.insert(id.to_string(), index);
        Some(index)
    }

    /// Push a labeled edge.
    pub(super) fn edge(&mut self, from: usize, to: usize, label: Option<String>, style: EdgeStyle) {
        self.edges.push(GraphEdge {
            from,
            to,
            label,
            style,
        });
    }
}

/// Register `node` as a member of the innermost open subgraph.
fn note_member(groups: &mut [(String, Vec<usize>)], stack: &[usize], node: usize) {
    if let Some(&group) = stack.last() {
        let members = &mut groups[group].1;
        if !members.contains(&node) {
            members.push(node);
        }
    }
}

// ---------------------------------------------------------------------------
// flowchart / graph
// ---------------------------------------------------------------------------

/// Parse a flowchart source; `None` for unsupported diagrams.
pub(super) fn parse(source: &str) -> Option<Diagram> {
    let mut diagram = Diagram::new(Direction::TD);
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut direction_seen = false;
    let mut group_stack: Vec<usize> = Vec::new();
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
            "graph" | "flowchart" => {
                let dir = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_ascii_uppercase();
                diagram.direction = match dir.as_str() {
                    "TD" | "TB" => Direction::TD,
                    "LR" => Direction::LR,
                    _ => return None,
                };
                direction_seen = true;
            }
            "subgraph" => {
                let title = subgraph_title(line["subgraph".len()..].trim());
                diagram.groups.push((title, Vec::new()));
                group_stack.push(diagram.groups.len() - 1);
            }
            "end" => {
                group_stack.pop();
            }
            // Styling directives ride along ignored.
            "classdef" | "class" | "style" | "linkstyle" | "click" | "direction" => {}
            _ => parse_statement(line, &mut diagram, &mut ids, &group_stack)?,
        }
    }
    if !direction_seen || diagram.labels.is_empty() {
        return None;
    }
    Some(diagram)
}

/// The display title of a subgraph: `id [title]` takes the bracket
/// text, anything else is the raw remainder.
fn subgraph_title(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(open) = rest.find('[')
        && let Some(close) = rest.rfind(']')
        && close > open
    {
        let title = rest[open + 1..close].trim();
        if !title.is_empty() {
            return title.to_string();
        }
    }
    if rest.is_empty() {
        "subgraph".to_string()
    } else {
        rest.to_string()
    }
}

/// Parse one statement line: an edge chain or a bare node definition.
fn parse_statement(
    line: &str,
    diagram: &mut Diagram,
    ids: &mut HashMap<String, usize>,
    groups: &[usize],
) -> Option<()> {
    let (left, consumed) = scan_node_token(line)?;
    let from = diagram.node(ids, &node_id(left)?, vec![node_label(left)?])?;
    note_member(&mut diagram.groups, groups, from);
    let rest = line[consumed..].trim();
    if rest.is_empty() {
        return Some(()); // bare node definition
    }
    parse_edges(from, rest, diagram, ids, groups)
}

/// Parse an edge chain iteratively: each pass consumes one
/// `<arrow> <target>` pair and continues from its tail, so chains of
/// any length cannot exhaust the stack.
fn parse_edges(
    mut from: usize,
    mut rest: &str,
    diagram: &mut Diagram,
    ids: &mut HashMap<String, usize>,
    groups: &[usize],
) -> Option<()> {
    loop {
        let (label, consumed, style) = scan_arrow(rest)?;
        let after = rest[consumed..].trim_start();
        let (label, after) = pipe_label(after, label);
        let (target, tail_consumed) = scan_node_token(after)?;
        let to = diagram.node(ids, &node_id(target)?, vec![node_label(target)?])?;
        note_member(&mut diagram.groups, groups, to);
        diagram.edge(from, to, label, style);
        let tail = after[tail_consumed..].trim();
        if tail.is_empty() {
            return Some(());
        }
        from = to;
        rest = tail;
    }
}

/// Split a leading node token off `line`: the token ends at the first
/// top-level edge start — `-->`, `---`, `-.`, `==>` — so a labeled
/// arrow (`B -- 是 --> C`) cuts at the opening `--` instead of
/// swallowing the label into the node token.
fn scan_node_token(line: &str) -> Option<(&str, usize)> {
    let bytes = line.as_bytes();
    let mut depth = 0i32;
    for (index, byte) in bytes.iter().enumerate() {
        match *byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'-' | b'=' if depth == 0 => {
                let rest = &line[index..];
                let edge =
                    rest.starts_with("--") || rest.starts_with("-.") || rest.starts_with("==");
                if edge {
                    let token = line[..index].trim_end();
                    if token.is_empty() {
                        return None;
                    }
                    return Some((token, index));
                }
            }
            _ => {}
        }
    }
    let token = line.trim_end();
    (!token.is_empty()).then_some((token, line.len()))
}

/// Match one arrow (with optional inline label) at the start of
/// `line`; returns the label, the consumed width, and the edge style
/// the arrow spelling implies.
fn scan_arrow(line: &str) -> Option<(Option<String>, usize, EdgeStyle)> {
    let dotted = line.starts_with("-.");
    let thick = !dotted && line.starts_with("==");
    let plain = !dotted && !thick && line.starts_with("--");
    // Mermaid accepts runs of the link character (`---->`, `====>`,
    // `-..->`): consume the whole run instead of the shortest form, so a
    // longer spelling cannot leak `>` into a node id or slice past the
    // label start (an `===>` line panicked on `line[2..1]`).
    if dotted {
        let dots = line[1..].bytes().take_while(|&b| b == b'.').count();
        if line[1 + dots..].starts_with("->") {
            return Some((None, 1 + dots + 2, EdgeStyle::Dotted));
        }
        let end = line[2..].find(".->")? + 2;
        let label = line[2..end].trim();
        let label = (!label.is_empty()).then(|| label.to_string());
        return Some((label, end + 3, EdgeStyle::Dotted));
    }
    if thick {
        let run = line.bytes().take_while(|&b| b == b'=').count();
        if line[run..].starts_with('>') {
            return Some((None, run + 1, EdgeStyle::Thick));
        }
        let end = line[2..].find("==>")? + 2;
        let label = line[2..end].trim();
        let label = (!label.is_empty()).then(|| label.to_string());
        return Some((label, end + 3, EdgeStyle::Thick));
    }
    if plain {
        let run = line.bytes().take_while(|&b| b == b'-').count();
        if line[run..].starts_with('>') {
            return Some((None, run + 1, EdgeStyle::Solid));
        }
        let end = line.find("-->")?;
        let label = line[2..end].trim();
        let label = (!label.is_empty()).then(|| label.to_string());
        return Some((label, end + 3, EdgeStyle::Solid));
    }
    None
}

/// Absorb a `-->|label|` suffix: when `after` opens with a pipe, the
/// label between the pipes wins over the inline form.
fn pipe_label(after: &str, label: Option<String>) -> (Option<String>, &str) {
    let Some(rest) = after.strip_prefix('|') else {
        return (label, after);
    };
    match rest.find('|') {
        Some(end) => {
            let inner = rest[..end].trim();
            let label = (!inner.is_empty()).then(|| inner.to_string());
            (label, &rest[end + 1..])
        }
        None => (label, after),
    }
}

/// The node id of a token: the text before the shape opener.
fn node_id(token: &str) -> Option<String> {
    let token = token.trim();
    let id_end = token.find(['(', '[', '{']).unwrap_or(token.len());
    let id = token[..id_end].trim();
    if id.is_empty() {
        return None;
    }
    // An opener with nothing after it (`A(`) holds no node.
    if id_end < token.len() && id_end + 1 >= token.len() {
        return None;
    }
    Some(id.to_string())
}

/// The node label of a token: the peeled shape text, or the bare id.
/// An unclosed shape (a streaming prefix) keeps its text as the label.
fn node_label(token: &str) -> Option<String> {
    let token = token.trim();
    let id_end = token.find(['(', '[', '{']).unwrap_or(token.len());
    let id = token[..id_end].trim();
    if id.is_empty() {
        return None;
    }
    if id_end == token.len() {
        return Some(id.to_string());
    }
    let opener = token.as_bytes()[id_end];
    let closer = match opener {
        b'(' => ')',
        b'[' => ']',
        _ => '}',
    };
    let mut label = &token[id_end + 1..];
    if label.ends_with(closer) {
        label = &label[..label.len() - 1];
    }
    // Doubled shapes (`A[[仓库]]`, `A((开始))`) peel the inner layer.
    if label.len() >= 2 && label.starts_with(opener as char) && label.ends_with(closer) {
        label = &label[1..label.len() - 1];
    }
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_string())
}

// ---------------------------------------------------------------------------
// State-diagram adapter
// ---------------------------------------------------------------------------

/// Rewrite state-diagram syntax into flowchart syntax: `a --> b: lbl`
/// becomes `a -- lbl --> b`, `[*]` terminators become marker nodes.
/// Composite states and notes are unsupported (`None`).
pub(super) fn adapt_state_diagram(source: &str) -> Option<String> {
    let mut out = String::from("graph TD\n");
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let keyword = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match keyword.as_str() {
            "statediagram" | "statediagram-v2" => {}
            "direction" => {
                let dir = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if !matches!(dir.as_str(), "TB" | "TD") {
                    return None;
                }
            }
            "state" | "note" | "class" => return None,
            _ => {
                let line = line.strip_suffix(';').unwrap_or(line).trim_end();
                let (edge, label) = match line.split_once(':') {
                    Some((edge, label)) => (edge.trim(), label.trim()),
                    None => (line, ""),
                };
                if label.is_empty() {
                    out.push_str(edge);
                    out.push('\n');
                    continue;
                }
                // Labeled edge: move the target behind the label arrow —
                // `a --> b: lbl` becomes `a -- lbl --> b`.
                match edge.split_once("-->") {
                    Some((left, right)) => {
                        out.push_str(&format!("{} -- {} -->{}\n", left.trim_end(), label, right));
                    }
                    None => return None,
                }
            }
        }
    }
    // Distinct entry/exit terminators: mapping every marker onto one
    // node would fold the entry and exit edges into a layout cycle.
    let out = out.replace("[*] -->", "▶ -->");
    let out = out.replace("--> [*]", "--> ■");
    Some(out)
}
