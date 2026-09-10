/*!
 * @file MetricsFold
 * @description Turn wire events into cumulative operations metrics.
 *
 * Responsibilities:
 * - Fold every wire event variant into counters exactly once.
 * - Accumulate token usage for cost accounting.
 * - Expose point-in-time snapshots without stopping the fold.
 *
 * This module must not depend on: any workspace crate except the wire
 * protocol data types. It observes; it never drives.
 */

//! Metrics fold: a pure observer over the event stream.
//!
//! Dashboards, cost guards, and evaluations all read [`Metrics`]; none of
//! them can perturb execution because recording is a read-only fold.

use operations_wire::{Event, EventMsg};

/// Cumulative counters folded from the event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metrics {
    /// TurnStarted events seen.
    pub turns_started: u64,
    /// TurnCompleted events seen.
    pub turns_completed: u64,
    /// ToolCallBegin events seen.
    pub tool_calls: u64,
    /// ToolCallEnd events with `is_error`.
    pub tool_errors: u64,
    /// ApprovalRequested events seen.
    pub approvals_requested: u64,
    /// Sum of reported input tokens.
    pub tokens_in: u64,
    /// Sum of reported output tokens.
    pub tokens_out: u64,
    /// CompactCompleted events seen.
    pub compactions: u64,
    /// Warning events seen.
    pub warnings: u64,
    /// Error events seen.
    pub errors: u64,
}

impl Metrics {
    /// Create zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event into the counters.
    pub fn record(&mut self, event: &Event) {
        match &event.msg {
            EventMsg::TurnStarted => self.turns_started += 1,
            EventMsg::TurnCompleted => self.turns_completed += 1,
            EventMsg::ToolCallBegin { .. } => self.tool_calls += 1,
            EventMsg::ToolCallEnd { is_error, .. } => {
                if *is_error {
                    self.tool_errors += 1;
                }
            }
            EventMsg::ApprovalRequested { .. } => self.approvals_requested += 1,
            EventMsg::TokenCount {
                input_tokens,
                output_tokens,
            } => {
                self.tokens_in += input_tokens;
                self.tokens_out += output_tokens;
            }
            EventMsg::CompactCompleted => self.compactions += 1,
            EventMsg::Warning { .. } => self.warnings += 1,
            EventMsg::Error { .. } => self.errors += 1,
            // Deltas, partial completions, and compaction starts carry no
            // metric signal and are deliberately ignored.
            _ => {}
        }
    }

    /// Error rate over completed tool calls, if any calls exist.
    pub fn tool_error_rate(&self) -> Option<f64> {
        if self.tool_calls == 0 {
            return None;
        }
        Some(self.tool_errors as f64 / self.tool_calls as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(msg: EventMsg) -> Event {
        Event {
            id: "s1".to_string(),
            msg,
        }
    }

    #[test]
    fn folds_a_full_turn_sequence() {
        let mut metrics = Metrics::new();
        for msg in [
            EventMsg::TurnStarted,
            EventMsg::ToolCallBegin {
                call_id: "c1".to_string(),
                name: "shell".to_string(),
            },
            EventMsg::ApprovalRequested {
                call_id: "c1".to_string(),
                detail: "d".to_string(),
            },
            EventMsg::ToolCallEnd {
                call_id: "c1".to_string(),
                is_error: true,
            },
            EventMsg::TokenCount {
                input_tokens: 100,
                output_tokens: 25,
            },
            EventMsg::CompactCompleted,
            EventMsg::Warning {
                message: "w".to_string(),
            },
            EventMsg::TurnCompleted,
        ] {
            metrics.record(&event(msg));
        }
        assert_eq!(metrics.turns_started, 1);
        assert_eq!(metrics.turns_completed, 1);
        assert_eq!(metrics.tool_calls, 1);
        assert_eq!(metrics.tool_errors, 1);
        assert_eq!(metrics.approvals_requested, 1);
        assert_eq!(metrics.tokens_in, 100);
        assert_eq!(metrics.tokens_out, 25);
        assert_eq!(metrics.compactions, 1);
        assert_eq!(metrics.warnings, 1);
        assert_eq!(metrics.errors, 0);
        assert_eq!(metrics.tool_error_rate(), Some(1.0));
    }

    #[test]
    fn error_rate_is_none_without_calls() {
        assert_eq!(Metrics::new().tool_error_rate(), None);
    }

    #[test]
    fn error_events_count_separately_from_tool_errors() {
        let mut metrics = Metrics::new();
        metrics.record(&event(EventMsg::Error {
            message: "e".to_string(),
            recoverable: true,
        }));
        assert_eq!(metrics.errors, 1);
        assert_eq!(metrics.tool_errors, 0);
    }
}
