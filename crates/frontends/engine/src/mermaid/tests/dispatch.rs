//! Keyword dispatch, streaming prefixes, size caps, fit
//! fallbacks, and the process-global render toggle.

use super::super::*;
use super::FLOW;

use std::sync::Mutex;

/// Serializes tests that flip the global render toggle.
static TOGGLE_LOCK: Mutex<()> = Mutex::new(());

/// Extended Mermaid link spellings (runs of `-`/`=`, dotted runs) parse
/// as one edge with the right style instead of panicking or leaking `>`
/// into a node id.
#[test]
fn extended_link_spellings_render_one_edge_each() {
    for src in [
        "graph TD
    A --- B",
        "graph TD
    A ---> B",
        "graph TD
    A ----> B",
        "graph TD
    A ===> B",
        "graph TD
    A ====> B",
        "graph TD
    A -..-> B",
        "graph TD
    A -- text ---> B",
    ] {
        let lines =
            render_diagram(src, 80).unwrap_or_else(|| panic!("extended link must render: {src}"));
        let joined = lines.join(
            "
",
        );
        assert!(joined.contains('A'), "{src}: {joined}");
        assert!(joined.contains('B'), "{src}: {joined}");
        assert!(!joined.contains(">B"), "{src}: {joined}");
    }
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
    let sequence =
        format!("sequenceDiagram\n    participant A\n    participant B\n    A->>B: {long_label}");
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

/// The markdown hook only renders when the global toggle is on.
#[test]
fn toggle_switches_render_mode() {
    let _guard = TOGGLE_LOCK.lock().unwrap();
    let previous = render_enabled();
    set_render_enabled(!previous);
    assert_eq!(render_enabled(), !previous);
    set_render_enabled(previous);
    assert_eq!(render_enabled(), previous);
}
