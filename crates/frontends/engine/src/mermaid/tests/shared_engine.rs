//! The remaining shared-engine kinds: class boxes, ER
//! relationships, C4 elements, and requirement blocks.

use super::super::*;

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
fn requirement_diagrams_render_blocks_and_edges() {
    let src = "requirementDiagram\n    requirement test_req {\n        id: 1\n        risk: high\n        verifymethod: test\n    }\n    element test_entity {\n        type: simulation\n    }\n    test_entity - satisfies -> test_req";
    let lines = render_diagram(src, 80).expect("requirement renders");
    let joined = lines.join("\n");
    for label in ["test_req", "test_entity", "risk: high", "satisfies"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
}
