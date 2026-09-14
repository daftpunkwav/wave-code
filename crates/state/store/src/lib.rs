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

/// One content block of a history entry.
///
/// Text carries prose; ToolUse / ToolResult preserve the request-result
/// pairing across samples so providers see structured tool history
/// instead of flattened text.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Plain message text.
    Text(String),
    /// A tool call requested by the model.
    ToolUse {
        /// Identifier pairing the call with its future result.
        call_id: String,
        /// Tool name as registered in the capability registry.
        name: String,
        /// Raw JSON input for the tool.
        input: serde_json::Value,
    },
    /// A tool execution result fed back to the model.
    ToolResult {
        /// Identifier pairing the result with its call.
        call_id: String,
        /// Result payload text.
        content: String,
        /// True when the call failed.
        is_error: bool,
    },
}

/// One message in the persisted conversation history.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    /// Who produced this entry.
    pub role: Role,
    /// Ordered content blocks of the entry.
    pub blocks: Vec<Block>,
}

impl HistoryEntry {
    /// Legacy text view of the entry.
    ///
    /// Text blocks join with newlines; tool payloads render in the
    /// bracketed convention the text-only history used before blocks
    /// existed, so text-based consumers (resume import, token estimates)
    /// keep seeing the same shape.
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .map(|block| match block {
                Block::Text(text) => text.clone(),
                Block::ToolUse {
                    call_id,
                    name,
                    input,
                } => format!("[{call_id}] call {name} {input}"),
                Block::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => format!(
                    "[{call_id}] {}: {content}",
                    if *is_error { "error" } else { "ok" }
                ),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Text of all text blocks, joined with newlines.
    ///
    /// Unlike [`HistoryEntry::text`] this never renders tool payloads;
    /// use it where tool noise would corrupt prose (summaries, display).
    pub fn prose(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
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
        self.push_blocks(role, vec![Block::Text(text.into())]);
    }

    /// Append one entry from content blocks.
    pub fn push_blocks(&mut self, role: Role, blocks: Vec<Block>) {
        self.entries.push(HistoryEntry { role, blocks });
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
/// Rationale: some providers reject consecutive same-role messages. The
/// merged entry keeps every block in order, preserving tool pairing on
/// the wire while text blocks still read as one message.
pub fn normalize_history(entries: &[HistoryEntry]) -> Vec<HistoryEntry> {
    let mut out: Vec<HistoryEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        match out.last_mut() {
            Some(last) if last.role == entry.role => {
                last.blocks.extend(entry.blocks.iter().cloned());
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
                blocks: vec![Block::Text("a".to_string())],
            },
            HistoryEntry {
                role: Role::User,
                blocks: vec![Block::Text("b".to_string())],
            },
            HistoryEntry {
                role: Role::Assistant,
                blocks: vec![Block::Text("c".to_string())],
            },
        ];
        let normalized = normalize_history(&entries);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].text(), "a\nb");
        assert!(find_pairing_violations(&normalized).is_empty());
    }

    #[test]
    fn block_entries_render_legacy_text_views() {
        let entry = HistoryEntry {
            role: Role::User,
            blocks: vec![
                Block::Text("before".to_string()),
                Block::ToolResult {
                    call_id: "c1".to_string(),
                    content: "ls out".to_string(),
                    is_error: false,
                },
                Block::ToolResult {
                    call_id: "c2".to_string(),
                    content: "boom".to_string(),
                    is_error: true,
                },
                Block::ToolUse {
                    call_id: "c3".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
        };
        assert_eq!(
            entry.text(),
            "before\n[c1] ok: ls out\n[c2] error: boom\n[c3] call shell {\"command\":\"ls\"}"
        );
        assert_eq!(entry.prose(), "before");
    }

    #[test]
    fn push_blocks_preserves_pairing_in_snapshots() {
        let mut conv = Conversation::new();
        conv.push(Role::User, "list the files");
        conv.push_blocks(
            Role::Assistant,
            vec![Block::ToolUse {
                call_id: "c1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"command": "ls"}),
            }],
        );
        conv.push_blocks(
            Role::User,
            vec![Block::ToolResult {
                call_id: "c1".to_string(),
                content: "a.rs".to_string(),
                is_error: false,
            }],
        );
        let snap = conv.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(matches!(snap[1].blocks[0], Block::ToolUse { .. }));
        assert!(matches!(snap[2].blocks[0], Block::ToolResult { .. }));
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
