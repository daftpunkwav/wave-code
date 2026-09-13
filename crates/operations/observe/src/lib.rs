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

use wavecode_wire::{Event, EventMsg};

/// Cumulative counters folded from the event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metrics {
    /// TurnStarted events seen.
    pub turns_started: u64,
    /// TurnCompleted events seen.
    pub turns_completed: u64,
    /// TurnCompleted events with `interrupted` set: cancellation storms
    /// surface here while `turns_completed` keeps the total.
    pub turns_interrupted: u64,
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
    /// Sum of prompt-cache read tokens reported by the provider (0 when
    /// the provider reports no cache accounting). Cache reads bill at a
    /// fraction of fresh input, so cost estimates must weight them apart.
    pub cache_read_tokens: u64,
    /// Sum of prompt-cache write tokens reported by the provider (billed
    /// at a premium over fresh input).
    pub cache_creation_tokens: u64,
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
            EventMsg::TurnCompleted { interrupted } => {
                self.turns_completed += 1;
                if *interrupted {
                    self.turns_interrupted += 1;
                }
            }
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
                cache_read_tokens,
                cache_creation_tokens,
            } => {
                self.tokens_in += input_tokens;
                self.tokens_out += output_tokens;
                self.cache_read_tokens += cache_read_tokens;
                self.cache_creation_tokens += cache_creation_tokens;
            }
            EventMsg::CompactCompleted { .. } => self.compactions += 1,
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

    /// Average output tokens per completed turn, if any turns completed.
    pub fn avg_output_per_turn(&self) -> Option<f64> {
        if self.turns_completed == 0 {
            return None;
        }
        Some(self.tokens_out as f64 / self.turns_completed as f64)
    }
}

/// One timed span with an explicit millisecond clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanTimer {
    /// Span name for correlation.
    pub name: String,
    /// Start time in caller milliseconds.
    pub start_ms: u64,
    /// End time once finished.
    pub end_ms: Option<u64>,
}

impl SpanTimer {
    /// Start a span at `now_ms`.
    pub fn start(name: impl Into<String>, now_ms: u64) -> Self {
        Self {
            name: name.into(),
            start_ms: now_ms,
            end_ms: None,
        }
    }

    /// Finish the span at `now_ms`.
    pub fn finish(&mut self, now_ms: u64) {
        self.end_ms = Some(now_ms);
    }

    /// Elapsed milliseconds, saturating on backwards clocks.
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        self.end_ms.unwrap_or(now_ms).saturating_sub(self.start_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_wire::ApprovalKind;

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
                input: serde_json::Value::Null,
            },
            EventMsg::ApprovalRequested {
                call_id: "c1".to_string(),
                kind: ApprovalKind::Exec,
                detail: "d".to_string(),
            },
            EventMsg::ToolCallEnd {
                call_id: "c1".to_string(),
                is_error: true,
            },
            EventMsg::TokenCount {
                input_tokens: 100,
                output_tokens: 25,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            EventMsg::CompactCompleted { summary_tokens: 25 },
            EventMsg::Warning {
                message: "w".to_string(),
            },
            EventMsg::TurnCompleted { interrupted: false },
        ] {
            metrics.record(&event(msg));
        }
        assert_eq!(metrics.turns_started, 1);
        assert_eq!(metrics.turns_completed, 1);
        assert_eq!(metrics.turns_interrupted, 0);
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
    fn interrupted_completions_count_separately() {
        let mut metrics = Metrics::new();
        metrics.record(&event(EventMsg::TurnCompleted { interrupted: false }));
        metrics.record(&event(EventMsg::TurnCompleted { interrupted: true }));
        assert_eq!(metrics.turns_completed, 2);
        assert_eq!(metrics.turns_interrupted, 1);
    }

    /// Cache accounting accumulates across samples and stays separable
    /// from the raw in/out totals.
    #[test]
    fn cache_tokens_accumulate_per_sample() {
        let mut metrics = Metrics::new();
        for (read, creation) in [(100u64, 7u64), (50, 0)] {
            metrics.record(&event(EventMsg::TokenCount {
                input_tokens: 10,
                output_tokens: 3,
                cache_read_tokens: read,
                cache_creation_tokens: creation,
            }));
        }
        metrics.record(&event(EventMsg::TurnCompleted { interrupted: false }));
        assert_eq!(metrics.cache_read_tokens, 150);
        assert_eq!(metrics.cache_creation_tokens, 7);
        assert_eq!(metrics.tokens_in, 20);
        assert_eq!(metrics.avg_output_per_turn(), Some(6.0));
    }

    #[test]
    fn avg_output_is_none_without_completed_turns() {
        assert_eq!(Metrics::new().avg_output_per_turn(), None);
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

    #[test]
    fn spans_measure_with_explicit_clocks() {
        let mut span = SpanTimer::start("turn", 100);
        assert_eq!(span.elapsed_ms(150), 50);
        span.finish(180);
        assert_eq!(span.elapsed_ms(999), 80);
        // Backwards clocks saturate instead of underflowing.
        assert_eq!(SpanTimer::start("x", 50).elapsed_ms(10), 0);
    }
}
