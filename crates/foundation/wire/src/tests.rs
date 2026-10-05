//! Wire-format lock tests: the serde tag vocabulary, field shapes, and
//! backward compatibility of this crate's types are pinned here so any
//! rename or shape change is an explicit, reviewed breaking change.
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
fn wire_tags_op_are_locked() {
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
}

#[test]
fn wire_tags_event_msg_are_locked() {
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
                // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                input_tokens: 1,
                // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                output_tokens: 2,
                // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                cache_read_tokens: 0,
                // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
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
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
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
}

#[test]
fn wire_tags_wire_decision_are_locked() {
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
}

#[test]
fn wire_tags_approval_kind_are_locked() {
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
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            input_tokens: 10,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            output_tokens: 20,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            cache_read_tokens: 5,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            cache_creation_tokens: 6,
            context_window: Some(200_000),
            context_used: Some(1_000),
        },
        EventMsg::CompactStarted {
            trigger: "auto".to_string(),
        },
        // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
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
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            input_tokens: 1,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            output_tokens: 2,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            cache_read_tokens: 0,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
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
