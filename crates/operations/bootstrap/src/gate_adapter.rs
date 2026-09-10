/*!
 * @file GateApprovalSource
 * @description Parks approval waits on the legacy approval gate with timeout.
 *
 * Responsibilities:
 * - Park the loop until a user decision arrives or the wait expires.
 * - Resolve expired waits to Deny so the loop never parks forever.
 * - Clear stale waiters at turn start.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::ApprovalSource`] implemented over `safety-gate`.

use std::sync::Arc;
use std::time::Duration;

use runtime_runner::{ApprovalResolution, ApprovalSource, AskKind};
use safety_gate::{ApprovalDecision, ApprovalGate};

/// Approval waits backed by a shared gate with a fixed timeout.
pub struct GateApprovalSource {
    gate: Arc<ApprovalGate>,
    timeout: Duration,
}

impl GateApprovalSource {
    /// Wrap a shared gate; waits longer than `timeout` resolve to Deny.
    pub fn new(gate: Arc<ApprovalGate>, timeout: Duration) -> Self {
        Self { gate, timeout }
    }
}

#[async_trait::async_trait]
impl ApprovalSource for GateApprovalSource {
    async fn decide(&self, call_id: &str, _kind: AskKind, _detail: &str) -> ApprovalResolution {
        let waiter = match self.gate.wait_for(call_id) {
            Ok(rx) => rx,
            Err(_) => {
                // Another waiter already parked this id; treat as denied
                // rather than stacking a second park on one decision.
                return ApprovalResolution::Deny {
                    reason: "duplicate approval wait".to_string(),
                };
            }
        };
        match tokio::time::timeout(self.timeout, waiter).await {
            Ok(Ok(ApprovalDecision::AllowOnce)) => ApprovalResolution::AllowOnce,
            Ok(Ok(ApprovalDecision::AllowAlways)) => ApprovalResolution::AllowAlways,
            Ok(Ok(ApprovalDecision::Deny { reason })) => ApprovalResolution::Deny { reason },
            // The waiter was dropped (gate cleared mid-wait): deny instead
            // of hanging, and let the stale waiter stay dropped.
            Ok(Err(_)) => ApprovalResolution::Deny {
                reason: "approval gate cleared while waiting".to_string(),
            },
            // Expired waits deny with an explicit reason; the tool never
            // executes and the turn continues instead of parking forever.
            // Late decisions find no waiter and are dropped by the gate.
            Err(_) => ApprovalResolution::Deny {
                reason: format!(
                    "approval request timed out after {}s; not executed",
                    self.timeout.as_secs()
                ),
            },
        }
    }

    fn clear_stale(&self) {
        self.gate.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(timeout: Duration) -> (GateApprovalSource, Arc<ApprovalGate>) {
        let gate = Arc::new(ApprovalGate::new());
        (GateApprovalSource::new(gate.clone(), timeout), gate)
    }

    #[tokio::test]
    async fn delivered_decisions_unblock_the_wait() {
        let (source, gate) = source(Duration::from_secs(5));
        let gate_clone = gate.clone();
        let driver = tokio::spawn(async move { source.decide("c1", AskKind::Exec, "d").await });
        // Let the waiter park before delivering.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(gate_clone.decide("c1", ApprovalDecision::AllowOnce));
        assert_eq!(driver.await.unwrap(), ApprovalResolution::AllowOnce);
    }

    #[tokio::test]
    async fn expired_waits_deny_and_never_execute() {
        let (source, _gate) = source(Duration::from_millis(20));
        let resolution = source.decide("c1", AskKind::Write, "d").await;
        assert!(matches!(
            resolution,
            ApprovalResolution::Deny { reason } if reason.contains("timed out")
        ));
    }

    #[tokio::test]
    async fn cleared_gates_deny_instead_of_hanging() {
        let (source, gate) = source(Duration::from_secs(5));
        let driver = tokio::spawn(async move { source.decide("c1", AskKind::Exec, "d").await });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        gate.clear();
        assert!(matches!(
            driver.await.unwrap(),
            ApprovalResolution::Deny { .. }
        ));
    }
}
