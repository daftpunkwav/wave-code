//! The repo's one checked-in recording: a two-sample tool-using turn.
//!
//! This is the exact shape a real `wavecode exec --json` run over a
//! tool-using turn produces — one submission carrying two completes,
//! two deltas bursts, a paired tool call, and two settles. The contract
//! validator used to carry once-only rules that false-positived on
//! precisely this shape; the fixture keeps that regression pinned and
//! gives contract validation a standing input.

use operations_eval::{RecordedCase, evaluate_recorded, read_events_jsonl, validate_contract};

#[test]
fn checked_in_recording_passes_the_contract_and_scores() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("two_sample_turn.jsonl");
    let events = read_events_jsonl(&path).expect("the fixture parses");
    assert_eq!(events.len(), 10, "fixture shape changed in place");

    let violations = validate_contract(&events);
    assert!(
        violations.is_empty(),
        "a real tool-using turn must be contract-clean: {violations:?}"
    );

    let report = evaluate_recorded(&[RecordedCase {
        name: "two-sample-turn".to_string(),
        events: events.clone(),
        must_contain: vec!["all done".to_string()],
    }]);
    assert!(report.results[0].passed, "{:?}", report.results[0]);
}
