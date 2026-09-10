/*!
 * @file AuditLog
 * @description Append-only audit trail of security-relevant decisions.
 *
 * Responsibilities:
 * - Record every allow/ask/deny/error decision with a sequence number.
 * - Filter entries by actor for incident review.
 * - Keep the trail in memory in emission order.
 *
 * This module must not depend on: any other workspace crate. Durable
 * export (files, remote sinks) belongs to a future appender.
 */

//! Audit: who did what to which tool, and what policy said.

/// Verdict recorded for one audited action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditVerdict {
    /// Executed without asking.
    Allow,
    /// Asked the user first.
    Ask,
    /// Refused.
    Deny,
    /// Failed during or after the decision.
    Error,
}

/// One audit entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// Monotonic sequence number starting at 1.
    pub seq: u64,
    /// Who acted: user id, session id, or subsystem name.
    pub actor: String,
    /// What was attempted: tool name or operation.
    pub action: String,
    /// What it targeted: path, command summary, or call id.
    pub target: String,
    /// What policy decided.
    pub verdict: AuditVerdict,
}

/// Append-only audit log.
#[derive(Debug, Default)]
pub struct AuditLog {
    events: Vec<AuditEvent>,
}

impl AuditLog {
    /// Create an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one entry, returning its sequence number.
    pub fn append(
        &mut self,
        actor: impl Into<String>,
        action: impl Into<String>,
        target: impl Into<String>,
        verdict: AuditVerdict,
    ) -> u64 {
        let seq = self.events.len() as u64 + 1;
        self.events.push(AuditEvent {
            seq,
            actor: actor.into(),
            action: action.into(),
            target: target.into(),
            verdict,
        });
        seq
    }

    /// Entries by one actor in emission order.
    pub fn by_actor(&self, actor: &str) -> Vec<&AuditEvent> {
        self.events.iter().filter(|e| e.actor == actor).collect()
    }

    /// Every entry in emission order.
    pub fn all(&self) -> &[AuditEvent] {
        &self.events
    }

    /// Number of recorded entries.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// True when nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_grow_and_filters_hold_order() {
        let mut log = AuditLog::new();
        log.append("alice", "shell", "ls", AuditVerdict::Allow);
        log.append("bob", "write_file", "a.txt", AuditVerdict::Deny);
        log.append("alice", "read_file", "a.txt", AuditVerdict::Allow);
        assert_eq!(log.len(), 3);
        let alice: Vec<_> = log.by_actor("alice").iter().map(|e| e.seq).collect();
        assert_eq!(alice, vec![1, 3]);
        assert!(log.by_actor("nobody").is_empty());
    }
}
