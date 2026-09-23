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

/// One inline image attached to user input. Shapes mirror the llm
/// crate's `ContentBlock::Image`; validation (mime allowlist, size cap)
/// happens at request translation so every transport can carry it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserImage {
    /// Optional client-side label (preserved, never sent to providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// MIME type (must be in the provider-allowed image mimes).
    pub mime: String,
    /// Base64-encoded image bytes.
    pub base64: String,
}

/// Operations a frontend may submit.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Op {
    /// Start or continue a run with user text.
    UserInput {
        /// Raw user input text.
        text: String,
        /// Optional inline images attached to the input. Absent on older
        /// senders; providers without vision support reject them at
        /// request translation with a visible constraint error.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<UserImage>,
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
    Compact {
        /// Optional user steering for the summary (`/compact <focus>`).
        #[serde(default)]
        instruction: Option<String>,
    },
    /// Rewind the conversation by whole user turns (idle `/undo` path).
    ///
    /// Drops the last `turns` user messages and everything after each,
    /// as far as the history allows. Compaction summaries and any
    /// entries before the first user message are never dropped. Only
    /// valid while idle; mid-turn submissions are rejected downstream.
    Rewind {
        /// How many user turns to drop.
        turns: u32,
    },
    /// Switch the session permission mode by wire name.
    SetPermissionMode {
        /// Mode wire name, e.g. `plan` or `wave` (the canonical names of
        /// `wavecode_protocol::PermissionMode`; legacy aliases such as
        /// `acceptEdits` also parse, with a warning downstream).
        mode: String,
    },
    /// Switch the sampling model by wire name (same provider only;
    /// cross-provider switches need session re-assembly and are rejected
    /// downstream with a warning).
    SetModel {
        /// Wire model name, e.g. `claude-sonnet-4-5`.
        name: String,
    },
    /// Switch the reasoning-effort level for subsequent samples.
    ///
    /// The value is provider-specific: `off` clears the parameter where
    /// the client supports that, other non-empty strings pass through
    /// best-effort. Gateways without a mutable effort (fixed models,
    /// budget-driven Anthropic thinking) reject downstream with a
    /// warning.
    SetThinking {
        /// Effort level, e.g. `low` or `off`.
        effort: String,
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
    /// Approve this call and remember it for the session (the console
    /// approval dialog wires it to session rules).
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

/// Which exit of the dispatch pipeline produced a tool result, carried on
/// [`EventMsg::ToolCallEnd`]. `is_error` says the call did not succeed; this
/// says *who* stopped it — the tool, the model's caller, the policy, a hook,
/// or the user. Metrics read the distinction to rank tool quality separately
/// from approval friction; nothing about execution behavior depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// The tool body ran (see `is_error` for what it reported).
    #[default]
    Executed,
    /// Denied by a policy rule before execution.
    Denied,
    /// Refused at the approval prompt.
    Refused,
    /// A `PreToolUse` hook blocked the call.
    HookBlocked,
    /// Outside this run's declared tool surface (fork `allowed-tools`).
    SurfaceBlocked,
    /// Interrupted before the body ran.
    Interrupted,
    /// The model reused a call id; only the first occurrence runs.
    Duplicate,
    /// An interactive question's answer stood in for running the tool.
    Answered,
    /// No display could carry the question, so it was never asked.
    AskUnavailable,
    /// No result slot was filled: an internal defect, never a tool failure.
    Missing,
}

/// Bounded head of a tool result, emitted with [`EventMsg::ToolCallEnd`]
/// for transcript rendering. The harness caps the text and never splits a
/// UTF-8 character; `truncated` marks a cut so frontends can hint at more.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCallPreview {
    /// Leading portion of the tool result content.
    pub text: String,
    /// True when the content exceeded the preview budget.
    pub truncated: bool,
}

/// Opening tag of a harness-injected system reminder.
///
/// The canonical marker for guidance the harness injects into model-facing
/// history (plan nudges, repeat-breaker escalations, date-change notices):
/// wrapping text in it tells the model the text is harness guidance rather
/// than user-authored input. It lives here because both the loop
/// (`runtime-runner`, restricted to wire/store/infrastructure deps) and the
/// context layer need one definition; frontends may strip or restyle blocks
/// carrying it.
pub const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";

/// Closing tag of a harness-injected system reminder (see
/// [`SYSTEM_REMINDER_OPEN`]).
pub const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// Wrap `text` in the canonical `<system-reminder>` block.
pub fn wrap_system_reminder(text: &str) -> String {
    format!("{SYSTEM_REMINDER_OPEN}\n{text}\n{SYSTEM_REMINDER_CLOSE}")
}

impl ToolCallPreview {
    /// Character-boundary-safe head of `content` capped at `max_bytes`.
    pub fn head(content: &str, max_bytes: usize) -> Self {
        if content.len() <= max_bytes {
            return Self {
                text: content.to_string(),
                truncated: false,
            };
        }
        let mut cut = max_bytes;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        Self {
            text: content[..cut].to_string(),
            truncated: true,
        }
    }
}

/// One outbound event, correlated with a submission by `id`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    /// Correlation id of the originating submission. Events raised outside
    /// any submission (session lifecycle: startup hints, teardown notices)
    /// carry a harness-reserved synthetic id instead of a caller id, so
    /// consumers must not treat "no matching submission" as corruption.
    pub id: String,
    /// The event payload.
    #[serde(flatten)]
    pub msg: EventMsg,
}

/// Events emitted by the harness during and after a run.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    /// A run started, naming the model the loop samples.
    TurnStarted {
        /// Wire model name for this turn (`""` when the driver has none).
        /// Recordings and the metrics ledger attribute per-turn results with
        /// it, since `/model` can switch models mid-session.
        #[serde(default)]
        model: String,
    },
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
        /// Bounded head of the tool result for transcript rendering; the
        /// full content stays in conversation history and is never sent.
        /// Absent from senders that predate this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<ToolCallPreview>,
        /// Which of the dispatch pipeline's exits produced this result.
        /// Distinguishes "the model's call was wrong" from "the harness
        /// never ran it", which `is_error` alone conflates. Senders that
        /// predate this field decode as [`ToolOutcome::default`].
        #[serde(default)]
        outcome: ToolOutcome,
        /// Wall-clock milliseconds of the tool body. `0` when the body
        /// never ran, so a duration is never read as a refusal.
        #[serde(default)]
        duration_ms: u64,
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
        /// Session context window in tokens; absent from senders that
        /// predate this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        /// Tokens estimated inside the context window at settle time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_used: Option<u64>,
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
    /// A rewind landed: the conversation no longer contains the
    /// dropped turns.
    HistoryRewound {
        /// User turns actually removed (fewer than requested when the
        /// history ran out; never 0 — an empty rewind is reported as
        /// a warning instead of this event).
        turns: u32,
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
        /// Machine-readable class in `domain.reason` form (`hook.blocked`,
        /// `context.overflow`, `provider.error`, `queue.full`, …) so
        /// programmatic consumers can switch on it instead of sniffing
        /// the message. Absent on older events; optional on the wire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
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

    /// The reminder marker is protocol vocabulary: the loop writes it, the
    /// context channel writes it, and frontends recognize it, so its literal
    /// shape is locked here.
    #[test]
    fn reminder_marker_shape_is_locked() {
        assert_eq!(SYSTEM_REMINDER_OPEN, "<system-reminder>");
        assert_eq!(SYSTEM_REMINDER_CLOSE, "</system-reminder>");
        assert_eq!(
            wrap_system_reminder("notice"),
            "<system-reminder>\nnotice\n</system-reminder>"
        );
    }

    #[test]
    fn wire_tags_are_locked() {
        // Renaming any tag below is a breaking protocol change: update the
        // frontends in the same commit.
        let cases = [
            (
                Op::UserInput {
                    text: "hi".to_string(),
                    images: Vec::new(),
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
            (Op::Compact { instruction: None }, "compact"),
            (Op::Rewind { turns: 1 }, "rewind"),
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
            (
                Op::SetThinking {
                    effort: "low".to_string(),
                },
                "set_thinking",
            ),
            (Op::Shutdown, "shutdown"),
        ];
        for (op, tag) in cases {
            let value = serde_json::to_value(&op).unwrap();
            assert_eq!(value.get("type").unwrap(), &tag);
        }

        let events = [
            (
                EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
                "turn_started",
            ),
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
                    output: None,
                    outcome: ToolOutcome::Executed,
                    duration_ms: 0,
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
                    context_window: Some(200_000),
                    context_used: Some(3),
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
            (EventMsg::HistoryRewound { turns: 1 }, "history_rewound"),
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
                    code: None,
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

    /// The error `code` field is optional on the wire: events recorded
    /// before it existed deserialize unchanged, and `None` codes are
    /// omitted from the JSON so consumers never see explicit nulls.
    #[test]
    fn error_code_is_backward_compatible() {
        // Old payload without `code` still parses.
        let legacy: EventMsg = serde_json::from_value(serde_json::json!({
            "type": "error",
            "message": "boom",
            "recoverable": true,
        }))
        .unwrap();
        assert_eq!(
            legacy,
            EventMsg::Error {
                message: "boom".to_string(),
                recoverable: true,
                code: None,
            }
        );
        // `None` codes stay off the wire; `Some` rides along.
        let none = serde_json::to_value(&EventMsg::Error {
            message: "boom".to_string(),
            recoverable: true,
            code: None,
        })
        .unwrap();
        assert!(none.get("code").is_none(), "{none}");
        let classified = serde_json::to_value(&EventMsg::Error {
            message: "slow".to_string(),
            recoverable: false,
            code: Some("provider.timeout".to_string()),
        })
        .unwrap();
        assert_eq!(classified.get("code").unwrap(), "provider.timeout");
    }

    #[test]
    fn submissions_round_trip_through_json() {
        let sub = Submission {
            id: "sub-1".to_string(),
            op: Op::UserInput {
                text: "hello".to_string(),
                images: Vec::new(),
            },
        };
        let json = serde_json::to_string(&sub).unwrap();
        let back: Submission = serde_json::from_str(&json).unwrap();
        assert_eq!(sub, back);
    }

    #[test]
    fn extended_fields_are_backward_compatible() {
        // Legacy senders omit the optional compaction instruction.
        let legacy = serde_json::json!({ "type": "compact" });
        let op: Op = serde_json::from_value(legacy).unwrap();
        assert_eq!(op, Op::Compact { instruction: None });

        // Older senders omit the new fields entirely; parsing must accept
        // that and default to None.
        let legacy = serde_json::json!({
            "id": "sub-1",
            "type": "tool_call_end",
            "call_id": "c",
            "is_error": false,
        });
        let event: Event = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            event.msg,
            EventMsg::ToolCallEnd {
                call_id: "c".to_string(),
                is_error: false,
                output: None,
                outcome: ToolOutcome::Executed,
                duration_ms: 0,
            }
        );

        let legacy = serde_json::json!({
            "id": "sub-1",
            "type": "token_count",
            "input_tokens": 1,
            "output_tokens": 2,
        });
        let event: Event = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            event.msg,
            EventMsg::TokenCount {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                context_window: None,
                context_used: None,
            }
        );

        // New fields are omitted on the wire when unset and round-trip
        // when set.
        let plain = serde_json::to_value(EventMsg::ToolCallEnd {
            call_id: "c".to_string(),
            is_error: false,
            output: None,
            outcome: ToolOutcome::Executed,
            duration_ms: 0,
        })
        .unwrap();
        assert!(plain.get("output").is_none());

        let full = EventMsg::ToolCallEnd {
            call_id: "c".to_string(),
            is_error: false,
            output: Some(ToolCallPreview {
                text: "out".to_string(),
                truncated: true,
            }),
            outcome: ToolOutcome::SurfaceBlocked,
            duration_ms: 1234,
        };
        let wire = serde_json::to_value(&full).unwrap();
        // Outcomes cross as snake_case tags; a metrics reader aggregates on
        // them, so the spelling is part of the format.
        assert_eq!(wire["outcome"], "surface_blocked");
        assert_eq!(wire["duration_ms"], 1234);
        let back: EventMsg = serde_json::from_value(wire).unwrap();
        assert_eq!(full, back);
    }

    #[test]
    fn preview_head_respects_char_boundaries() {
        let preview = ToolCallPreview::head("short", 16);
        assert_eq!(preview.text, "short");
        assert!(!preview.truncated);

        let preview = ToolCallPreview::head("0123456789", 4);
        assert_eq!(preview.text, "0123");
        assert!(preview.truncated);

        // Multi-byte characters never split mid-character.
        let preview = ToolCallPreview::head("你好世界", 7);
        assert_eq!(preview.text, "你好");
        assert!(preview.truncated);
    }
}
