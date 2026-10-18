//! Tests for the mermaid renderer ([`crate::mermaid`]): the
//! keyword dispatch, the per-kind layouts, the fit fallbacks, and
//! the process-global render toggle.

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

/// A leading status tag (`done`) precedes the id, and `after x` starts
/// the dependent task when `x` ends, not when it starts.
#[test]
fn gantt_tags_and_after_dependencies_resolve() {
    let src = "gantt\n    title plan\n    Task1 :done, t1, 2024-01-01, 3d\n    Task2 :crit, done, after t1, 2d";
    let lines = render_diagram(src, 80).expect("gantt with tag and after renders");
    let joined = lines.join("\n");
    assert!(joined.contains("Task1"), "{joined}");
    assert!(joined.contains("Task2"), "{joined}");
    // Task1 ends 2024-01-04; `after t1` starts Task2 there, so its bar
    // ends 2024-01-06 (not 01-03, which a start-aligned read would give).
    assert!(joined.contains("01-04"), "{joined}");
    assert!(joined.contains("01-06"), "{joined}");
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
    let src = "classDiagram\n    class VeryLongClassName {\n        +run()\n    }\n    class Short";
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

/// A class box with members draws a full-width rule between the title
/// and the members (the classic three-section class box).
#[test]
fn class_boxes_separate_their_title_from_members() {
    let src = "classDiagram\n    class Animal {\n        +String name\n        +makeSound()\n    }";
    let lines = render_diagram(src, 80).expect("class diagram renders");
    let rule_row = lines
        .iter()
        .find(|l| l.contains('│') && l.contains('─') && !l.contains('┌') && !l.contains('└'));
    assert!(rule_row.is_some(), "a rule row under the title: {lines:?}");
}

/// Members past the box cap collapse into an explicit tail row
/// instead of vanishing silently.
#[test]
fn class_member_overflow_shows_a_tail_row() {
    let mut src = String::from("classDiagram\n    class Big {\n");
    for n in 0..20 {
        src.push_str(&format!("        +field{n}()\n"));
    }
    src.push_str("    }");
    let lines = render_diagram(&src, 80).expect("big class renders");
    let joined = lines.join("\n");
    assert!(joined.contains("field14()"), "last visible row: {joined}");
    assert!(!joined.contains("field15()"), "past the cap: {joined}");
    assert!(joined.contains("… (+5 more)"), "tail row: {joined}");
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

/// The crow's-foot cardinality heads decode into the edge label: the
/// relationship's multiplicity is its most load-bearing information,
/// and parallel lanes keep their labels apart.
#[test]
fn er_cardinalities_render_into_the_edge_label() {
    let src = "erDiagram\n    USER ||--o{ ORDER : places\n    CUSTOMER }o--|| ACCOUNT : holds";
    let lines = render_diagram(src, 80).expect("ER renders");
    let joined = lines.join("\n");
    assert!(joined.contains("1→0..*"), "one to many: {joined}");
    assert!(joined.contains("0..*→1"), "many to one: {joined}");
}

/// A lone relationship has the lane to itself: the full label
/// (multiplicity plus verb) renders beside the lifeline.
#[test]
fn er_cardinality_label_renders_in_full_when_alone() {
    let src = "erDiagram\n    USER ||--o{ ORDER : places";
    let lines = render_diagram(src, 80).expect("ER renders");
    let joined = lines.join("\n");
    assert!(joined.contains("1→0..* places"), "full label: {joined}");
}

/// A bare `..` marker has no cardinality heads and keeps the plain
/// relationship label.
#[test]
fn er_marker_without_heads_keeps_the_plain_label() {
    let src = "erDiagram\n    CUSTOMER .. CUSTOMER_ACCOUNT : has";
    let lines = render_diagram(src, 80).expect("dotted ER renders");
    let joined = lines.join("\n");
    assert!(joined.contains("has"), "{joined}");
    assert!(!joined.contains("→"), "no cardinality: {joined}");
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

/// Points sharing one cell spread onto neighboring cells instead of
/// overwriting each other: three coincident points draw three glyphs.
#[test]
fn overlapping_quadrant_points_stay_visible() {
    let src = "quadrantChart\n    x-axis a --> b\n    y-axis c --> d\n    \"P1\": [0.5, 0.5]\n    \"P2\": [0.5, 0.5]\n    \"P3\": [0.5, 0.5]";
    let lines = render_diagram(src, 80).expect("coincident points render");
    let dots = lines
        .iter()
        .take_while(|l| !l.starts_with("y:") && !l.starts_with("x:"))
        .map(|l| l.matches('●').count())
        .sum::<usize>();
    assert_eq!(dots, 3, "three visible glyphs: {lines:?}");
}

/// A legend wider than the terminal wraps instead of falling back to
/// the source view, and every wrapped row fits the budget.
#[test]
fn quadrant_legends_wrap_instead_of_falling_back() {
    let mut src = String::from("quadrantChart\n    x-axis a --> b\n    y-axis c --> d\n");
    for n in 0..6 {
        src.push_str(&format!(
            "    \"point-with-a-long-name-{n}\": [0.{n}, 0.9]\n"
        ));
    }
    let lines = render_diagram(&src, 60).expect("wrapped legend renders");
    let legend: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("point-with-a-long-name"))
        .collect();
    assert!(legend.len() >= 2, "legend split across rows: {lines:?}");
    assert!(
        lines.iter().all(|l| width::width(l) <= 60),
        "every row fits: {lines:?}"
    );
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

/// Bars sit at their offset from the earliest start: two same-length
/// tasks with staggered dates draw their bars at staggered positions.
#[test]
fn gantt_bars_offset_by_their_start_dates() {
    let src = "gantt\n    t1 :a, 2026-01-01, 2d\n    t2 :b, 2026-01-10, 2d";
    let lines = render_diagram(src, 80).expect("staggered gantt renders");
    let bar_col = |name: &str| {
        lines
            .iter()
            .find(|l| l.contains(name))
            .and_then(|l| l.find('\u{2588}'))
            .expect("a bar row")
    };
    assert!(
        bar_col("t2") > bar_col("t1"),
        "later start sits further right: {lines:?}"
    );
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
