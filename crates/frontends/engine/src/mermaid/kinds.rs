//! The remaining shared-engine diagram kinds: class, ER, C4, and
//! requirement, each parsed into the graph IR's nodes and edges.

use std::collections::HashMap;

use super::graph::{Diagram, Direction, EdgeStyle};
use super::starts_with_keyword;
use super::truncate_width;

// ---------------------------------------------------------------------------
// classDiagram — class boxes with member rows over the shared engine
// ---------------------------------------------------------------------------

/// Relation markers, longest first so `<|--` wins over `--`.
const CLASS_RELATIONS: [&str; 10] = [
    "<|--", "--|>", "<|..", "..|>", "*--", "o--", "..>", "-->", "--", "..",
];

/// Parse a class diagram into the graph IR: one node per class with
/// member rows as label lines, one edge per relation.
pub(super) fn parse_class_diagram(source: &str) -> Option<Diagram> {
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
                } else if open.is_some() {
                    // Bare member row (`int age`, `quack()`): Mermaid
                    // accepts members without a visibility prefix inside
                    // an open class block.
                    if let Some(index) = open {
                        let rows = &mut diagram.labels[index];
                        if rows.len() < 16 {
                            rows.push(line.to_string());
                        }
                    }
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
pub(super) fn parse_er_diagram(source: &str) -> Option<Diagram> {
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
pub(super) fn parse_c4_diagram(source: &str) -> Option<Diagram> {
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
            // A boundary call opens a `{ ... }` block; its closing brace
            // carries no keyword and must not fail the parse.
            "}" => {}
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
pub(super) fn parse_requirement_diagram(source: &str) -> Option<Diagram> {
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
