/*!
 * @file MetricsFold
 * @description Turn wire events into cumulative operations metrics.
 *
 * Responsibilities:
 * - Fold every wire event variant into counters exactly once.
 * - Split tool results per tool so quality and approval friction stay
 *   separable.
 * - Accumulate token usage for cost accounting, including cache share.
 * - Expose point-in-time snapshots without stopping the fold.
 * - Re-export the append-once ledger that persists those snapshots.
 *
 * This module must not depend on: any workspace crate except the wire
 * protocol data types. It observes; it never drives.
 */

//! Metrics fold: a pure observer over the event stream.
//!
//! Dashboards, cost guards, and evaluations all read [`Metrics`]; none of
//! them can perturb execution because recording is a read-only fold.

mod ledger;

pub use ledger::{Ledger, LedgerRead, TurnSample};

use std::collections::HashMap;

use wavecode_wire::{Event, EventMsg, ToolOutcome};

/// Cumulative counters folded from the event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Per-tool quality counters, keyed by tool name.
    #[serde(default)]
    pub tools: HashMap<String, ToolStat>,
    /// Open tool calls awaiting their end event, keyed by call id. Only the
    /// name is retained: ends carry an id, and without this pairing the
    /// per-tool split would be impossible.
    #[serde(skip)]
    open_calls: HashMap<String, String>,
}

/// Per-tool counters folded from one stream of tool calls.
///
/// The split mirrors [`ToolOutcome`]: calls whose bodies ran are separated
/// from calls the harness never started, because only the first group says
/// something about the tool and only the second says something about
/// approval friction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolStat {
    /// Calls whose body ran and reported success.
    pub executed_ok: u64,
    /// Calls whose body ran and reported a business failure.
    pub executed_failed: u64,
    /// Denied by a policy rule.
    pub denied: u64,
    /// Refused at the approval prompt.
    pub refused: u64,
    /// Blocked by a `PreToolUse` hook or by the run's tool surface.
    pub blocked: u64,
    /// Interrupted before the body ran.
    pub interrupted: u64,
    /// Ends that never ran a body for any other reason (duplicate call id,
    /// `ask_user` answers, unavailable question, missing slot).
    pub other: u64,
    /// Sum of reported body durations, in milliseconds.
    pub busy_ms: u64,
}

impl ToolStat {
    /// Calls whose body ran.
    pub fn executed(&self) -> u64 {
        self.executed_ok + self.executed_failed
    }

    /// Calls that reached the pipeline at all.
    pub fn total(&self) -> u64 {
        self.executed() + self.denied + self.refused + self.blocked + self.interrupted + self.other
    }

    /// Share of executed calls that reported no business failure.
    ///
    /// This is the tool-quality signal the plan asks for: a failed call is
    /// what forces the model to retry, so over a session it tracks how often
    /// the model's first attempt at this tool worked. `None` when the body
    /// never ran, so an approval-heavy tool is never scored as a broken one.
    pub fn success_rate(&self) -> Option<f64> {
        let executed = self.executed();
        if executed == 0 {
            return None;
        }
        Some(self.executed_ok as f64 / executed as f64)
    }
}

impl Metrics {
    /// Create zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event into the counters.
    pub fn record(&mut self, event: &Event) {
        match &event.msg {
            EventMsg::TurnStarted { .. } => self.turns_started += 1,
            EventMsg::TurnCompleted { interrupted } => {
                self.turns_completed += 1;
                if *interrupted {
                    self.turns_interrupted += 1;
                }
            }
            EventMsg::ToolCallBegin { call_id, name, .. } => {
                self.tool_calls += 1;
                self.open_calls.insert(call_id.clone(), name.clone());
            }
            EventMsg::ToolCallEnd {
                call_id,
                is_error,
                outcome,
                duration_ms,
                ..
            } => {
                if *is_error {
                    self.tool_errors += 1;
                }
                // Ends can arrive without a begin (a recording that starts
                // mid-turn); fold those under a placeholder key so their
                // outcome still counts somewhere.
                let name = self
                    .open_calls
                    .remove(call_id)
                    .unwrap_or_else(|| String::from("<unknown>"));
                let stat = self.tools.entry(name).or_default();
                match outcome {
                    ToolOutcome::Executed => {
                        stat.busy_ms += duration_ms;
                        if *is_error {
                            stat.executed_failed += 1;
                        } else {
                            stat.executed_ok += 1;
                        }
                    }
                    ToolOutcome::Denied => stat.denied += 1,
                    ToolOutcome::Refused => stat.refused += 1,
                    ToolOutcome::HookBlocked | ToolOutcome::SurfaceBlocked => stat.blocked += 1,
                    ToolOutcome::Interrupted => stat.interrupted += 1,
                    ToolOutcome::Duplicate
                    | ToolOutcome::Answered
                    | ToolOutcome::AskUnavailable
                    | ToolOutcome::Missing => stat.other += 1,
                }
            }
            EventMsg::ApprovalRequested { .. } => self.approvals_requested += 1,
            EventMsg::TokenCount {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                ..
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

    /// Fold another fold into this one.
    ///
    /// Ledger aggregation: each stored sample is one turn's fold, and the
    /// read side merges them back into session- or model-level totals.
    /// Merging stays here so the arithmetic lives beside the counters.
    pub fn merge(&mut self, other: &Metrics) {
        self.turns_started += other.turns_started;
        self.turns_completed += other.turns_completed;
        self.turns_interrupted += other.turns_interrupted;
        self.tool_calls += other.tool_calls;
        self.tool_errors += other.tool_errors;
        self.approvals_requested += other.approvals_requested;
        self.tokens_in += other.tokens_in;
        self.tokens_out += other.tokens_out;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.compactions += other.compactions;
        self.warnings += other.warnings;
        self.errors += other.errors;
        for (name, stat) in &other.tools {
            let entry = self.tools.entry(name.clone()).or_default();
            entry.executed_ok += stat.executed_ok;
            entry.executed_failed += stat.executed_failed;
            entry.denied += stat.denied;
            entry.refused += stat.refused;
            entry.blocked += stat.blocked;
            entry.interrupted += stat.interrupted;
            entry.other += stat.other;
            entry.busy_ms += stat.busy_ms;
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

    /// Share of prompt input served from the provider's cache.
    ///
    /// The long-session cost signal: it stays near 1.0 while the cached
    /// prefix holds and falls off a cliff when compaction or a reordered
    /// tool catalog invalidates it. `None` when no sample reported cache
    /// accounting, so providers without caching never read as a 0% hit rate.
    pub fn cache_read_share(&self) -> Option<f64> {
        if self.tokens_in == 0 || (self.cache_read_tokens == 0 && self.cache_creation_tokens == 0) {
            return None;
        }
        Some(self.cache_read_tokens as f64 / self.tokens_in as f64)
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
    use wavecode_wire::{ApprovalKind, ToolOutcome};

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
            EventMsg::TurnStarted {
                model: "claude-test".to_string(),
            },
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
                output: None,
                outcome: ToolOutcome::Executed,
                duration_ms: 0,
            },
            EventMsg::TokenCount {
                input_tokens: 100,
                output_tokens: 25,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                context_window: None,
                context_used: None,
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

    /// The per-tool split is what tells a weak tool apart from a tool that
    /// merely gets refused: `edit` failing 9 of 10 executed calls and `shell`
    /// refused 9 of 10 must not fold into the same number.
    #[test]
    fn splits_tool_quality_from_approval_friction() {
        let mut metrics = Metrics::new();
        for (name, outcome, is_error, ms) in [
            ("edit", ToolOutcome::Executed, true, 3u64),
            ("edit", ToolOutcome::Executed, true, 4),
            ("edit", ToolOutcome::Executed, false, 1),
            ("shell", ToolOutcome::Executed, false, 1200),
            ("shell", ToolOutcome::Refused, true, 0),
            ("shell", ToolOutcome::Refused, true, 0),
            ("shell", ToolOutcome::Denied, true, 0),
        ] {
            metrics.record(&event(EventMsg::ToolCallBegin {
                call_id: name.to_string(),
                name: name.to_string(),
                input: serde_json::Value::Null,
            }));
            metrics.record(&event(EventMsg::ToolCallEnd {
                call_id: name.to_string(),
                is_error,
                output: None,
                outcome,
                duration_ms: ms,
            }));
        }
        let edit = &metrics.tools["edit"];
        assert_eq!(edit.executed(), 3);
        assert_eq!(edit.total(), 3);
        assert_eq!(edit.success_rate(), Some(1.0 / 3.0));
        assert_eq!(edit.busy_ms, 8);

        let shell = &metrics.tools["shell"];
        assert_eq!(shell.executed(), 1);
        assert_eq!(shell.success_rate(), Some(1.0));
        assert_eq!(shell.refused, 2);
        assert_eq!(shell.denied, 1);
        assert_eq!(shell.total(), 4);

        // A tool that only ever meets the approval gate scores no quality
        // signal at all, rather than a misleading zero.
        let mut gated = Metrics::new();
        gated.record(&event(EventMsg::ToolCallBegin {
            call_id: "c".to_string(),
            name: "write".to_string(),
            input: serde_json::Value::Null,
        }));
        gated.record(&event(EventMsg::ToolCallEnd {
            call_id: "c".to_string(),
            is_error: true,
            output: None,
            outcome: ToolOutcome::Refused,
            duration_ms: 0,
        }));
        assert_eq!(gated.tools["write"].success_rate(), None);
    }

    /// Cache share reads as absent, not zero, for providers with no cache
    /// accounting — otherwise a non-caching gateway looks like a total
    /// prefix-cache failure.
    #[test]
    fn cache_share_separates_unreported_from_unhit() {
        let mut metrics = Metrics::new();
        metrics.record(&event(EventMsg::TokenCount {
            input_tokens: 100,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            context_window: None,
            context_used: None,
        }));
        assert_eq!(metrics.cache_read_share(), None);

        let mut hit = Metrics::new();
        hit.record(&event(EventMsg::TokenCount {
            input_tokens: 100,
            output_tokens: 5,
            cache_read_tokens: 75,
            cache_creation_tokens: 25,
            context_window: None,
            context_used: None,
        }));
        assert_eq!(hit.cache_read_share(), Some(0.75));
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
                context_window: None,
                context_used: None,
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
            code: None,
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
