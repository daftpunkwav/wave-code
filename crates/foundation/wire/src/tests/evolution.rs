//! Field evolution is additive: new fields are optional on the wire,
//! absent on older senders, and decode to the documented defaults;
//! set fields round-trip. These tests pin both directions so a
//! sender that predates a field still parses.

use crate::*;

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
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            input_tokens: 1,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            output_tokens: 2,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            cache_read_tokens: 0,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
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

    // The synthetic pre-dispatch settle carries its own locked tag and
    // round-trips unchanged, the same contract every other outcome
    // spelling is held to.
    let undispatched = EventMsg::ToolCallEnd {
        call_id: "c".to_string(),
        is_error: true,
        output: None,
        outcome: ToolOutcome::NotDispatched,
        duration_ms: 0,
    };
    let wire = serde_json::to_value(&undispatched).unwrap();
    assert_eq!(wire["outcome"], "not_dispatched");
    let back: EventMsg = serde_json::from_value(wire).unwrap();
    assert_eq!(back, undispatched);
}
