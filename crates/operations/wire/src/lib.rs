/*!
 * @file WireProtocol
 * @description Frontend/backend wire types for submissions and events.
 *
 * Responsibilities:
 * - Define the submission operations a frontend may send.
 * - Define the event stream the harness emits back.
 * - Lock the wire tag format with tests so renames are explicit.
 *
 * This module must not depend on: runtime, state, action, safety,
 * transport, or any orchestration layer. It is pure data.
 */

//! Wire protocol: the single source of truth for frontend communication.
//!
//! Variants are intentionally exhaustive in this harness generation: adding
//! or renaming a variant is a breaking change and must update the locked
//! wire-tag test below.

/// One inbound operation from a frontend, correlated by `id`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Submission {
    /// Correlation id, echoed by every event of this submission.
    pub id: String,
    /// The requested operation.
    #[serde(flatten)]
    pub op: Op,
}

/// Operations a frontend may submit.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Op {
    /// Start or continue a run with user text.
    UserInput {
        /// Raw user input text.
        text: String,
    },
    /// Interrupt the running turn at the next safe point.
    Interrupt,
    /// Deliver a user decision for a parked approval request.
    ExecApproval {
        /// Tool call id parked in the approval gate.
        call_id: String,
        /// The user decision.
        decision: WireDecision,
    },
    /// Request context compaction.
    Compact,
    /// Shut the session down after draining in-flight work.
    Shutdown,
}

/// User decision carried over the wire for an approval request.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireDecision {
    /// Approve this call only.
    AllowOnce,
    /// Approve this call and remember it for the session.
    AllowAlways,
    /// Refuse with a reason.
    Deny {
        /// Human-readable refusal reason.
        reason: String,
    },
}

/// One outbound event, correlated with a submission by `id`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    /// Correlation id of the originating submission.
    pub id: String,
    /// The event payload.
    #[serde(flatten)]
    pub msg: EventMsg,
}

/// Events emitted by the harness during and after a run.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    /// A run started.
    TurnStarted,
    /// Incremental assistant text.
    AgentMessageDelta {
        /// New text since the previous delta.
        text: String,
    },
    /// The assistant message completed.
    AgentMessageComplete,
    /// A tool call started.
    ToolCallBegin {
        /// Tool call id pairing begin with end.
        call_id: String,
        /// Tool name being invoked.
        name: String,
    },
    /// A tool call finished.
    ToolCallEnd {
        /// Tool call id pairing end with begin.
        call_id: String,
        /// True when the tool reported a business failure.
        is_error: bool,
    },
    /// An approval request is parked and needs a user decision.
    ApprovalRequested {
        /// Tool call id parked in the approval gate.
        call_id: String,
        /// Bounded detail string for display.
        detail: String,
    },
    /// Token usage settled after a sample.
    TokenCount {
        /// Input tokens of the sample.
        input_tokens: u64,
        /// Output tokens of the sample.
        output_tokens: u64,
    },
    /// Compaction started.
    CompactStarted,
    /// Compaction completed.
    CompactCompleted,
    /// Non-fatal condition worth surfacing.
    Warning {
        /// Human-readable warning text.
        message: String,
    },
    /// Fatal condition for this run.
    Error {
        /// Human-readable error text.
        message: String,
        /// True when the session survives and accepts new submissions.
        recoverable: bool,
    },
    /// The run terminated; always the last event of a submission.
    TurnCompleted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_tags_are_locked() {
        // Renaming any tag below is a breaking protocol change: update the
        // frontends in the same commit.
        let cases = [
            (
                Op::UserInput {
                    text: "hi".to_string(),
                },
                "user_input",
            ),
            (Op::Interrupt, "interrupt"),
            (
                Op::ExecApproval {
                    call_id: "c".to_string(),
                    decision: WireDecision::AllowOnce,
                },
                "exec_approval",
            ),
            (Op::Compact, "compact"),
            (Op::Shutdown, "shutdown"),
        ];
        for (op, tag) in cases {
            let value = serde_json::to_value(&op).unwrap();
            assert_eq!(value.get("type").unwrap(), &tag);
        }

        let events = [
            (EventMsg::TurnStarted, "turn_started"),
            (
                EventMsg::AgentMessageDelta {
                    text: "x".to_string(),
                },
                "agent_message_delta",
            ),
            (EventMsg::AgentMessageComplete, "agent_message_complete"),
            (
                EventMsg::ToolCallBegin {
                    call_id: "c".to_string(),
                    name: "shell".to_string(),
                },
                "tool_call_begin",
            ),
            (
                EventMsg::ToolCallEnd {
                    call_id: "c".to_string(),
                    is_error: false,
                },
                "tool_call_end",
            ),
            (
                EventMsg::ApprovalRequested {
                    call_id: "c".to_string(),
                    detail: "d".to_string(),
                },
                "approval_requested",
            ),
            (
                EventMsg::TokenCount {
                    input_tokens: 1,
                    output_tokens: 2,
                },
                "token_count",
            ),
            (EventMsg::CompactStarted, "compact_started"),
            (EventMsg::CompactCompleted, "compact_completed"),
            (
                EventMsg::Warning {
                    message: "w".to_string(),
                },
                "warning",
            ),
            (
                EventMsg::Error {
                    message: "e".to_string(),
                    recoverable: true,
                },
                "error",
            ),
            (EventMsg::TurnCompleted, "turn_completed"),
        ];
        for (msg, tag) in events {
            let value = serde_json::to_value(&msg).unwrap();
            assert_eq!(value.get("type").unwrap(), &tag);
        }
    }

    #[test]
    fn submissions_round_trip_through_json() {
        let sub = Submission {
            id: "sub-1".to_string(),
            op: Op::UserInput {
                text: "hello".to_string(),
            },
        };
        let json = serde_json::to_string(&sub).unwrap();
        let back: Submission = serde_json::from_str(&json).unwrap();
        assert_eq!(sub, back);
    }
}
