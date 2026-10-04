/*!
 * @file ReplayEval
 * @description Recorded-session replay tier: wire-contract validation and
 * expectation scoring over recorded event streams — no model, no tools.
 *
 * Responsibilities:
 * - Validate a recorded session's wire events against the protocol
 *   contract (ordering, pairing, settle-once).
 * - Score recorded assistant output against must-contain expectations.
 * - Load recordings from JSONL (one `Event` per line).
 *
 * This module must not depend on: drivers, tools, models, or transport.
 */

//! Recorded-session replay evaluation (the keyless regression tier).
//!
//! A recording is the `Vec<Event>` a session emitted, serialized one
//! [`Event`] per JSONL line. Replay checks the protocol contract that the
//! run loop and frontends rely on, then scores assistant text against
//! expectations — so a recorded session that once behaved correctly fails
//! loudly when a later change breaks ordering, pairing, or content.

use wavecode_wire::{Event, EventMsg};

use std::collections::HashMap;
use std::collections::HashSet;

/// One recorded-session evaluation case.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCase {
    /// Case name for reporting.
    pub name: String,
    /// Recorded wire events of the session (in emission order).
    pub events: Vec<Event>,
    /// Substrings that must all appear in the recorded assistant messages
    /// ([`EventMsg::AgentMessageComplete`] texts, joined).
    pub must_contain: Vec<String>,
}

/// Outcome of one recorded case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayResult {
    /// Case name.
    pub name: String,
    /// True when the contract held and every expectation matched.
    pub passed: bool,
    /// Wire-contract violations, one human-readable line each.
    pub contract_violations: Vec<String>,
    /// Expectations missing from the recorded assistant text.
    pub missing: Vec<String>,
}

/// Aggregate report over recorded cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// Per-case outcomes in run order.
    pub results: Vec<ReplayResult>,
}

impl ReplayReport {
    /// Fraction of passing cases; 1.0 for an empty suite.
    pub fn pass_rate(&self) -> f64 {
        if self.results.is_empty() {
            return 1.0;
        }
        self.passed() as f64 / self.results.len() as f64
    }

    /// Number of passing cases.
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }
}

/// Validate one recording against the wire protocol contract.
///
/// Checks are keyed by submission id so concurrent interleavings between
/// submissions never false-positive. Violations are accumulated (every
/// broken rule is reported, not just the first).
pub fn validate_contract(events: &[Event]) -> Vec<String> {
    let mut violations: Vec<String> = Vec::new();
    // Per-submission contract state.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut started: HashSet<&str> = HashSet::new();
    let mut completed: HashSet<&str> = HashSet::new();
    let mut open_calls: HashMap<String, &str> = HashMap::new();

    for (index, event) in events.iter().enumerate() {
        let id = event.id.as_str();
        if !seen.insert(id) && completed.contains(id) {
            violations.push(format!("#{index} ({id}): event after TurnCompleted"));
        }
        match &event.msg {
            EventMsg::TurnStarted { .. } => {
                if !started.insert(id) {
                    violations.push(format!("#{index} ({id}): duplicate TurnStarted"));
                }
            }
            EventMsg::ToolCallBegin { call_id, .. } => {
                if !started.contains(id) {
                    violations.push(format!("#{index} ({id}): ToolCallBegin before TurnStarted"));
                }
                open_calls.insert(call_id.clone(), id);
            }
            EventMsg::ToolCallEnd { call_id, .. } => match open_calls.remove(call_id) {
                Some(_) => {}
                None => violations.push(format!(
                    "#{index} ({id}): ToolCallEnd for unopened call {call_id}"
                )),
            },
            // One AgentMessageComplete per sample is the documented order:
            // a tool-using (or continued) turn completes once per round, so
            // repeats under one submission id are legal.
            EventMsg::AgentMessageComplete { .. } => {
                if !started.contains(id) {
                    violations.push(format!(
                        "#{index} ({id}): AgentMessageComplete before TurnStarted"
                    ));
                }
            }
            // A delta after a completion opens the next sample of the same
            // submission (truncation continuation or a later tool round),
            // so no per-round ordering rule applies beyond TurnStarted.
            EventMsg::AgentMessageDelta { .. } | EventMsg::AgentThinkingDelta { .. } => {
                if !started.contains(id) {
                    violations.push(format!("#{index} ({id}): delta before TurnStarted"));
                }
            }
            // Settles run once per completed sample, so a multi-sample turn
            // legitimately carries several TokenCount events.
            EventMsg::TokenCount { .. } => {}
            EventMsg::TurnCompleted { interrupted } => {
                if !started.contains(id) {
                    violations.push(format!("#{index} ({id}): TurnCompleted before TurnStarted"));
                }
                if !completed.insert(id) {
                    violations.push(format!("#{index} ({id}): duplicate TurnCompleted"));
                }
                if !*interrupted {
                    for (call_id, owner) in &open_calls {
                        if owner == &id {
                            violations.push(format!(
                                "#{index} ({id}): call {call_id} never ended in a completed turn"
                            ));
                        }
                    }
                }
                open_calls.retain(|_, owner| owner != &id);
            }
            // Warnings/errors/approvals carry no ordering constraint beyond
            // the after-completion guard above.
            _ => {}
        }
    }
    violations
}

/// Score one recorded case: contract first, then text expectations.
pub fn evaluate_recorded(cases: &[RecordedCase]) -> ReplayReport {
    let results = cases
        .iter()
        .map(|case| {
            let contract_violations = validate_contract(&case.events);
            let assistant = case
                .events
                .iter()
                .filter_map(|e| match &e.msg {
                    EventMsg::AgentMessageComplete { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let missing: Vec<String> = case
                .must_contain
                .iter()
                .filter(|want| !assistant.contains(want.as_str()))
                .cloned()
                .collect();
            let passed = contract_violations.is_empty() && missing.is_empty();
            ReplayResult {
                name: case.name.clone(),
                passed,
                contract_violations,
                missing,
            }
        })
        .collect();
    ReplayReport { results }
}

/// True when a JSONL line is a harness control line rather than a wire
/// event: it carries a `meta` key and no event `type`. Exec recordings
/// open with one such line (`{"meta":"session",…}`, the resume handle).
fn is_meta_control_line(value: &serde_json::Value) -> bool {
    value.get("meta").is_some() && value.get("type").is_none()
}

/// Load a recording from JSONL (one serialized [`Event`] per line; blank
/// lines skipped; the leading `{"meta":"session",…}` control line
/// `wavecode exec --json` emits is recognized and skipped; malformed lines
/// fail loudly — a recording is a test fixture, not untrusted user data).
pub fn read_events_jsonl(path: &std::path::Path) -> std::io::Result<Vec<Event>> {
    let text = std::fs::read_to_string(path)?;
    let mut events = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} line {}: {e}", path.display(), index + 1),
            )
        })?;
        if is_meta_control_line(&value) {
            continue;
        }
        events.push(serde_json::from_value(value).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} line {}: {e}", path.display(), index + 1),
            )
        })?);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_wire::{ApprovalKind, ToolOutcome};

    fn event(id: &str, msg: EventMsg) -> Event {
        Event {
            id: id.to_string(),
            msg,
        }
    }

    /// A clean minimal session: contract holds, expectations score.
    #[test]
    fn clean_session_passes_and_scores() {
        let events = vec![
            event(
                "s1",
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            ),
            event("s1", EventMsg::AgentMessageDelta { text: "hel".into() }),
            event(
                "s1",
                EventMsg::AgentMessageComplete {
                    text: "hello world".into(),
                },
            ),
            event(
                "s1",
                EventMsg::TokenCount {
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    input_tokens: 3,
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    output_tokens: 2,
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    cache_read_tokens: 0,
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    cache_creation_tokens: 0,
                    context_window: None,
                    context_used: None,
                },
            ),
            event("s1", EventMsg::TurnCompleted { interrupted: false }),
        ];
        let report = evaluate_recorded(&[RecordedCase {
            name: "clean".into(),
            events: events.clone(),
            must_contain: vec!["hello".into()],
        }]);
        assert!(report.results[0].passed, "{:?}", report.results[0]);
        assert!(validate_contract(&events).is_empty());
    }

    #[test]
    fn broken_pairing_and_ordering_are_reported() {
        let events = vec![
            // ToolCallBegin before TurnStarted + never ended.
            event(
                "s1",
                EventMsg::ToolCallBegin {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: serde_json::Value::Null,
                },
            ),
            event(
                "s1",
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            ),
            // A mid-turn delta that never completed a first message.
            event(
                "s1",
                EventMsg::AgentMessageComplete {
                    text: "done".into(),
                },
            ),
            event(
                "s1",
                EventMsg::AgentMessageDelta {
                    text: "late".into(),
                },
            ),
            // Two settles.
            event(
                "s1",
                EventMsg::TokenCount {
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    input_tokens: 1,
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    context_window: None,
                    context_used: None,
                },
            ),
            event(
                "s1",
                EventMsg::TokenCount {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    context_window: None,
                    context_used: None,
                },
            ),
            event("s1", EventMsg::TurnCompleted { interrupted: false }),
            // Event after completion.
            event(
                "s1",
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            ),
        ];
        let violations = validate_contract(&events);
        let joined = violations.join("\n");
        assert!(joined.contains("before TurnStarted"), "{joined}");
        assert!(joined.contains("never ended"), "{joined}");
        assert!(joined.contains("after TurnCompleted"), "{joined}");
        // Repeated completes/deltas/settles are legal within one submission
        // (one per sample); the removed once-only rules must not fire.
        assert_eq!(violations.len(), 4, "{joined}");
    }

    /// A tool-using turn completes once per sample: deltas, completes, tool
    /// pairing, and settles all repeat under the same submission id, and the
    /// contract must accept the whole sequence (the once-only rules this
    /// suite used to carry false-positived on exactly this shape).
    #[test]
    fn multi_sample_turn_passes_contract() {
        let events = vec![
            event(
                "s1",
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            ),
            event("s1", EventMsg::AgentMessageDelta { text: "r1".into() }),
            event(
                "s1",
                EventMsg::AgentMessageComplete {
                    text: "round one".into(),
                },
            ),
            event(
                "s1",
                EventMsg::ToolCallBegin {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: serde_json::Value::Null,
                },
            ),
            event(
                "s1",
                EventMsg::ToolCallEnd {
                    call_id: "c1".into(),
                    is_error: false,
                    output: None,
                    outcome: ToolOutcome::Executed,
                    duration_ms: 0,
                },
            ),
            event("s1", EventMsg::AgentMessageDelta { text: "r2".into() }),
            event(
                "s1",
                EventMsg::AgentMessageComplete {
                    text: "round two".into(),
                },
            ),
            event(
                "s1",
                EventMsg::TokenCount {
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    input_tokens: 10,
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    output_tokens: 4,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    context_window: None,
                    context_used: None,
                },
            ),
            event(
                "s1",
                EventMsg::TokenCount {
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    input_tokens: 20,
                    // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                    output_tokens: 6,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    context_window: None,
                    context_used: None,
                },
            ),
            event("s1", EventMsg::TurnCompleted { interrupted: false }),
        ];
        assert!(
            validate_contract(&events).is_empty(),
            "a per-sample turn is contract-clean"
        );
    }

    #[test]
    fn interrupted_turns_may_leave_calls_open() {
        let events = vec![
            event(
                "s1",
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            ),
            event(
                "s1",
                EventMsg::ToolCallBegin {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: serde_json::Value::Null,
                },
            ),
            event("s1", EventMsg::TurnCompleted { interrupted: true }),
        ];
        assert!(validate_contract(&events).is_empty());
    }

    #[test]
    fn unpaired_end_is_a_violation_but_approval_flow_is_free() {
        let events = vec![
            event(
                "s1",
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            ),
            event(
                "s1",
                EventMsg::ApprovalRequested {
                    call_id: "c9".into(),
                    kind: ApprovalKind::Exec,
                    detail: "d".into(),
                },
            ),
            event(
                "s1",
                EventMsg::ToolCallEnd {
                    call_id: "c9".into(),
                    is_error: false,
                    output: None,
                    outcome: ToolOutcome::Executed,
                    duration_ms: 0,
                },
            ),
            event("s1", EventMsg::TurnCompleted { interrupted: false }),
        ];
        let violations = validate_contract(&events);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("unopened call c9"));
    }

    #[test]
    fn missing_expectations_fail_the_case() {
        let events = vec![
            event(
                "s1",
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            ),
            event(
                "s1",
                EventMsg::AgentMessageComplete {
                    text: "answer".into(),
                },
            ),
            event("s1", EventMsg::TurnCompleted { interrupted: false }),
        ];
        let report = evaluate_recorded(&[RecordedCase {
            name: "miss".into(),
            events,
            must_contain: vec!["absent-token".into()],
        }]);
        assert!(!report.results[0].passed);
        assert_eq!(report.results[0].missing, vec!["absent-token".to_string()]);
        assert!(report.results[0].contract_violations.is_empty());
        assert_eq!(report.passed(), 0);
        assert!((report.pass_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn jsonl_round_trips_through_serialization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.jsonl");
        let events = vec![
            event(
                "s1",
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            ),
            event("s1", EventMsg::AgentMessageComplete { text: "hi".into() }),
            event("s1", EventMsg::TurnCompleted { interrupted: false }),
        ];
        let text = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, text).unwrap();
        let loaded = read_events_jsonl(&path).unwrap();
        assert_eq!(loaded, events);
    }

    #[test]
    fn malformed_jsonl_lines_fail_with_location() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jsonl");
        // Line 1 is a valid event (flattened wire shape); line 2 is not
        // JSON: the error names it.
        std::fs::write(
            &path,
            "{\"id\":\"s1\",\"type\":\"turn_started\"}\nnot json\n",
        )
        .unwrap();
        let err = read_events_jsonl(&path).unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    /// `wavecode exec --json` opens its recording with the
    /// `{"meta":"session",…}` resume-handle control line (no `id`/`type`).
    /// The loader must skip it — otherwise no real exec recording could be
    /// reloaded — while malformed event lines still fail with locations.
    #[test]
    fn exec_session_meta_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exec.jsonl");
        let events = vec![
            event("s1", EventMsg::TurnStarted { model: "m".into() }),
            event("s1", EventMsg::TurnCompleted { interrupted: false }),
        ];
        let mut text = serde_json::json!({
            "meta": "session",
            "session_id": "abc",
            "version": "0.0.0",
            "resume": "wavecode --session abc",
        })
        .to_string();
        text.push('\n');
        text.push_str(
            &events
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        std::fs::write(&path, text).unwrap();
        assert_eq!(read_events_jsonl(&path).unwrap(), events);

        // A meta key on an actual event line never hides the event: the
        // `type` field wins.
        let mixed =
            "{\"id\":\"s1\",\"type\":\"turn_started\",\"model\":\"m\"}\n{\"meta\":\"session\"}\n"
                .to_string();
        let path = dir.path().join("mixed.jsonl");
        std::fs::write(&path, mixed).unwrap();
        let loaded = read_events_jsonl(&path).unwrap();
        assert_eq!(loaded.len(), 1, "only the event line loads");
        assert!(matches!(loaded[0].msg, EventMsg::TurnStarted { .. }));

        // A line that is neither a control line nor a valid event still
        // fails loudly with its location (meta lines only excuse
        // themselves, never later damage).
        let path = dir.path().join("torn.jsonl");
        std::fs::write(&path, "{\"meta\":\"session\"}\n{\"id\":\"s1\"}\n").unwrap();
        let err = read_events_jsonl(&path).unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
    }
}
