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
#[derive(Debug, Clone)]
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
            // The reservation is withdrawn so a later call reusing the id
            // parks fresh; late decisions then find no waiter and drop.
            Err(_) => {
                self.gate.cancel(call_id);
                ApprovalResolution::Deny {
                    reason: format!(
                        "approval request timed out after {}s; not executed",
                        self.timeout.as_secs()
                    ),
                }
            },
        }
    }

    fn clear_stale(&self) {
        self.gate.clear();
    }
}

/// Approval source selected at assembly time without trait objects.
///
/// Parking gates and headless denials share one concrete type so run
/// loops stay monomorphized; branching lives here, not in generics.
#[derive(Debug, Clone)]
pub enum Approvals {
    /// Park on a shared gate until users decide or the wait expires.
    Gate(GateApprovalSource),
    /// Deny openly for non-interactive drivers.
    Headless(HeadlessDeny),
}

#[async_trait::async_trait]
impl ApprovalSource for Approvals {
    async fn decide(&self, call_id: &str, kind: AskKind, detail: &str) -> ApprovalResolution {
        match self {
            Self::Gate(gate) => gate.decide(call_id, kind, detail).await,
            Self::Headless(deny) => deny.decide(call_id, kind, detail).await,
        }
    }

    fn clear_stale(&self) {
        match self {
            Self::Gate(gate) => gate.clear_stale(),
            Self::Headless(deny) => deny.clear_stale(),
        }
    }
}

/// Headless approvals: every request is denied explicitly.
///
/// Non-interactive drivers (exec, eval, CI) have nobody to answer the
/// prompt, so parking would hang forever. Denials carry the reason
/// openly instead of timing out, and the turn continues with an error
/// result the model can react to.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeadlessDeny;

#[async_trait::async_trait]
impl ApprovalSource for HeadlessDeny {
    async fn decide(&self, call_id: &str, _kind: AskKind, _detail: &str) -> ApprovalResolution {
        let _ = call_id;
        ApprovalResolution::Deny {
            reason: "non-interactive session: approval required but nobody can answer".to_string(),
        }
    }

    fn clear_stale(&self) {}
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
        let (source, gate) = source(Duration::from_millis(20));
        let resolution = source.decide("c1", AskKind::Write, "d").await;
        assert!(matches!(
            resolution,
            ApprovalResolution::Deny { reason } if reason.contains("timed out")
        ));
        // Expiry withdraws the reservation: the id parks fresh afterwards.
        assert_eq!(gate.pending_count(), 0);
        assert!(gate.wait_for("c1").is_ok());
        assert_eq!(gate.pending_count(), 1);
    }

    #[tokio::test]
    async fn headless_denies_openly_without_parking() {
        let outcome = HeadlessDeny.decide("c1", AskKind::Exec, "run ls").await;
        assert!(matches!(
            outcome,
            ApprovalResolution::Deny { reason } if reason.contains("non-interactive")
        ));
        HeadlessDeny.clear_stale();
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
