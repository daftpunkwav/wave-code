//! wavecode-context — context-management pipeline (one implementation, pluggable policy).
//!
//! One pipeline in three stages (SPEC section 6):
//! 1. **Accounting**: prefer the provider-returned usage (`input_tokens`
//!    already authoritatively covers the full history); with no usage (first
//!    turn / not yet re-sampled after compaction) fall back to the
//!    [`estimate_tokens`] character estimate.
//! 2. **Three-level thresholds** ([`Thresholds`], parameterized by window
//!    proportion, defaults aligned with SPEC section 6): warn line at
//!    window-20k / auto-compact line at window-13k / blocking line at
//!    window-3k.
//! 3. **Compaction**: abstracted behind the [`CompactionStrategy`] trait
//!    (replaceable); the first implementation is [`ModelSummary`] (a
//!    five-element structured summary from one model call); the new history =
//!    summary message + the most recent N verbatim messages, with
//!    [`normalize_history`] guaranteeing pairing integrity.
//!
//! This crate depends only on `wavecode-llm` (SPEC section 3 matrix); trigger
//! timing is orchestrated by core.

use std::sync::Arc;

use futures::StreamExt;
use wavecode_llm::{ChatModel, ChatRequest, ContentBlock, Message, Role, StreamEvent};

// ---------------------------------------------------------------------------
// token accounting
// ---------------------------------------------------------------------------

/// Default value of the character-estimate ratio (chars/token).
pub const DEFAULT_CHARS_PER_TOKEN: usize = 4;

/// Fixed overhead quota for the system prompt and tool manifest (SPEC section 6
/// "expected system overhead").
/// Rough quota: the system-prompt template runs ~hundreds of tokens plus the
/// builtin tool schemas at ~1-2k tokens; it only participates on the estimate
/// path (no usage) — the usage path's input_tokens already includes all
/// overhead.
pub const SYSTEM_OVERHEAD_TOKENS: u64 = 2_000;

/// Token estimate for the history messages (fallback path when no provider
/// usage is available).
///
/// Error bounds (know them; never treat this as authoritative): English/code
/// text runs ~4 chars/token (±20%); CJK text runs ~1.5-2 chars/token, so this
/// estimate may undercount CJK history by about half. The three-level
/// thresholds therefore trigger off usage; the estimate only serves windows
/// that never produced usage yet (first turn, unsampled after compaction),
/// with the thresholds' margin scale (>= 3k) absorbing the error.
pub fn estimate_tokens(messages: &[Message], chars_per_token: usize) -> u64 {
    let ratio = chars_per_token.max(1) as u64;
    let mut chars = 0u64;
    for m in messages {
        for b in &m.content {
            chars += match b {
                ContentBlock::Text { text } => text.chars().count() as u64,
                ContentBlock::ToolUse { name, input, .. } => {
                    name.chars().count() as u64 + input.to_string().chars().count() as u64
                }
                ContentBlock::ToolResult { content, .. } => content.chars().count() as u64,
            };
        }
    }
    // Per-message structural overhead (role / block framing) at a flat ~4 tokens.
    chars / ratio + 4 * messages.len() as u64
}

/// Estimated usage including the fixed system overhead.
///
/// Convenience wrapper over [`estimate_tokens`] using the config ratio, with
/// [`SYSTEM_OVERHEAD_TOKENS`] added. Callers computing a PreTurn budget without
/// provider usage must use this instead of hand-rolling `estimate + overhead`,
/// so the overhead can never be silently dropped (a 2k underestimate would
/// shift every threshold line).
pub fn estimate_with_overhead(messages: &[Message], cfg: &ContextConfig) -> u64 {
    estimate_tokens(messages, cfg.estimate_chars_per_token).saturating_add(SYSTEM_OVERHEAD_TOKENS)
}

/// Stage-1 usage accounting: authoritative provider `input_tokens` win,
/// otherwise fall back to [`estimate_with_overhead`].
///
/// `usage_input_tokens` already covers the full history including system
/// overhead, so it is returned as-is; only the estimation path adds the fixed
/// overhead quota.
pub fn resolve_used_tokens(
    messages: &[Message],
    usage_input_tokens: Option<u64>,
    cfg: &ContextConfig,
) -> u64 {
    match usage_input_tokens {
        Some(input) => input,
        None => estimate_with_overhead(messages, cfg),
    }
}

// ---------------------------------------------------------------------------
// three-level thresholds
// ---------------------------------------------------------------------------

/// Budget water level (the verdict of [`Thresholds::check`], deepening stepwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetLevel {
    /// Comfortable.
    Ok,
    /// Warn line: `used >= window - warning_margin` — "nearing the limit".
    Warning,
    /// Auto-compact line: `used >= window - auto_compact_margin` — compact now.
    AutoCompact,
    /// Blocking line: `used >= window - blocking_margin` — compact before sampling.
    Blocking,
}

/// Three-level thresholds (SPEC section 6, defaults aligned with measured
/// Claude Code values).
///
/// Parameterized as "margins below the top of the window" rather than ratio
/// floats: 20k/13k/3k are measured token-scale experience values that shift
/// directly across window sizes; window-proportional configuration, if ever
/// needed, is converted into this struct's margins by the caller (the config
/// layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// Warn-line margin (default 20_000).
    pub warning_margin: u64,
    /// Auto-compact-line margin (default 13_000).
    pub auto_compact_margin: u64,
    /// Blocking-line margin (default 3_000).
    pub blocking_margin: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            warning_margin: 20_000,
            auto_compact_margin: 13_000,
            blocking_margin: 3_000,
        }
    }
}

impl Thresholds {
    /// Judge which water level `used / window` sits at (deepest level wins).
    ///
    /// When `window` is smaller than a margin, saturating arithmetic pins that
    /// waterline at 0 (any usage triggers the deepest level) — a misconfigured
    /// window over-compacts rather than silently overflowing.
    pub fn check(&self, used: u64, window: u64) -> BudgetLevel {
        if used >= window.saturating_sub(self.blocking_margin) {
            BudgetLevel::Blocking
        } else if used >= window.saturating_sub(self.auto_compact_margin) {
            BudgetLevel::AutoCompact
        } else if used >= window.saturating_sub(self.warning_margin) {
            BudgetLevel::Warning
        } else {
            BudgetLevel::Ok
        }
    }

    /// Validate margin ordering: `warning >= auto_compact >= blocking`.
    ///
    /// [`check`] probes the deepest level first, so inverted margins silently
    /// make an outer level unreachable (e.g. `warning_margin < blocking_margin`
    /// means `Warning` can never fire). Returns a human-readable reason on
    /// misconfiguration; [`check`] itself stays total and never panics.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.warning_margin < self.auto_compact_margin {
            return Err(format!(
                "warning_margin ({}) < auto_compact_margin ({})",
                self.warning_margin, self.auto_compact_margin
            ));
        }
        if self.auto_compact_margin < self.blocking_margin {
            return Err(format!(
                "auto_compact_margin ({}) < blocking_margin ({})",
                self.auto_compact_margin, self.blocking_margin
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// compaction
// ---------------------------------------------------------------------------

/// Default count of most-recent verbatim messages kept after compaction (SPEC
/// section 6 "default 10").
pub const DEFAULT_KEEP_RECENT: usize = 10;

/// Default output budget (max_tokens) for summary calls.
pub const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 4096;

/// Context pipeline config (core embeds one in SessionConfig, frozen after
/// construction).
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Three-level thresholds.
    pub thresholds: Thresholds,
    /// Count of most-recent verbatim messages kept after compaction.
    pub keep_recent: usize,
    /// Output budget (max_tokens) for summary calls.
    pub summary_max_tokens: u32,
    /// Chars/token ratio for the no-usage fallback estimate (error bounds, see
    /// [`estimate_tokens`]).
    pub estimate_chars_per_token: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            thresholds: Thresholds::default(),
            keep_recent: DEFAULT_KEEP_RECENT,
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            estimate_chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        }
    }
}

impl ContextConfig {
    /// Validate the whole pipeline config in one call.
    ///
    /// Numeric fields are already saturation/clamp-defended at their use sites
    /// (`saturating_sub` in [`Thresholds::check`], `max(1)` in the estimate and
    /// summary paths), so only the threshold margin ordering can fail silently
    /// and is checked here. Returns a human-readable reason on misconfiguration.
    pub fn validate(&self) -> std::result::Result<(), String> {
        self.thresholds.validate()
    }
}

/// Crate-wide error type.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// Summary model call or stream consumption failed.
    #[error("summary model call failed: {0}")]
    Model(#[from] wavecode_llm::LlmError),
    /// The summary model produced no text (malformed stream / empty response).
    #[error("summary model produced no text")]
    EmptySummary,
}

/// Crate-wide Result alias.
pub type Result<T> = std::result::Result<T, ContextError>;

/// Compaction strategy abstraction (SPEC section 6:
/// `summarize(history, budget) -> summary`).
///
/// Strategies are replaceable (local summary / stronger-model summary, …),
/// but there is exactly one trigger pipeline (core orchestration: the
/// threshold line / reactive compact / `/compact` share one entry point).
#[async_trait::async_trait]
pub trait CompactionStrategy: Send + Sync {
    /// Produce a structured summary of `history`; `budget` is the summary
    /// call's output token budget.
    async fn summarize(&self, history: &[Message], budget: u32) -> Result<String>;
}

/// System prompt for summary requests (kept distinct from the main session;
/// test mocks use it to route scripted responses).
const SUMMARY_SYSTEM: &str =
    "You are a context compaction assistant producing structured conversation summaries.";

/// Summary instruction (a user message appended at the end of history): the
/// five element titles are pinned verbatim — Goal / Progress / Key decisions /
/// File inventory / Todo (SPEC section 6 / DEV-PLAN P3 acceptance anchor).
const SUMMARY_INSTRUCTION: &str = "\
The above is the conversation history between a coding agent and the user. Compress it into a structured summary that contains the following five section titles verbatim:
## Goal — the user's overall objective and current task
## Progress — work completed and where things currently stand
## Key decisions — confirmed technical choices, plans, and constraints (with reasons)
## File inventory — key files created / modified / read, with their status
## Todo — unfinished items and next steps
Requirements: keep concrete filenames, paths, commands, and error messages; output only the summary itself, no pleasantries.";

pub const SUMMARY_MESSAGE_PREFIX: &str =
    "[context compaction] earlier conversation compacted into the summary below:";

/// First-version compaction strategy: a five-element structured summary from
/// one call to the current model.
pub struct ModelSummary {
    model: Arc<dyn ChatModel>,
    model_name: String,
}

impl ModelSummary {
    /// `model` reuses the main session's model channel (compaction uses the
    /// same model as the main session, SPEC section 6 first version).
    pub fn new(model: Arc<dyn ChatModel>, model_name: String) -> Self {
        Self { model, model_name }
    }
}

#[async_trait::async_trait]
impl CompactionStrategy for ModelSummary {
    async fn summarize(&self, history: &[Message], budget: u32) -> Result<String> {
        let mut messages = history.to_vec();
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: SUMMARY_INSTRUCTION.to_owned(),
            }],
        });
        let req = ChatRequest {
            model: self.model_name.clone(),
            system: SUMMARY_SYSTEM.to_owned(),
            messages: Arc::new(messages),
            tools: Vec::new(),
            max_tokens: budget.max(1),
        };
        let mut stream = self.model.stream(req).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let StreamEvent::TextDelta { text: delta } = item? {
                text.push_str(&delta);
            }
        }
        if text.trim().is_empty() {
            return Err(ContextError::EmptySummary);
        }
        Ok(text)
    }
}

/// Compaction product.
#[derive(Debug)]
pub struct CompactOutcome {
    /// New post-compaction history (summary message + most recent N verbatim
    /// messages, normalized).
    pub messages: Vec<Message>,
    /// Summary body (excluding [`SUMMARY_MESSAGE_PREFIX`]).
    pub summary: String,
}

/// The compaction pipeline's single entry point (shared by core's three
/// trigger kinds): the summary message plus the most recent
/// `cfg.keep_recent` verbatim messages form the new history.
///
/// Pairing policy at the truncation boundary (two options; this implementation
/// picks **drop orphans**): when taking N messages from the back, a window
/// whose first message is a tool_result user message has lost its paired
/// assistant tool_use — extending forward to a user-text boundary would keep
/// more verbatim history (often large tool outputs) inside the window, making
/// the message count uncontrollable and defeating compaction; instead
/// [`normalize_history`] drops the orphan blocks, and the summary carries the
/// dropped part's information.
pub async fn compact_history(
    history: &[Message],
    strategy: &dyn CompactionStrategy,
    cfg: &ContextConfig,
) -> Result<CompactOutcome> {
    let summary = strategy.summarize(history, cfg.summary_max_tokens).await?;
    let start = history.len().saturating_sub(cfg.keep_recent);
    let mut messages = Vec::with_capacity(history.len() - start + 1);
    messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!("{SUMMARY_MESSAGE_PREFIX}\n\n{summary}"),
        }],
    });
    messages.extend_from_slice(&history[start..]);
    let messages = normalize_history(&messages);
    Ok(CompactOutcome { messages, summary })
}

// ---------------------------------------------------------------------------
// history normalize and pairing checks
// ---------------------------------------------------------------------------

/// Backfill text for orphan tool_use completions (is_error — the model can
/// retry or give up from it).
const MISSING_RESULT_CONTENT: &str = "tool result unavailable (history normalized)";

/// History normalize (a standalone pure function shared by the compaction /
/// restore paths, 100% unit-tested):
/// 1. Drop empty-content messages (interrupted empty messages, … — Anthropic
///    rejects empty content arrays);
/// 2. Orphan tool_use (an assistant declared a call with no paired
///    tool_result): backfill is_error results in declaration order (matching
///    Anthropic's "every tool_use needs a paired tool_result" constraint);
/// 3. Orphan tool_result (no paired tool_use, typically from a compaction cut):
///    drop the block, removing the whole message if it goes empty; other
///    blocks (text, …) in the same user message are kept.
pub fn normalize_history(history: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    let mut i = 0;
    while i < history.len() {
        let m = &history[i];
        if m.content.is_empty() {
            i += 1; // Rule 1: drop empty-content messages.
            continue;
        }
        let tool_use_ids: Vec<&str> = if m.role == Role::Assistant {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        if !tool_use_ids.is_empty() {
            out.push(m.clone());
            // The immediately following user message supplies the paired
            // results (matched per id, in declaration order).
            let next = history.get(i + 1).filter(|n| n.role == Role::User);
            let mut results = Vec::with_capacity(tool_use_ids.len());
            for id in &tool_use_ids {
                let matched = next.and_then(|n| {
                    n.content.iter().find(
                        |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
                    )
                });
                results.push(matched.cloned().unwrap_or(ContentBlock::ToolResult {
                    tool_use_id: (*id).to_owned(),
                    content: MISSING_RESULT_CONTENT.to_owned(),
                    is_error: true,
                }));
            }
            out.push(Message {
                role: Role::User,
                content: results,
            });
            if let Some(n) = next {
                // Unconsumed blocks in next: orphan ToolResults dropped (rule
                // 3), everything else kept.
                let rest: Vec<ContentBlock> = n
                    .content
                    .iter()
                    .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
                    .cloned()
                    .collect();
                if !rest.is_empty() {
                    out.push(Message {
                        role: Role::User,
                        content: rest,
                    });
                }
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Rule 3: drop orphan ToolResult blocks in user messages, keep the
        // remaining blocks.
        if m.role == Role::User
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            let rest: Vec<ContentBlock> = m
                .content
                .iter()
                .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
                .cloned()
                .collect();
            if !rest.is_empty() {
                out.push(Message {
                    role: Role::User,
                    content: rest,
                });
            }
            i += 1;
            continue;
        }
        out.push(m.clone());
        i += 1;
    }
    out
}

/// Pairing integrity check (shared by the compaction / restore paths' test
/// assertions): returns every violation description; an empty Vec means the
/// pairing is intact.
///
/// Constraints (Anthropic): every assistant tool_use must have a same-id
/// tool_result in the immediately following user message; every tool_result in
/// a user message must pair with a tool_use in the previous assistant message.
pub fn find_pairing_violations(history: &[Message]) -> Vec<String> {
    let mut violations = Vec::new();
    for (i, m) in history.iter().enumerate() {
        match m.role {
            Role::Assistant => {
                let ids: Vec<&str> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                        _ => None,
                    })
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                let next = history.get(i + 1).filter(|n| n.role == Role::User);
                for id in ids {
                    let paired = next.is_some_and(|n| {
                        n.content.iter().any(
                            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
                        )
                    });
                    if !paired {
                        violations.push(format!(
                            "message[{i}] tool_use({id}) has no paired tool_result"
                        ));
                    }
                }
            }
            Role::User => {
                let prev_ids: Vec<&str> = history
                    .get(i.wrapping_sub(1))
                    .filter(|_| i > 0)
                    .filter(|p| p.role == Role::Assistant)
                    .map(|p| {
                        p.content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for b in &m.content {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = b
                        && !prev_ids.contains(&tool_use_id.as_str())
                    {
                        violations.push(format!(
                            "message[{i}] tool_result({tool_use_id}) has no paired tool_use"
                        ));
                    }
                }
            }
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::sync::Mutex;
    use wavecode_llm::Usage;

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    fn tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: "read_file".to_owned(),
                input: serde_json::json!({"path": "a.txt"}),
            }],
        }
    }

    fn tool_result(id: &str, is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: "content".to_owned(),
                is_error,
            }],
        }
    }

    // --- accounting ---

    #[test]
    fn estimate_tokens_scales_with_chars_and_ratio() {
        let history = vec![user_text(&"x".repeat(400))];
        let est4 = estimate_tokens(&history, 4);
        let est2 = estimate_tokens(&history, 2);
        assert_eq!(
            est4,
            100 + 4,
            "400 chars / 4 + one message of structural overhead"
        );
        assert_eq!(est2, 200 + 4);
        assert_eq!(estimate_tokens(&[], 4), 0);
        // Zero-proof ratio: treated as 1, never panics.
        assert!(estimate_tokens(&history, 0) > 0);
    }

    // --- three-level threshold edges ---

    #[test]
    fn threshold_boundaries() {
        let t = Thresholds::default();
        let w = 200_000u64;
        // Warn line: window - 20k = 180_000.
        assert_eq!(t.check(179_999, w), BudgetLevel::Ok);
        assert_eq!(t.check(180_000, w), BudgetLevel::Warning);
        // Auto-compact line: window - 13k = 187_000.
        assert_eq!(t.check(186_999, w), BudgetLevel::Warning);
        assert_eq!(t.check(187_000, w), BudgetLevel::AutoCompact);
        // Blocking line: window - 3k = 197_000.
        assert_eq!(t.check(196_999, w), BudgetLevel::AutoCompact);
        assert_eq!(t.check(197_000, w), BudgetLevel::Blocking);
        assert_eq!(t.check(200_000, w), BudgetLevel::Blocking);
    }

    #[test]
    fn threshold_saturates_when_window_smaller_than_margin() {
        let t = Thresholds::default();
        // 2k window < 3k blocking_margin: any usage blocks (over-compact
        // rather than overflow the window).
        assert_eq!(t.check(1, 2_000), BudgetLevel::Blocking);
        // 10k window: between 13k and 3k — warn/auto lines pin at 0, blocking
        // line at 7k.
        assert_eq!(t.check(100, 10_000), BudgetLevel::AutoCompact);
        assert_eq!(t.check(7_000, 10_000), BudgetLevel::Blocking);
    }

    // --- normalize ---

    #[test]
    fn normalize_removes_empty_messages() {
        let history = vec![
            user_text("hi"),
            Message {
                role: Role::Assistant,
                content: vec![],
            },
            assistant_text("hello"),
        ];
        let out = normalize_history(&history);
        assert_eq!(out.len(), 2);
        assert!(find_pairing_violations(&out).is_empty());
    }

    #[test]
    fn normalize_completes_orphan_tool_use_with_error_result() {
        // The assistant declared two calls; the user answered only one, with
        // extra text attached.
        let history = vec![
            user_text("get to work"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({}),
                    },
                    ContentBlock::ToolUse {
                        id: "t2".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "A".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "extra note".into(),
                    },
                ],
            },
        ];
        let out = normalize_history(&history);
        assert!(find_pairing_violations(&out).is_empty());
        // Paired message: t1 keeps its original result, t2 gets an is_error
        // backfill.
        let pair = &out[2];
        assert_eq!(pair.role, Role::User);
        let contents: Vec<(&str, bool)> = pair
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => Some((tool_use_id.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(contents, vec![("t1", false), ("t2", true)]);
        // The text block survives as a standalone user message.
        assert!(matches!(&out[3].content[0], ContentBlock::Text { text } if text == "extra note"));
    }

    #[test]
    fn normalize_drops_orphan_tool_results() {
        let history = vec![
            tool_result("ghost", false), // No paired tool_use (e.g. a compaction-cut window head).
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "ghost2".into(),
                        content: "x".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "kept text".into(),
                    },
                ],
            },
            user_text("hello"),
        ];
        let out = normalize_history(&history);
        assert!(find_pairing_violations(&out).is_empty());
        assert_eq!(
            out.len(),
            2,
            "wholly orphaned messages removed, mixed ones keep only text"
        );
        assert!(matches!(&out[0].content[0], ContentBlock::Text { text } if text == "kept text"));
    }

    #[test]
    fn normalize_is_idempotent_on_wellformed_history() {
        let history = vec![
            user_text("read the file"),
            tool_use("t1"),
            tool_result("t1", false),
            assistant_text("done reading"),
        ];
        let out = normalize_history(&history);
        assert_eq!(
            out, history,
            "fully paired history passes through untouched"
        );
        assert!(find_pairing_violations(&out).is_empty());
    }

    // --- compaction (incl. the retention acceptance anchor) ---

    /// Scripted mock: replays a pre-arranged event sequence.
    struct MockModel {
        scripts: Vec<Vec<StreamEvent>>,
        calls: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl ChatModel for MockModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<wavecode_llm::EventStream> {
            // Summary-request assertions: full history passed through + the
            // five-element instruction + no tools + the budget applied.
            assert_eq!(req.system, SUMMARY_SYSTEM);
            assert!(req.tools.is_empty());
            assert_eq!(req.max_tokens, 777);
            let last = req.messages.last().expect("summary instruction appended");
            assert!(
                matches!(&last.content[0], ContentBlock::Text { text } if text.contains("## Goal") && text.contains("## Todo"))
            );
            let mut n = self.calls.lock().unwrap();
            let idx = (*n as usize).min(self.scripts.len() - 1);
            *n += 1;
            Ok(Box::pin(stream::iter(
                self.scripts[idx].clone().into_iter().map(Ok),
            )))
        }
    }

    /// Scripted summary response carrying all five elements.
    fn scripted_summary() -> Vec<StreamEvent> {
        let summary = "\
## Goal
Build the shop backend (Rust workspace).
## Progress
Cart service and order skeleton done; stock deduction in progress.
## Key decisions
SQLite for v1 (reason: zero ops); payments via a mock gateway.
## File inventory
crates/shop/src/cart.rs (created); crates/shop/src/order.rs (modified).
## Todo
Concurrent stock-deduction test; settlement ledger integration.";
        vec![
            StreamEvent::TextDelta {
                text: summary.into(),
            },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 5000,
                    output_tokens: 120,
                },
            },
        ]
    }

    /// A long session history (16 messages) with five-element content + tool
    /// pairing.
    fn long_history() -> Vec<Message> {
        let mut h = vec![
            user_text("Goal: build the shop backend, cart first."),
            assistant_text("Key decision: SQLite for v1, zero ops."),
            tool_use("t1"),
            tool_result("t1", false),
            assistant_text("Created crates/shop/src/cart.rs."),
            tool_use("t2"),
            tool_result("t2", false),
            assistant_text("Order skeleton done; todo: concurrent stock-deduction test."),
        ];
        // Pad the length (> keep_recent) with mid-process content unrelated to
        // the five elements.
        for i in 0..8 {
            h.push(user_text(&format!("filler step {i}")));
        }
        h
    }

    /// P3 acceptance anchor: compaction retention — the summary carries each
    /// of the five elements, and the most recent N verbatim messages survive.
    #[tokio::test]
    async fn compact_retains_five_elements_and_recent_tail() {
        let history = long_history();
        let tail_start = history.len() - 4;
        let model = Arc::new(MockModel {
            scripts: vec![scripted_summary()],
            calls: Mutex::new(0),
        });
        let strategy = ModelSummary::new(model, "mock".into());
        let cfg = ContextConfig {
            keep_recent: 4,
            summary_max_tokens: 777,
            ..Default::default()
        };
        let outcome = compact_history(&history, &strategy, &cfg).await.unwrap();

        // The summary message comes first (user meta), carrying each element.
        let first = &outcome.messages[0];
        assert_eq!(first.role, Role::User);
        let ContentBlock::Text { text } = &first.content[0] else {
            panic!("first message should be the summary text message")
        };
        assert!(text.starts_with(SUMMARY_MESSAGE_PREFIX));
        for element in [
            "Goal",
            "Progress",
            "Key decisions",
            "File inventory",
            "Todo",
        ] {
            assert!(
                text.contains(element),
                "summary missing element \"{element}\": {text}"
            );
        }

        // The most recent 4 verbatim messages survive intact (message by
        // message equal to the source history).
        assert_eq!(outcome.messages.len(), 1 + 4);
        assert_eq!(
            &outcome.messages[1..],
            &history[tail_start..],
            "most recent N verbatim messages should survive message by message"
        );

        // Pairing integrity (reuses the assertion helper as an anchor).
        assert_eq!(
            find_pairing_violations(&outcome.messages),
            Vec::<String>::new()
        );
    }

    /// Cut boundary: a window whose first message is a tool_result (its paired
    /// assistant dropped) -> orphan dropped.
    #[tokio::test]
    async fn compact_drops_orphan_at_cut_boundary() {
        let mut history = vec![
            user_text("start"),
            tool_use("t9"),
            tool_result("t9", false), // Becomes the window head at keep_recent=2.
            assistant_text("end"),
        ];
        history.extend_from_slice(&[user_text("final")]);
        let model = Arc::new(MockModel {
            scripts: vec![scripted_summary()],
            calls: Mutex::new(0),
        });
        let strategy = ModelSummary::new(model, "mock".into());
        let cfg = ContextConfig {
            keep_recent: 2,
            summary_max_tokens: 777,
            ..Default::default()
        };
        // Window = [tool_result("t9"), assistant_text("end")] — orphaned head.
        let outcome = compact_history(&history[..4], &strategy, &cfg)
            .await
            .unwrap();
        assert_eq!(
            find_pairing_violations(&outcome.messages),
            Vec::<String>::new(),
            "compacted history must have no orphans: {:?}",
            outcome.messages
        );
        assert!(
            !outcome.messages.iter().any(|m| m.content.iter().any(
                |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t9")
            )),
            "orphan tool_result should be dropped"
        );
    }

    /// An empty summary-model response is an EmptySummary error (never silently
    /// swap history for an empty summary).
    #[tokio::test]
    async fn empty_summary_is_an_error() {
        let model = Arc::new(MockModel {
            scripts: vec![vec![StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            }]],
            calls: Mutex::new(0),
        });
        let strategy = ModelSummary::new(model, "mock".into());
        let result = strategy.summarize(&long_history(), 777).await;
        assert!(matches!(result, Err(ContextError::EmptySummary)));
    }
    // --- stage-1 accounting ---

    #[test]
    fn resolve_used_tokens_prefers_authoritative_usage() {
        let cfg = ContextConfig::default();
        let history = vec![user_text(&"x".repeat(4000))];
        // Authoritative input_tokens returned as-is, never re-estimated.
        assert_eq!(resolve_used_tokens(&history, Some(9999), &cfg), 9999);
        assert_eq!(resolve_used_tokens(&[], Some(0), &cfg), 0);
    }

    #[test]
    fn resolve_used_tokens_falls_back_to_estimate_plus_overhead() {
        let cfg = ContextConfig::default();
        let history = vec![user_text(&"x".repeat(400))];
        let expected =
            estimate_tokens(&history, cfg.estimate_chars_per_token) + SYSTEM_OVERHEAD_TOKENS;
        assert_eq!(resolve_used_tokens(&history, None, &cfg), expected);
        // Empty history still accounts for the fixed system overhead.
        assert_eq!(resolve_used_tokens(&[], None, &cfg), SYSTEM_OVERHEAD_TOKENS);
        assert_eq!(
            estimate_with_overhead(&history, &cfg),
            expected,
            "overhead must be impossible to forget on the estimate path"
        );
    }

    // --- config validation ---

    #[test]
    fn thresholds_validate_accepts_ordered_margins() {
        assert!(Thresholds::default().validate().is_ok());
        assert!(
            Thresholds {
                warning_margin: 200,
                auto_compact_margin: 100,
                blocking_margin: 10,
            }
            .validate()
            .is_ok()
        );
        // Equal margins are degenerate but well-defined (levels collapse).
        assert!(
            Thresholds {
                warning_margin: 100,
                auto_compact_margin: 100,
                blocking_margin: 100,
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn thresholds_validate_rejects_inverted_margins() {
        let err = Thresholds {
            warning_margin: 100,
            ..Default::default()
        }
        .validate()
        .expect_err("warning < auto_compact must be rejected");
        assert!(err.contains("warning_margin"), "unexpected reason: {err}");
        let err = Thresholds {
            auto_compact_margin: 100,
            ..Default::default()
        }
        .validate()
        .expect_err("auto_compact < blocking must be rejected");
        assert!(
            err.contains("auto_compact_margin"),
            "unexpected reason: {err}"
        );
    }

    #[test]
    fn context_config_validate_delegates_to_thresholds() {
        assert!(ContextConfig::default().validate().is_ok());
        let bad = ContextConfig {
            thresholds: Thresholds {
                warning_margin: 1,
                auto_compact_margin: 100,
                blocking_margin: 10,
            },
            ..Default::default()
        };
        assert!(bad.validate().is_err());
    }
}
