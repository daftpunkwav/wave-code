/*!
 * @file SnapshotReplayGoldens
 * @description Snapshot tests pinning user-visible replay behavior.
 *
 * Responsibilities:
 * - Replay committed transcript fixtures into trajectories.
 * - Assert exact step and observation sequences per fixture.
 * - Assert exact timeline text so display regressions fail loudly.
 *
 * This module must not depend on: drivers, tools, models, or transport.
 * Fixtures are offline JSON event arrays under benchmarks/fixtures.
 */

use operations_replay::replay_to_trajectory;
use state_trajectory::ActionKind;
use wavecode_wire::Event;

/// Load one committed fixture as wire events.
fn load_fixture(name: &str) -> Vec<Event> {
    let path = format!(
        "{}/../../../benchmarks/fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// Short kind name matching `Trajectory::replay` rendering.
fn kind_name(step: &state_trajectory::Step) -> String {
    match &step.action {
        ActionKind::Sample => "sample".to_string(),
        ActionKind::ToolCall { name } => format!("tool:{name}"),
        ActionKind::Compact { trigger } => format!("compact:{trigger}"),
        ActionKind::Note => "note".to_string(),
    }
}

#[test]
fn basic_flow_replays_exact_shape() {
    let events = load_fixture("basic");
    assert_eq!(events.len(), 5);
    let trajectory = replay_to_trajectory(&events);
    let steps = trajectory.steps();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0].seq, 1);
    assert_eq!(steps[0].detail, "turn started");
    assert_eq!(kind_name(&steps[0]), "note");
    assert_eq!(kind_name(&steps[1]), "tool:shell");
    assert_eq!(steps[1].detail, "c1 started");
    assert_eq!(kind_name(&steps[2]), "sample");
    assert_eq!(steps[2].detail, "assistant message");
    // Paired call gains exactly one success observation, nothing truncated.
    let obs = trajectory.observations_for(steps[1].seq);
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].content, "c1 ended");
    assert!(!obs[0].is_error);
    assert!(trajectory.observations_for(steps[0].seq).is_empty());
    assert!(trajectory.observations_for(steps[2].seq).is_empty());
    // Timeline text is user-visible: pin it exactly.
    assert_eq!(
        trajectory.replay(),
        vec![
            "#1 note: turn started".to_string(),
            "#2 tool:shell: c1 started".to_string(),
            "#3 sample: assistant message".to_string(),
        ]
    );
}

#[test]
fn approval_gates_stay_visible_in_order() {
    let events = load_fixture("approval");
    let trajectory = replay_to_trajectory(&events);
    let steps = trajectory.steps();
    assert_eq!(steps.len(), 4);
    assert_eq!(steps[0].detail, "turn started");
    assert_eq!(
        steps[1].detail,
        "approval requested: c7 (exec): rm -rf /tmp/demo"
    );
    assert_eq!(kind_name(&steps[1]), "note");
    assert_eq!(kind_name(&steps[2]), "tool:shell");
    assert_eq!(steps[2].detail, "c7 started");
    assert_eq!(kind_name(&steps[3]), "sample");
    // Approval step carries no observation; the paired call ends cleanly.
    assert!(trajectory.observations_for(steps[1].seq).is_empty());
    let obs = trajectory.observations_for(steps[2].seq);
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].content, "c7 ended");
    assert!(!obs[0].is_error);
    let timeline = trajectory.replay();
    assert_eq!(timeline.len(), 4);
    assert!(timeline[1].contains("approval requested: c7 (exec)"));
    assert!(timeline[2].contains("tool:shell"));
    assert!(!timeline.iter().any(|line| line.contains("[error]")));
}

#[test]
fn interrupts_mark_the_turn_and_truncate_open_calls() {
    let events = load_fixture("interrupt");
    let trajectory = replay_to_trajectory(&events);
    let steps = trajectory.steps();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0].detail, "turn started");
    assert_eq!(kind_name(&steps[1]), "tool:shell");
    assert_eq!(steps[1].detail, "c3 started");
    assert_eq!(steps[2].detail, "turn interrupted");
    // The open call never closed: it gains a non-error truncation mark.
    let obs = trajectory.observations_for(steps[1].seq);
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].content, "c3 truncated: no end event");
    assert!(!obs[0].is_error);
    assert_eq!(
        trajectory.replay(),
        vec![
            "#1 note: turn started".to_string(),
            "#2 tool:shell: c3 started".to_string(),
            "#3 note: turn interrupted".to_string(),
        ]
    );
}

#[test]
fn truncation_fixtures_keep_every_signal() {
    let events = load_fixture("truncation");
    let trajectory = replay_to_trajectory(&events);
    let steps = trajectory.steps();
    assert_eq!(steps.len(), 4);
    assert_eq!(kind_name(&steps[0]), "tool:shell");
    assert_eq!(steps[0].detail, "c5 started");
    assert_eq!(steps[1].detail, "unpaired end: c9");
    assert_eq!(kind_name(&steps[2]), "compact:replayed");
    assert_eq!(steps[2].detail, "context compacted");
    assert_eq!(steps[3].detail, "disk full");
    // Unclosed begin is truncated (non-error); unpaired end stays visible;
    // the error observation is the only error mark in the timeline.
    let open_obs = trajectory.observations_for(steps[0].seq);
    assert_eq!(open_obs.len(), 1);
    assert_eq!(open_obs[0].content, "c5 truncated: no end event");
    assert!(!open_obs[0].is_error);
    let unpaired_obs = trajectory.observations_for(steps[1].seq);
    assert_eq!(unpaired_obs.len(), 1);
    assert_eq!(unpaired_obs[0].content, "c9 ended");
    assert!(!unpaired_obs[0].is_error);
    let err_obs = trajectory.observations_for(steps[3].seq);
    assert_eq!(err_obs.len(), 1);
    assert_eq!(err_obs[0].content, "disk full");
    assert!(err_obs[0].is_error);
    assert_eq!(
        trajectory.replay(),
        vec![
            "#1 tool:shell: c5 started".to_string(),
            "#2 note: unpaired end: c9".to_string(),
            "#3 compact:replayed: context compacted".to_string(),
            "#4 note: disk full [error]".to_string(),
        ]
    );
}

/// Replay goldens are pure CPU over tiny inputs: assert a generous wall
/// bound, report JSON, and compare against the committed baseline. Timing
/// beyond 10x median fails; anything within only prints PASS/WARN.
#[test]
fn replay_goldens_stay_fast_against_baseline() {
    let baseline_text = std::fs::read_to_string(format!(
        "{}/../../../benchmarks/baseline.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read baseline.json");
    let baseline: serde_json::Value =
        serde_json::from_str(&baseline_text).expect("baseline.json parses");
    let median_ms = baseline
        .pointer("/benches/replay_golden/median_ms")
        .and_then(|v| v.as_f64())
        .expect("replay_golden median_ms present");
    let warn_beyond = baseline
        .get("warn_beyond")
        .and_then(|v| v.as_f64())
        .expect("warn_beyond present");
    let fail_beyond = baseline
        .get("fail_beyond")
        .and_then(|v| v.as_f64())
        .expect("fail_beyond present");
    let start = std::time::Instant::now();
    let mut total_steps = 0;
    for name in ["basic", "approval", "interrupt", "truncation"] {
        let trajectory = replay_to_trajectory(&load_fixture(name));
        // Rendering is part of the pinned surface; include it in the time.
        total_steps += trajectory.replay().len();
    }
    let elapsed_ms = start.elapsed().as_millis();
    assert_eq!(total_steps, 3 + 4 + 3 + 4);
    let verdict = if elapsed_ms as f64 <= median_ms * warn_beyond {
        "PASS"
    } else if elapsed_ms as f64 <= median_ms * fail_beyond {
        "WARN"
    } else {
        "FAIL"
    };
    println!(
        "{{\"bench\":\"replay_golden\",\"fixtures\":4,\"steps\":{total_steps},\"elapsed_ms\":{elapsed_ms},\"median_ms\":{median_ms},\"verdict\":\"{verdict}\"}}"
    );
    assert!(elapsed_ms < 5_000, "replay goldens stalled: {elapsed_ms}ms");
    assert_ne!(verdict, "FAIL", "replay slower than 10x baseline median");
}
