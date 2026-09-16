//! Streaming delta coalescing: draft buffers flushed on a cadence.

use std::time::{Duration, Instant};

/// Flush cadence for streamed drafts (token deltas batch into renders).
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// Byte budget for the live streaming draft (runaway-sample guard).
pub const MAX_DRAFT_BYTES: usize = 64 * 1024;

/// Buffers streamed thinking/assistant text and decides when the frame
/// should re-render. The full frame is cheap to diff downstream; this
/// only bounds re-render frequency during heavy streaming.
#[derive(Debug)]
pub struct StreamingController {
    pub thinking: String,
    pub assistant: String,
    dirty: bool,
    last_flush: Option<Instant>,
}

impl StreamingController {
    /// An empty controller.
    pub fn new() -> Self {
        Self {
            thinking: String::new(),
            assistant: String::new(),
            dirty: false,
            last_flush: None,
        }
    }

    /// Append thinking text; returns true when a render is due now.
    pub fn push_thinking(&mut self, text: &str) -> bool {
        self.thinking.push_str(text);
        self.mark_dirty()
    }

    /// Append assistant text; returns true when a render is due now.
    pub fn push_assistant(&mut self, text: &str) -> bool {
        self.assistant.push_str(text);
        self.mark_dirty()
    }

    fn mark_dirty(&mut self) -> bool {
        self.dirty = true;
        // Runaway-sample guard: hard-cap the live drafts without ever
        // splitting a UTF-8 character (`String::truncate` would panic).
        truncate_at_boundary(&mut self.assistant, MAX_DRAFT_BYTES);
        truncate_at_boundary(&mut self.thinking, MAX_DRAFT_BYTES);
        self.due(Instant::now())
    }

    /// True when a draft changed since the last flush.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// True when dirty and the flush interval has elapsed.
    pub fn due(&self, now: Instant) -> bool {
        self.dirty
            && self
                .last_flush
                .is_none_or(|at| now.duration_since(at) >= FLUSH_INTERVAL)
    }

    /// Note that a flush happened.
    pub fn flushed(&mut self, now: Instant) {
        self.dirty = false;
        self.last_flush = Some(now);
    }

    /// Take the accumulated assistant draft.
    pub fn take_assistant(&mut self) -> String {
        std::mem::take(&mut self.assistant)
    }

    /// Drop all drafts (turn teardown).
    pub fn clear(&mut self) {
        self.thinking.clear();
        self.assistant.clear();
        self.dirty = false;
    }

    /// True when neither draft holds text.
    pub fn is_empty(&self) -> bool {
        self.thinking.is_empty() && self.assistant.is_empty()
    }
}

impl Default for StreamingController {
    fn default() -> Self {
        Self::new()
    }
}

/// Cap `text` at `max` bytes on the nearest character boundary.
fn truncate_at_boundary(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_delta_is_due_immediately() {
        let mut controller = StreamingController::new();
        assert!(controller.push_assistant("hello"));
        controller.flushed(Instant::now());
    }

    #[test]
    fn second_flush_waits_for_interval() {
        let mut controller = StreamingController::new();
        controller.push_assistant("a");
        let now = Instant::now();
        controller.flushed(now);
        controller.push_assistant("b");
        assert!(!controller.due(now), "just flushed: not due");
        assert!(
            controller.due(now + FLUSH_INTERVAL + Duration::from_millis(1)),
            "after the interval: due"
        );
    }

    #[test]
    fn take_clears_assistant_draft() {
        let mut controller = StreamingController::new();
        controller.push_assistant("body");
        assert_eq!(controller.take_assistant(), "body");
        assert!(controller.is_empty());
    }

    #[test]
    fn runaway_draft_is_capped() {
        let mut controller = StreamingController::new();
        controller.push_assistant(&"x".repeat(MAX_DRAFT_BYTES + 1024));
        assert!(controller.assistant.len() <= MAX_DRAFT_BYTES);
    }

    #[test]
    fn cjk_draft_caps_without_panicking() {
        let mut controller = StreamingController::new();
        // Multi-byte characters straddle the byte cap: truncation must
        // fall back to a character boundary instead of panicking.
        controller.push_assistant(&"你".repeat(MAX_DRAFT_BYTES / 3 + 16));
        assert!(controller.assistant.len() <= MAX_DRAFT_BYTES);
        assert!(
            controller
                .assistant
                .is_char_boundary(controller.assistant.len())
        );
        controller.push_thinking(&"海".repeat(MAX_DRAFT_BYTES / 3 + 16));
        assert!(controller.thinking.len() <= MAX_DRAFT_BYTES);
    }

    #[test]
    fn clear_resets_everything() {
        let mut controller = StreamingController::new();
        controller.push_thinking("t");
        controller.push_assistant("a");
        controller.clear();
        assert!(controller.is_empty());
        assert!(!controller.dirty);
    }
}
