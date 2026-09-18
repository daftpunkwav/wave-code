/*!
 * @file GateApprovalSource
 * @description Parks approval and question waits on the safety gates with timeout.
 *
 * Responsibilities:
 * - Park the loop until a user decision arrives or the wait expires.
 * - Park interactive questions until the user answers or the wait expires.
 * - End a parked wait immediately as Interrupted when the session
 *   interrupt fires (reservation withdrawn, so the id can park fresh).
 * - Resolve expired waits to Deny/Unavailable so the loop never parks forever.
 * - Expose clear_stale for session-owned turn starts (the run loop
 *   skips it for child turns, so a child start never wipes a parent
 *   approval parked mid-wait).
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::ApprovalSource`] implemented over `safety-gate`.

use std::sync::Arc;
use std::time::Duration;

use infrastructure_base::InterruptHandle;
use runtime_runner::{ApprovalResolution, ApprovalSource, AskKind, QuestionResolution};
use safety_gate::{ApprovalDecision, ApprovalGate, QuestionGate};

/// Approval waits backed by a shared gate with a fixed timeout.
#[derive(Debug, Clone)]
pub struct GateApprovalSource {
    gate: Arc<ApprovalGate>,
    questions: Arc<QuestionGate>,
    timeout: Duration,
    interrupt: InterruptHandle,
}

impl GateApprovalSource {
    /// Poll granularity while parked: a user interrupt ends the wait
    /// within one slice instead of at the full timeout.
    const INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

    /// Wrap shared gates; waits longer than `timeout` resolve to Deny /
    /// Unavailable instead of parking forever, and a session interrupt
    /// ends them immediately as `Interrupted`.
    pub fn new(
        gate: Arc<ApprovalGate>,
        questions: Arc<QuestionGate>,
        timeout: Duration,
        interrupt: InterruptHandle,
    ) -> Self {
        Self {
            gate,
            questions,
            timeout,
            interrupt,
        }
    }

    /// Park on `waiter` until answered, interrupted, or expired.
    ///
    /// The interrupt check rides short timeout slices: a mid-wait Ctrl+C
    /// resolves to `Interrupted` (with the gate reservation withdrawn, so
    /// a later turn can park fresh) instead of holding the turn hostage
    /// for the full timeout.
    async fn park<T>(&self, waiter: &mut T) -> Result<T::Output, ParkExit>
    where
        T: std::future::Future + Unpin,
    {
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            if self.interrupt.is_triggered() {
                return Err(ParkExit::Interrupted);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ParkExit::Expired);
            }
            match tokio::time::timeout(remaining.min(Self::INTERRUPT_POLL_INTERVAL), &mut *waiter)
                .await
            {
                Ok(answer) => return Ok(answer),
                Err(_elapsed) => continue,
            }
        }
    }
}

/// Why a park ended without an answer.
enum ParkExit {
    /// The session interrupt fired mid-wait.
    Interrupted,
    /// The fixed timeout elapsed with no decision.
    Expired,
}

#[async_trait::async_trait]
impl ApprovalSource for GateApprovalSource {
    async fn decide(&self, call_id: &str, _kind: AskKind, _detail: &str) -> ApprovalResolution {
        let mut waiter = match self.gate.wait_for(call_id) {
            Ok(rx) => rx,
            Err(_) => {
                // Another waiter already parked this id; treat as denied
                // rather than stacking a second park on one decision.
                return ApprovalResolution::Deny {
                    reason: "duplicate approval wait".to_string(),
                };
            }
        };
        match self.park(&mut waiter).await {
            Ok(Ok(ApprovalDecision::AllowOnce)) => ApprovalResolution::AllowOnce,
            Ok(Ok(ApprovalDecision::AllowAlways)) => ApprovalResolution::AllowAlways,
            Ok(Ok(ApprovalDecision::Deny { reason })) => ApprovalResolution::Deny { reason },
            // The waiter was dropped (gate cleared mid-wait): deny instead
            // of hanging, and let the stale waiter stay dropped.
            Ok(Err(_)) => ApprovalResolution::Deny {
                reason: "approval gate cleared while waiting".to_string(),
            },
            // Interrupted waits stay cancelable: the reservation is
            // withdrawn so a later call reusing the id parks fresh.
            Err(ParkExit::Interrupted) => {
                self.gate.cancel(call_id);
                ApprovalResolution::Interrupted
            }
            // Expired waits deny with an explicit reason; the tool never
            // executes and the turn continues instead of parking forever.
            // The reservation is withdrawn so a later call reusing the id
            // parks fresh; late decisions then find no waiter and drop.
            Err(ParkExit::Expired) => {
                self.gate.cancel(call_id);
                ApprovalResolution::Deny {
                    reason: format!(
                        "approval request timed out after {}s; not executed",
                        self.timeout.as_secs()
                    ),
                }
            }
        }
    }

    async fn ask(&self, call_id: &str, _question: &str, _options: &[String]) -> QuestionResolution {
        let mut waiter = match self.questions.wait_for(call_id) {
            Ok(rx) => rx,
            Err(_) => {
                return QuestionResolution::Unavailable {
                    reason: "duplicate question wait".to_string(),
                };
            }
        };
        match self.park(&mut waiter).await {
            Ok(Ok(answer)) => QuestionResolution::Answered(answer),
            // The waiter was dropped (gate cleared mid-wait): fail the
            // question instead of hanging.
            Ok(Err(_)) => QuestionResolution::Unavailable {
                reason: "question gate cleared while waiting".to_string(),
            },
            Err(ParkExit::Interrupted) => {
                self.questions.cancel(call_id);
                QuestionResolution::Interrupted
            }
            // Expired waits fail with an explicit reason; the reservation
            // is withdrawn so a later call reusing the id parks fresh.
            Err(ParkExit::Expired) => {
                self.questions.cancel(call_id);
                QuestionResolution::Unavailable {
                    reason: format!(
                        "question timed out after {}s with no answer",
                        self.timeout.as_secs()
                    ),
                }
            }
        }
    }

    fn clear_stale(&self) {
        self.gate.clear();
        self.questions.clear();
    }
}

impl GateApprovalSource {
    /// Deliver a user answer through the question gate; false when no
    /// waiter exists (late or unknown id).
    pub fn answer_question(&self, call_id: &str, answer: String) -> bool {
        self.questions.answer(call_id, answer)
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

    async fn ask(&self, call_id: &str, question: &str, options: &[String]) -> QuestionResolution {
        match self {
            Self::Gate(gate) => gate.ask(call_id, question, options).await,
            Self::Headless(deny) => deny.ask(call_id, question, options).await,
        }
    }

    fn clear_stale(&self) {
        match self {
            Self::Gate(gate) => gate.clear_stale(),
            Self::Headless(deny) => deny.clear_stale(),
        }
    }
}

impl Approvals {
    /// Deliver a user answer through the question gate; false when no
    /// waiter exists (late or unknown id).
    pub fn answer_question(&self, call_id: &str, answer: String) -> bool {
        match self {
            Self::Gate(gate) => gate.answer_question(call_id, answer),
            Self::Headless(_) => false,
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
        let questions = Arc::new(QuestionGate::new());
        (
            GateApprovalSource::new(
                gate.clone(),
                questions,
                timeout,
                infrastructure_base::InterruptHandle::new(),
            ),
            gate,
        )
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
    async fn questions_park_until_answered_and_expire_openly() {
        let gate = Arc::new(ApprovalGate::new());
        let questions = Arc::new(QuestionGate::new());
        let source = GateApprovalSource::new(
            gate,
            questions.clone(),
            Duration::from_secs(5),
            infrastructure_base::InterruptHandle::new(),
        );
        let driver = tokio::spawn(async move {
            source
                .ask("c1", "pick one", &["a".to_string(), "b".to_string()])
                .await
        });
        // Let the waiter park before answering.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(questions.answer("c1", "b".to_string()));
        assert_eq!(
            driver.await.unwrap(),
            QuestionResolution::Answered("b".to_string())
        );
        // Expired waits fail openly and free their id.
        let gate = Arc::new(ApprovalGate::new());
        let questions = Arc::new(QuestionGate::new());
        let source = GateApprovalSource::new(
            gate,
            questions.clone(),
            Duration::from_millis(20),
            infrastructure_base::InterruptHandle::new(),
        );
        let outcome = source.ask("c1", "pick one", &[]).await;
        assert!(matches!(
            outcome,
            QuestionResolution::Unavailable { reason } if reason.contains("timed out")
        ));
        assert_eq!(questions.pending_count(), 0);
        // Headless drivers answer nobody: the trait default applies.
        assert!(matches!(
            HeadlessDeny.ask("c1", "pick one", &[]).await,
            QuestionResolution::Unavailable { .. }
        ));
    }

    #[tokio::test]
    async fn session_interrupt_ends_a_parked_wait_as_interrupted() {
        let gate = Arc::new(ApprovalGate::new());
        let questions = Arc::new(QuestionGate::new());
        let interrupt = infrastructure_base::InterruptHandle::new();
        let source = GateApprovalSource::new(
            gate.clone(),
            questions,
            Duration::from_secs(30),
            interrupt.clone(),
        );
        let driver = tokio::spawn(async move { source.decide("c1", AskKind::Exec, "d").await });
        // Park the waiter, then interrupt the session mid-wait.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        interrupt.trigger();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), driver)
                .await
                .expect("interrupt must end the wait quickly")
                .unwrap(),
            ApprovalResolution::Interrupted
        );
        // The reservation is withdrawn: the id can park fresh afterwards.
        assert_eq!(gate.pending_count(), 0);
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
