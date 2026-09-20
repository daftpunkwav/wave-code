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
    let mut message_completed: HashSet<&str> = HashSet::new();
    let mut token_counts: HashMap<&str, u32> = HashMap::new();

    for (index, event) in events.iter().enumerate() {
        let id = event.id.as_str();
        if !seen.insert(id) && completed.contains(id) {
            violations.push(format!("#{index} ({id}): event after TurnCompleted"));
        }
        match &event.msg {
            EventMsg::TurnStarted => {
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
            EventMsg::AgentMessageComplete { .. } => {
                if !started.contains(id) {
                    violations.push(format!(
                        "#{index} ({id}): AgentMessageComplete before TurnStarted"
                    ));
                }
                if !message_completed.insert(id) {
                    violations.push(format!("#{index} ({id}): duplicate AgentMessageComplete"));
                }
            }
            EventMsg::AgentMessageDelta { .. } | EventMsg::AgentThinkingDelta { .. } => {
                if message_completed.contains(id) {
                    violations.push(format!(
                        "#{index} ({id}): delta after AgentMessageComplete (ordering contract)"
                    ));
                }
                if !started.contains(id) {
                    violations.push(format!("#{index} ({id}): delta before TurnStarted"));
                }
            }
            EventMsg::TokenCount { .. } => {
                let count = token_counts.entry(id).or_insert(0);
                *count += 1;
                if *count > 1 {
                    violations.push(format!(
                        "#{index} ({id}): more than one TokenCount (settle-once contract)"
                    ));
                }
            }
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

/// Load a recording from JSONL (one serialized [`Event`] per line; blank
/// lines skipped; malformed lines fail loudly — a recording is a test
/// fixture, not untrusted user data).
pub fn read_events_jsonl(path: &std::path::Path) -> std::io::Result<Vec<Event>> {
    let text = std::fs::read_to_string(path)?;
    let mut events = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        events.push(serde_json::from_str(line).map_err(|e| {
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
            event("s1", EventMsg::TurnStarted),
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
                    input_tokens: 3,
                    output_tokens: 2,
                    cache_read_tokens: 0,
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
            event("s1", EventMsg::TurnStarted),
            // Delta after the message completed.
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
                    input_tokens: 1,
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
            event("s1", EventMsg::TurnStarted),
        ];
        let violations = validate_contract(&events);
        let joined = violations.join("\n");
        assert!(joined.contains("before TurnStarted"), "{joined}");
        assert!(joined.contains("never ended"), "{joined}");
        assert!(
            joined.contains("delta after AgentMessageComplete"),
            "{joined}"
        );
        assert!(joined.contains("settle-once"), "{joined}");
        assert!(joined.contains("after TurnCompleted"), "{joined}");
    }

    #[test]
    fn interrupted_turns_may_leave_calls_open() {
        let events = vec![
            event("s1", EventMsg::TurnStarted),
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
            event("s1", EventMsg::TurnStarted),
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
            event("s1", EventMsg::TurnStarted),
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
            event("s1", EventMsg::TurnStarted),
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
}
