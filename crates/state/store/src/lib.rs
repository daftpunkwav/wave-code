/*!
 * @file ConversationStore
 * @description Persisted conversation history and context budget checks.
 *
 * Responsibilities:
 * - Own the append-only message history behind a single write entry.
 * - Report every mutation to an optional durability sink (no push may bypass
 *   the journal by forgetting a second write).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
        /// Wall-clock time the result was produced, as seconds since the
        /// Unix epoch. Absent for results produced before the field
        /// existed (old journals keep loading) and for synthetic closing
        /// results whose real production time is unknowable. The serde
        /// attributes keep un-stamped results byte-identical to the old
        /// record shape on disk.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        produced_at: Option<u64>,
    },
    /// Inline image attachment (base64; providers validate mime and size
    /// at the translation layer).
    Image {
        /// Optional client-side label, preserved but never sent to providers.
        id: Option<String>,
        /// MIME type of the image.
        mime: String,
        /// Base64-encoded image bytes.
        base64: String,
    },
    /// Reasoning the model emitted alongside its turn (Anthropic extended
    /// thinking and compatible reasoning streams).
    ///
    /// Kept in history because the Anthropic wire requires it back: a
    /// tool-call turn whose thinking is missing is rejected outright, so
    /// multi-round tool use needs these blocks preserved. Providers with no
    /// thinking wire shape drop the block at translation. It is deliberately
    /// invisible to [`HistoryEntry::text`] so token estimates, compaction
    /// and resume keep their prose-only view.
    Thinking {
        /// Reasoning text as streamed by the model.
        text: String,
        /// Provider signature binding the text to its turn (Anthropic
        /// `signature`). `None` for endpoints that stream unsigned
        /// thinking; Claude models reject unsigned blocks, so translation
        /// emits those only for other models.
        signature: Option<String>,
    },
}

/// One message in the persisted conversation history.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// keep seeing the same shape. A stamped tool result opens with a
    /// compact wall-clock header (`[YYYY-MM-DD HH:MM UTC]`) ahead of the
    /// legacy shape; unstamped results render byte-identical to before.
    /// Thinking blocks render nothing: they are
    /// wire-round-trip state, not prose, and folding per-turn reasoning
    /// into estimates or resumed transcripts would inflate both.
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.clone()),
                Block::ToolUse {
                    call_id,
                    name,
                    input,
                } => Some(format!("[{call_id}] call {name} {input}")),
                Block::ToolResult {
                    call_id,
                    content,
                    is_error,
                    produced_at,
                } => {
                    // A stamped result opens with a compact wall-clock
                    // header so a resumed agent can reason about recency
                    // ("passed 3 minutes ago") straight from the text view;
                    // unstamped results keep the exact legacy shape.
                    let stamp = produced_at
                        .map(|secs| format!("[{}] ", infrastructure_base::format_timestamp(secs)))
                        .unwrap_or_default();
                    Some(format!(
                        "{stamp}[{call_id}] {}: {content}",
                        if *is_error { "error" } else { "ok" }
                    ))
                }
                Block::Image { id, mime, .. } => Some(match id {
                    Some(label) => format!("[image {mime}: {label}]"),
                    None => format!("[image {mime}]"),
                }),
                Block::Thinking { .. } => None,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactTrigger {
    /// Automatic compaction from the budget check.
    Auto,
    /// Blocking compaction required before any further sampling.
    Blocking,
    /// Reactive compaction after a prompt-too-long sampling error.
    Reactive,
    /// Explicit user request, optionally steered (`/compact <focus>`).
    Manual {
        /// User-supplied focus for the summary, when given.
        instruction: Option<String>,
    },
    /// Model request via the `compact_context` tool, granted by the
    /// loop's gate only when usage is high enough.
    Model,
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
/// Split accounting mirrors the context crate: ASCII rides the ~4
/// chars/token ratio while every non-ASCII character counts as one token,
/// so dense CJK history is not systematically undercounted by a flat
/// character division. Authoritative provider usage must always win over
/// this function.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

/// Notification seam for every committed history mutation.
///
/// The store owns history; it does not know where durability lives. A journal
/// attaches here instead of trusting each call site to remember a second
/// write — a push that skips the journal is a durable gap nobody can see.
/// Implementations must be cheap and non-fatal: they run inside the append
/// path, on the turn's critical way.
pub trait HistorySink: Send + Sync + std::fmt::Debug {
    /// One entry was appended.
    fn appended(&self, entry: &HistoryEntry);
    /// The whole history was replaced (compaction, rewind).
    fn replaced(&self, entries: &[HistoryEntry]);
}

/// Append-only conversation with frozen snapshots.
///
/// All mutations flow through [`Conversation::push`]; readers hold an `Arc`
/// snapshot that never changes under them.
#[derive(Debug, Clone, Default)]
pub struct Conversation {
    entries: Vec<HistoryEntry>,
    usage_carry: Usage,
    sink: Option<std::sync::Arc<dyn HistorySink>>,
}

impl Conversation {
    /// Create an empty conversation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a conversation that reports every mutation to `sink`.
    pub fn with_sink(sink: std::sync::Arc<dyn HistorySink>) -> Self {
        Self {
            entries: Vec::new(),
            usage_carry: Usage::default(),
            sink: Some(sink),
        }
    }

    /// Append one entry; the single write entry of this store.
    pub fn push(&mut self, role: Role, text: impl Into<String>) {
        self.push_blocks(role, vec![Block::Text(text.into())]);
    }

    /// Append one entry from content blocks.
    pub fn push_blocks(&mut self, role: Role, blocks: Vec<Block>) {
        let entry = HistoryEntry { role, blocks };
        if let Some(sink) = &self.sink {
            sink.appended(&entry);
        }
        self.entries.push(entry);
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

    /// Borrowed access to the current history, for readers that scan it
    /// (token estimates) without paying the snapshot's deep copy.
    pub fn with_entries<R>(&self, read: impl FnOnce(&[HistoryEntry]) -> R) -> R {
        read(&self.entries)
    }

    /// Replace the whole history, e.g. with a compaction summary.
    ///
    /// The caller owns re-establishing the usage carry via [`Conversation::settle`].
    pub fn replace(&mut self, entries: Vec<HistoryEntry>) {
        if let Some(sink) = &self.sink {
            sink.replaced(&entries);
        }
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

/// Close every tool call that has no result, appending one error result per
/// lost call; returns the repaired history and the affected call ids.
///
/// Providers reject an assistant entry whose `tool_use` has no matching
/// `tool_result`, so a history restored from a partial store (a journal
/// whose last round never settled) must be closed before it is sampled
/// again. The closing result says the outcome is *unknown* rather than
/// "failed": the side effect may well have happened, and the model must not
/// be invited to repeat it.
pub fn close_open_calls(entries: &[HistoryEntry], note: &str) -> (Vec<HistoryEntry>, Vec<String>) {
    use std::collections::HashSet;

    let answered: HashSet<&str> = entries
        .iter()
        .flat_map(|entry| entry.blocks.iter())
        .filter_map(|block| match block {
            Block::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let mut lost: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for entry in entries {
        for block in &entry.blocks {
            if let Block::ToolUse { call_id, .. } = block
                && !answered.contains(call_id.as_str())
                && seen.insert(call_id.as_str())
            {
                lost.push(call_id.clone());
            }
        }
    }
    if lost.is_empty() {
        return (entries.to_vec(), lost);
    }
    let mut repaired = entries.to_vec();
    repaired.push(HistoryEntry {
        role: Role::User,
        blocks: lost
            .iter()
            .map(|call_id| Block::ToolResult {
                call_id: call_id.clone(),
                content: note.to_string(),
                is_error: true,
                // The real production time went down with the lost record;
                // an honest header cannot exist for this result.
                produced_at: None,
            })
            .collect(),
    });
    (repaired, lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the store reports, in order.
    #[derive(Debug, Default)]
    struct RecordingSink(std::sync::Mutex<Vec<String>>);

    impl RecordingSink {
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl HistorySink for RecordingSink {
        fn appended(&self, entry: &HistoryEntry) {
            let role = if entry.role == Role::User { "u" } else { "a" };
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("append {role} {}", entry.blocks.len()));
        }
        fn replaced(&self, entries: &[HistoryEntry]) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("replace {}", entries.len()));
        }
    }

    /// A journal hangs off this seam, so no mutation may bypass it — both
    /// writers, including the history-replacing one.
    #[test]
    fn sink_sees_appends_and_replacements() {
        let sink = std::sync::Arc::new(RecordingSink::default());
        let mut conv = Conversation::with_sink(sink.clone());
        conv.push(Role::User, "hello");
        conv.push_blocks(
            Role::Assistant,
            vec![
                Block::Text("working".to_string()),
                Block::ToolUse {
                    call_id: "c1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "a.txt"}),
                },
            ],
        );
        conv.replace(vec![HistoryEntry {
            role: Role::User,
            blocks: vec![Block::Text("[compact summary]".to_string())],
        }]);
        assert_eq!(sink.lines(), vec!["append u 1", "append a 2", "replace 1"]);
        // An unsunk conversation keeps working (tests, subagents without a
        // durability owner).
        let mut plain = Conversation::new();
        plain.push(Role::User, "x");
        assert_eq!(plain.len(), 1);
    }

    #[test]
    fn open_calls_close_with_an_unknown_outcome() {
        let entries = vec![
            HistoryEntry {
                role: Role::User,
                blocks: vec![Block::Text("read a.txt".to_string())],
            },
            HistoryEntry {
                role: Role::Assistant,
                blocks: vec![
                    Block::ToolUse {
                        call_id: "c1".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({}),
                    },
                    Block::ToolUse {
                        call_id: "c2".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            HistoryEntry {
                role: Role::User,
                blocks: vec![Block::ToolResult {
                    call_id: "c1".to_string(),
                    content: "ok".to_string(),
                    is_error: false,
                    produced_at: None,
                }],
            },
        ];
        let (repaired, lost) = close_open_calls(&entries, "outcome lost");
        // Only the unanswered call closes, and pairing is restored.
        assert_eq!(lost, vec!["c2".to_string()]);
        assert_eq!(repaired.len(), entries.len() + 1);
        let closing = repaired.last().unwrap();
        assert_eq!(closing.role, Role::User);
        assert_eq!(closing.blocks.len(), 1);
        match &closing.blocks[0] {
            Block::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(call_id, "c2");
                assert_eq!(content, "outcome lost");
                assert!(is_error);
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
        // Already-paired history is returned untouched and reports nothing.
        let clean = vec![HistoryEntry {
            role: Role::User,
            blocks: vec![Block::Text("hi".to_string())],
        }];
        let (same, lost) = close_open_calls(&clean, "outcome lost");
        assert_eq!(same, clean);
        assert!(lost.is_empty());
    }

    /// The journal format is this type's serde form: a lost block shape must
    /// fail here, not silently at resume time.
    #[test]
    fn history_round_trips_through_json() {
        let entries = vec![
            HistoryEntry {
                role: Role::User,
                blocks: vec![
                    Block::Text("look".to_string()),
                    Block::Image {
                        id: Some("img1".to_string()),
                        mime: "image/png".to_string(),
                        base64: "AAAA".to_string(),
                    },
                ],
            },
            HistoryEntry {
                role: Role::Assistant,
                blocks: vec![
                    Block::Thinking {
                        text: "reasoning".to_string(),
                        signature: None,
                    },
                    Block::ToolUse {
                        call_id: "c1".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({"path": "a.txt"}),
                    },
                    Block::ToolResult {
                        call_id: "c1".to_string(),
                        content: "file body".to_string(),
                        is_error: true,
                        produced_at: Some(1_789_000_000),
                    },
                ],
            },
        ];
        let text = serde_json::to_string(&entries).unwrap();
        let back: Vec<HistoryEntry> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, entries);
    }

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
                    produced_at: None,
                },
                Block::ToolResult {
                    call_id: "c2".to_string(),
                    content: "boom".to_string(),
                    is_error: true,
                    produced_at: None,
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

    /// A stamped tool result renders a compact wall-clock header in text
    /// views (recency reasoning for resumed agents) but never leaks into
    /// the prose view.
    #[test]
    fn stamped_tool_results_render_a_wall_clock_header() {
        let entry = HistoryEntry {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                call_id: "c1".to_string(),
                content: "ls out".to_string(),
                is_error: false,
                produced_at: Some(20_715 * 86_400 + 18 * 3_600 + 3 * 60),
            }],
        };
        assert_eq!(entry.text(), "[2026-09-19 18:03 UTC] [c1] ok: ls out");
        assert_eq!(entry.prose(), "");
    }

    /// A tool result persisted before the timestamp existed (no
    /// `produced_at` key on disk) must still load, landing as unstamped.
    #[test]
    fn tool_results_without_a_stamp_still_load() {
        let legacy = r#"{"role":"user","blocks":[{"tool_result":{"call_id":"c1","content":"old","is_error":false}}]}"#;
        let entry: HistoryEntry = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            entry.text(),
            "[c1] ok: old",
            "unstamped rendering matches the legacy shape"
        );
        match &entry.blocks[0] {
            Block::ToolResult { produced_at, .. } => assert_eq!(*produced_at, None),
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    /// Reasoning blocks stay in the block list (the Anthropic wire needs them
    /// back) but never leak into the text views that feed estimates, resume
    /// and summaries.
    #[test]
    fn thinking_blocks_are_invisible_to_text_views() {
        let entry = HistoryEntry {
            role: Role::Assistant,
            blocks: vec![
                Block::Thinking {
                    text: "weighing the options".to_string(),
                    signature: Some("sig".to_string()),
                },
                Block::Text("done".to_string()),
            ],
        };
        assert_eq!(entry.text(), "done");
        assert_eq!(entry.prose(), "done");
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
                produced_at: None,
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
