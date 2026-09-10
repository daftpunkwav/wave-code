/*!
 * @file EventReplay
 * @description Structural replay of recorded wire events to trajectories.
 *
 * Responsibilities:
 * - Fold recorded events into trajectory steps and observations.
 * - Keep replay structural: deltas collapse, pairing stays explicit.
 * - Never re-execute anything; replay is read-only analysis.
 *
 * This module must not depend on: drivers, tools, models, or transport.
 */

//! Replay: recorded events back into an inspectable trajectory.
//!
//! Text deltas are intentionally skipped: replay answers "what ran, in
//! what order, with what outcome", while transcripts keep the words.

use operations_wire::{Event, EventMsg};
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
            // Deltas, token counts, approvals, and completions carry no
            // structural signal beyond the steps above.
            _ => {}
        }
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
}
