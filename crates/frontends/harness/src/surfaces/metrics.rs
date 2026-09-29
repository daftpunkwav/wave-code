//! `wavecode metrics`: aggregate the local metrics ledger into
//! per-model, per-tool quality numbers.
//!
//! Reads `~/.wavecode/metrics` only; no session, no provider.

use std::path::Path;

/// Model name for a sample that carries none (a driver predating model
/// attribution, or a stub turn in tests).
const METRICS_UNKNOWN_MODEL: &str = "(unknown)";

/// Merge ledger samples into per-model totals.
///
/// Grouping by model is what makes the table answer "is this tool weak, or
/// is this model bad at it" — the same tool ranked across two models.
fn metrics_totals(
    samples: &[operations_observe::TurnSample],
) -> std::collections::BTreeMap<String, operations_observe::Metrics> {
    let mut totals: std::collections::BTreeMap<String, operations_observe::Metrics> =
        std::collections::BTreeMap::new();
    for sample in samples {
        let model = if sample.model.is_empty() {
            METRICS_UNKNOWN_MODEL.to_string()
        } else {
            sample.model.clone()
        };
        totals.entry(model).or_default().merge(&sample.metrics);
    }
    totals
}

/// Render one model's tool table, ranked by how often the model reached for
/// the tool. Success rate covers executed calls only, so a tool that is
/// mostly refused never reads as broken.
fn metrics_table(metrics: &operations_observe::Metrics) -> String {
    use operations_observe::ToolStat;

    let mut rows: Vec<(&String, &ToolStat)> = metrics
        .tools
        .iter()
        .filter(|(_, stat)| stat.total() > 0)
        .collect();
    rows.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(b.0)));
    if rows.is_empty() {
        return "  (no tool calls recorded)
"
        .to_string();
    }
    let mut out = format!(
        "  {:<22}{:>7}{:>7}{:>7}{:>9}{:>8}{:>7}{:>10}
",
        "tool", "calls", "ok", "fail", "refused", "denied", "busy", "success"
    );
    for (name, stat) in rows {
        let success = match stat.success_rate() {
            Some(rate) => format!("{:.1}%", rate * 100.0),
            None => "-".to_string(),
        };
        out.push_str(&format!(
            "  {:<22}{:>7}{:>7}{:>7}{:>9}{:>8}{:>6.1}s{:>10}
",
            name,
            stat.total(),
            stat.executed_ok,
            stat.executed_failed,
            stat.refused,
            stat.denied + stat.blocked,
            stat.busy_ms as f64 / 1000.0,
            success
        ));
    }
    if let Some(share) = metrics.cache_read_share() {
        out.push_str(&format!(
            "  prompt cache read {:.1}% of input (in {} / out {} tokens, {} compactions)
",
            share * 100.0,
            metrics.tokens_in,
            metrics.tokens_out,
            metrics.compactions
        ));
    }
    let turns = metrics.turns_completed;
    out.push_str(&format!(
        "  {} turns ({} interrupted), {} approvals requested, {} warnings, {} errors
",
        turns,
        metrics.turns_interrupted,
        metrics.approvals_requested,
        metrics.warnings,
        metrics.errors
    ));
    out
}

/// Full report text for the merged totals.
fn metrics_report(
    totals: &std::collections::BTreeMap<String, operations_observe::Metrics>,
) -> String {
    if totals.is_empty() {
        return "no metric samples recorded yet".to_string();
    }
    let mut out = String::new();
    for (model, metrics) in totals {
        out.push_str(&format!("model: {model}\n"));
        out.push_str(&metrics_table(metrics));
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// `metrics`: aggregate the local ledger into a tool-quality report.
pub(crate) fn run_metrics(home: Option<&Path>, session: Option<&str>, json: bool) {
    let Some(home) = home else {
        eprintln!("metrics: no home directory available");
        return;
    };
    let ledger = operations_observe::Ledger::in_home(home);
    let read = ledger.read();
    let kept: Vec<operations_observe::TurnSample> = read
        .samples
        .iter()
        .filter(|sample| session.is_none_or(|id| sample.session == id))
        .cloned()
        .collect();
    if read.malformed > 0 {
        eprintln!(
            "[warn] {} malformed ledger line(s) skipped in {}",
            read.malformed,
            ledger.path().display()
        );
    }
    let totals = metrics_totals(&kept);
    if json {
        match serde_json::to_string_pretty(&totals) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("metrics: serialization failed: {e}"),
        }
        return;
    }
    println!(
        "ledger {} ({} turn samples, {} session(s))",
        ledger.path().display(),
        kept.len(),
        kept.iter()
            .map(|sample| sample.session.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    );
    println!();
    println!("{}", metrics_report(&totals));
}

#[cfg(test)]
mod metrics_tests {
    use super::{metrics_report, metrics_totals};
    use operations_observe::{Ledger, Metrics, TurnSample};
    use wavecode_wire::{Event, EventMsg, ToolCallPreview, ToolOutcome};

    /// Metrics built the way the tap builds them — by folding events — so
    /// the read side is tested against the real fold, not a hand-built
    /// struct that could drift from it.
    fn folded(tool: &str, outcomes: &[(ToolOutcome, bool, u64)]) -> Metrics {
        let mut metrics = Metrics::new();
        for (index, (outcome, is_error, ms)) in outcomes.iter().enumerate() {
            let id = format!("c{index}");
            metrics.record(&Event {
                id: id.clone(),
                msg: EventMsg::ToolCallBegin {
                    call_id: id.clone(),
                    name: tool.to_string(),
                    input: serde_json::Value::Null,
                },
            });
            metrics.record(&Event {
                id: id.clone(),
                msg: EventMsg::ToolCallEnd {
                    call_id: id,
                    is_error: *is_error,
                    output: Some(ToolCallPreview::head("out", 16)),
                    outcome: *outcome,
                    duration_ms: *ms,
                },
            });
        }
        metrics
    }

    fn sample(model: &str, session: &str, metrics: Metrics) -> TurnSample {
        TurnSample {
            ts_secs: 0,
            session: session.to_string(),
            model: model.to_string(),
            metrics,
        }
    }

    const OK: ToolOutcome = ToolOutcome::Executed;
    const REFUSED: ToolOutcome = ToolOutcome::Refused;

    /// The table is the decision surface: the same tool ranked per model,
    /// with a refusal-heavy tool scored on its executed calls only.
    #[test]
    fn report_ranks_tools_per_model_and_ignores_refusals_in_rate() {
        let samples = vec![
            sample(
                "opus",
                "s1",
                folded("edit", &[(OK, true, 3), (OK, true, 4), (OK, false, 1)]),
            ),
            sample(
                "opus",
                "s1",
                folded("shell", &[(OK, false, 1200), (REFUSED, true, 0)]),
            ),
            sample("sonnet", "s2", folded("edit", &[(OK, false, 1)])),
        ];
        let report = metrics_report(&metrics_totals(&samples));
        assert!(report.starts_with("model: opus"), "{report}");
        let opus = report.split("model: sonnet").next().unwrap();
        // One of three executed edit calls landed.
        assert!(opus.contains("33.3%"), "{opus}");
        let shell = opus
            .lines()
            .find(|line| line.starts_with("  shell"))
            .unwrap();
        // One executed call that succeeded: 100%, and the refusal shows.
        assert!(shell.contains("100.0%"), "{shell}");
        assert!(shell.contains("1"), "{shell}");
        assert!(shell.contains("1.2s"), "{shell}");
        let sonnet = report.split("model: sonnet").nth(1).unwrap();
        assert!(sonnet.contains("edit"), "{sonnet}");
    }

    #[test]
    fn report_says_empty_rather_than_printing_nothing() {
        assert_eq!(
            metrics_report(&std::collections::BTreeMap::new()),
            "no metric samples recorded yet"
        );
        let turned = sample("opus", "s1", folded("read", &[]));
        let report = metrics_report(&metrics_totals(&[turned]));
        assert!(report.contains("no tool calls recorded"), "{report}");
    }

    /// Aggregation is keyed by the model on the sample, so a session that
    /// switched models mid-flight contributes to both rows.
    #[test]
    fn samples_merge_across_sessions_by_model() {
        let totals = metrics_totals(&[
            sample("opus", "a", folded("edit", &[(OK, false, 1)])),
            sample("opus", "b", folded("edit", &[(OK, true, 1)])),
        ]);
        let edit = &totals["opus"].tools["edit"];
        assert_eq!((edit.executed_ok, edit.executed_failed), (1, 1));
        assert_eq!(edit.success_rate(), Some(0.5));
    }

    /// What the tap writes is what the CLI reads: one round trip through
    /// the on-disk format, no shared fixture.
    #[test]
    fn ledger_round_trips_into_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::in_home(dir.path());
        ledger
            .append(&sample("opus", "s1", folded("read", &[(OK, false, 2)])))
            .unwrap();
        let read = ledger.read();
        assert_eq!(read.malformed, 0);
        let report = metrics_report(&metrics_totals(&read.samples));
        assert!(report.contains("model: opus"), "{report}");
        assert!(report.contains("read"), "{report}");
    }
}
