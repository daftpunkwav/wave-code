//! Terminal rendering for fenced `mermaid` blocks.
//!
//! One shared engine carries most diagram kinds: parsers lower their
//! syntax into a common graph IR (node label lines + styled edges +
//! subgraph groups) and the layered band layout draws it. `graph` /
//! `flowchart` (TD/TB/LR), `stateDiagram(-v2)` (through a flowchart
//! adapter), `classDiagram`, `erDiagram`, `requirementDiagram`, and
//! the `C4*` family all ride that engine. Kinds with a genuinely
//! different geometry get small dedicated layouts: `sequenceDiagram`
//! (participant lanes), `gitGraph` (branch timeline), `mindmap` and
//! `timeline` (indented tree), and the chart rows of `pie`, `journey`,
//! `quadrantChart`, and `xychart-beta`.
//!
//! Anything else — `block-beta`, `sankey-beta`, `architecture`, exotic
//! constructs inside supported kinds — returns `None` so the caller
//! falls back to the source view. Rendering is on by default; Ctrl+M
//! in the console flips to source view (a process-global toggle, like
//! the color depth).
//!
//! The band layout stacks nodes by longest-path depth, routes edges
//! down the band gaps with one elbow each (pass-through edges run a
//! straight vertical), and frames subgraph groups with a border. A
//! cycle, a too-wide diagram, or an over-large one falls back to the
//! source view rather than rendering garbage.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::width;

static RENDER: AtomicBool = AtomicBool::new(true);

/// True when mermaid fences render as diagrams.
pub fn render_enabled() -> bool {
    RENDER.load(Ordering::Relaxed)
}

/// Toggle diagram rendering for mermaid fences. Callers own cache
/// invalidation: every rendered Markdown instance holds the previous
/// mode in its cache, so a toggle must be followed by an invalidate
/// pass over live components (and the screen).
pub fn set_render_enabled(on: bool) {
    RENDER.store(on, Ordering::Relaxed);
}

/// Render `source` into diagram lines at most `columns` wide; `None`
/// when the diagram kind is unsupported or does not fit.
pub fn render_diagram(source: &str, columns: usize) -> Option<Vec<String>> {
    let lines = diagram_lines(source, columns)?;
    // The fit contract holds per line: a diagram whose labels overflow
    // the budget (a long sequence label, a huge chart value) falls
    // back to the source view instead of rows the frame would clip.
    (!lines.iter().any(|line| width::width(line) > columns)).then_some(lines)
}

/// The per-kind dispatch behind [`render_diagram`]'s fit check.
fn diagram_lines(source: &str, columns: usize) -> Option<Vec<String>> {
    let keyword = source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("%%"))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("")
        .to_ascii_lowercase();
    match keyword.as_str() {
        "graph" | "flowchart" => layout(&parse(source)?, columns),
        // A state diagram is a flowchart with colon labels and `[*]`
        // terminators; the adapter rewrites it into flowchart syntax.
        "statediagram" | "statediagram-v2" => {
            let adapted = adapt_state_diagram(source)?;
            layout(&parse(&adapted)?, columns)
        }
        "sequencediagram" => {
            let sequence = parse_sequence(source)?;
            layout_sequence(&sequence, columns)
        }
        "classdiagram" => layout(&parse_class_diagram(source)?, columns),
        "erdiagram" => layout(&parse_er_diagram(source)?, columns),
        "requirementdiagram" => layout(&parse_requirement_diagram(source)?, columns),
        "c4context" | "c4container" | "c4component" | "c4dynamic" => {
            layout(&parse_c4_diagram(source)?, columns)
        }
        "gitgraph" => {
            let events = parse_git_graph(source)?;
            layout_git_graph(&events, columns)
        }
        "mindmap" => {
            let tree = parse_mindmap(source)?;
            render_tree(&tree, columns)
        }
        "timeline" => {
            let tree = parse_timeline(source)?;
            render_tree(&tree, columns)
        }
        "pie" => {
            let pie = parse_pie(source)?;
            layout_pie(&pie, columns)
        }
        "journey" => {
            let journey = parse_journey(source)?;
            layout_journey(&journey, columns)
        }
        "quadrantchart" => {
            let quadrant = parse_quadrant(source)?;
            layout_quadrant(&quadrant, columns)
        }
        "xychart-beta" => {
            let chart = parse_xychart(source)?;
            layout_xychart(&chart, columns)
        }
        "gantt" => {
            let gantt = parse_gantt(source)?;
            layout_gantt(&gantt, columns)
        }
        _ => None,
    }
}

/// The fenced-block renderer wiring mermaid into the markdown seam:
/// fences tagged `mermaid` render as diagrams while the toggle is on;
/// any other language — or an unsupported, too-wide diagram — is
/// `None`, and the fence falls back to the source view.
pub struct MermaidFences;

impl crate::markdown::FenceRenderer for MermaidFences {
    fn render_fence(&self, lang: &str, code: &str, columns: usize) -> Option<Vec<String>> {
        if lang != "mermaid" || !render_enabled() {
            return None;
        }
        render_diagram(code, columns)
    }
}

// ---------------------------------------------------------------------------
// The shared graph IR: nodes with label lines, styled edges, subgraph
// groups. Every node-and-edge kind lowers into this and rides `layout`.
// ---------------------------------------------------------------------------

/// Flow direction of a graph diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Top-down bands.
    TD,
    /// Left-to-right columns.
    LR,
}

/// Edge rendering style: the vertical fill and the arrowhead glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeStyle {
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
    fn vertical(self) -> &'static str {
        match self {
            EdgeStyle::Solid | EdgeStyle::Hollow => "│",
            EdgeStyle::Dotted => "┆",
            EdgeStyle::Thick => "┃",
        }
    }

    /// The arrowhead landing on the target band.
    fn head(self) -> &'static str {
        match self {
            EdgeStyle::Hollow => "▽",
            _ => "▼",
        }
    }

    /// The horizontal run glyph along elbow rows.
    fn horizontal(self) -> &'static str {
        match self {
            EdgeStyle::Dotted => "┄",
            _ => "─",
        }
    }
}

/// One directed edge between node indices.
struct GraphEdge {
    from: usize,
    to: usize,
    label: Option<String>,
    style: EdgeStyle,
}

/// Upper bound on nodes in one diagram: the band/column layouts refuse
/// more, so refusing at creation keeps parsing (and per-group member
/// dedup) bounded on oversized sources.
const MAX_NODES: usize = 100;

/// The shared graph IR: node label lines in definition order, styled
/// edges between indices, and subgraph groups over member indices.
struct Diagram {
    direction: Direction,
    labels: Vec<Vec<String>>,
    edges: Vec<GraphEdge>,
    groups: Vec<(String, Vec<usize>)>,
}

impl Diagram {
    fn new(direction: Direction) -> Self {
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
    fn node(
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
    fn edge(&mut self, from: usize, to: usize, label: Option<String>, style: EdgeStyle) {
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
fn parse(source: &str) -> Option<Diagram> {
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
    if dotted {
        if line.starts_with("-.->") {
            return Some((None, 4, EdgeStyle::Dotted));
        }
        let end = line.find(".->")?;
        let label = line[2..end].trim();
        let label = (!label.is_empty()).then(|| label.to_string());
        return Some((label, end + 3, EdgeStyle::Dotted));
    }
    if thick {
        if line.starts_with("==>") {
            return Some((None, 3, EdgeStyle::Thick));
        }
        let end = line.find("==>")?;
        let label = line[2..end].trim();
        let label = (!label.is_empty()).then(|| label.to_string());
        return Some((label, end + 3, EdgeStyle::Thick));
    }
    if plain {
        if line.starts_with("-->") {
            return Some((None, 3, EdgeStyle::Solid));
        }
        if line.starts_with("---") {
            return Some((None, 3, EdgeStyle::Solid));
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
// classDiagram — class boxes with member rows over the shared engine
// ---------------------------------------------------------------------------

/// Relation markers, longest first so `<|--` wins over `--`.
const CLASS_RELATIONS: [&str; 10] = [
    "<|--", "--|>", "<|..", "..|>", "*--", "o--", "..>", "-->", "--", "..",
];

/// Parse a class diagram into the graph IR: one node per class with
/// member rows as label lines, one edge per relation.
fn parse_class_diagram(source: &str) -> Option<Diagram> {
    let mut diagram = Diagram::new(Direction::TD);
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut open: Option<usize> = None;
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
            "classdiagram" => {}
            "class" | "interface" | "enumeration" => {
                let rest = line[keyword.len()..].trim();
                let (head, block) = match rest.strip_suffix('{') {
                    Some(head) => (head.trim(), true),
                    None => (rest, false),
                };
                let (id, display) = class_token(head)?;
                let lines = vec![display];
                if block {
                    open = Some(diagram.node(&mut ids, &id, lines)?);
                } else {
                    diagram.node(&mut ids, &id, lines)?;
                    open = None;
                }
            }
            "}" => open = None,
            "note" | "direction" | "classdef" | "cssclass" | "cssclasses" | "style" | "click"
            | "namespace" => {}
            _ => {
                let member = open.is_some()
                    && (line.starts_with(['+', '-', '#', '~']) || line.starts_with("<<"));
                if member {
                    if let Some(index) = open {
                        let rows = &mut diagram.labels[index];
                        if rows.len() < 16 {
                            rows.push(line.to_string());
                        }
                    }
                } else if let Some((from, to, label, style)) =
                    parse_class_relation(line, &mut diagram, &mut ids)?
                {
                    diagram.edge(from, to, label, style);
                } else {
                    return None;
                }
            }
        }
    }
    if diagram.labels.is_empty() {
        return None;
    }
    Some(diagram)
}

/// Split a class token into (id, display): `Name["Label"]` takes the
/// quoted display, generics ride along raw (`List~Animal~`).
fn class_token(token: &str) -> Option<(String, String)> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    if let Some(open) = token.find('[')
        && token.ends_with(']')
        && open > 0
    {
        let id = token[..open].trim().to_string();
        let display = token[open + 1..token.len() - 1]
            .trim()
            .trim_matches('"')
            .trim()
            .to_string();
        if id.is_empty() || display.is_empty() {
            return None;
        }
        return Some((id, display));
    }
    Some((token.to_string(), token.to_string()))
}

/// Parse one class relation line into an edge `(from, to, label,
/// style)`. The head end is the parent: it layers above the child.
#[allow(clippy::type_complexity)]
fn parse_class_relation(
    line: &str,
    diagram: &mut Diagram,
    ids: &mut HashMap<String, usize>,
) -> Option<Option<(usize, usize, Option<String>, EdgeStyle)>> {
    for marker in CLASS_RELATIONS {
        let Some(pos) = line.find(marker) else {
            continue;
        };
        // `--` must not steal from longer markers: re-scan for a longer
        // match at the same position.
        if marker == "--"
            && CLASS_RELATIONS
                .iter()
                .any(|m| *m != "--" && line.find(m) == Some(pos))
        {
            continue;
        }
        let (left, right) = (line[..pos].trim(), line[pos + marker.len()..].trim());
        let (right, label) = match right.split_once(':') {
            Some((head, tail)) => (head.trim(), Some(tail.trim().to_string())),
            None => (right, None),
        };
        // Relation ends may carry quoted cardinalities (`Owner "1" -->`);
        // they are display metadata, not part of the class name.
        let (left, right) = (strip_quoted_spans(left), strip_quoted_spans(right));
        if left.is_empty() || right.is_empty() {
            return None;
        }
        let (from, to) = match marker {
            // Head at the left end: the left side is the parent.
            "<|--" | "<|.." => (left, right),
            // Head at the right end: the right side is the parent.
            "--|>" | "..|>" => (right, left),
            // Owner on the left, owned on the right.
            _ => (left, right),
        };
        let style = match marker {
            "<|--" | "--|>" | "<|.." | "..|>" => EdgeStyle::Hollow,
            ".." | "..>" => EdgeStyle::Dotted,
            _ => EdgeStyle::Solid,
        };
        let from = diagram.node(ids, &from, vec![from.clone()])?;
        let to = diagram.node(ids, &to, vec![to.clone()])?;
        return Some(Some((from, to, label, style)));
    }
    Some(None)
}

/// Drop `"..."` spans (quoted cardinalities) from a relation end.
fn strip_quoted_spans(s: &str) -> String {
    let mut out = String::new();
    let mut in_quotes = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            _ if !in_quotes => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// erDiagram — entity boxes with attribute rows over the shared engine
// ---------------------------------------------------------------------------

/// Parse an ER diagram into the graph IR: one node per entity with
/// attribute rows, one edge per relationship (dotted when
/// non-identifying).
fn parse_er_diagram(source: &str) -> Option<Diagram> {
    let mut diagram = Diagram::new(Direction::TD);
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut open: Option<usize> = None;
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "erdiagram") {
            continue;
        }
        // Relationship line: the marker rides a standalone token.
        if line.contains("--") || line.contains("..") {
            open = None;
            let (rels, label) = match line.split_once(':') {
                Some((rels, label)) => (rels, Some(label.trim().to_string())),
                None => (line, None),
            };
            let tokens = split_quoted(rels, char::is_whitespace);
            let rel_pos = tokens
                .iter()
                .position(|t| t.contains("--") || t.contains(".."))?;
            if rel_pos == 0 || rel_pos + 1 >= tokens.len() {
                return None;
            }
            let ent1 = tokens[..rel_pos].join(" ");
            let ent2 = tokens[rel_pos + 1..].join(" ");
            if ent1.is_empty() || ent2.is_empty() {
                return None;
            }
            let style = if tokens[rel_pos].contains("..") {
                EdgeStyle::Dotted
            } else {
                EdgeStyle::Solid
            };
            let from = diagram.node(&mut ids, &ent1, vec![ent1.clone()])?;
            let to = diagram.node(&mut ids, &ent2, vec![ent2.clone()])?;
            diagram.edge(from, to, label, style);
            continue;
        }
        // Entity block open / close.
        if let Some(name) = line.strip_suffix('{') {
            let name = name.trim().trim_matches('"').trim();
            if name.is_empty() {
                return None;
            }
            open = Some(diagram.node(&mut ids, name, vec![name.to_string()])?);
            continue;
        }
        if line == "}" {
            open = None;
            continue;
        }
        // Attribute row of the open entity block (or a bare entity).
        match open {
            Some(index) => {
                let rows = &mut diagram.labels[index];
                if rows.len() < 16 {
                    rows.push(line.to_string());
                }
            }
            None => {
                let name = line.trim_matches('"').trim();
                if name.is_empty() {
                    return None;
                }
                diagram.node(&mut ids, name, vec![name.to_string()])?;
            }
        }
    }
    if diagram.labels.is_empty() {
        return None;
    }
    Some(diagram)
}

/// Split `s` on `sep` runs outside double quotes; parts are trimmed
/// and the quotes dropped.
fn split_quoted(s: &str, sep: impl Fn(char) -> bool + Copy) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in s.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && sep(ch) {
            if !current.trim().is_empty() {
                parts.push(current.trim().to_string());
                current.clear();
            }
        } else {
            current.push(ch);
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

// ---------------------------------------------------------------------------
// C4* — persons, systems, containers, components, relations
// ---------------------------------------------------------------------------

/// Parse a C4 diagram into the graph IR: one node per element (name
/// plus description lines), one edge per `Rel*` call. Boundaries and
/// styling directives ride along ignored.
fn parse_c4_diagram(source: &str) -> Option<Diagram> {
    let mut diagram = Diagram::new(Direction::TD);
    let mut ids: HashMap<String, usize> = HashMap::new();
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let (keyword, rest) = match line.find(['(', ' ']) {
            Some(pos) => (line[..pos].to_ascii_lowercase(), &line[pos..]),
            None => (line.to_ascii_lowercase(), ""),
        };
        let rest = rest.strip_prefix('(').unwrap_or(rest);
        match keyword.as_str() {
            "c4context" | "c4container" | "c4component" | "c4dynamic" | "title" => {}
            "boundary" | "enterprise_boundary" | "system_boundary" | "container_boundary" => {}
            "update_element_style"
            | "update_rel_style"
            | "add_element_tag"
            | "add_rel_tag"
            | "show_animation"
            | "layout_stack"
            | "layout" => {}
            "person" | "person_ext" | "system" | "system_ext" | "systemdb" | "system_db"
            | "systemqueue" | "systemqueue_ext" | "container" | "containerdb" | "container_db"
            | "component" | "componentdb" | "component_db" => {
                let args = parse_call_args(rest)?;
                let alias = args.first()?.clone();
                let name = args.get(1).cloned().unwrap_or_else(|| alias.clone());
                let mut lines = vec![name];
                for extra in args.iter().skip(2).filter(|a| !a.is_empty()).take(2) {
                    lines.push(extra.clone());
                }
                diagram.node(&mut ids, &alias, lines)?;
            }
            "rel" | "rel_u" | "rel_d" | "rel_l" | "rel_r" | "rel_back" | "birel" => {
                let args = parse_call_args(rest)?;
                if args.len() < 2 {
                    return None;
                }
                let (from, to) = if keyword == "rel_back" {
                    (args[1].clone(), args[0].clone())
                } else {
                    (args[0].clone(), args[1].clone())
                };
                let label = args.get(2).cloned().filter(|l| !l.is_empty());
                let from = diagram.node(&mut ids, &from, vec![from.clone()])?;
                let to = diagram.node(&mut ids, &to, vec![to.clone()])?;
                diagram.edge(from, to, label, EdgeStyle::Solid);
            }
            _ => return None,
        }
    }
    if diagram.labels.is_empty() {
        return None;
    }
    Some(diagram)
}

/// Split the argument list of a call (the text after the opening
/// paren) on top-level commas, stripping quotes.
fn parse_call_args(rest: &str) -> Option<Vec<String>> {
    let close = rest.rfind(')')?;
    let parts = split_quoted(&rest[..close], |c| c == ',');
    (!parts.is_empty()).then_some(parts)
}

// ---------------------------------------------------------------------------
// requirementDiagram — requirement/element boxes over the shared engine
// ---------------------------------------------------------------------------

/// Parse a requirement diagram into the graph IR: one node per
/// requirement or element with its field rows, one edge per
/// `a - rel -> b` line. Long field values are truncated so the box
/// still fits a terminal row.
fn parse_requirement_diagram(source: &str) -> Option<Diagram> {
    const FIELD_MAX: usize = 40;
    let mut diagram = Diagram::new(Direction::TD);
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut open: Option<usize> = None;
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "requirementdiagram") {
            continue;
        }
        // Block open: `requirement name {`, `element name {`, ...
        let keyword = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            keyword.as_str(),
            "requirement"
                | "functionalrequirement"
                | "interfacerequirement"
                | "performancerequirement"
                | "physicalrequirement"
                | "designconstraint"
                | "element"
        ) && let Some(head) = line.strip_suffix('{')
        {
            let name = head[keyword.len()..].trim().trim_matches('"').trim();
            if name.is_empty() {
                return None;
            }
            open = Some(diagram.node(&mut ids, name, vec![name.to_string()])?);
            continue;
        }
        if line == "}" {
            open = None;
            continue;
        }
        if let Some(index) = open {
            if line.split_once(':').is_some() {
                let row = truncate_width(line, FIELD_MAX);
                let rows = &mut diagram.labels[index];
                if rows.len() < 8 {
                    rows.push(row);
                }
            }
            continue;
        }
        // Edge: `a - rel -> b`.
        if let Some((left, right)) = line.split_once("->") {
            let right = right.trim();
            if right.is_empty() {
                return None;
            }
            let (from_name, rel) = match left.trim_end().split_once('-') {
                Some((name, rel)) => (name.trim(), rel.trim()),
                None => return None,
            };
            if from_name.is_empty() {
                return None;
            }
            let label = (!rel.is_empty()).then(|| rel.to_string());
            let from = diagram.node(&mut ids, from_name, vec![from_name.to_string()])?;
            let to = diagram.node(&mut ids, right, vec![right.to_string()])?;
            diagram.edge(from, to, label, EdgeStyle::Solid);
            continue;
        }
        return None;
    }
    if diagram.labels.is_empty() {
        return None;
    }
    Some(diagram)
}

/// Truncate `text` to at most `max` display columns, ending with `…`.
fn truncate_width(text: &str, max: usize) -> String {
    if width::width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cw = width::width(ch.to_string().as_str());
        if used + cw + 1 > max {
            break;
        }
        used += cw;
        out.push(ch);
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// gitGraph — a branch-labeled commit timeline
// ---------------------------------------------------------------------------

/// One commit/merge event on a branch.
struct GitEvent {
    branch: String,
    merge: bool,
    label: String,
}

/// Parse a gitGraph source: a flat event list with the current branch
/// tracked through `branch`/`checkout`/`switch`.
fn parse_git_graph(source: &str) -> Option<Vec<GitEvent>> {
    let mut events = Vec::new();
    let mut current = "main".to_string();
    let mut seq = 0usize;
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let (keyword, rest) = match line.split_once(char::is_whitespace) {
            Some((kw, rest)) => (kw.to_ascii_lowercase(), rest.trim()),
            None => (line.to_ascii_lowercase(), ""),
        };
        match keyword.as_str() {
            "gitgraph" => {}
            "commit" => {
                seq += 1;
                let id = git_attr(rest, "id").unwrap_or(format!("#{seq}"));
                let label = match git_attr(rest, "tag") {
                    Some(tag) => format!("{id} ({tag})"),
                    None => id,
                };
                events.push(GitEvent {
                    branch: current.clone(),
                    merge: false,
                    label,
                });
            }
            "cherry-pick" => {
                seq += 1;
                let id = git_attr(rest, "id").unwrap_or(format!("#{seq}"));
                events.push(GitEvent {
                    branch: current.clone(),
                    merge: false,
                    label: format!("{id} (cherry)"),
                });
            }
            "branch" => {
                let name = rest.split_whitespace().next()?;
                if name.is_empty() {
                    return None;
                }
                current = name.trim_matches('"').to_string();
            }
            "checkout" | "switch" => {
                let name = rest.split_whitespace().next()?;
                current = name.trim_matches('"').to_string();
            }
            "merge" => {
                let name = rest.split_whitespace().next()?;
                let mut label = format!("merge {}", name.trim_matches('"'));
                if let Some(tag) = git_attr(rest, "tag") {
                    label.push_str(&format!(" ({tag})"));
                }
                events.push(GitEvent {
                    branch: current.clone(),
                    merge: true,
                    label,
                });
            }
            _ => return None,
        }
    }
    if events.is_empty() || events.len() > 80 {
        return None;
    }
    Some(events)
}

/// Lay a gitGraph out: one row per event, the branch lane labeled on
/// the left, commits as `○` and merges as `●`.
fn layout_git_graph(events: &[GitEvent], columns: usize) -> Option<Vec<String>> {
    let mut branches: Vec<String> = Vec::new();
    for event in events {
        if !branches.contains(&event.branch) {
            branches.push(event.branch.clone());
        }
    }
    if branches.len() > 8 {
        return None;
    }
    let lane = branches.iter().map(|b| width::width(b)).max()?.max(4);
    let label_w = events.iter().map(|e| width::width(&e.label)).max()?.max(4);
    if lane + 3 + label_w > columns {
        return None;
    }
    let mut lines = Vec::with_capacity(events.len() + 1);
    lines.push(branches.join(" · "));
    for event in events {
        let glyph = if event.merge { "●" } else { "○" };
        let branch_w = width::width(&event.branch);
        lines.push(format!(
            "{}{}  {glyph} {}",
            event.branch,
            " ".repeat(lane - branch_w),
            event.label
        ));
    }
    Some(lines)
}

/// The quoted value of a `key: "value"` attribute inside a gitGraph
/// statement; both `id: "x"` and `id:"x"` spellings work.
fn git_attr(rest: &str, key: &str) -> Option<String> {
    for (index, token) in rest.split_whitespace().enumerate() {
        let (name, value) = match token.strip_suffix(':') {
            Some(name) => (name, rest.split_whitespace().nth(index + 1)),
            None => match token.split_once(':') {
                Some((name, value)) => (name, Some(value)),
                None => continue,
            },
        };
        if name.eq_ignore_ascii_case(key) {
            return value.map(|v| v.trim_matches('"').trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// mindmap / timeline — an indented tree renderer
// ---------------------------------------------------------------------------

/// A tree node: display text plus children.
struct TreeNode {
    text: String,
    children: Vec<TreeNode>,
}

/// Parse a mindmap: indentation (two spaces per level) defines the
/// tree; the first content line is the root. `::icon(...)` metadata
/// lines are skipped.
fn parse_mindmap(source: &str) -> Option<TreeNode> {
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
fn render_tree(root: &TreeNode, columns: usize) -> Option<Vec<String>> {
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
fn parse_timeline(source: &str) -> Option<TreeNode> {
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

// ---------------------------------------------------------------------------
// journey / quadrantChart / xychart-beta — small chart rows
// ---------------------------------------------------------------------------

/// A journey: optional title plus section headers and scored tasks.
struct Journey {
    title: Option<String>,
    rows: Vec<JourneyRow>,
}

enum JourneyRow {
    Section(String),
    Task {
        name: String,
        score: u8,
        actors: String,
    },
}

/// Parse a journey diagram: `task: score: actors` rows under `section`
/// headers. Scores must be 1..=5.
fn parse_journey(source: &str) -> Option<Journey> {
    let mut title = None;
    let mut rows = Vec::new();
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "journey") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = Some(line["title".len()..].trim().to_string());
            continue;
        }
        if starts_with_keyword(line, "section") {
            let section = line["section".len()..].trim().to_string();
            if section.is_empty() {
                return None;
            }
            rows.push(JourneyRow::Section(section));
            continue;
        }
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() < 3 {
            return None;
        }
        let name = parts[0].trim().to_string();
        let score: u8 = parts[1].trim().parse().ok()?;
        if !(1..=5).contains(&score) {
            return None;
        }
        let actors = parts[2..].join(":");
        rows.push(JourneyRow::Task {
            name: truncate_width(&name, 32),
            score,
            actors: truncate_width(actors.trim(), 24),
        });
        if rows.len() > 40 {
            return None;
        }
    }
    if rows.is_empty() {
        return None;
    }
    Some(Journey { title, rows })
}

/// Lay a journey out: section rules, one scored bar row per task.
fn layout_journey(j: &Journey, columns: usize) -> Option<Vec<String>> {
    let max_name = j
        .rows
        .iter()
        .map(|row| match row {
            JourneyRow::Section(s) => width::width(s) + 2,
            JourneyRow::Task { name, .. } => width::width(name),
        })
        .max()?
        .max(4);
    let mut lines = Vec::new();
    if let Some(title) = &j.title {
        lines.push(title.clone());
    }
    for row in &j.rows {
        match row {
            JourneyRow::Section(name) => lines.push(format!("── {name}")),
            JourneyRow::Task {
                name,
                score,
                actors,
            } => {
                let pad = " ".repeat(max_name - width::width(name));
                let bar = "█".repeat(*score as usize * 2);
                if max_name + 1 + 10 + 4 + width::width(actors) > columns {
                    return None;
                }
                lines.push(format!("{name}{pad} {bar} {score} {actors}"));
            }
        }
    }
    Some(lines)
}

/// A quadrant chart: four quadrant labels, axes, and points in [0,1]².
struct Quadrant {
    title: Option<String>,
    quadrants: [String; 4],
    points: Vec<(String, f64, f64)>,
    x_axis: String,
    y_axis: String,
}

/// Parse a quadrant chart; points are `"name": [x, y]` with floats.
fn parse_quadrant(source: &str) -> Option<Quadrant> {
    let mut title = None;
    let mut quadrants: [Option<String>; 4] = [None, None, None, None];
    let mut points = Vec::new();
    let mut x_axis = String::new();
    let mut y_axis = String::new();
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "quadrantchart") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = Some(line["title".len()..].trim().to_string());
            continue;
        }
        if starts_with_keyword(line, "x-axis") {
            x_axis = truncate_width(line["x-axis".len()..].trim(), 36);
            continue;
        }
        if starts_with_keyword(line, "y-axis") {
            y_axis = truncate_width(line["y-axis".len()..].trim(), 36);
            continue;
        }
        if let Some(rest) = line.strip_prefix("quadrant-") {
            let (index, label) = rest.split_once(char::is_whitespace)?;
            let index: usize = index.parse().ok()?;
            if !(1..=4).contains(&index) {
                return None;
            }
            quadrants[index - 1] = Some(truncate_width(label.trim(), 14));
            continue;
        }
        // Point: `"name": [x, y]`.
        let (name, coords) = line.split_once(':')?;
        let name = name.trim().trim_matches('"').trim().to_string();
        if name.is_empty() {
            return None;
        }
        let inner = coords.trim().strip_prefix('[')?.strip_suffix(']')?;
        let mut parts = inner.split(',');
        let x: f64 = parts.next()?.trim().parse().ok()?;
        let y: f64 = parts.next()?.trim().parse().ok()?;
        if parts.next().is_some() || !x.is_finite() || !y.is_finite() {
            return None;
        }
        points.push((name, x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
        if points.len() > 24 {
            return None;
        }
    }
    if points.is_empty() {
        return None;
    }
    Some(Quadrant {
        title,
        quadrants: quadrants.map(|q| q.unwrap_or_default()),
        points,
        x_axis,
        y_axis,
    })
}

/// Lay a quadrant chart out: a 36×12 grid, the cross at the middle,
/// quadrant labels in the corners, points as `●`, a legend below.
fn layout_quadrant(q: &Quadrant, columns: usize) -> Option<Vec<String>> {
    const W: usize = 36;
    const H: usize = 12;
    if columns < W + 2 {
        return None;
    }
    let mid_r = H / 2;
    let mid_c = W / 2;
    let mut rows: Vec<RowCanvas> = (0..H).map(|_| RowCanvas::new(W)).collect();
    for (r, row) in rows.iter_mut().enumerate() {
        for c in 0..W {
            if r == mid_r && c == mid_c {
                row.put(c, "┼");
            } else if r == mid_r {
                row.put(c, "─");
            } else if c == mid_c {
                row.put(c, "│");
            }
        }
    }
    // Quadrant labels: 1 top-right, 2 top-left, 3 bottom-left, 4 bottom-right.
    let place = |row: &mut RowCanvas, text: &str, right: bool| {
        if text.is_empty() {
            return;
        }
        let w = width::width(text);
        let col = if right { W.saturating_sub(w + 1) } else { 1 };
        row.put(col, text.to_string());
    };
    place(&mut rows[1], &q.quadrants[1], false);
    place(&mut rows[1], &q.quadrants[0], true);
    place(&mut rows[H - 1], &q.quadrants[2], false);
    place(&mut rows[H - 1], &q.quadrants[3], true);
    for (_name, x, y) in &q.points {
        let col = (1.0 + x * (W as f64 - 3.0)).round() as usize;
        let row = (1.0 + (1.0 - y) * (H as f64 - 3.0)).round() as usize;
        let col = col.clamp(1, W - 2);
        let row = row.clamp(1, H - 2);
        rows[row].put(col, "●");
    }
    let legend = q
        .points
        .iter()
        .map(|(name, _, _)| format!("● {name}"))
        .collect::<Vec<_>>()
        .join("  ");
    if width::width(&legend) + 2 > columns {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    if let Some(title) = &q.title {
        lines.push(title.clone());
    }
    for row in &rows {
        lines.push(row.render());
    }
    if !q.y_axis.is_empty() {
        lines.push(format!("y: {}", q.y_axis));
    }
    if !q.x_axis.is_empty() {
        lines.push(format!("x: {}", q.x_axis));
    }
    lines.push(legend);
    Some(lines)
}

/// An xychart: one numeric series over categorical x values, rendered
/// as scaled bar rows.
struct XyChart {
    title: Option<String>,
    categories: Vec<String>,
    values: Vec<f64>,
}

/// Parse an xychart-beta; the first `bar` series wins, then `line`.
fn parse_xychart(source: &str) -> Option<XyChart> {
    let mut title = None;
    let mut categories: Option<Vec<String>> = None;
    let mut values: Option<Vec<f64>> = None;
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "xychart-beta") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = Some(line["title".len()..].trim().trim_matches('"').to_string());
            continue;
        }
        if starts_with_keyword(line, "x-axis") {
            let rest = line["x-axis".len()..].trim();
            let list = match rest.find('[') {
                Some(open) => &rest[open..],
                None => rest,
            };
            let inner = list.strip_prefix('[')?.strip_suffix(']')?;
            let cats: Vec<String> = inner
                .split(',')
                .map(|c| truncate_width(c.trim().trim_matches('"').trim(), 12))
                .collect();
            if cats.is_empty() || cats.iter().any(|c| c.is_empty()) {
                return None;
            }
            categories = Some(cats);
            continue;
        }
        if starts_with_keyword(line, "y-axis") || starts_with_keyword(line, "tspan") {
            continue;
        }
        if starts_with_keyword(line, "bar") || starts_with_keyword(line, "line") {
            let open = line.find('[')?;
            let inner = line[open..].trim().strip_prefix('[')?.strip_suffix(']')?;
            let series: Vec<f64> = inner
                .split(',')
                .map(|v| v.trim().parse().ok())
                .collect::<Option<_>>()?;
            // A non-finite value (NaN, infinities) poisons the scale:
            // fall back instead of rendering `NaN` rows.
            if series.is_empty() || series.iter().any(|v| !v.is_finite()) {
                return None;
            }
            if values.is_none() {
                values = Some(series);
            }
            continue;
        }
        return None;
    }
    let categories = categories?;
    let values = values?;
    if categories.len() != values.len() || categories.len() > 30 {
        return None;
    }
    Some(XyChart {
        title,
        categories,
        values,
    })
}

/// Lay an xychart out: one scaled bar row per category.
fn layout_xychart(chart: &XyChart, columns: usize) -> Option<Vec<String>> {
    let max_label = chart
        .categories
        .iter()
        .map(|c| width::width(c))
        .max()?
        .max(4);
    let bar_max = 20usize;
    let max_value = chart
        .values
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if !max_value.is_finite() || max_value <= 0.0 {
        return None;
    }
    if max_label + 1 + bar_max + 10 > columns {
        return None;
    }
    let mut lines = Vec::new();
    if let Some(title) = &chart.title {
        lines.push(title.clone());
    }
    for (category, value) in chart.categories.iter().zip(&chart.values) {
        let share = (value / max_value).clamp(0.0, 1.0);
        let bar_len = (share * bar_max as f64).round() as usize;
        let bar = "█".repeat(bar_len);
        let label_col = max_label - width::width(category);
        let value_text = if value.fract() == 0.0 {
            format!("{value:.0}")
        } else {
            format!("{value:.1}")
        };
        lines.push(format!(
            "{}{} {bar} {value_text}",
            category,
            " ".repeat(label_col)
        ));
    }
    Some(lines)
}

// ---------------------------------------------------------------------------
// sequenceDiagram (unchanged geometry), pie, gantt
// ---------------------------------------------------------------------------

/// A parsed sequence diagram: participant display names and messages
/// `(from, to, label, dotted)` by participant index.
struct Sequence {
    names: Vec<String>,
    messages: Vec<(usize, usize, String, bool)>,
}

/// Upper bound on rendered messages: each message draws two canvas
/// rows, so the cap keeps oversized sources on the source view like
/// the other layouts' row budgets.
const MAX_SEQUENCE_MESSAGES: usize = 200;

/// Parse a sequence diagram; `None` for unsupported constructs
/// (activations, notes, loops) and malformed messages.
fn parse_sequence(source: &str) -> Option<Sequence> {
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
                let from = *ids.get(from.trim())?;
                let to = *ids.get(to.trim())?;
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
fn layout_sequence(s: &Sequence, columns: usize) -> Option<Vec<String>> {
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
                // let the canvas clip what follows.
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
            arrow_row.put(right, "▶");
        }
        lines.push(arrow_row.render());
    }
    Some(lines)
}

/// A parsed gantt chart: title plus `(name, start_day, days)` tasks in
/// declaration order.
struct Gantt {
    title: Option<String>,
    tasks: Vec<(String, i64, i64)>,
}

/// Parse a gantt chart. Only `YYYY-MM-DD` dates and day durations
/// (`5d`) are supported; `after x` resolves through earlier tasks.
/// Anything else (unnamed formats, milestones, excludes) falls back.
fn parse_gantt(source: &str) -> Option<Gantt> {
    let mut title = None;
    let mut tasks: Vec<(String, i64, i64)> = Vec::new();
    let mut starts: HashMap<String, i64> = HashMap::new();
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "gantt") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = Some(line["title".len()..].trim().to_string());
            continue;
        }
        if starts_with_keyword(line, "dateformat")
            || starts_with_keyword(line, "section")
            || starts_with_keyword(line, "excludes")
        {
            continue;
        }
        // Task: `name :[id,] (start|after x), Nd[, tag...]`. The id is
        // optional and the trailing tag (`done`) is ignored; an
        // id-less task is referenced by its name.
        let (name, spec) = line.split_once(':')?;
        let name = name.trim().to_string();
        let parts: Vec<&str> = spec.split(',').map(str::trim).collect();
        // A leading date or `after` clause means the id was omitted.
        let (id, rest): (&str, &[&str]) = match parts.split_first() {
            Some((first, _)) if is_start_spec(first) => ("", parts.as_slice()),
            Some((first, rest)) => (first, rest),
            None => return None,
        };
        let start_spec = rest.first().copied()?;
        let duration: i64 = rest
            .get(1)
            .and_then(|part| part.strip_suffix('d'))
            .and_then(|days| days.parse().ok())?;
        if !(0..=MAX_GANTT_DAYS).contains(&duration) {
            return None;
        }
        let start = if let Some(dep) = start_spec.strip_prefix("after ") {
            *starts.get(dep.trim())?
        } else {
            days_from_civil(start_spec)?
        };
        if !id.is_empty() {
            starts.insert(id.to_string(), start);
        } else {
            starts.insert(name.clone(), start);
        }
        tasks.push((name, start, duration));
    }
    if tasks.is_empty() {
        return None;
    }
    Some(Gantt { title, tasks })
}

/// True when the part is a start spec (a `YYYY-MM-DD` date or an
/// `after id` clause) rather than a task id.
fn is_start_spec(part: &str) -> bool {
    part.starts_with("after ") || days_from_civil(part).is_some()
}

/// True when `line` starts with the case-insensitive directive `kw`
/// followed by whitespace or end-of-line: task and slice lines whose
/// first word merely extends the keyword stay data.
fn starts_with_keyword(line: &str, kw: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    match lower.strip_prefix(kw) {
        Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
        None => false,
    }
}

/// Upper bound on a task duration in days (ten thousand years): keeps
/// `start + days` and the inverse civil-date arithmetic inside i64.
const MAX_GANTT_DAYS: i64 = 3_652_059;

/// Days since 1970-01-01 for a `YYYY-MM-DD` date (Howard Hinnant's
/// days_from_civil, proleptic Gregorian).
fn days_from_civil(date: &str) -> Option<i64> {
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    // Gregorian wall-clock dates only: the year bound keeps the day
    // arithmetic (and `civil_from_days` of `start + days`) inside i64.
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Render a gantt chart: title row plus one bar row per task. Bars are
/// scaled against the longest task; dates render MM-DD.
fn layout_gantt(g: &Gantt, columns: usize) -> Option<Vec<String>> {
    let max_name = g
        .tasks
        .iter()
        .map(|(name, _, _)| width::width(name))
        .max()?
        .max(4);
    let bar_max = 24usize;
    let max_days = g.tasks.iter().map(|(_, _, d)| *d).max()?;
    if max_days <= 0 || max_name + bar_max + 18 > columns {
        return None;
    }
    let mut lines = Vec::new();
    if let Some(title) = &g.title {
        lines.push(title.clone());
    }
    for (name, start, days) in &g.tasks {
        let bar_len = ((*days as f64 / max_days as f64) * bar_max as f64).round() as usize;
        let bar = "\u{2588}".repeat(bar_len.max(1));
        let end = civil_from_days(start + days);
        // Pad to the display width: char-count padding would misalign
        // wide (CJK) task names.
        let pad = " ".repeat(max_name - width::width(name));
        lines.push(format!("{name}{pad} {bar} {}", &end[5..]));
    }
    Some(lines)
}

/// Inverse of [`days_from_civil`] (Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> String {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// A parsed pie chart: title plus `(label, value)` slices.
struct Pie {
    title: Option<String>,
    slices: Vec<(String, f64)>,
}

/// Parse a pie chart; `None` on malformed slices.
fn parse_pie(source: &str) -> Option<Pie> {
    let mut title = None;
    let mut slices = Vec::new();
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if starts_with_keyword(line, "pie") {
            // `pie title X` or a bare `pie`.
            let rest = line["pie".len()..].trim();
            if let Some(t) = rest.strip_prefix("title") {
                title = Some(t.trim().to_string());
            }
            continue;
        }
        if lower == "showdata" {
            continue;
        }
        let (label, value) = line.split_once(':')?;
        let label = label.trim().trim_matches('"').to_string();
        let value: f64 = value.trim().parse().ok()?;
        slices.push((label, value));
    }
    if slices.is_empty() {
        return None;
    }
    Some(Pie { title, slices })
}

/// Render a pie chart as a horizontal bar chart with percentages —
/// the honest terminal encoding of a pie.
fn layout_pie(pie: &Pie, columns: usize) -> Option<Vec<String>> {
    let max_label = pie
        .slices
        .iter()
        .map(|(label, _)| width::width(label))
        .max()?
        .max(4);
    let bar_max = 20usize;
    let total: f64 = pie.slices.iter().map(|(_, value)| value).sum();
    // A poisoned total (NaN, or non-positive after negative slices)
    // falls back to the source view instead of rendering `NaN%`.
    if !total.is_finite() || total <= 0.0 {
        return None;
    }
    if max_label + 1 + bar_max + 8 > columns {
        return None;
    }
    let mut lines = Vec::new();
    if let Some(title) = &pie.title {
        lines.push(title.clone());
    }
    for (label, value) in &pie.slices {
        let share = (value / total * 100.0).clamp(0.0, 100.0);
        let bar_len = ((value / total) * bar_max as f64).round() as usize;
        let bar = "█".repeat(bar_len);
        let label_col = max_label - width::width(label);
        lines.push(format!(
            "{}{} {bar} {:>5.1}%",
            label,
            " ".repeat(label_col),
            share
        ));
    }
    Some(lines)
}

// ---------------------------------------------------------------------------
// The layered band layout (TD) and column layout (LR)
// ---------------------------------------------------------------------------

/// Dispatch to the direction's layout; `None` when it cannot fit.
fn layout(d: &Diagram, columns: usize) -> Option<Vec<String>> {
    match d.direction {
        Direction::TD => layout_td(d, columns),
        Direction::LR => layout_lr(d, columns),
    }
}

/// Lay the diagram out into band rows; `None` when it cannot fit
/// `columns` (the caller falls back to the source view).
fn layout_td(d: &Diagram, columns: usize) -> Option<Vec<String>> {
    let count = d.labels.len();
    if count == 0 || count > 100 {
        return None;
    }
    // Longest-path layering by relaxation. A cycle makes the layers
    // grow by one per pass without ever settling: detect it by capping
    // every layer at the node count and fall back to the source view —
    // a cyclic flow (state loops, recursive calls) has no faithful
    // top-down layout in this model.
    let mut layer = vec![0usize; count];
    for _ in 0..count {
        for edge in &d.edges {
            if layer[edge.to] < layer[edge.from] + 1 {
                layer[edge.to] = layer[edge.from] + 1;
                if layer[edge.to] >= count {
                    return None; // cycle
                }
            }
        }
    }
    let max_layer = layer.iter().copied().max().unwrap_or(0);
    let mut bands: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for (index, depth) in layer.iter().enumerate() {
        bands[*depth].push(index);
    }

    // Box geometry: label lines stacked in the box, sequential
    // placement inside each band, 3-space gutters between boxes.
    let box_w: Vec<usize> = d
        .labels
        .iter()
        .map(|lines| {
            lines
                .iter()
                .map(|l| width::width(l))
                .max()
                .unwrap_or(2)
                .max(2)
                + 4
        })
        .collect();
    let box_h: Vec<usize> = d.labels.iter().map(|lines| lines.len() + 2).collect();
    let mut x = vec![0usize; count];
    let mut total_width = 0usize;
    for band in &bands {
        let mut cursor = 0usize;
        for &index in band {
            x[index] = cursor;
            cursor += box_w[index] + 3;
        }
        total_width = total_width.max(cursor.saturating_sub(3));
    }
    if total_width > columns {
        return None;
    }
    let band_h: Vec<usize> = bands
        .iter()
        .map(|band| band.iter().map(|&n| box_h[n]).max().unwrap_or(3))
        .collect();

    // Gap heights: one elbow row per terminating edge, plus a vertical
    // row, the arrowhead row, and — when any edge terminates here —
    // one visible line row above the elbow (a dashed edge must show).
    let mut gap_height = vec![2usize; max_layer];
    let mut terminating = vec![0usize; max_layer];
    for edge in &d.edges {
        if layer[edge.to] > layer[edge.from] {
            terminating[layer[edge.to] - 1] += 1;
        }
    }
    for (gap, count) in terminating.iter().enumerate() {
        if *count > 0 {
            gap_height[gap] = gap_height[gap].max(count + 2);
        }
    }

    // One flat canvas per display row: bands contribute an annotation
    // row (subgraph frame titles, only when frames exist) plus their
    // box rows, gaps theirs; `band_top` is the absolute row of each
    // band's annotation row.
    let frame_padded = d.groups.iter().any(|(_, members)| members.len() >= 2);
    let band_lead = usize::from(frame_padded);
    if frame_padded {
        // Side margins so frames clear their member boxes.
        for xi in x.iter_mut() {
            *xi += 2;
        }
        total_width += 5;
        if total_width > columns {
            return None;
        }
    }
    let mut rows: Vec<RowCanvas> = Vec::new();
    let mut band_top = vec![0usize; max_layer + 1];
    for depth in 0..=max_layer {
        band_top[depth] = rows.len();
        for _ in 0..band_lead + band_h[depth] {
            rows.push(RowCanvas::new(total_width));
        }
        if depth < max_layer {
            for _ in 0..gap_height[depth] {
                rows.push(RowCanvas::new(total_width));
            }
        }
    }
    // Absolute row where a band's boxes start / its gaps begin.
    let band_box_top = |depth: usize| band_top[depth] + band_lead;
    let gap_start_of = |gap: usize| band_top[gap] + band_lead + band_h[gap];

    // Edges route down the band gaps.
    let mut elbow_slot = vec![0usize; max_layer];
    for edge in &d.edges {
        let (fl, tl) = (layer[edge.from], layer[edge.to]);
        if tl == fl {
            continue; // same-layer edges do not route
        }
        let from_center = x[edge.from] + box_w[edge.from] / 2;
        let to_center = x[edge.to] + box_w[edge.to] / 2;
        let vfill = edge.style.vertical();
        let head = edge.style.head();
        let hfill = edge.style.horizontal();
        for gap in fl..tl {
            let gap_start = gap_start_of(gap);
            let height = gap_height[gap];
            if gap + 1 < tl {
                // Pass-through: a straight vertical through every gap row.
                for row in &mut rows[gap_start..gap_start + height] {
                    row.put(from_center, vfill);
                }
                continue;
            }
            if from_center == to_center {
                for row in &mut rows[gap_start..gap_start + height - 1] {
                    row.put(from_center, vfill);
                }
                rows[gap_start + height - 1].put(to_center, head);
                continue;
            }
            let slot = elbow_slot[gap];
            elbow_slot[gap] += 1;
            let elbow = gap_start + slot.min(height.saturating_sub(2));
            for row in &mut rows[gap_start..elbow] {
                row.put(from_center, vfill);
            }
            // Stroke-correct corners: right turn (└,┐), left turn (┘,┌).
            let (corner_from, corner_to) = if from_center < to_center {
                ("└", "┐")
            } else {
                ("┘", "┌")
            };
            rows[elbow].put(from_center, corner_from);
            rows[elbow].put(to_center, corner_to);
            let (left, right) = if from_center < to_center {
                (from_center, to_center)
            } else {
                (to_center, from_center)
            };
            if !draw_horizontal(
                &mut rows[elbow],
                left + 1,
                right,
                edge.label.as_deref(),
                hfill,
            ) && let Some(text) = &edge.label
            {
                // Span too narrow for the label: annotate beside the
                // arrowhead instead of dropping it.
                rows[gap_start + height - 2].put(right + 2, text.clone());
            }
            for row in &mut rows[elbow + 1..gap_start + height - 1] {
                row.put(to_center, vfill);
            }
            rows[gap_start + height - 1].put(to_center, head);
        }
    }

    // Subgraph groups frame their members' bounding box.
    for (title, members) in &d.groups {
        let _ = draw_group_frame(
            &mut rows,
            members,
            title,
            &layer,
            &x,
            &box_w,
            &band_h,
            &band_top,
            band_lead,
            total_width,
        );
    }

    // Boxes paint last so corners survive edge and frame lines.
    for (depth, band) in bands.iter().enumerate() {
        let top = band_box_top(depth);
        for &index in band {
            let (bx, bw) = (x[index], box_w[index]);
            rows[top].put(bx, format!("┌{}┐", "─".repeat(bw - 2)));
        }
        let body_rows = band_h[depth] - 2;
        for (row_index, row) in rows[top + 1..top + 1 + body_rows].iter_mut().enumerate() {
            for &index in band {
                if let Some(line) = d.labels[index].get(row_index) {
                    let pad = box_w[index] - 2 - width::width(line);
                    let left = pad / 2;
                    row.put(
                        x[index],
                        format!("│{}{line}{}│", " ".repeat(left), " ".repeat(pad - left)),
                    );
                }
            }
        }
        for &index in band {
            let (bx, bw) = (x[index], box_w[index]);
            rows[top + 1 + box_h[index] - 2].put(bx, format!("└{}┘", "─".repeat(bw - 2)));
        }
    }

    Some(rows.iter().map(RowCanvas::render).collect())
}

/// Draw a subgraph group's bounding frame. The top border rides the
/// band's annotation row (exclusively the frame's), so the title never
/// collides with member boxes; sides and bottom are per-cell glyphs
/// that member boxes (painted later) cut their own corners out of.
#[allow(clippy::too_many_arguments)]
fn draw_group_frame(
    rows: &mut [RowCanvas],
    members: &[usize],
    title: &str,
    layer: &[usize],
    x: &[usize],
    box_w: &[usize],
    band_h: &[usize],
    band_top: &[usize],
    band_lead: usize,
    total_width: usize,
) -> Option<()> {
    if members.len() < 2 {
        return Some(());
    }
    let min_x = members.iter().map(|&n| x[n]).min()?;
    let max_end = members.iter().map(|&n| x[n] + box_w[n]).max()?;
    let top_band = members.iter().map(|&n| layer[n]).min()?;
    let bottom_band = members.iter().map(|&n| layer[n]).max()?;
    let left = min_x.saturating_sub(2);
    let right = max_end + 1;
    if right + 1 >= total_width || right <= left + 4 {
        return Some(());
    }
    let top_row = band_top[top_band];
    let bottom_row = band_top[bottom_band] + band_lead + band_h[bottom_band] - 1;
    if bottom_row >= rows.len() {
        return Some(());
    }
    // Top border with the inline title on the annotation row.
    let mut top = format!("┌─ {title} ");
    if width::width(&top) > right - left {
        top = String::from("┌");
    }
    while width::width(&top) < right - left {
        top.push('─');
    }
    top.push('┐');
    rows[top_row].put(left, top);
    // Sides and bottom.
    for row in &mut rows[top_row + 1..bottom_row] {
        row.put(left, "│");
        row.put(right, "│");
    }
    rows[bottom_row].put(left, "└");
    for col in left + 1..right {
        rows[bottom_row].put(col, "─");
    }
    rows[bottom_row].put(right, "┘");
    Some(())
}

/// Fill a horizontal run with dashes, swapping the middle for an edge
/// label when one fits. Runs either direction; an overlapping vertical
/// at the run's own column is kept (the corner glyph wins).
fn draw_horizontal(
    row: &mut RowCanvas,
    start: usize,
    end: usize,
    label: Option<&str>,
    fill: &str,
) -> bool {
    if end <= start {
        return false;
    }
    for col in start..end {
        row.put(col, fill);
    }
    if let Some(label) = label {
        let label_w = width::width(label);
        let room = end - start;
        if label_w < room {
            let col = start + (room - label_w) / 2;
            row.put(col, label.to_string());
            return true;
        }
    }
    false
}

/// Lay the diagram out left-to-right: layers become columns, boxes
/// stack inside a column, edges run through the column gutters with
/// one elbow each. Boxes paint last, so an edge crossing a taller
/// neighbor's column is clipped rather than corrupting the boxes.
fn layout_lr(d: &Diagram, columns: usize) -> Option<Vec<String>> {
    let count = d.labels.len();
    if count == 0 || count > 100 {
        return None;
    }
    let mut layer = vec![0usize; count];
    for _ in 0..count {
        for edge in &d.edges {
            if layer[edge.to] < layer[edge.from] + 1 {
                layer[edge.to] = layer[edge.from] + 1;
                if layer[edge.to] >= count {
                    return None; // cycle
                }
            }
        }
    }
    let max_layer = layer.iter().copied().max().unwrap_or(0);
    let mut cols: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for (index, depth) in layer.iter().enumerate() {
        cols[*depth].push(index);
    }

    let box_w: Vec<usize> = d
        .labels
        .iter()
        .map(|lines| {
            lines
                .iter()
                .map(|l| width::width(l))
                .max()
                .unwrap_or(2)
                .max(2)
                + 4
        })
        .collect();
    let box_h: Vec<usize> = d.labels.iter().map(|lines| lines.len() + 2).collect();
    let mut col_x = vec![0usize; max_layer + 1];
    let mut col_w = vec![0usize; max_layer + 1];
    let mut total_width = 0usize;
    for depth in 0..=max_layer {
        col_w[depth] = cols[depth].iter().map(|&n| box_w[n]).max().unwrap_or(2);
        col_x[depth] = total_width;
        total_width += col_w[depth] + 3;
    }
    let total_width = total_width.saturating_sub(3);
    let mut total_height = 0usize;
    let mut y = vec![0usize; count];
    for band in &cols {
        let mut cursor = 0usize;
        for &index in band {
            y[index] = cursor;
            cursor += box_h[index] + 2;
        }
        total_height = total_height.max(cursor.saturating_sub(2));
    }
    if total_width > columns || total_height > 120 || total_height == 0 {
        return None;
    }
    let mut rows: Vec<RowCanvas> = (0..total_height)
        .map(|_| RowCanvas::new(total_width))
        .collect();

    for edge in &d.edges {
        let (fl, tl) = (layer[edge.from], layer[edge.to]);
        if tl == fl {
            continue;
        }
        let from_row = y[edge.from] + box_h[edge.from] / 2;
        let to_row = y[edge.to] + box_h[edge.to] / 2;
        let src_right = col_x[fl] + box_w[edge.from];
        let dst_left = col_x[tl];
        let vfill = edge.style.vertical();
        let hfill = edge.style.horizontal();
        if from_row == to_row || tl > fl + 1 {
            // Straight run into the target's left edge.
            let head_col = dst_left.saturating_sub(1);
            draw_horizontal(
                &mut rows[from_row],
                src_right,
                head_col,
                edge.label.as_deref(),
                hfill,
            );
            if head_col < total_width {
                rows[from_row].put(head_col, "▶");
            }
            continue;
        }
        // Adjacent columns, different rows: elbow in the gutter.
        let gx = col_x[fl] + col_w[fl] + 1;
        for col in src_right..gx {
            rows[from_row].put(col, hfill);
        }
        rows[from_row].put(gx, if to_row > from_row { "┐" } else { "┘" });
        let (top, bottom) = (from_row.min(to_row), from_row.max(to_row));
        for row in &mut rows[top + 1..bottom] {
            row.put(gx, vfill);
        }
        rows[to_row].put(gx, if to_row > from_row { "└" } else { "┌" });
        let head_col = dst_left.saturating_sub(1);
        draw_horizontal(
            &mut rows[to_row],
            gx + 1,
            head_col,
            edge.label.as_deref(),
            hfill,
        );
        if head_col < total_width {
            rows[to_row].put(head_col, "▶");
        }
    }

    // Boxes paint last.
    for depth in 0..=max_layer {
        for &index in &cols[depth] {
            let (bx, bw, by, bh) = (col_x[depth], box_w[index], y[index], box_h[index]);
            rows[by].put(bx, format!("┌{}┐", "─".repeat(bw - 2)));
            for (row_index, row) in rows[by + 1..by + bh - 1].iter_mut().enumerate() {
                if let Some(line) = d.labels[index].get(row_index) {
                    let pad = bw - 2 - width::width(line);
                    let left = pad / 2;
                    row.put(
                        bx,
                        format!("│{}{line}{}│", " ".repeat(left), " ".repeat(pad - left)),
                    );
                }
            }
            rows[by + bh - 1].put(bx, format!("└{}┘", "─".repeat(bw - 2)));
        }
    }
    Some(rows.iter().map(RowCanvas::render).collect())
}

/// One display row: cells placed at exact display columns, last write
/// wins. Renders with single-width spaces in the gaps.
struct RowCanvas {
    placements: Vec<(usize, String)>,
    width: usize,
}

impl RowCanvas {
    fn new(width: usize) -> Self {
        Self {
            placements: Vec::new(),
            width,
        }
    }

    /// Place `text` at display column `col`, dropping overlapped cells.
    fn put(&mut self, col: usize, text: impl Into<String>) {
        let text = text.into();
        let span = width::width(&text);
        if span == 0 || col >= self.width {
            return;
        }
        let end = col + span;
        self.placements
            .retain(|(c, t)| c + width::width(t) <= col || *c >= end);
        self.placements.push((col, text));
    }

    /// Render the row: placements sorted by column, spaces elsewhere.
    fn render(&self) -> String {
        let mut items = self.placements.clone();
        items.sort_by_key(|(col, _)| *col);
        let mut out = String::new();
        let mut pos = 0usize;
        for (col, text) in items {
            if col < pos {
                continue; // safety: overlap filter already removed these
            }
            out.push_str(&" ".repeat(col - pos));
            pos = col + width::width(&text);
            out.push_str(&text);
        }
        if pos < self.width {
            out.push_str(&" ".repeat(self.width - pos));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// State-diagram adapter
// ---------------------------------------------------------------------------

/// Rewrite state-diagram syntax into flowchart syntax: `a --> b: lbl`
/// becomes `a -- lbl --> b`, `[*]` terminators become marker nodes.
/// Composite states and notes are unsupported (`None`).
fn adapt_state_diagram(source: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that flip the global render toggle.
    static TOGGLE_LOCK: Mutex<()> = Mutex::new(());

    const FLOW: &str = "\
graph TD
    A[开始] --> B{是否?}
    B -- 是 --> C[执行]
    B -- 否 --> D[跳过]
    C --> E[结束]
    D --> E";

    #[test]
    fn flowchart_renders_bands_and_arrows() {
        let lines = render_diagram(FLOW, 80).expect("supported flowchart renders");
        let joined = lines.join("\n");
        // Every node label appears exactly once inside a box row.
        for label in ["开始", "是否?", "执行", "跳过", "结束"] {
            assert!(joined.contains(label), "label {label}: {joined}");
        }
        // Boxes carry all three borders.
        assert!(lines.iter().any(|l| l.contains('┌') && l.contains('┐')));
        assert!(lines.iter().any(|l| l.contains('└') && l.contains('┘')));
        // Edge labels ride the elbow rows.
        assert!(joined.contains('是'), "edge label 是: {joined}");
        assert!(joined.contains('否'), "edge label 否: {joined}");
        // Arrowheads point into the next band.
        assert!(joined.contains('▼'), "arrowheads: {joined}");
        // Every line fits the budget.
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn chains_and_pipe_labels_parse() {
        let lines =
            render_diagram("graph TD\n a --> b --> c\n b -->|yes| d", 80).expect("chain renders");
        let joined = lines.join("\n");
        for label in ["a", "b", "c", "d"] {
            assert!(joined.contains(label), "node {label}: {joined}");
        }
    }

    #[test]
    fn unsupported_constructs_fall_back_to_none() {
        assert!(render_diagram("graph BT\n a --> b", 80).is_none(), "BT");
        assert!(render_diagram("graph RL\n a --> b", 80).is_none(), "RL");
        assert!(render_diagram("a --> b", 80).is_none(), "no direction line");
        assert!(
            render_diagram("sequenceDiagram\n a->>b", 80).is_none(),
            "sequence without participants"
        );
        assert!(
            render_diagram("block-beta\n block: id\n columns 1", 80).is_none(),
            "block-beta"
        );
        assert!(
            render_diagram("sankey-beta\n\nA,B,10", 80).is_none(),
            "sankey"
        );
    }

    /// A streaming fence delivers prefixes before any closing bracket:
    /// an unclosed shape with a multibyte label (`A[开始`) must parse
    /// instead of panicking on a mid-glyph byte slice.
    #[test]
    fn unclosed_shapes_with_multibyte_labels_do_not_panic() {
        assert!(render_diagram("graph TD\n    A[开始\n    B --> C", 80).is_some());
        assert!(render_diagram("graph TD\n    A{循环判断", 80).is_some());
        // Empty unclosed shapes hold no label and stay unsupported.
        assert!(render_diagram("graph TD\n    A(\n    B --> C", 80).is_none());
    }

    /// Every supported shape peels to its bare label, doubled layers
    /// (`[[..]]`, `((..))`) included.
    #[test]
    fn shape_labels_peel_all_bracket_layers() {
        for (token, label) in [
            ("A[text]", "text"),
            ("A(圆)", "圆"),
            ("A{菱形}", "菱形"),
            ("A[[仓库]]", "仓库"),
            ("A((开始))", "开始"),
            ("A[开 始]", "开 始"),
        ] {
            let src = format!("graph TD\n {token} --> B");
            let lines = render_diagram(&src, 80).expect("shape renders");
            let joined = lines.join("\n");
            assert!(joined.contains(label), "{token} -> {label}: {joined}");
        }
    }

    /// Edge chains parse iteratively: a chain far beyond any stack
    /// budget must not overflow the stack.
    #[test]
    fn long_edge_chains_parse_without_stack_overflow() {
        let mut chain = String::from("graph TD\n n0");
        for i in 0..50_000 {
            chain.push_str(&format!(" --> n{}", i + 1));
        }
        // Too wide to lay out at 80 columns, but the parse must hold.
        assert!(render_diagram(&chain, 80).is_none());
    }

    #[test]
    fn oversized_diagrams_fall_back_to_none() {
        assert!(render_diagram(FLOW, 10).is_none(), "too narrow: fallback");
    }

    /// The fit contract is per line: a diagram whose labels overflow
    /// the budget (a sequence label wider than the frame, a chart
    /// value with hundreds of digits) falls back instead of returning
    /// rows the frame would clip.
    #[test]
    fn overflowing_labels_fall_back_to_none() {
        let long_label = "x".repeat(120);
        let sequence = format!(
            "sequenceDiagram\n    participant A\n    participant B\n    A->>B: {long_label}"
        );
        assert!(render_diagram(&sequence, 80).is_none(), "long label");
        let huge_value = "xychart-beta\n    x-axis [a, b]\n    bar [1e300, 5]";
        assert!(render_diagram(huge_value, 80).is_none(), "huge value");
    }

    /// The node cap refuses oversized diagrams at parse time — a huge
    /// subgraph must fall back instead of grinding the member dedup.
    #[test]
    fn diagrams_beyond_the_node_cap_fall_back_to_none() {
        let mut source = String::from("graph TD\n subgraph big\n a0");
        for i in 1..150 {
            source.push_str(&format!(" --> n{i}"));
        }
        source.push_str("\n end");
        assert!(render_diagram(&source, 80).is_none(), "node cap");
        // A sequence beyond the message cap falls back too.
        let mut sequence = String::from("sequenceDiagram\n    participant A\n    participant B\n");
        for _ in 0..201 {
            sequence.push_str("    A->>B: ping\n");
        }
        assert!(render_diagram(&sequence, 80).is_none(), "message cap");
    }

    /// LR flowcharts lay out left-to-right with side arrowheads.
    #[test]
    fn lr_flowcharts_render_columns_and_side_arrows() {
        let src = "graph LR\n A[输入] --> B[处理] --> C[输出]";
        let lines = render_diagram(src, 80).expect("LR renders");
        let joined = lines.join("\n");
        for label in ["输入", "处理", "输出"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains('▶'), "side arrowheads: {joined}");
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    /// Subgraph members render inside a titled frame.
    #[test]
    fn subgraphs_render_as_titled_frames() {
        let src = "graph TD\n subgraph 组\n a --> b\n end\n b --> c";
        let lines = render_diagram(src, 80).expect("subgraph renders");
        let joined = lines.join("\n");
        for label in ["组", "a", "b", "c"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains('▼'), "edges still route: {joined}");
    }

    /// The markdown hook only renders when the global toggle is on.
    #[test]
    fn state_diagrams_render_as_flowcharts() {
        let src = "stateDiagram-v2\n    [*] --> \u{5f85}\u{5904}\u{7406}\n    \u{5f85}\u{5904}\u{7406} --> \u{5b8c}\u{6210}: start\n    \u{5b8c}\u{6210} --> [*]";
        let lines = render_diagram(src, 80).expect("state diagram renders");
        let joined = lines.join("\n");
        assert!(joined.contains("\u{5f85}\u{5904}\u{7406}"), "{joined}");
        assert!(joined.contains("\u{25b6}"), "entry terminator: {joined}");
        assert!(joined.contains("\u{25a0}"), "exit terminator: {joined}");
        assert!(joined.contains("start"), "edge label: {joined}");
    }

    #[test]
    fn sequence_diagrams_render_participants_and_arrows() {
        let src = "sequenceDiagram\n    participant U as \u{7528}\u{6237}\n    participant A as Agent\n    U->>A: hello\n    A-->>U: hi";
        let lines = render_diagram(src, 80).expect("sequence diagram renders");
        let joined = lines.join("\n");
        for label in ["hello", "hi", "Agent"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains("\u{25b6}"), "arrowheads: {joined}");
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn pie_charts_render_as_labeled_bars() {
        let src = "pie title langs\n    \"JS\" : 35\n    \"Go\" : 15";
        let lines = render_diagram(src, 80).expect("pie renders");
        let joined = lines.join("\n");
        assert!(joined.contains("langs"), "title: {joined}");
        assert!(joined.contains("JS") && joined.contains("Go"), "{joined}");
        assert!(joined.contains('%'), "percentage: {joined}");
    }

    /// A pie slice whose label merely starts with the word `pie` is
    /// data, not the diagram header.
    #[test]
    fn pie_slices_named_like_the_header_still_render() {
        let src = "pie\n    piece : 5\n    other : 5";
        let lines = render_diagram(src, 80).expect("pie renders");
        let joined = lines.join("\n");
        assert!(joined.contains("piece"), "slice kept: {joined}");
        assert!(joined.contains("other"), "{joined}");
        assert!(joined.contains("50.0%"), "share: {joined}");
    }

    /// A NaN slice value poisons the total: fall back instead of
    /// rendering `NaN%`.
    #[test]
    fn pie_nan_values_fall_back_to_none() {
        let src = "pie\n    a : NaN\n    b : 5";
        assert!(render_diagram(src, 80).is_none(), "NaN total falls back");
    }

    #[test]
    fn class_diagrams_render_boxes_and_inheritance() {
        let src = "classDiagram\n    class Animal {\n        +String name\n        +makeSound()\n    }\n    class Dog\n    Animal <|-- Dog";
        let lines = render_diagram(src, 80).expect("class diagram renders");
        let joined = lines.join("\n");
        assert!(joined.contains("Animal"), "{joined}");
        assert!(joined.contains("makeSound()"), "{joined}");
        assert!(
            joined.contains("\u{25bd}"),
            "hollow inheritance head: {joined}"
        );
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    /// A class name wider than its members sizes the box: a long name
    /// must not bleed across the gutter and erase the neighbor box.
    #[test]
    fn class_boxes_grow_to_fit_their_names() {
        let src =
            "classDiagram\n    class VeryLongClassName {\n        +run()\n    }\n    class Short";
        let lines = render_diagram(src, 80).expect("class diagram renders");
        let name_row = lines
            .iter()
            .find(|l| l.contains("VeryLongClassName"))
            .expect("name row");
        assert!(
            name_row.contains("Short"),
            "neighbor box survives the long name: {name_row:?}"
        );
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    /// Composition, aggregation, and dependency relations ride the
    /// shared engine with their own edge styles.
    #[test]
    fn class_diagrams_render_all_relation_kinds() {
        let src = "classDiagram\n    Engine *-- Car\n    Wheel o-- Car\n    Driver ..> Car : uses";
        let lines = render_diagram(src, 80).expect("relations render");
        let joined = lines.join("\n");
        for label in ["Engine", "Car", "Wheel", "Driver", "uses"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains('┆'), "dotted dependency: {joined}");
    }

    #[test]
    fn er_diagrams_render_entities_and_relationships() {
        let src = "erDiagram\n    USER ||--o{ ORDER : places\n    USER {\n        int id PK\n        string username\n    }";
        let lines = render_diagram(src, 80).expect("ER renders");
        let joined = lines.join("\n");
        assert!(
            joined.contains("USER") && joined.contains("ORDER"),
            "{joined}"
        );
        assert!(joined.contains("int id PK"), "attribute rows: {joined}");
        assert!(joined.contains("places"), "relationship label: {joined}");
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    /// Non-identifying relationships (`..`) render with dashed edges.
    #[test]
    fn er_diagrams_mark_non_identifying_relationships() {
        let src = "erDiagram\n    CUSTOMER .. CUSTOMER_ACCOUNT : has";
        let lines = render_diagram(src, 80).expect("dotted ER renders");
        let joined = lines.join("\n");
        assert!(joined.contains("CUSTOMER"), "{joined}");
        assert!(joined.contains('┆'), "dashed edge: {joined}");
    }

    #[test]
    fn c4_diagrams_render_persons_systems_and_relations() {
        let src = "C4Context\n    title demo\n    Person(customer, \"Customer\", \"A user\")\n    System(billing, \"Billing\", \"The system\")\n    Rel(customer, billing, \"Uses\")";
        let lines = render_diagram(src, 80).expect("C4 renders");
        let joined = lines.join("\n");
        for label in ["Customer", "Billing", "Uses", "A user"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains('▼'), "relations: {joined}");
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn git_graphs_render_branch_timelines() {
        let src = "gitGraph\n    commit id: \"one\"\n    branch develop\n    commit id: \"two\"\n    checkout main\n    merge develop";
        let lines = render_diagram(src, 80).expect("gitGraph renders");
        let joined = lines.join("\n");
        for label in ["main", "develop", "one", "two", "merge develop"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains('●'), "merge glyph: {joined}");
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    /// Unnamed commits fall back to sequence numbers instead of
    /// dropping the event.
    #[test]
    fn git_graphs_name_unnamed_commits() {
        let src = "gitGraph\n    commit\n    commit tag: \"v1\"";
        let lines = render_diagram(src, 80).expect("unnamed commits render");
        let joined = lines.join("\n");
        assert!(joined.contains("#1") && joined.contains("v1"), "{joined}");
    }

    #[test]
    fn mindmaps_render_as_trees() {
        let src = "mindmap\n  root((编程语言))\n    静态类型\n      Java\n      Go\n    动态类型\n      Python";
        let lines = render_diagram(src, 80).expect("mindmap renders");
        let joined = lines.join("\n");
        for label in ["编程语言", "静态类型", "Java", "Go", "动态类型", "Python"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(
            joined.contains("├─") || joined.contains("└─"),
            "connectors: {joined}"
        );
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn timelines_render_periods_and_events() {
        let src = "timeline\n    title 历史\n    2021 : 发现漏洞\n         : 修复\n    2022 : 发布";
        let lines = render_diagram(src, 80).expect("timeline renders");
        let joined = lines.join("\n");
        for label in ["历史", "2021", "发现漏洞", "修复", "2022", "发布"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn journeys_render_scored_task_rows() {
        let src =
            "journey\n    title 工作日\n    section 早晨\n        泡茶: 5: 我\n        上楼: 3: 我";
        let lines = render_diagram(src, 80).expect("journey renders");
        let joined = lines.join("\n");
        for label in ["工作日", "早晨", "泡茶", "上楼", "我"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains('█'), "score bars: {joined}");
    }

    #[test]
    fn quadrant_charts_render_grid_and_points() {
        let src = "quadrantChart\n    title 影响力\n    x-axis Low --> High\n    y-axis Low --> High\n    quadrant-1 Plan\n    quadrant-2 Promote\n    quadrant-3 Demo\n    quadrant-4 Build\n    \"Point A\": [0.3, 0.7]\n    \"Point B\": [0.8, 0.2]";
        let lines = render_diagram(src, 80).expect("quadrant renders");
        let joined = lines.join("\n");
        for label in ["Plan", "Promote", "Demo", "Build", "Point A", "Point B"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains('┼'), "axis cross: {joined}");
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn xycharts_render_scaled_bar_rows() {
        let src = "xychart-beta\n    title \"销量\"\n    x-axis [1月, 2月, 3月]\n    y-axis \"销量\" 0 --> 50\n    bar [10, 30, 20]";
        let lines = render_diagram(src, 80).expect("xychart renders");
        let joined = lines.join("\n");
        assert!(joined.contains("销量"), "{joined}");
        for label in ["1月", "2月", "3月"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains('█'), "bars: {joined}");
        let bar_of = |name: &str| {
            lines
                .iter()
                .find(|l| l.contains(name))
                .map(|l| l.matches('█').count())
                .unwrap()
        };
        assert!(bar_of("2月") > bar_of("1月"), "scaled to max: {lines:?}");
    }

    /// A NaN series value poisons the scale: fall back instead of
    /// rendering a `NaN` row.
    #[test]
    fn xychart_nan_values_fall_back_to_none() {
        let src = "xychart-beta\n    x-axis [a, b]\n    bar [NaN, 10]";
        assert!(render_diagram(src, 80).is_none(), "NaN series falls back");
    }

    #[test]
    fn requirement_diagrams_render_blocks_and_edges() {
        let src = "requirementDiagram\n    requirement test_req {\n        id: 1\n        risk: high\n        verifymethod: test\n    }\n    element test_entity {\n        type: simulation\n    }\n    test_entity - satisfies -> test_req";
        let lines = render_diagram(src, 80).expect("requirement renders");
        let joined = lines.join("\n");
        for label in ["test_req", "test_entity", "risk: high", "satisfies"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn gantt_charts_render_scaled_bars() {
        let src = "gantt\n    title plan\n    dateFormat YYYY-MM-DD\n    section s\n    任务一 :a1, 2026-01-01, 10d\n    任务二 :a2, after a1, 5d";
        let lines = render_diagram(src, 80).expect("gantt renders");
        let joined = lines.join("\n");
        assert!(joined.contains("plan"), "title: {joined}");
        for name in ["任务一", "任务二"] {
            assert!(joined.contains(name), "{name}: {joined}");
        }
        // The 10-day task gets a longer bar than the 5-day one.
        let bar_of = |name: &str| {
            lines
                .iter()
                .find(|l| l.contains(name))
                .map(|l| l.matches('\u{2588}').count())
                .unwrap()
        };
        assert!(bar_of("任务一") > bar_of("任务二"), "{lines:?}");
    }

    /// Real-world gantt shapes: a task with a trailing tag (`done`) and
    /// a task with the id omitted must both render, not fall back.
    #[test]
    fn gantt_tasks_with_tags_and_without_ids_render() {
        let tagged =
            "gantt\n    任务一 :a1, 2026-01-01, 10d, done\n    任务二 :a2, after a1, 5d, active";
        let lines = render_diagram(tagged, 80).expect("tagged tasks render");
        let joined = lines.join("\n");
        assert!(
            joined.contains("任务一") && joined.contains("任务二"),
            "{joined}"
        );
        let idless = "gantt\n    任务一 :2026-01-01, 10d\n    任务二 :after 任务一, 5d";
        let lines = render_diagram(idless, 80).expect("id-less tasks render");
        let joined = lines.join("\n");
        assert!(
            joined.contains("任务一") && joined.contains("任务二"),
            "{joined}"
        );
    }

    /// Leap-year dates round-trip: 2024-02-29 plus one day ends 03-01.
    #[test]
    fn gantt_leap_year_dates_render() {
        let src = "gantt\n    跳日 :a1, 2024-02-29, 1d";
        let lines = render_diagram(src, 80).expect("leap date renders");
        let joined = lines.join("\n");
        assert!(joined.contains("03-01"), "end after the leap day: {joined}");
    }

    /// Absurd dates and durations fall back to the source view instead
    /// of overflowing the day arithmetic.
    #[test]
    fn gantt_out_of_range_values_fall_back_to_none() {
        let huge_year = "gantt\n    t :a, 9223372036854775807-01-01, 5d";
        assert!(render_diagram(huge_year, 80).is_none(), "huge year");
        let huge_span = "gantt\n    t :a, 2026-01-01, 9223372036854775807d";
        assert!(render_diagram(huge_span, 80).is_none(), "huge duration");
        let negative = "gantt\n    t :a, 2026-01-01, -5d";
        assert!(render_diagram(negative, 80).is_none(), "negative duration");
    }

    #[test]
    fn toggle_switches_render_mode() {
        let _guard = TOGGLE_LOCK.lock().unwrap();
        let previous = render_enabled();
        set_render_enabled(!previous);
        assert_eq!(render_enabled(), !previous);
        set_render_enabled(previous);
        assert_eq!(render_enabled(), previous);
    }
}
