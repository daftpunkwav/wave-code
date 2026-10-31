//! The serde tag vocabulary is locked value by value: renaming any
//! tag is a breaking protocol change and must update the frontends
//! (and the TS SDK) in the same commit.

use crate::*;

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
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                input_tokens: 1,
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                output_tokens: 2,
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                cache_read_tokens: 0,
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
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
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
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
