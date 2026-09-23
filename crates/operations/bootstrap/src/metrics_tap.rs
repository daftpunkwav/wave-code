/*!
 * @file MetricsTap
 * @description Persist one session's per-turn metric samples.
 *
 * Responsibilities:
 * - Fold the events a session actually emitted through the observe counters.
 * - Attribute each turn to the model named on its `TurnStarted` event.
 * - Append one ledger sample per finished turn and reset the fold.
 * - Stay invisible to execution: a failed write warns and never fails a turn.
 *
 * This module must not depend on: UI state, tool implementations, or policy
 * verdicts; it consumes the wire stream and hands counters to the ledger.
 */

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use operations_actor::EventTap;
use operations_observe::{Ledger, Metrics, TurnSample};
use wavecode_wire::{Event, EventMsg};

/// Per-session fold guarded for the tap's shared use.
#[derive(Default)]
struct TapState {
    /// Counters accumulated since the last finished turn.
    fold: Metrics,
    /// Model reported by the most recent `TurnStarted`.
    model: String,
}

/// One journaled session's event tap.
struct MetricsTap {
    /// Session id the samples are keyed by (same id `resume` takes).
    session: String,
    /// Where finished turns are appended.
    ledger: Ledger,
    state: std::sync::Mutex<TapState>,
}

impl MetricsTap {
    fn new(home: &Path, session: &str) -> Self {
        Self {
            session: session.to_string(),
            ledger: Ledger::in_home(home),
            state: std::sync::Mutex::new(TapState::default()),
        }
    }

    /// Fold one event; a finished turn flushes its sample.
    fn on_event(&self, event: &Event) {
        let finished = matches!(event.msg, EventMsg::TurnCompleted { .. });
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let EventMsg::TurnStarted { model } = &event.msg {
            state.model = model.clone();
        }
        state.fold.record(event);
        if !finished {
            return;
        }
        let sample = TurnSample {
            ts_secs: now_secs(),
            session: self.session.clone(),
            model: std::mem::take(&mut state.model),
            metrics: std::mem::take(&mut state.fold),
        };
        if let Err(e) = self.ledger.append(&sample) {
            // Measurement is never a reason to fail the turn it observed.
            tracing::warn!("metrics ledger write failed: {e}");
        }
    }
}

/// Unix seconds, saturating on pre-epoch clocks.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Build the tap for a session that journals under `home`.
pub fn metrics_tap(home: &Path, session: &str) -> Arc<EventTap> {
    let tap = Arc::new(MetricsTap::new(home, session));
    Arc::new(move |event: &Event| tap.on_event(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_wire::{ToolCallPreview, ToolOutcome};

    fn event(id: &str, msg: EventMsg) -> Event {
        Event {
            id: id.to_string(),
            msg,
        }
    }

    /// Two turns on two models must land as two attributed samples: the
    /// model×tool table depends on per-turn, not per-session, attribution.
    #[tokio::test]
    async fn writes_one_attributed_sample_per_turn() {
        let dir = tempfile::tempdir().unwrap();
        let tap = metrics_tap(dir.path(), "s1");
        let events = [
            event(
                "t1",
                EventMsg::TurnStarted {
                    model: "opus".into(),
                },
            ),
            event(
                "t1",
                EventMsg::ToolCallBegin {
                    call_id: "c1".into(),
                    name: "edit".into(),
                    input: serde_json::Value::Null,
                },
            ),
            event(
                "t1",
                EventMsg::ToolCallEnd {
                    call_id: "c1".into(),
                    is_error: true,
                    output: Some(ToolCallPreview::head("no match", 64)),
                    outcome: ToolOutcome::Executed,
                    duration_ms: 3,
                },
            ),
            event("t1", EventMsg::TurnCompleted { interrupted: false }),
            event(
                "t2",
                EventMsg::TurnStarted {
                    model: "sonnet".into(),
                },
            ),
            event("t2", EventMsg::TurnCompleted { interrupted: true }),
        ];
        for event in &events {
            tap(event);
        }

        let read = Ledger::in_home(dir.path()).read();
        assert_eq!(read.samples.len(), 2);
        assert_eq!(read.samples[0].model, "opus");
        assert_eq!(read.samples[0].session, "s1");
        assert_eq!(read.samples[0].metrics.tools["edit"].executed_failed, 1);
        // The second turn carries its own model and an empty tool fold:
        // counters never bleed across the boundary.
        assert_eq!(read.samples[1].model, "sonnet");
        assert_eq!(read.samples[1].metrics.tool_calls, 0);
        assert_eq!(read.samples[1].metrics.turns_interrupted, 1);
    }

    /// An unfinished turn leaves nothing on disk, so a crashed session is
    /// absent from the baseline rather than half-counted.
    #[tokio::test]
    async fn holds_partial_turns_until_completion() {
        let dir = tempfile::tempdir().unwrap();
        let tap = metrics_tap(dir.path(), "s1");
        tap(&event(
            "t1",
            EventMsg::TurnStarted {
                model: "opus".into(),
            },
        ));
        assert!(Ledger::in_home(dir.path()).read().samples.is_empty());
    }
}
