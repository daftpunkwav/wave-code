/*!
 * @file RuntimeBase
 * @description OS runtime primitives: channels, interruption, and limits.
 *
 * Responsibilities:
 * - Centralize channel capacities and timeout constants.
 * - Provide the cooperative interrupt handle shared across tasks.
 * - Document truncation budgets for event payloads.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Runtime base primitives with centralized limits.
//!
//! Scattered magic numbers are a maintenance hazard, so every capacity,
//! timeout, and truncation budget lives here with its rationale.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Capacity of the main event channel per session.
pub const EVENT_CHANNEL_CAP: usize = 256;
/// Capacity of the control channel for interrupts and approvals.
pub const CONTROL_CHANNEL_CAP: usize = 32;
/// Maximum queued submissions per session; overflow is an explicit error.
pub const PENDING_QUEUE_CAP: usize = 64;
/// Grace period for draining in-flight work on shutdown.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
/// Poll interval while a run parks on an approval decision.
pub const APPROVAL_POLL: Duration = Duration::from_millis(25);
/// Default timeout for lifecycle hook commands.
pub const HOOK_DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Character budget for event text payloads.
pub const EVENT_TEXT_TRUNCATION: usize = 2000;
/// Character budget for approval detail strings.
pub const APPROVAL_DETAIL_TRUNCATION: usize = 500;

/// Cooperative interrupt handle shared across spawned tasks.
///
/// Interrupts are delivered at safe points only: tasks poll
/// [`InterruptHandle::is_triggered`] between units of work instead of being
/// cancelled mid-operation, so no half-written state is left behind.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle {
    flag: Arc<AtomicBool>,
}

impl InterruptHandle {
    /// Create an untriggered handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal interruption; safe to call from any thread.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// True after [`InterruptHandle::trigger`] until [`InterruptHandle::reset`].
    pub fn is_triggered(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Clear a previous trigger, e.g. when a new run starts.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Truncate text to a character budget, appending an ellipsis on cut.
///
/// Operates on `char` boundaries so truncation never splits UTF-8.
pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_handle_triggers_and_resets() {
        let handle = InterruptHandle::new();
        assert!(!handle.is_triggered());
        handle.trigger();
        assert!(handle.is_triggered());
        // Clones share the same flag.
        assert!(handle.clone().is_triggered());
        handle.reset();
        assert!(!handle.is_triggered());
    }

    #[test]
    fn truncation_keeps_short_text_and_cuts_long_text() {
        assert_eq!(truncate("abc", 10), "abc");
        let cut = truncate("abcdef", 3);
        assert_eq!(cut, "abc...");
    }
}
