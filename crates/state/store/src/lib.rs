/*!
 * @file ConversationStore
 * @description Persisted conversation history and context budget checks.
 *
 * Responsibilities:
 * - Own the append-only message history behind a single write entry.
 * - Track cross-run token usage carried over after each sample.
 * - Evaluate three-level context budgets and normalize history pairing.
 *
 * This module must not depend on: runtime, action, safety, operations,
 * transport, or any orchestration layer.
 */

//! Conversation history with copy-on-write snapshots and budget levels.
//!
//! Readers receive an `Arc` snapshot so sampling can proceed while the run
//! loop appends new entries; the snapshot stays frozen by construction.

use std::sync::Arc;

/// Remaining-token threshold that emits a one-time warning.
pub const BUDGET_WARN_REMAINING: u64 = 20_000;
/// Remaining-token threshold that triggers automatic compaction.
pub const BUDGET_AUTO_COMPACT_REMAINING: u64 = 13_000;
/// Remaining-token threshold that blocks sampling until compaction.
pub const BUDGET_BLOCKING_REMAINING: u64 = 3_000;

/// Author of one history entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Human or tool-result content.
    User,
    /// Model content.
    Assistant,
}

/// One message in the persisted conversation history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Who produced this entry.
    pub role: Role,
    /// Text payload of the entry.
    pub text: String,
}

/// Token usage carried across samples and runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens of the latest sample.
    pub input_tokens: u64,
    /// Cumulative output tokens.
    pub output_tokens: u64,
    /// Cumulative prompt-cache read tokens (0 when the provider reports no
    /// cache accounting).
    pub cache_read_tokens: u64,
    /// Cumulative prompt-cache write tokens (0 when the provider reports no
    /// cache accounting).
    pub cache_creation_tokens: u64,
}

/// Budget level evaluated from remaining context tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLevel {
    /// Plenty of room; proceed normally.
    Ok,
    /// Room is shrinking; emit a one-time warning.
    Warn,
    /// Room is low; compact automatically before sampling.
    AutoCompact,
    /// Room is exhausted; block sampling until compaction completes.
    Blocking,
}

/// Trigger that caused a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    /// Automatic compaction from the budget check.
    Auto,
    /// Blocking compaction required before any further sampling.
    Blocking,
    /// Reactive compaction after a prompt-too-long sampling error.
    Reactive,
    /// Explicit user request.
    Manual,
}

/// Evaluate the budget level from remaining context tokens.
pub fn check_budget(remaining: u64) -> BudgetLevel {
    if remaining <= BUDGET_BLOCKING_REMAINING {
        BudgetLevel::Blocking
    } else if remaining <= BUDGET_AUTO_COMPACT_REMAINING {
        BudgetLevel::AutoCompact
    } else if remaining <= BUDGET_WARN_REMAINING {
        BudgetLevel::Warn
    } else {
        BudgetLevel::Ok
    }
}

/// Fallback context overhead added to estimates.
///
/// Covers the system prompt and framing the estimator cannot see. Usage
/// figures reported by the provider always take precedence; this constant
/// only shapes the no-usage-yet fallback path.
pub const CONTEXT_OVERHEAD_TOKENS: u64 = 2000;

/// Rough token estimate for text, fallback only.
///
/// Character-division estimates undercount dense scripts by about two times;
/// authoritative provider usage must always win over this function.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// Append-only conversation with frozen snapshots.
///
/// All mutations flow through [`Conversation::push`]; readers hold an `Arc`
/// snapshot that never changes under them.
#[derive(Debug, Clone, Default)]
pub struct Conversation {
    entries: Vec<HistoryEntry>,
    usage_carry: Usage,
}

impl Conversation {
    /// Create an empty conversation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one entry; the single write entry of this store.
    pub fn push(&mut self, role: Role, text: impl Into<String>) {
        self.entries.push(HistoryEntry {
            role,
            text: text.into(),
        });
    }

    /// Number of persisted entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entries have been persisted yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Frozen snapshot of the current history for sampling.
    pub fn snapshot(&self) -> Arc<Vec<HistoryEntry>> {
        Arc::new(self.entries.clone())
    }

    /// Replace the whole history, e.g. with a compaction summary.
    ///
    /// The caller owns re-establishing the usage carry via [`Conversation::settle`].
    pub fn replace(&mut self, entries: Vec<HistoryEntry>) {
        self.entries = entries;
    }

    /// Usage carried over from the latest settled sample.
    pub fn usage_carry(&self) -> Usage {
        self.usage_carry
    }

    /// Settle usage after a sample; every sampling exit must call this.
    pub fn settle(&mut self, usage: Usage) {
        self.usage_carry = usage;
    }
}

/// Merge adjacent entries from the same role.
///
/// Rationale: some providers reject consecutive same-role messages. Merging
/// with a newline keeps the wire payload valid without losing content.
pub fn normalize_history(entries: &[HistoryEntry]) -> Vec<HistoryEntry> {
    let mut out: Vec<HistoryEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        match out.last_mut() {
            Some(last) if last.role == entry.role => {
                last.text.push('\n');
                last.text.push_str(&entry.text);
            }
            _ => out.push(entry.clone()),
        }
    }
    out
}

/// Detect pairing violations: entries that would break role alternation.
pub fn find_pairing_violations(entries: &[HistoryEntry]) -> Vec<usize> {
    let mut bad = Vec::new();
    for (i, window) in entries.windows(2).enumerate() {
        if window[0].role == window[1].role {
            bad.push(i + 1);
        }
    }
    bad
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_stay_frozen_after_further_pushes() {
        let mut conv = Conversation::new();
        conv.push(Role::User, "hello");
        let snap = conv.snapshot();
        conv.push(Role::Assistant, "hi");
        assert_eq!(snap.len(), 1);
        assert_eq!(conv.len(), 2);
    }

    #[test]
    fn normalize_merges_adjacent_same_role_entries() {
        let entries = vec![
            HistoryEntry {
                role: Role::User,
                text: "a".to_string(),
            },
            HistoryEntry {
                role: Role::User,
                text: "b".to_string(),
            },
            HistoryEntry {
                role: Role::Assistant,
                text: "c".to_string(),
            },
        ];
        let normalized = normalize_history(&entries);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].text, "a\nb");
        assert!(find_pairing_violations(&normalized).is_empty());
    }

    #[test]
    fn budget_levels_follow_remaining_thresholds() {
        assert_eq!(check_budget(u64::MAX), BudgetLevel::Ok);
        assert_eq!(check_budget(BUDGET_WARN_REMAINING), BudgetLevel::Warn);
        assert_eq!(
            check_budget(BUDGET_AUTO_COMPACT_REMAINING),
            BudgetLevel::AutoCompact
        );
        assert_eq!(
            check_budget(BUDGET_BLOCKING_REMAINING),
            BudgetLevel::Blocking
        );
        assert_eq!(check_budget(0), BudgetLevel::Blocking);
    }

    #[test]
    fn settle_carries_usage_to_the_next_sample() {
        let mut conv = Conversation::new();
        conv.settle(Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        });
        assert_eq!(conv.usage_carry().input_tokens, 100);
    }
}
