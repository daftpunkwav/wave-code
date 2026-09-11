/*!
 * @file EventReplay
 * @description Structural replay of recorded wire events to trajectories.
 *
 * Responsibilities:
 * - Fold recorded events into trajectory steps and observations.
 * - Keep replay structural: deltas collapse, pairing stays explicit.
 * - Surface approvals, interruptions, and truncated calls in the output.
 * - Never re-execute anything; replay is read-only analysis.
 *
 * This module must not depend on: drivers, tools, models, or transport.
 */

//! Replay: recorded events back into an inspectable trajectory.
//!
//! Text deltas are intentionally skipped: replay answers "what ran, in
//! what order, with what outcome", while transcripts keep the words.

use operations_wire::{ApprovalKind, Event, EventMsg};
use state_trajectory::{ActionKind, Trajectory};

/// Replay recorded events into a fresh trajectory.
pub fn replay_to_trajectory(events: &[Event]) -> Trajectory {
    let mut trajectory = Trajectory::new();
    // Open tool calls awaiting their end event.
    let mut open: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for event in events {
        match &event.msg {
            EventMsg::TurnStarted => {
                trajectory.push_action(ActionKind::Note, "turn started");
            }
            EventMsg::AgentMessageComplete { .. } => {
                trajectory.push_action(ActionKind::Sample, "assistant message");
            }
            EventMsg::ToolCallBegin { call_id, name, .. } => {
                let seq = trajectory.push_action(
                    ActionKind::ToolCall { name: name.clone() },
                    format!("{call_id} started"),
                );
                open.insert(call_id.clone(), seq);
            }
            EventMsg::ToolCallEnd { call_id, is_error } => {
                match open.remove(call_id) {
                    Some(seq) => {
                        trajectory.push_observation(seq, format!("{call_id} ended"), *is_error)
                    }
                    None => {
                        // Unpaired end (truncated recording): keep the
                        // observation visible instead of dropping it.
                        let seq = trajectory
                            .push_action(ActionKind::Note, format!("unpaired end: {call_id}"));
                        trajectory.push_observation(seq, format!("{call_id} ended"), *is_error);
                    }
                }
            }
            EventMsg::CompactCompleted { .. } => {
                trajectory.push_action(
                    ActionKind::Compact {
                        trigger: "replayed".to_string(),
                    },
                    "context compacted",
                );
            }
            EventMsg::Warning { message } => {
                let seq = trajectory.push_action(ActionKind::Note, message.clone());
                trajectory.push_observation(seq, message.clone(), false);
            }
            EventMsg::Error { message, .. } => {
                let seq = trajectory.push_action(ActionKind::Note, message.clone());
                trajectory.push_observation(seq, message.clone(), true);
            }
            EventMsg::ApprovalRequested { call_id, kind, detail } => {
                // Approval gates are structural: dropping them hides
                // safety-relevant pauses in the trajectory.
                let kind_name = match kind {
                    ApprovalKind::Exec => "exec",
                    ApprovalKind::Write => "write",
                };
                trajectory.push_action(
                    ActionKind::Note,
                    format!("approval requested: {call_id} ({kind_name}): {detail}"),
                );
            }
            EventMsg::TurnCompleted { interrupted: true } => {
                // Clean completions carry no signal beyond the steps above;
                // interruptions are an outcome worth keeping.
                trajectory.push_action(ActionKind::Note, "turn interrupted");
            }
            // Deltas and token counts carry no structural signal beyond
            // the steps above.
            _ => {}
        }
    }
    // Truncated recordings may end with calls that never closed: mark them
    // so readers can tell "still open" apart from success.
    let mut dangling: Vec<(String, u64)> = open.into_iter().collect();
    dangling.sort_by_key(|(_, seq)| *seq);
    for (call_id, seq) in dangling {
        trajectory.push_observation(seq, format!("{call_id} truncated: no end event"), false);
    }
    trajectory
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(msg: EventMsg) -> Event {
        Event {
            id: "s1".to_string(),
            msg,
        }
    }

    #[test]
    fn replays_structure_in_order() {
        let trajectory = replay_to_trajectory(&[
            event(EventMsg::TurnStarted),
            event(EventMsg::ToolCallBegin {
                call_id: "c1".to_string(),
                name: "shell".to_string(),
                input: serde_json::Value::Null,
            }),
            event(EventMsg::ToolCallEnd {
                call_id: "c1".to_string(),
                is_error: true,
            }),
            event(EventMsg::AgentMessageComplete {
                text: "hi".to_string(),
            }),
            event(EventMsg::TurnCompleted { interrupted: false }),
        ]);
        let replay = trajectory.replay();
        assert_eq!(replay.len(), 3);
        assert!(replay[0].contains("turn started"));
        assert!(replay[1].contains("tool:shell"));
        assert!(replay[1].contains("[error]"));
        assert!(replay[2].contains("sample"));
    }

    #[test]
    fn unpaired_ends_stay_visible() {
        let trajectory = replay_to_trajectory(&[event(EventMsg::ToolCallEnd {
            call_id: "c9".to_string(),
            is_error: false,
        })]);
        let replay = trajectory.replay();
        assert_eq!(replay.len(), 1);
        assert!(replay[0].contains("unpaired end"));
    }

    #[test]
    fn approval_requests_stay_visible() {
        let trajectory = replay_to_trajectory(&[event(EventMsg::ApprovalRequested {
            call_id: "c7".to_string(),
            kind: ApprovalKind::Exec,
            detail: "rm -rf /".to_string(),
        })]);
        let replay = trajectory.replay();
        assert_eq!(replay.len(), 1);
        assert!(replay[0].contains("approval requested"));
        assert!(replay[0].contains("c7"));
    }

    #[test]
    fn interrupted_turns_stay_visible() {
        let trajectory =
            replay_to_trajectory(&[event(EventMsg::TurnCompleted { interrupted: true })]);
        let replay = trajectory.replay();
        assert_eq!(replay.len(), 1);
        assert!(replay[0].contains("interrupted"));
    }

    #[test]
    fn unclosed_calls_are_marked_truncated() {
        let trajectory = replay_to_trajectory(&[event(EventMsg::ToolCallBegin {
            call_id: "c5".to_string(),
            name: "shell".to_string(),
            input: serde_json::Value::Null,
        })]);
        let replay = trajectory.replay();
        assert_eq!(replay.len(), 1);
        assert!(!replay[0].contains("[error]"));
        let seq = trajectory.steps()[0].seq;
        let observations = trajectory.observations_for(seq);
        assert_eq!(observations.len(), 1);
        assert!(observations[0].content.contains("truncated"));
        assert!(!observations[0].is_error);
    }

    #[test]
    fn paired_calls_gain_no_truncation_mark() {
        let trajectory = replay_to_trajectory(&[
            event(EventMsg::ToolCallBegin {
                call_id: "c1".to_string(),
                name: "shell".to_string(),
                input: serde_json::Value::Null,
            }),
            event(EventMsg::ToolCallEnd {
                call_id: "c1".to_string(),
                is_error: false,
            }),
        ]);
        let seq = trajectory.steps()[0].seq;
        let observations = trajectory.observations_for(seq);
        assert_eq!(observations.len(), 1);
        assert!(!observations[0].content.contains("truncated"));
    }
}
