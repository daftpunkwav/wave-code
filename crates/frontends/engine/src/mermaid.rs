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

use std::sync::atomic::{AtomicBool, Ordering};

use crate::width;

use charts::{
    layout_gantt, layout_git_graph, layout_journey, layout_pie, layout_quadrant, layout_xychart,
    parse_gantt, parse_git_graph, parse_journey, parse_pie, parse_quadrant, parse_xychart,
};
use graph::{adapt_state_diagram, parse};
use kinds::{parse_c4_diagram, parse_class_diagram, parse_er_diagram, parse_requirement_diagram};
use layout::layout;
use sequence::{layout_sequence, parse_sequence};
use tree::{parse_mindmap, parse_timeline, render_tree};

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

/// Truncate `text` to at most `max` display columns, ending with `…`.
pub(super) fn truncate_width(text: &str, max: usize) -> String {
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

/// True when `line` starts with the case-insensitive directive `kw`
/// followed by whitespace or end-of-line: task and slice lines whose
/// first word merely extends the keyword stay data.
pub(super) fn starts_with_keyword(line: &str, kw: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    match lower.strip_prefix(kw) {
        Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
        None => false,
    }
}

mod charts;
mod graph;
mod kinds;
mod layout;
mod sequence;
mod tree;

#[cfg(test)]
mod tests;
