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
    /// Deliver the user's answer to a parked interactive question.
    QuestionAnswer {
        /// Tool call id parked in the question gate.
        call_id: String,
        /// The chosen option text or free-form answer; empty means the
        /// user dismissed the question.
        answer: String,
    },
    /// Request context compaction.
    Compact,
    /// Switch the session permission mode by wire name.
    SetPermissionMode {
        /// Mode wire name, e.g. `plan` or `acceptEdits`.
        mode: String,
    },
    /// Switch the sampling model by wire name (same provider only;
    /// cross-provider switches need session re-assembly and are rejected
    /// downstream with a warning).
    SetModel {
        /// Wire model name, e.g. `claude-sonnet-4-5`.
        name: String,
    },
    /// Shut the session down after draining in-flight work.
    Shutdown,
}

/// User decision carried over the wire for an approval request.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireDecision {
    /// Approve this call only.
    AllowOnce,
    /// Approve this call and remember it for the session. Reserved: no
    /// frontend emits it yet and no persistence is wired, so a decision
    /// carrying it currently behaves like AllowOnce.
    AllowAlways,
    /// Refuse with a reason.
    Deny {
        /// Human-readable refusal reason.
        reason: String,
    },
}

/// Approval kind carried with an approval request for display routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Arbitrary command execution.
    Exec,
    /// File modification.
    Write,
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
    /// Incremental extended-thinking text (display-only; thinking never
    /// enters conversation history, so there is no complete counterpart).
    AgentThinkingDelta {
        /// New thinking text since the previous delta.
        text: String,
    },
    /// The assistant message completed, carrying its full text.
    AgentMessageComplete {
        /// Full assembled assistant text for transcript rendering.
        text: String,
    },
    /// A tool call started.
    ToolCallBegin {
        /// Tool call id pairing begin with end.
        call_id: String,
        /// Tool name being invoked.
        name: String,
        /// Validated input for transcript inspection.
        input: serde_json::Value,
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
        /// Display routing kind.
        kind: ApprovalKind,
        /// Bounded detail string for display.
        detail: String,
    },
    /// An interactive question is parked and needs a user answer.
    QuestionRequested {
        /// Tool call id parked in the question gate.
        call_id: String,
        /// The question text for display.
        question: String,
        /// Numbered answer options; empty when free text is expected.
        options: Vec<String>,
    },
    /// Token usage settled after a sample.
    TokenCount {
        /// Input tokens of the sample.
        input_tokens: u64,
        /// Output tokens of the sample.
        output_tokens: u64,
        /// Tokens served from the provider prompt cache; 0 when the
        /// provider reports no cache accounting (default for
        /// backward-compatible deserialization of older emitters).
        #[serde(default)]
        cache_read_tokens: u64,
        /// Tokens written to the provider prompt cache by this sample;
        /// 0 when the provider reports no cache accounting.
        #[serde(default)]
        cache_creation_tokens: u64,
    },
    /// Compaction started.
    CompactStarted {
        /// Trigger name for display.
        trigger: String,
    },
    /// Compaction completed.
    CompactCompleted {
        /// Token estimate of the summary message.
        summary_tokens: u64,
    },
    /// A reviewed plan was proposed and awaits approval (display routing).
    PlanProposed {
        /// Proposed plan text for transcript rendering.
        text: String,
    },
    /// A reviewed plan was approved.
    PlanApproved,
    /// A durable goal was set (display routing).
    GoalSet {
        /// Goal objective text for transcript rendering.
        objective: String,
    },
    /// A durable goal was completed.
    GoalCompleted,
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
    TurnCompleted {
        /// True when the run stopped at an interrupt checkpoint.
        interrupted: bool,
    },
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
            (
                Op::QuestionAnswer {
                    call_id: "c".to_string(),
                    answer: "a".to_string(),
                },
                "question_answer",
            ),
            (Op::Compact, "compact"),
            (
                Op::SetPermissionMode {
                    mode: "plan".to_string(),
                },
                "set_permission_mode",
            ),
            (
                Op::SetModel {
                    name: "m".to_string(),
                },
                "set_model",
            ),
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
            (
                EventMsg::AgentThinkingDelta {
                    text: "x".to_string(),
                },
                "agent_thinking_delta",
            ),
            (
                EventMsg::AgentMessageComplete {
                    text: "x".to_string(),
                },
                "agent_message_complete",
            ),
            (
                EventMsg::ToolCallBegin {
                    call_id: "c".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::Value::Null,
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
                    kind: ApprovalKind::Exec,
                    detail: "d".to_string(),
                },
                "approval_requested",
            ),
            (
                EventMsg::QuestionRequested {
                    call_id: "c".to_string(),
                    question: "q".to_string(),
                    options: vec!["a".to_string()],
                },
                "question_requested",
            ),
            (
                EventMsg::TokenCount {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                "token_count",
            ),
            (
                EventMsg::CompactStarted {
                    trigger: "auto".to_string(),
                },
                "compact_started",
            ),
            (
                EventMsg::CompactCompleted { summary_tokens: 10 },
                "compact_completed",
            ),
            (
                EventMsg::PlanProposed {
                    text: "x".to_string(),
                },
                "plan_proposed",
            ),
            (EventMsg::PlanApproved, "plan_approved"),
            (
                EventMsg::GoalSet {
                    objective: "x".to_string(),
                },
                "goal_set",
            ),
            (EventMsg::GoalCompleted, "goal_completed"),
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
            (
                EventMsg::TurnCompleted { interrupted: false },
                "turn_completed",
            ),
        ];
        for (msg, tag) in events {
            let value = serde_json::to_value(&msg).unwrap();
            assert_eq!(value.get("type").unwrap(), &tag);
        }

        // Inner enum values lock too: frontends match on these strings.
        let decisions = [
            (WireDecision::AllowOnce, "allow_once"),
            (WireDecision::AllowAlways, "allow_always"),
        ];
        for (decision, tag) in decisions {
            let value = serde_json::to_value(&decision).unwrap();
            assert_eq!(value.get("type").unwrap(), &tag);
        }
        let denied = serde_json::to_value(&WireDecision::Deny {
            reason: "no".to_string(),
        })
        .unwrap();
        assert_eq!(denied.get("type").unwrap(), &"deny");
        assert_eq!(denied.get("reason").unwrap(), &"no");
        // Approval display kinds serialize as bare strings.
        assert_eq!(
            serde_json::to_value(ApprovalKind::Exec).unwrap(),
            serde_json::json!("exec")
        );
        assert_eq!(
            serde_json::to_value(ApprovalKind::Write).unwrap(),
            serde_json::json!("write")
        );
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
