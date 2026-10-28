//! Protocol vocabulary shapes: the reminder markers, the submission
//! envelope, and the preview cap all carry literal shapes consumers
//! parse; each is pinned here.

use crate::*;

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
