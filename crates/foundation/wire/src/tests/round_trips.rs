//! Every enum variant round-trips through JSON unchanged and keeps
//! its locked snake_case tag: frontends and the TS SDK mirror these
//! enums field-for-field, so a variant that silently renames, drops
//! a field, or changes its tag is a cross-process contract break.

use crate::*;

/// Wire tag of every `Op` variant. The match is exhaustive on
/// purpose: adding or renaming a variant breaks this compile and
/// forces the table below (and every wire consumer) to be updated.
fn op_tag(op: &Op) -> &'static str {
    match op {
        Op::UserInput { .. } => "user_input",
        Op::Interrupt => "interrupt",
        Op::ExecApproval { .. } => "exec_approval",
        Op::QuestionAnswer { .. } => "question_answer",
        Op::Compact { .. } => "compact",
        Op::Rewind { .. } => "rewind",
        Op::SetPermissionMode { .. } => "set_permission_mode",
        Op::SetModel { .. } => "set_model",
        Op::SetThinking { .. } => "set_thinking",
        Op::Shutdown => "shutdown",
    }
}

/// Every `Op` variant round-trips through JSON unchanged and keeps
/// its locked snake_case wire tag: frontends and the gateway depend
/// on both the shape and the spelling, and a variant that silently
/// loses a field on the wire is a cross-process contract break.
#[test]
fn every_op_variant_round_trips_with_its_locked_tag() {
    let variants: Vec<Op> = vec![
        Op::UserInput {
            text: "hi".to_string(),
            images: vec![UserImage {
                id: Some("img-1".to_string()),
                mime: "image/png".to_string(),
                base64: "aGk=".to_string(),
            }],
        },
        Op::Interrupt,
        Op::ExecApproval {
            call_id: "call-1".to_string(),
            decision: WireDecision::AllowAlways,
        },
        Op::ExecApproval {
            call_id: "call-2".to_string(),
            decision: WireDecision::Deny {
                reason: "no".to_string(),
            },
        },
        Op::QuestionAnswer {
            call_id: "call-3".to_string(),
            answer: "option two".to_string(),
        },
        Op::Compact {
            instruction: Some("keep the plan".to_string()),
        },
        Op::Compact { instruction: None },
        Op::Rewind { turns: 2 },
        Op::SetPermissionMode {
            mode: "plan".to_string(),
        },
        Op::SetModel {
            name: "claude-sonnet-4-5".to_string(),
        },
        Op::SetThinking {
            effort: "low".to_string(),
        },
        Op::Shutdown,
    ];
    for op in variants {
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(
            json.get("type").and_then(serde_json::Value::as_str),
            Some(op_tag(&op)),
            "{op:?} must keep its wire tag"
        );
        let back: Op = serde_json::from_value(json).unwrap();
        assert_eq!(op, back, "{op:?} must round-trip unchanged");
    }
}

/// Wire tag of every `EventMsg` variant. The match is exhaustive on
/// purpose: adding or renaming a variant breaks this compile and
/// forces the table below (and every frontend / TS SDK mirror) to be
/// updated.
fn event_tag(msg: &EventMsg) -> &'static str {
    match msg {
        EventMsg::TurnStarted { .. } => "turn_started",
        EventMsg::AgentMessageDelta { .. } => "agent_message_delta",
        EventMsg::AgentThinkingDelta { .. } => "agent_thinking_delta",
        EventMsg::AgentMessageComplete { .. } => "agent_message_complete",
        EventMsg::ToolCallBegin { .. } => "tool_call_begin",
        EventMsg::ToolCallEnd { .. } => "tool_call_end",
        EventMsg::ApprovalRequested { .. } => "approval_requested",
        EventMsg::QuestionRequested { .. } => "question_requested",
        EventMsg::TokenCount { .. } => "token_count",
        EventMsg::CompactStarted { .. } => "compact_started",
        EventMsg::CompactCompleted { .. } => "compact_completed",
        EventMsg::HistoryRewound { .. } => "history_rewound",
        EventMsg::PlanProposed { .. } => "plan_proposed",
        EventMsg::PlanApproved => "plan_approved",
        EventMsg::GoalSet { .. } => "goal_set",
        EventMsg::GoalCompleted => "goal_completed",
        EventMsg::Warning { .. } => "warning",
        EventMsg::Error { .. } => "error",
        EventMsg::TurnCompleted { .. } => "turn_completed",
    }
}

/// Every `EventMsg` variant round-trips through JSON unchanged and
/// keeps its locked snake_case wire tag: frontends and the TS SDK
/// mirror this enum field-for-field, so a variant that silently
/// renames, drops a field, or changes its tag is a cross-process
/// contract break. The populated `ToolCallEnd` also locks its key
/// field shapes on the wire (preview object, outcome spelling,
/// duration) because metrics and transcripts parse those names.
#[test]
fn every_event_msg_variant_round_trips_with_its_locked_tag() {
    let variants: Vec<EventMsg> = vec![
        EventMsg::TurnStarted {
            model: "claude-sonnet-4-5".to_string(),
        },
        EventMsg::AgentMessageDelta {
            text: "hel".to_string(),
        },
        EventMsg::AgentThinkingDelta {
            text: "hmm".to_string(),
        },
        EventMsg::AgentMessageComplete {
            text: "hello".to_string(),
        },
        EventMsg::ToolCallBegin {
            call_id: "call-1".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "ls"}),
        },
        // The fully-populated end event: optional fields set, a
        // non-default outcome, and a real duration lock the whole
        // field shape, not just the tag.
        EventMsg::ToolCallEnd {
            call_id: "call-2".to_string(),
            is_error: true,
            output: Some(ToolCallPreview {
                text: "out".to_string(),
                truncated: true,
            }),
            outcome: ToolOutcome::Refused,
            duration_ms: 42,
        },
        EventMsg::ApprovalRequested {
            call_id: "call-3".to_string(),
            kind: ApprovalKind::Write,
            detail: "d".to_string(),
        },
        EventMsg::QuestionRequested {
            call_id: "call-4".to_string(),
            question: "q".to_string(),
            options: vec!["a".to_string(), "b".to_string()],
        },
        EventMsg::TokenCount {
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            input_tokens: 10,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            output_tokens: 20,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            cache_read_tokens: 5,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            cache_creation_tokens: 6,
            context_window: Some(200_000),
            context_used: Some(1_000),
        },
        EventMsg::CompactStarted {
            trigger: "auto".to_string(),
        },
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        EventMsg::CompactCompleted { summary_tokens: 99 },
        EventMsg::HistoryRewound { turns: 2 },
        EventMsg::PlanProposed {
            text: "plan".to_string(),
        },
        EventMsg::PlanApproved,
        EventMsg::GoalSet {
            objective: "ship".to_string(),
        },
        EventMsg::GoalCompleted,
        EventMsg::Warning {
            message: "w".to_string(),
        },
        EventMsg::Error {
            message: "e".to_string(),
            recoverable: false,
            code: Some("provider.error".to_string()),
        },
        EventMsg::TurnCompleted { interrupted: true },
    ];
    for msg in &variants {
        let json = serde_json::to_value(msg).unwrap();
        assert_eq!(
            json.get("type").and_then(serde_json::Value::as_str),
            Some(event_tag(msg)),
            "{msg:?} must keep its wire tag"
        );
        let back: EventMsg = serde_json::from_value(json).unwrap();
        assert_eq!(msg, &back, "{msg:?} must round-trip unchanged");
    }

    // Key field shapes of the populated ToolCallEnd: transcript
    // renderers read `output.{text,truncated}`, metrics read
    // `outcome` and `duration_ms`, and pairing reads `call_id`.
    let end = serde_json::to_value(&variants[5]).unwrap();
    assert_eq!(end["call_id"], "call-2", "{end}");
    assert_eq!(end["is_error"], true, "{end}");
    assert_eq!(end["output"]["text"], "out", "{end}");
    assert_eq!(end["output"]["truncated"], true, "{end}");
    assert_eq!(end["outcome"], "refused", "{end}");
    assert_eq!(end["duration_ms"], 42, "{end}");
}
