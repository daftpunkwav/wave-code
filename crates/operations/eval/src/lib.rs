/*!
 * @file BenchmarkRunner
 * @description Behavioural benchmarks over any turn driver.
 *
 * Responsibilities:
 * - Run scripted cases with fresh conversations each.
 * - Check history for expected content per case.
 * - Report pass rates without touching execution.
 *
 * This module must not depend on: concrete drivers, tools, or models.
 * Cases run through the TurnDriver seam with dropped events.
 */

//! Evaluation: scripted expectations against observable history.

use runtime_runner::{RunContext, StopReason, TurnDriver};
use state_store::Conversation;

/// One benchmark case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCase {
    /// Case name for reporting.
    pub name: String,
    /// Input submitted to the driver.
    pub input: String,
    /// Substrings that must all appear in the final history.
    pub must_contain: Vec<String>,
}

/// Outcome of one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalResult {
    /// Case name.
    pub name: String,
    /// True when every expectation held.
    pub passed: bool,
    /// Expectations missing from the final history.
    pub missing: Vec<String>,
}

/// Aggregate report over all cases.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalReport {
    /// Per-case outcomes in run order.
    pub results: Vec<EvalResult>,
}

impl EvalReport {
    /// Fraction of passing cases; 1.0 for an empty suite.
    pub fn pass_rate(&self) -> f64 {
        if self.results.is_empty() {
            return 1.0;
        }
        let passed = self.results.iter().filter(|r| r.passed).count();
        passed as f64 / self.results.len() as f64
    }

    /// Number of passing cases.
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }
}

/// Run every case with a fresh conversation and check expectations.
pub async fn evaluate<D: TurnDriver>(driver: &D, cases: &[EvalCase], system: &str) -> EvalReport {
    let mut results = Vec::with_capacity(cases.len());
    for (index, case) in cases.iter().enumerate() {
        let ctx = RunContext {
            run_id: format!("eval-{index}"),
            submission_id: format!("eval-{index}"),
            input: case.input.clone(),
        };
        let mut conv = Conversation::new();
        let outcome = driver
            .drive_turn(&ctx, &mut conv, &case.input, system, &|_| {})
            .await;
        let history = conv
            .snapshot()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let missing: Vec<String> = case
            .must_contain
            .iter()
            .filter(|want| !history.contains(want.as_str()))
            .cloned()
            .collect();
        // A failed turn fails the case even when text happens to match.
        let passed = missing.is_empty() && matches!(outcome, StopReason::Completed);
        results.push(EvalResult {
            name: case.name.clone(),
            passed,
            missing,
        });
    }
    EvalReport { results }
}

#[cfg(test)]
mod tests {
    use super::*;
    use operations_wire::Event;
    use runtime_runner::{HookPoint, TurnDriver};
    use state_store::{CompactTrigger, Role};

    /// Stub driver echoing the input back as the assistant message.
    struct EchoDriver;

    #[async_trait::async_trait]
    impl TurnDriver for EchoDriver {
        async fn drive_turn(
            &self,
            _ctx: &RunContext,
            conv: &mut Conversation,
            input: &str,
            _system: &str,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> StopReason {
            conv.push(Role::User, input);
            conv.push(Role::Assistant, format!("echo:{input}"));
            StopReason::Completed
        }

        async fn drive_compact(
            &self,
            _conv: &mut Conversation,
            _trigger: CompactTrigger,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> Result<(), String> {
            Ok(())
        }

        async fn drive_hook(
            &self,
            _point: HookPoint,
            _payload: &str,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn matching_cases_pass_and_others_fail() {
        let report = evaluate(
            &EchoDriver,
            &[
                EvalCase {
                    name: "echo".to_string(),
                    input: "hello".to_string(),
                    must_contain: vec!["echo:hello".to_string()],
                },
                EvalCase {
                    name: "missing".to_string(),
                    input: "hello".to_string(),
                    must_contain: vec!["never-appears".to_string()],
                },
            ],
            "sys",
        )
        .await;
        assert_eq!(report.passed(), 1);
        assert_eq!(report.pass_rate(), 0.5);
        assert!(report.results[0].passed);
        assert!(!report.results[1].passed);
        assert_eq!(report.results[1].missing, vec!["never-appears".to_string()]);
    }

    #[test]
    fn empty_suites_score_perfectly() {
        assert_eq!(
            EvalReport {
                results: Vec::new()
            }
            .pass_rate(),
            1.0
        );
    }
}
