/*!
 * @file ApprovalGate
 * @description Policy verdicts and the approval gate.
 *
 * Responsibilities:
 * - Decide allow/ask/deny inputs for the run loop (pure policy data).
 * - Park approval requests keyed by call id with one-shot delivery.
 *
 * This module must not depend on: runtime, state, action, operations,
 * transport, or any orchestration layer. Permission modes are wire
 * vocabulary owned by `wavecode-protocol`; this crate never redefines
 * them.
 */

//! Pure-policy safety primitives with a race-free approval gate.
//!
//! OS-level isolation (landlock, seatbelt, ACLs) is out of scope here; this
//! crate decides policy only. Locks use the single-operation poison recovery
//! rule: a poisoned mutex yields its inner guard because critical sections
//! never leave half-written invariants behind.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

/// What kind of approval an ask verdict requests.
///
/// Deprecated: this duplicates the canonical protocol types without
/// adding methods or serialization. Use `wavecode_wire::ApprovalKind`
/// (new harness stack) or `wavecode_protocol::ApprovalKind` (legacy
/// stack) instead; this alias disappears in a later breaking-change
/// window.
#[deprecated(note = "use wavecode_wire::ApprovalKind or wavecode_protocol::ApprovalKind instead")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// Arbitrary command execution.
    Exec,
    /// File modification.
    Write,
}

/// User decision delivered through the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Approve this call only.
    AllowOnce,
    /// Approve this call and remember the approval for the session.
    AllowAlways,
    /// Refuse with a reason surfaced back to the model.
    Deny {
        /// Human-readable refusal reason.
        reason: String,
    },
}

/// Errors raised by the approval gate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    /// Another waiter already parked the same call id.
    #[error("duplicate approval waiter: {0}")]
    DuplicateWaiter(String),
}

/// Parks approval requests keyed by tool call id.
///
/// Each call id admits exactly one waiter. Decisions are one-shot: the first
/// `decide` takes the slot, late decisions return false and are dropped so a
/// stale UI click can never approve a future call reusing the id.
#[derive(Debug, Default)]
pub struct ApprovalGate {
    pending: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
}

impl ApprovalGate {
    /// Create an empty gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Recover the mutex guard after a poison; see crate docs for why.
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<ApprovalDecision>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Park the current run, awaiting a decision for `call_id`.
    pub fn wait_for(
        &self,
        call_id: &str,
    ) -> Result<oneshot::Receiver<ApprovalDecision>, GateError> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.lock();
        if pending.contains_key(call_id) {
            return Err(GateError::DuplicateWaiter(call_id.to_string()));
        }
        pending.insert(call_id.to_string(), tx);
        Ok(rx)
    }

    /// Deliver a decision; false when no waiter exists (late or unknown id).
    pub fn decide(&self, call_id: &str, decision: ApprovalDecision) -> bool {
        let sender = self.lock().remove(call_id);
        match sender {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Withdraw one parked waiter without deciding; true when anything
    /// was parked.
    ///
    /// Expiry paths call this so a timed-out wait stops reserving its
    /// call id: without it, a later call reusing the id fails `wait_for`
    /// with `DuplicateWaiter` and is spuriously denied. Dropping the
    /// sender also releases any still-held receiver instead of leaving
    /// it parked on a decision that will never arrive.
    pub fn cancel(&self, call_id: &str) -> bool {
        self.lock().remove(call_id).is_some()
    }

    /// Drop all parked waiters, e.g. on run teardown.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Number of currently parked waiters.
    pub fn pending_count(&self) -> usize {
        self.lock().len()
    }
}

/// Parks interactive questions keyed by tool call id.
///
/// Mirrors [`ApprovalGate`] with free-text answers instead of decisions:
/// each call id admits exactly one waiter, the first `answer` takes the
/// slot, and late answers return false so a stale UI submission can
/// never answer a future call reusing the id.
#[derive(Debug, Default)]
pub struct QuestionGate {
    pending: Mutex<HashMap<String, oneshot::Sender<String>>>,
}

impl QuestionGate {
    /// Create an empty gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Recover the mutex guard after a poison; see crate docs for why.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<String>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Park the current run, awaiting an answer for `call_id`.
    pub fn wait_for(&self, call_id: &str) -> Result<oneshot::Receiver<String>, GateError> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.lock();
        if pending.contains_key(call_id) {
            return Err(GateError::DuplicateWaiter(call_id.to_string()));
        }
        pending.insert(call_id.to_string(), tx);
        Ok(rx)
    }

    /// Deliver an answer; false when no waiter exists (late or unknown id).
    pub fn answer(&self, call_id: &str, answer: String) -> bool {
        let sender = self.lock().remove(call_id);
        match sender {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// Withdraw one parked waiter without answering; true when anything
    /// was parked (expiry paths, mirroring [`ApprovalGate::cancel`]).
    pub fn cancel(&self, call_id: &str) -> bool {
        self.lock().remove(call_id).is_some()
    }

    /// Drop all parked waiters, e.g. on run teardown.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Number of currently parked waiters.
    pub fn pending_count(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn decision_is_one_shot_and_late_decisions_drop() {
        let gate = ApprovalGate::new();
        let rx = gate.wait_for("call-1").unwrap();
        assert!(gate.decide("call-1", ApprovalDecision::AllowOnce));
        assert_eq!(rx.await.unwrap(), ApprovalDecision::AllowOnce);
        // Late decision for a consumed id is dropped, never stored.
        assert!(!gate.decide("call-1", ApprovalDecision::AllowOnce));
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn duplicate_waiters_are_rejected() {
        let gate = ApprovalGate::new();
        let _rx = gate.wait_for("call-1").unwrap();
        let err = gate.wait_for("call-1").unwrap_err();
        assert_eq!(err, GateError::DuplicateWaiter("call-1".to_string()));
    }

    #[tokio::test]
    async fn cancelled_waits_free_their_call_id() {
        let gate = ApprovalGate::new();
        let rx = gate.wait_for("call-1").unwrap();
        assert_eq!(gate.pending_count(), 1);
        assert!(gate.cancel("call-1"));
        assert_eq!(gate.pending_count(), 0);
        // Withdrawing twice withdraws nothing; the id parks fresh.
        assert!(!gate.cancel("call-1"));
        assert!(!gate.cancel("ghost"));
        let _fresh = gate.wait_for("call-1").unwrap();
        // The withdrawn receiver resolves as dropped, never as approved.
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn question_answers_are_one_shot_and_late_answers_drop() {
        let gate = QuestionGate::new();
        let rx = gate.wait_for("call-1").unwrap();
        assert_eq!(gate.pending_count(), 1);
        assert!(gate.answer("call-1", "option two".to_string()));
        assert_eq!(rx.await.unwrap(), "option two");
        // Late answers for a consumed id are dropped, never stored.
        assert!(!gate.answer("call-1", "late".to_string()));
        assert_eq!(gate.pending_count(), 0);
        assert!(!gate.cancel("ghost"));
    }

    #[tokio::test]
    async fn question_cancelled_waits_free_their_call_id() {
        let gate = QuestionGate::new();
        let rx = gate.wait_for("call-1").unwrap();
        assert!(gate.cancel("call-1"));
        // The withdrawn receiver resolves as dropped, never answered.
        assert!(rx.await.is_err());
        let _fresh = gate.wait_for("call-1").unwrap();
    }
}
