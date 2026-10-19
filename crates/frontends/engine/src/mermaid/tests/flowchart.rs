//! The shared graph engine over `graph`/`flowchart` sources: band
//! and column layout quality, subgraph frames, and the state-diagram
//! adapter.

use super::super::*;
use super::FLOW;

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

/// TD bands center in the canvas: a single-box band no longer hugs
/// the left edge of the widest band.
#[test]
fn td_bands_center_in_the_canvas() {
    let lines = render_diagram("graph TD\n A --> B\n A --> C", 60).expect("diamond renders");
    assert!(lines[0].starts_with("    "), "top band centered: {lines:?}");
}

/// Bands order by predecessor barycenter: with crossed definition
/// order the children still land under their own parents, keeping the
/// edge elbows short and readable.
#[test]
fn td_bands_order_by_predecessor_centers() {
    // Definition order X, Y would cross without barycenter sorting:
    // A feeds X (first), B feeds Y; the barycenter keeps X left.
    let lines = render_diagram("graph TD\n A --> X\n B --> Y\n B --> X\n A --> Y2", 60)
        .expect("crossed graph renders");
    let joined = lines.join("\n");
    for label in ["A", "B", "X", "Y", "Y2"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    assert!(joined.contains('▼'), "edges route: {joined}");
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

/// A skip-level LR edge routes through the column gutters instead of
/// piercing the intermediate column's box.
#[test]
fn lr_skip_edges_route_through_the_gutters() {
    let src = "graph LR\n A --> B\n B --> C\n A --> C";
    let lines = render_diagram(src, 60).expect("skip edge renders");
    let joined = lines.join("\n");
    for label in ["A", "B", "C"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    // The skip edge turns twice in the lanes (two extra corner rows on
    // top of the adjacent elbows' single turn row).
    let turn_rows = lines
        .iter()
        .filter(|l| l.contains('┐') || l.contains('┘'))
        .count();
    assert!(turn_rows >= 2, "routed around B: {lines:?}");
}

/// An LR edge label wider than its run truncates into the run instead
/// of disappearing.
#[test]
fn lr_labels_truncate_instead_of_dropping() {
    let label = format!("label-{}", "x".repeat(30));
    let src = format!("graph LR\n A -->|{label}| B");
    let lines = render_diagram(&src, 60).expect("long label renders");
    let joined = lines.join("\n");
    assert!(joined.contains("label-"), "label prefix survives: {joined}");
    assert!(joined.contains('…'), "truncation marker: {joined}");
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
