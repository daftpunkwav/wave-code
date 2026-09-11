/*!
 * @file ApprovalGate
 * @description Permission modes, policy verdicts, and the approval gate.
 *
 * Responsibilities:
 * - Own the four permission modes with frozen wire names.
 * - Decide allow/ask/deny inputs for the run loop (pure policy data).
 * - Park approval requests keyed by call id with one-shot delivery.
 *
 * This module must not depend on: runtime, state, action, operations,
 * transport, or any orchestration layer.
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

/// Permission mode selected for a session.
///
/// Wire names are frozen (`camelCase`) so persisted configs keep parsing.
/// Renaming these variants is a breaking protocol change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Ask for anything sensitive; the default.
    Default,
    /// Propose plans without executing them.
    Plan,
    /// Auto-approve edits, ask for command execution.
    AcceptEdits,
    /// Bypass all approval prompts (explicit operator opt-in only).
    BypassPermissions,
}

impl PermissionMode {
    /// Canonical wire string of the mode.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// Parse a wire string back into a mode.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "plan" => Some(Self::Plan),
            "acceptEdits" => Some(Self::AcceptEdits),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of approval an ask verdict requests.
///
/// Deprecated: this duplicates the canonical protocol types without
/// adding methods or serialization. Use `operations_wire::ApprovalKind`
/// (new harness stack) or `wavecode_protocol::ApprovalKind` (legacy
/// stack) instead; this alias disappears in a later breaking-change
/// window.
#[deprecated(note = "use operations_wire::ApprovalKind or wavecode_protocol::ApprovalKind instead")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_stay_frozen() {
        assert_eq!(PermissionMode::Default.as_str(), "default");
        assert_eq!(PermissionMode::Plan.as_str(), "plan");
        assert_eq!(PermissionMode::AcceptEdits.as_str(), "acceptEdits");
        assert_eq!(
            PermissionMode::BypassPermissions.as_str(),
            "bypassPermissions"
        );
        assert_eq!(PermissionMode::parse("plan"), Some(PermissionMode::Plan));
        assert_eq!(PermissionMode::parse("unknown"), None);
    }

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
}
