//! sequenceDiagram lanes: participants, arrows, notes, and the
//! tolerated activation directives.

use super::super::*;

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

/// Messages may reference participants that were never declared: both
/// ends register on first use, as Mermaid does.
#[test]
fn sequence_messages_register_implicit_participants() {
    let src = "sequenceDiagram\n    A->>B: hi";
    let lines = render_diagram(src, 80).expect("implicit participants render");
    let joined = lines.join("\n");
    for label in ["A", "B", "hi"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
}

/// `note over A,B` renders as a box spanning the two lanes, between
/// the participant header and the following messages.
#[test]
fn sequence_notes_render_as_boxes_over_the_lanes() {
    let src = "sequenceDiagram\n    participant A\n    participant B\n    note over A,B: hello there\n    A->>B: hi";
    let lines = render_diagram(src, 80).expect("note renders");
    let joined = lines.join("\n");
    assert!(joined.contains("hello there"), "note text: {joined}");
    let note_row = lines.iter().find(|l| l.contains("hello there")).unwrap();
    assert!(
        note_row.contains('┌') && note_row.contains('┐'),
        "boxed note: {note_row:?}"
    );
    assert!(joined.contains("hi"), "messages still render: {joined}");
}

/// `note left of` / `note right of` hang the box beside one lane.
#[test]
fn sequence_side_notes_hang_beside_the_lane() {
    let left = "sequenceDiagram\n    participant A\n    participant B\n    note left of B: memo";
    let lines = render_diagram(left, 80).expect("left note renders");
    let joined = lines.join("\n");
    assert!(joined.contains("memo"), "{joined}");

    let right = "sequenceDiagram\n    participant A\n    participant B\n    note right of A: ping";
    let lines = render_diagram(right, 80).expect("right note renders");
    assert!(lines.join("\n").contains("ping"), "{lines:?}");
}

/// Activation bookkeeping is presentation-only: the `+`/`-` markers in
/// message heads must not fork phantom participants.
#[test]
fn sequence_activation_markers_do_not_fork_participants() {
    let src = "sequenceDiagram\n    A->>+B: start\n    B-->>-A: done";
    let lines = render_diagram(src, 80).expect("activation shorthand renders");
    let joined = lines.join("\n");
    assert!(
        joined.contains("start") && joined.contains("done"),
        "{joined}"
    );
    // Only two participants exist: the header row shows exactly two boxes.
    let header = width::strip_ansi(&lines[1]);
    assert_eq!(header.matches('│').count(), 4, "two boxes only: {header:?}");
}

/// Standalone `activate`/`deactivate`/`auton` lines are tolerated as
/// no-ops instead of falling back to the source view.
#[test]
fn sequence_directive_lines_are_tolerated() {
    let src = "sequenceDiagram\n    auton\n    participant A\n    participant B\n    activate A\n    A->>B: hi\n    deactivate A";
    let lines = render_diagram(src, 80).expect("directives tolerated");
    assert!(lines.join("\n").contains("hi"), "{lines:?}");
}

/// A right-to-left message points its arrowhead at the receiver: for
/// `B-->>A` the head is the left-pointing triangle, sitting on A's
/// lifeline (A is declared first, so its box sits left of B's).
#[test]
fn sequence_right_to_left_messages_point_at_the_receiver() {
    let src = "sequenceDiagram\n    participant A\n    participant B\n    B-->>A: reply";
    let lines = render_diagram(src, 80).expect("sequence renders");
    let arrow_row = lines
        .iter()
        .find(|l| l.contains('◀') || l.contains('▶'))
        .expect("an arrow row exists");
    assert!(
        arrow_row.contains('◀'),
        "right-to-left arrowhead points left: {arrow_row}"
    );
}
