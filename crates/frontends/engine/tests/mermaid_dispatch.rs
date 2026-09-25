//! Black-box integration tests for the mermaid entry points: the
//! `render_diagram` language dispatch and the `MermaidFences` seam.
//! Only public API (`tui_engine::mermaid::*`) is exercised.

use std::sync::Mutex;

use tui_engine::mermaid::{MermaidFences, render_diagram, render_enabled, set_render_enabled};
use tui_engine::markdown::{FenceRenderer, Markdown, MarkdownStyle, PlainHighlighter};

/// Serializes tests that flip the process-global render toggle.
static TOGGLE_LOCK: Mutex<()> = Mutex::new(());

/// The keyword on the first meaningful line decides the renderer.
#[test]
fn keyword_dispatches_to_one_renderer_per_kind() {
    let sources = [
        ("graph TD\n a --> b", "flowchart"),
        ("flowchart TD\n a --> b", "flowchart alias"),
        (
            "stateDiagram-v2\n    [*] --> s1\n    s1 --> [*]",
            "state adapter",
        ),
        (
            "sequenceDiagram\n    participant A\n    participant B\n    A->>B: hi",
            "sequence",
        ),
        ("pie\n    a : 1\n    b : 1", "pie"),
        (
            "classDiagram\n    class A {\n        +run()\n    }",
            "class",
        ),
        ("gantt\n    t :a, 2026-01-01, 5d", "gantt"),
    ];
    for (source, kind) in sources {
        let lines = render_diagram(source, 80)
            .unwrap_or_else(|| panic!("{kind} dispatches to a renderer"));
        assert!(!lines.is_empty(), "{kind}: no lines");
    }
}

/// Case and leading comments do not change the dispatch: the keyword
/// is the first non-comment, case-insensitive word.
#[test]
fn keyword_match_ignores_case_and_comments() {
    assert!(render_diagram("%% a comment\nGRAPH TD\n a --> b", 80).is_some());
    assert!(
        render_diagram("SequenceDiagram\n    participant A\n    participant B\n    A->>B: hi", 80)
            .is_some()
    );
}

/// Unsupported or malformed sources return `None` — the caller falls
/// back to the plain source view.
#[test]
fn unsupported_sources_return_none() {
    for source in [
        "graph LR\n a --> b",   // unsupported direction
        "graph TD\n subgraph s\n a --> b\n end", // subgraphs
        "a --> b",              // no keyword line
        "mindmap\n root",       // unsupported kind
        "",                     // empty source
    ] {
        assert!(
            render_diagram(source, 80).is_none(),
            "expected fallback: {source:?}"
        );
    }
}

/// A supported diagram that does not fit the column budget renders
/// nothing rather than a broken layout.
#[test]
fn oversized_diagrams_fall_back_to_none() {
    let source = "graph TD\n A[开始] --> B{是否?}";
    assert!(render_diagram(source, 80).is_some(), "fits at 80");
    assert!(render_diagram(source, 4).is_none(), "too narrow: None");
}

/// Diagram lines never exceed the requested width.
#[test]
fn rendered_lines_respect_the_column_budget() {
    let source = "graph TD\n A[输入] --> B{校验?} --> C[输出]";
    let lines = render_diagram(source, 40).expect("renders at 40");
    assert!(lines.iter().all(|line| tui_engine::width::width(line) <= 40));
}

/// Through the markdown seam, only `mermaid` fences (and only while
/// the toggle is on) dispatch into the diagram renderer; everything
/// else falls through to the highlighter.
#[test]
fn mermaid_fences_gate_on_language_and_toggle() {
    let _guard = TOGGLE_LOCK.lock().unwrap();
    let previous = render_enabled();

    set_render_enabled(false);
    assert!(MermaidFences.render_fence("mermaid", "graph TD\n a --> b", 80).is_none());
    set_render_enabled(true);
    assert!(MermaidFences.render_fence("mermaid", "graph TD\n a --> b", 80).is_some());
    set_render_enabled(previous);

    // Any other language (or an unsupported diagram) is None even with
    // the toggle on.
    assert!(MermaidFences.render_fence("python", "x = 1", 80).is_none());
    assert!(MermaidFences.render_fence("mermaid", "graph LR\n a --> b", 80).is_none());
}

/// End to end through the public markdown API: a mermaid fence renders
/// as diagram rows inside the code frame while the toggle is on, and
/// as plain source rows while it is off.
#[test]
fn markdown_renders_mermaid_fences_through_the_seam() {
    let _guard = TOGGLE_LOCK.lock().unwrap();
    let previous = render_enabled();
    let text = "```mermaid\ngraph TD\n a --> b\n```";
    let strip = |lines: &[String]| -> Vec<String> {
        lines.iter().map(|l| tui_engine::width::strip_ansi(l)).collect()
    };

    set_render_enabled(true);
    let mut md = Markdown::new(MarkdownStyle::default(), Box::new(PlainHighlighter));
    let on = strip(&md.render(text, 80));
    assert!(
        on.iter().any(|l| l.contains('▼')),
        "diagram rows while on: {on:?}"
    );

    set_render_enabled(false);
    md.clear_cache();
    let off = strip(&md.render(text, 80));
    assert!(
        off.iter().any(|l| l.contains("graph TD")),
        "source rows while off: {off:?}"
    );

    set_render_enabled(previous);
}
