//! wavecode-context — context-management pipeline (one implementation, pluggable policy).
//!
//! One pipeline in three stages:
//! 1. **Accounting**: prefer the provider-returned usage (`input_tokens`
//!    already authoritatively covers the full history); with no usage (first
//!    turn / not yet re-sampled after compaction) fall back to the
//!    [`estimate_tokens`] character estimate.
//! 2. **Three-level thresholds** ([`Thresholds`], parameterized by window
//!    proportion, defaults aligned with measured Claude Code values): warn line at
//!    window-20k / auto-compact line at window-13k / blocking line at
//!    window-3k.
//! 3. **Compaction**: abstracted behind the [`CompactionStrategy`] trait
//!    (replaceable); the first implementation is [`ModelSummary`] (a
//!    five-element structured summary from one model call); the new history =
//!    summary message + the most recent N verbatim messages, with
//!    [`normalize_history`] guaranteeing pairing integrity.
//!
//! This crate depends only on `wavecode-llm`; trigger
//! timing is orchestrated by core.
//!
//! Two auxiliary passes share the same history model: the cache-preserving
//! micro-compaction pass ([`evict_old_tool_results`], which stubs the payloads
//! of old tool results while leaving an anchored prefix and a recent window
//! untouched) and the system-reminder injection channel
//! ([`ReminderChannel`], the single channel for compaction notices, plan
//! nudges, and similar meta text).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use futures::StreamExt;
use wavecode_llm::{ChatModel, ChatRequest, ContentBlock, Message, Role, StreamEvent};

pub mod spill;
pub use spill::{
    DEFAULT_PRUNE_THRESHOLD_CHARS, PRUNE_HEAD_CHARS, PRUNE_MARKER, SPILL_SCHEME,
    SPILL_TOTAL_CAP_BYTES, SpillError, SpillStore, default_spill_store_root, parse_spill_uri,
    prune_tool_output, validate_spill_id,
};

// ---------------------------------------------------------------------------
// token accounting
// ---------------------------------------------------------------------------

/// Default value of the character-estimate ratio (chars/token).
pub const DEFAULT_CHARS_PER_TOKEN: usize = 4;

/// Fixed overhead quota for the system prompt and tool manifest.
/// Rough quota: the system-prompt template runs ~hundreds of tokens plus the
/// builtin tool schemas at ~1-2k tokens; it only participates on the estimate
/// path (no usage) — the usage path's input_tokens already includes all
/// overhead.
pub const SYSTEM_OVERHEAD_TOKENS: u64 = 2_000;

/// Token estimate for the history messages (fallback path when no provider
/// usage is available).
///
/// Split accounting: ASCII text runs ~[`DEFAULT_CHARS_PER_TOKEN`] chars/token
/// (±20%); every non-ASCII character counts as one token, which brackets CJK
/// (~0.6-1.5 tokens/char) without the systematic undercount a flat
/// chars/ratio division produces on Chinese/Japanese/Korean history. Base64
/// image payloads are pure ASCII and ride the ratio (upper bound; the
/// estimate path already carries a multi-k token margin). The three-level
/// thresholds trigger off usage; the estimate only serves windows that never
/// produced usage yet (first turn, unsampled after compaction), with the
/// thresholds' margin scale (>= 3k) absorbing the residual error.
pub fn estimate_tokens(messages: &[Message], chars_per_token: usize) -> u64 {
    let ratio = chars_per_token.max(1) as u64;
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for m in messages {
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => count_ascii_split(text, &mut ascii, &mut non_ascii),
                ContentBlock::ToolUse { name, input, .. } => {
                    count_ascii_split(name, &mut ascii, &mut non_ascii);
                    count_ascii_split(&input.to_string(), &mut ascii, &mut non_ascii);
                }
                ContentBlock::ToolResult { content, .. } => {
                    count_ascii_split(content, &mut ascii, &mut non_ascii)
                }
                // Images travel as base64: count the encoded chars (upper bound;
                // the estimate path already carries a multi-k token margin).
                ContentBlock::Image { base64, .. } => {
                    count_ascii_split(base64, &mut ascii, &mut non_ascii)
                }
                // Thinking is wire-round-trip state (see `Block::Thinking`):
                // it is re-sent with its assistant turn, so it does occupy
                // context — count it like any other text block.
                ContentBlock::Thinking { text, .. } => {
                    count_ascii_split(text, &mut ascii, &mut non_ascii)
                }
            };
        }
    }
    // Per-message structural overhead (role / block framing) at a flat ~4 tokens.
    ascii / ratio + non_ascii + 4 * messages.len() as u64
}

/// Split `text` into ASCII and non-ASCII character counts.
fn count_ascii_split(text: &str, ascii: &mut u64, non_ascii: &mut u64) {
    for c in text.chars() {
        if c.is_ascii() {
            *ascii += 1;
        } else {
            *non_ascii += 1;
        }
    }
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

/// Three-level thresholds (defaults aligned with measured Claude Code
/// values).
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

/// Compaction strategy abstraction
/// (`summarize(history, budget) -> summary`).
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
/// File inventory / Todo.
const SUMMARY_INSTRUCTION: &str = "\
The above is the conversation history between a coding agent and the user. Compress it into a structured summary that contains the following five section titles verbatim:
## Goal — the user's overall objective and current task
## Progress — work completed and where things currently stand
## Key decisions — confirmed technical choices, plans, and constraints (with reasons)
## File inventory — key files created / modified / read, with their status
## Todo — unfinished items and next steps
Requirements — the next turn sees only this summary and the most recent turns verbatim, so it must carry the task on its own:
- Keep concrete filenames, paths, and commands, and prefer results over actions: the exact values, key output lines, and error text, since re-running to recover them may be slow or impossible. The session's full turn journal stays on disk, so summarize long output instead of transcribing it.
- Keep decisions already settled separate from questions still open, and name what remains unknown: files or APIs assumed but never read, results never verified.
- End with the forward plan: the exact next step, the sequence that follows, and any choice already made for it.
- The session's task list is re-attached below the summary automatically; do not transcribe it.
- Write in the language of the conversation, stay proportional to the task, and output only the summary itself — no pleasantries, no tool calls.";

pub const SUMMARY_MESSAGE_PREFIX: &str =
    "[context compaction] earlier conversation compacted into the summary below:";

/// First-version compaction strategy: a five-element structured summary from
/// one call to the current model.
pub struct ModelSummary {
    model: Arc<dyn ChatModel>,
    model_name: String,
    /// Optional user steering appended to the summary instruction
    /// (`/compact <focus>`); `None` keeps the standard prompt.
    focus: Option<String>,
}

impl ModelSummary {
    /// `model` reuses the main session's model channel (compaction uses the
    /// same model as the main session, first version).
    pub fn new(model: Arc<dyn ChatModel>, model_name: String) -> Self {
        Self {
            model,
            model_name,
            focus: None,
        }
    }

    /// Steer the summary toward a user-supplied focus.
    pub fn with_focus(mut self, focus: impl Into<String>) -> Self {
        self.focus = Some(focus.into());
        self
    }
}

#[async_trait::async_trait]
impl CompactionStrategy for ModelSummary {
    async fn summarize(&self, history: &[Message], budget: u32) -> Result<String> {
        let mut instruction = SUMMARY_INSTRUCTION.to_owned();
        if let Some(focus) = &self.focus {
            instruction.push_str("\n\nAdditional user focus for this summary: ");
            instruction.push_str(focus);
        }
        let mut messages = history.to_vec();
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: instruction }],
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

// ---------------------------------------------------------------------------
// cache-preserving micro-compaction (tool-result eviction)
// ---------------------------------------------------------------------------

/// Header line every eviction stub starts with. Together with the paired
/// `tool_use_id` carried by the `ContentBlock::ToolResult` block itself, this
/// tells the model which tool call the stub belonged to. (Tool results are
/// recognized structurally via `ContentBlock::ToolResult` — never parsed
/// heuristically out of unrelated text.)
pub const EVICTED_RESULT_MARKER_PREFIX: &str = "[evicted tool result";

/// Default count of head messages never touched by the eviction pass (the
/// anchored prefix). Keeping the head byte-stable is what preserves Anthropic
/// prompt-cache prefixes: cache hits extend up to the first changed message.
pub const DEFAULT_EVICTION_ANCHORED_PREFIX: usize = 4;

/// Default count of tail messages never touched by the eviction pass (the
/// recent window). Aligned with [`DEFAULT_KEEP_RECENT`] so the eviction pass
/// never stubs what a full compaction would keep verbatim anyway.
pub const DEFAULT_EVICTION_RECENT_WINDOW: usize = DEFAULT_KEEP_RECENT;

/// Default soft token threshold above which the eviction pass runs. Deliberately
/// below the warn line of typical large windows so stale tool payloads are
/// relieved before full compaction becomes necessary; callers wanting a
/// window-proportional policy convert it into this flat value at the config
/// layer (same convention as [`Thresholds`]).
pub const DEFAULT_EVICTION_SOFT_THRESHOLD_TOKENS: u64 = 100_000;

/// Default number of messages the eviction frontier moves at a time.
///
/// The frontier only ever sits on a multiple of this distance from the anchor,
/// so the ordinary turn that adds one message changes no request bytes and
/// keeps the provider cache prefix intact. Without batching, the boundary
/// advances every turn and re-reads everything behind it at full price.
pub const DEFAULT_EVICTION_BATCH_MESSAGES: usize = 12;

/// Parameters of the eviction pass (see [`evict_old_tool_results`]).
#[derive(Debug, Clone)]
pub struct EvictionConfig {
    /// First `anchored_prefix` messages are never touched (anchored prefix,
    /// cache-prefix preservation).
    pub anchored_prefix: usize,
    /// Last `recent_window` messages are never touched (recent window).
    pub recent_window: usize,
    /// Soft threshold (estimated tokens) above which
    /// [`should_evict_tool_results`] fires.
    pub soft_threshold_tokens: u64,
    /// Messages per eviction step (see
    /// [`DEFAULT_EVICTION_BATCH_MESSAGES`]); `0` is treated as `1`.
    pub batch_messages: usize,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            anchored_prefix: DEFAULT_EVICTION_ANCHORED_PREFIX,
            recent_window: DEFAULT_EVICTION_RECENT_WINDOW,
            soft_threshold_tokens: DEFAULT_EVICTION_SOFT_THRESHOLD_TOKENS,
            batch_messages: DEFAULT_EVICTION_BATCH_MESSAGES,
        }
    }
}

/// Eviction trigger policy: run the pass once estimated usage reaches the
/// soft threshold. Pure and parameterized; callers feed it the same
/// `resolve_used_tokens` result they already compute for the threshold check.
pub fn should_evict_tool_results(used_tokens: u64, cfg: &EvictionConfig) -> bool {
    used_tokens >= cfg.soft_threshold_tokens
}

/// Map every assistant `tool_use` id to its tool name (ids are unique per
/// conversation); used to name eviction stubs after the call they belonged to.
fn tool_use_names(history: &[Message]) -> HashMap<&str, &str> {
    let mut names = HashMap::new();
    for m in history {
        if m.role != Role::Assistant {
            continue;
        }
        for b in &m.content {
            if let ContentBlock::ToolUse { id, name, .. } = b {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }
    names
}

/// Eviction stub for one tool result: a single deterministic header line
/// naming the tool call it belonged to (tool name when known, else the bare
/// id). Deterministic in `(tool_use_id, name)` so a second pass reproduces it
/// byte for byte — the root of [`evict_old_tool_results`]'s idempotency.
fn evicted_result_stub(tool_use_id: &str, tool_name: Option<&str>) -> String {
    match tool_name {
        Some(name) => format!("{EVICTED_RESULT_MARKER_PREFIX} for {name} ({tool_use_id})]"),
        None => format!("{EVICTED_RESULT_MARKER_PREFIX} for {tool_use_id}]"),
    }
}

/// Cache-preserving micro-compaction: replace the payload of old tool results
/// with a one-line eviction stub, keeping everything else verbatim.
///
/// What is never touched:
/// - the first `cfg.anchored_prefix` messages (anchored prefix — with
///   Anthropic prompt caching the cache prefix extends up to the first changed
///   message, so a stable head keeps the prefix cacheable);
/// - the last `cfg.recent_window` messages (recent window; defaults to
///   [`DEFAULT_KEEP_RECENT`], matching full compaction's verbatim tail);
/// - non-tool-result content: user/assistant text, images, and any text
///   blocks sharing a message with an evicted result.
///
/// Tool results are recognized structurally (`ContentBlock::ToolResult`
/// blocks in user messages) — never via text heuristics. Only `content` is
/// replaced; the block keeps its `tool_use_id` and `is_error`, so pairing
/// integrity ([`find_pairing_violations`]) is unaffected.
///
/// How much is evicted is demand-driven, not all-or-nothing (see
/// [`eviction_frontier`]): the pass reclaims oldest-first only until the stubs
/// free `total - cfg.soft_threshold_tokens` tokens (the threshold doubles as
/// the reclamation target, so no caller has to pass it twice), and the
/// frontier only advances in `cfg.batch_messages` groups. A history already
/// under the threshold therefore passes through byte-for-byte even when the
/// caller invokes the pass.
///
/// Idempotent: the stub is a pure function of `(tool_use_id, tool name)`, so
/// running the pass again reproduces the same stubs and changes nothing
/// further. When the anchored prefix and the recent window overlap (short
/// history), the evictable range is empty and the history passes through
/// unchanged (saturating arithmetic, never panics).
/// Estimated tokens of one text, using the same split accounting as
/// [`estimate_tokens`]. Wrapping in a single-block message makes the flat
/// per-message overhead cancel out of any difference of two such values.
fn text_tokens(text: &str) -> u64 {
    estimate_tokens(
        &[Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }],
        DEFAULT_CHARS_PER_TOKEN,
    )
}

/// Tokens freed by stubbing every tool result in one message: payload cost
/// minus the stub it is replaced with. Non-tool content never shrinks, and a
/// message that is already a stub saves nothing, which is what keeps the
/// pass idempotent.
fn eviction_saving(message: &Message, names: &HashMap<&str, &str>) -> u64 {
    if message.role != Role::User {
        return 0;
    }
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id, content)),
            _ => None,
        })
        .map(|(tool_use_id, content)| {
            let stub = evicted_result_stub(tool_use_id, names.get(tool_use_id.as_str()).copied());
            text_tokens(content).saturating_sub(text_tokens(&stub))
        })
        .sum()
}

/// Exclusive index the eviction frontier stops at for this history.
///
/// Two rules make the pass cache-safe:
/// - **reclaim only what is asked for**: oldest-first, until the stubs free at
///   least `total - soft_threshold` tokens, so recent evidence survives until
///   it is actually needed;
/// - **move in batches**: the frontier advances a whole `batch_messages` group
///   at a time, so turns that merely append one message keep the request bytes
///   identical and the cached prefix alive.
fn eviction_frontier(
    history: &[Message],
    cfg: &EvictionConfig,
    names: &HashMap<&str, &str>,
    start: usize,
    end: usize,
) -> usize {
    if start >= end {
        return start;
    }
    let total = estimate_tokens(history, DEFAULT_CHARS_PER_TOKEN);
    let needed = total.saturating_sub(cfg.soft_threshold_tokens);
    if needed == 0 {
        return start;
    }
    let step = cfg.batch_messages.max(1);
    let savings: Vec<u64> = (start..end)
        .map(|i| eviction_saving(&history[i], names))
        .collect();
    let mut freed = 0u64;
    let mut frontier = start;
    while frontier < end {
        let group_end = (frontier + step).min(end);
        while frontier < group_end {
            freed += savings[frontier - start];
            frontier += 1;
        }
        if freed >= needed {
            break;
        }
    }
    frontier
}

pub fn evict_old_tool_results(history: &[Message], cfg: &EvictionConfig) -> Vec<Message> {
    let end = history.len().saturating_sub(cfg.recent_window);
    let start = cfg.anchored_prefix.min(end);
    if start >= end {
        return history.to_vec();
    }
    let names = tool_use_names(history);
    let limit = eviction_frontier(history, cfg, &names, start, end);
    let mut out = Vec::with_capacity(history.len());
    for (i, m) in history.iter().enumerate() {
        let has_tool_result = m.role == Role::User
            && i >= start
            && i < limit
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
        if !has_tool_result {
            out.push(m.clone());
            continue;
        }
        let content = m
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: evicted_result_stub(
                        tool_use_id,
                        names.get(tool_use_id.as_str()).copied(),
                    ),
                    is_error: *is_error,
                },
                other => other.clone(),
            })
            .collect();
        out.push(Message {
            role: m.role,
            content,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// system-reminder injection channel
// ---------------------------------------------------------------------------

/// Opening tag of an injected system reminder.
///
/// Re-exported from `wavecode-wire`, which holds the single definition (the
/// loop needs the same marker and may not depend on this crate).
pub use wavecode_wire::{SYSTEM_REMINDER_CLOSE, SYSTEM_REMINDER_OPEN, wrap_system_reminder};

/// Default cap on pending (not yet injected) reminders. At the cap new
/// reminders are dropped rather than queued — a bounded channel, never a
/// silent queue growth.
pub const DEFAULT_MAX_PENDING_REMINDERS: usize = 8;

/// True when `wrapped` is still present as a whole text block in
/// `history`'s trailing user-role entry (i.e. not yet consumed by a model
/// turn).
fn reminder_still_present(history: &[Message], wrapped: &str) -> bool {
    history
        .last()
        .filter(|m| m.role == Role::User)
        .is_some_and(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == wrapped))
        })
}

/// FIFO queue of `<system-reminder>` blocks waiting for the next user-role
/// entry. The single channel future callers (compaction notices, plan
/// nudges, …) use to reach the model:
///
/// 1. `enqueue` a reminder text at any time (deduplicated while it is still
///    pending or still present in the trailing user entry; capped);
/// 2. `flush` right before the next user-role entry enters the history — the
///    reminders merge into that entry (appended as text blocks) instead of
///    each spawning its own message.
#[derive(Debug, Clone)]
pub struct ReminderChannel {
    pending: VecDeque<String>,
    max_pending: usize,
}

impl ReminderChannel {
    /// Queue with the [`DEFAULT_MAX_PENDING_REMINDERS`] cap.
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_MAX_PENDING_REMINDERS)
    }

    /// Queue with an explicit cap; a cap of 0 rejects every enqueue (kept
    /// literal rather than clamped — a caller asking for no queue gets one).
    pub fn with_cap(max_pending: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            max_pending,
        }
    }

    /// Number of reminders waiting for injection.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Queue `text` for injection into the next user-role entry. Returns
    /// `false` (no change) when an identical reminder is already pending,
    /// when it is still present in `history`'s trailing user entry (injected
    /// but not yet consumed by a model turn), or when the pending cap is
    /// reached.
    pub fn enqueue(&mut self, text: &str, history: &[Message]) -> bool {
        let wrapped = wrap_system_reminder(text);
        if self.pending.iter().any(|p| *p == wrapped) || reminder_still_present(history, &wrapped) {
            return false;
        }
        if self.pending.len() >= self.max_pending {
            return false;
        }
        self.pending.push_back(wrapped);
        true
    }

    /// Drain all pending reminders into `history`: merged into the trailing
    /// user-role entry when there is one (each reminder appended as a whole
    /// text block), otherwise pushed as a fresh user-role entry carrying just
    /// the reminders. Returns the number of reminders injected (0 leaves the
    /// history untouched). Call this right before the next user-role entry
    /// enters the history.
    pub fn flush(&mut self, history: &mut Vec<Message>) -> usize {
        let drained: Vec<String> = self.pending.drain(..).collect();
        if drained.is_empty() {
            return 0;
        }
        let blocks: Vec<ContentBlock> = drained
            .into_iter()
            .map(|text| ContentBlock::Text { text })
            .collect();
        let count = blocks.len();
        match history.last_mut() {
            Some(m) if m.role == Role::User => m.content.extend(blocks),
            _ => history.push(Message {
                role: Role::User,
                content: blocks,
            }),
        }
        count
    }
}

impl Default for ReminderChannel {
    fn default() -> Self {
        Self::new()
    }
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

    #[test]
    fn estimate_tokens_counts_non_ascii_per_character() {
        // 300 CJK characters: a flat chars/4 division would claim 75 tokens
        // while reality is ~1 token/char — non-ASCII counts directly instead.
        let cjk: String = "中".repeat(300);
        let history = vec![user_text(&cjk)];
        assert_eq!(estimate_tokens(&history, 4), 300 + 4);
        // Mixed text: ASCII rides the ratio, CJK does not.
        let mixed = vec![user_text(&format!("{}{}", "x".repeat(400), cjk))];
        assert_eq!(estimate_tokens(&mixed, 4), 100 + 300 + 4);
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

    /// `/compact <focus>` steering reaches the summary request: the
    /// instruction message carries the user focus after the standard
    /// five-element prompt.
    #[tokio::test]
    async fn model_summary_focus_reaches_the_request() {
        const SCRIPT: &str =
            "## Goal\nx\n## Progress\ny\n## Key decisions\nz\n## File inventory\nf\n## Todo\nt";
        struct CapturingModel {
            seen: std::sync::Mutex<Option<String>>,
        }
        #[async_trait::async_trait]
        impl ChatModel for CapturingModel {
            async fn stream(
                &self,
                req: ChatRequest,
            ) -> wavecode_llm::Result<wavecode_llm::EventStream> {
                let last = req.messages.last().expect("instruction message");
                let ContentBlock::Text { text } = &last.content[0] else {
                    panic!("instruction should be a text block")
                };
                *self.seen.lock().unwrap() = Some(text.clone());
                Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::TextDelta {
                    text: SCRIPT.to_string(),
                })])))
            }
        }
        let model = Arc::new(CapturingModel {
            seen: std::sync::Mutex::new(None),
        });
        let strategy = ModelSummary::new(model.clone(), "mock".into())
            .with_focus("keep the api design decisions");
        let summary = strategy.summarize(&[], 777).await.unwrap();
        assert!(summary.contains("## Goal"), "{summary}");
        let seen = model.seen.lock().unwrap().clone().unwrap();
        assert!(
            seen.starts_with(SUMMARY_INSTRUCTION),
            "standard prompt preserved: {seen}"
        );
        assert!(
            seen.contains("Additional user focus for this summary: keep the api design decisions"),
            "focus appended: {seen}"
        );
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
                    ..Usage::default()
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

    /// Compaction retention invariant: the summary carries each
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

    // --- eviction (cache-preserving micro-compaction) ---

    fn tool_result_with(id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: content.to_owned(),
                is_error: false,
            }],
        }
    }

    /// 11-message history with three tool rounds; the evictable band depends
    /// on the config used with it.
    fn eviction_history() -> Vec<Message> {
        vec![
            user_text("Goal: fix the build."),
            assistant_text("Plan: read config first."),
            tool_use("t1"),
            tool_result_with("t1", &"a".repeat(5000)),
            assistant_text("Config looks stale."),
            tool_use("t2"),
            tool_result_with("t2", &"b".repeat(5000)),
            assistant_text("Patched."),
            user_text("run tests next"),
            tool_use("t3"),
            tool_result("t3", false),
        ]
    }

    /// The pass is demand-driven: it reclaims only what the soft threshold
    /// asks for, oldest first. Barely crossing the line must stub far less
    /// than being asked to reclaim everything.
    #[test]
    fn evict_reclaims_only_what_the_threshold_asks_for() {
        let history = eviction_history();
        let stubbed = |soft_threshold_tokens: u64| {
            let cfg = EvictionConfig {
                anchored_prefix: 1,
                recent_window: 1,
                batch_messages: 1,
                soft_threshold_tokens,
            };
            evict_old_tool_results(&history, &cfg)
                .iter()
                .flat_map(|m| m.content.iter())
                .filter(|b| {
                    matches!(b, ContentBlock::ToolResult { content, .. }
                    if content.starts_with(EVICTED_RESULT_MARKER_PREFIX))
                })
                .count()
        };
        let total = estimate_tokens(&history, DEFAULT_CHARS_PER_TOKEN);
        // Demand for exactly nothing: the history is over the line by one
        // token, so a single reclaim step (or none) is enough.
        let barely = stubbed(total.saturating_sub(1));
        let everything = stubbed(0);
        assert!(
            barely < everything,
            "barely-over cleared {barely} of {everything}: the pass is still all-or-nothing"
        );
        assert_eq!(everything, stubbed(0), "fixture must be stable");
        assert!(
            everything > 0,
            "fixture evicts nothing: comparison is vacuous"
        );
    }

    #[test]
    fn evict_preserves_anchored_prefix_and_recent_window() {
        let history = eviction_history();
        let cfg = EvictionConfig {
            anchored_prefix: 2,
            recent_window: 2,
            // Reclaim-everything demand: these fixtures assert the pass does
            // stub the middle range, so the frontier must reach the range end.
            soft_threshold_tokens: 0,
            ..Default::default()
        };
        let out = evict_old_tool_results(&history, &cfg);
        // Message count unchanged (payload replacement, never removal).
        assert_eq!(out.len(), history.len());
        // Evictable band [2..8]: results at index 3 (t1) and 6 (t2) stubbed,
        // each naming the tool call it belonged to.
        for (idx, id) in [(3usize, "t1"), (6, "t2")] {
            assert!(
                matches!(&out[idx].content[0],
                    ContentBlock::ToolResult { content, is_error: false, .. }
                    if content.starts_with(EVICTED_RESULT_MARKER_PREFIX)
                        && content.contains("read_file") && content.contains(id)
                ),
                "message[{idx}] should carry the eviction stub for {id}: {:?}",
                out[idx].content[0]
            );
            let evicted = match &out[idx].content[0] {
                ContentBlock::ToolResult { content, .. } => content.clone(),
                _ => unreachable!(),
            };
            assert!(
                !history[idx].content.iter().any(
                    |b| matches!(b, ContentBlock::ToolResult { content, .. } if *content == evicted)
                ),
                "payload must not survive inline"
            );
        }
        // Anchored prefix (0..2), recent window (8..10) and all text entries
        // pass through byte-identical.
        assert_eq!(&out[..2], &history[..2]);
        assert_eq!(&out[8..], &history[8..]);
        for idx in [4usize, 5, 7] {
            assert_eq!(out[idx], history[idx], "assistant entry [{idx}] untouched");
        }
        // Replacing only the payload keeps pairing integrity intact.
        assert_eq!(find_pairing_violations(&out), Vec::<String>::new());
    }

    #[test]
    fn evict_is_idempotent() {
        let history = eviction_history();
        let cfg = EvictionConfig {
            anchored_prefix: 2,
            recent_window: 2,
            ..Default::default()
        };
        let once = evict_old_tool_results(&history, &cfg);
        let twice = evict_old_tool_results(&once, &cfg);
        assert_eq!(once, twice, "second pass must change nothing further");
    }

    #[test]
    fn evict_never_touches_non_tool_entries() {
        let history = vec![
            user_text("goal"),
            tool_use("t1"),
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "long payload".into(),
                        is_error: true,
                    },
                    ContentBlock::Text {
                        text: "steering note".into(),
                    },
                ],
            },
            assistant_text("done"),
        ];
        let cfg = EvictionConfig {
            anchored_prefix: 2,
            recent_window: 1,
            soft_threshold_tokens: 0,
            ..Default::default()
        };
        let out = evict_old_tool_results(&history, &cfg);
        assert_eq!(out.len(), 4);
        // Head and tail entries untouched.
        assert_eq!(out[0], history[0]);
        assert_eq!(out[3], history[3]);
        // In the evicted message only the ToolResult payload is replaced
        // (is_error preserved); the sibling text block survives verbatim.
        assert!(matches!(
            &out[2].content[0],
            ContentBlock::ToolResult { is_error: true, content, .. }
                if content.starts_with(EVICTED_RESULT_MARKER_PREFIX)
        ));
        assert!(matches!(
            &out[2].content[1],
            ContentBlock::Text { text } if text == "steering note"
        ));
    }

    #[test]
    fn evict_stub_falls_back_to_bare_id_for_orphans() {
        let history = vec![
            user_text("a"),              // 0: anchored
            tool_use("t1"),              // 1
            tool_result("t1", false),    // 2: evicted, named after the call
            assistant_text("mid"),       // 3
            tool_result("ghost", false), // 4: orphan -> bare-id stub
            assistant_text("end"),       // 5: recent window
        ];
        let cfg = EvictionConfig {
            anchored_prefix: 1,
            recent_window: 1,
            soft_threshold_tokens: 0,
            ..Default::default()
        };
        let out = evict_old_tool_results(&history, &cfg);
        assert!(matches!(
            &out[2].content[0],
            ContentBlock::ToolResult { content, .. }
                if content == "[evicted tool result for read_file (t1)]"
        ));
        assert!(matches!(
            &out[4].content[0],
            ContentBlock::ToolResult { content, .. }
                if content == "[evicted tool result for ghost]"
        ));
        assert_eq!(out[0], history[0]);
        assert_eq!(out[5], history[5]);
    }

    #[test]
    fn evict_noop_when_windows_overlap() {
        let history = vec![
            user_text("a"),
            tool_use("t1"),
            tool_result("t1", false),
            assistant_text("b"),
        ];
        // anchored 2 + recent 3 > len 4: the evictable range is empty.
        let cfg = EvictionConfig {
            anchored_prefix: 2,
            recent_window: 3,
            ..Default::default()
        };
        assert_eq!(
            evict_old_tool_results(&history, &cfg),
            history,
            "overlapping windows leave a short history untouched"
        );
    }

    #[test]
    fn should_evict_policy_fires_at_soft_threshold() {
        let cfg = EvictionConfig::default();
        assert!(!should_evict_tool_results(
            cfg.soft_threshold_tokens - 1,
            &cfg
        ));
        assert!(should_evict_tool_results(cfg.soft_threshold_tokens, &cfg));
        // Default coherence: the recent window matches full compaction's
        // verbatim tail so the two passes never fight over the same entries.
        assert_eq!(cfg.recent_window, DEFAULT_KEEP_RECENT);
    }

    // --- system-reminder injection channel ---

    #[test]
    fn wrap_system_reminder_format() {
        assert_eq!(
            wrap_system_reminder("hi"),
            "<system-reminder>\nhi\n</system-reminder>"
        );
    }

    #[test]
    fn reminder_dedup_while_pending_and_while_present() {
        let mut ch = ReminderChannel::new();
        let mut history: Vec<Message> = Vec::new();
        assert!(ch.enqueue("plan nudge", &history));
        assert!(
            !ch.enqueue("plan nudge", &history),
            "identical reminder must not re-queue while pending"
        );
        // Empty history: flush pushes a fresh user entry carrying the block.
        assert_eq!(ch.flush(&mut history), 1);
        assert_eq!(ch.pending(), 0);
        assert_eq!(history.len(), 1);
        assert!(
            !ch.enqueue("plan nudge", &history),
            "identical reminder must not re-queue while still present in the trailing user entry"
        );
        // A model turn consumes the trailing entry; the same reminder may be
        // queued again (recurring nudges stay possible).
        history.push(assistant_text("ok"));
        assert!(ch.enqueue("plan nudge", &history));
    }

    #[test]
    fn reminder_cap_rejects_new_reminders() {
        let mut history = vec![user_text("hi")];
        let mut ch = ReminderChannel::with_cap(2);
        assert!(ch.enqueue("a", &history));
        assert!(ch.enqueue("b", &history));
        assert!(
            !ch.enqueue("c", &history),
            "cap reached: new reminder dropped"
        );
        assert_eq!(ch.pending(), 2);
        // Dedup does not consume cap capacity.
        assert!(!ch.enqueue("a", &history));
        assert_eq!(ch.pending(), 2);
        // A literal cap of 0 rejects everything.
        assert!(!ReminderChannel::with_cap(0).enqueue("x", &history));
        // Flushing frees capacity.
        assert_eq!(ch.flush(&mut history), 2);
        assert_eq!(ch.pending(), 0);
        assert!(ch.enqueue("c", &history));
    }

    #[test]
    fn reminder_flush_merges_into_trailing_user_entry() {
        let mut ch = ReminderChannel::new();
        let mut history = vec![user_text("question"), assistant_text("answer")];
        assert!(ch.enqueue("compaction notice", &history));
        assert_eq!(ch.flush(&mut history), 1);
        // Trailing entry is assistant: reminders arrive as a fresh user entry.
        assert_eq!(history.len(), 3);
        assert!(matches!(
            &history[2].content[0],
            ContentBlock::Text { text } if text == &wrap_system_reminder("compaction notice")
        ));
        assert_eq!(find_pairing_violations(&history), Vec::<String>::new());

        // Trailing entry is user: reminders merge into it as extra text
        // blocks; existing content is kept.
        assert!(ch.enqueue("second notice", &history));
        assert_eq!(ch.flush(&mut history), 1);
        let last = history.last().unwrap();
        assert_eq!(last.content.len(), 2);
        assert!(matches!(
            &last.content[0],
            ContentBlock::Text { text } if text == &wrap_system_reminder("compaction notice")
        ));
        assert!(matches!(
            &last.content[1],
            ContentBlock::Text { text } if text == &wrap_system_reminder("second notice")
        ));
    }

    #[test]
    fn reminder_flush_empty_is_noop() {
        let mut ch = ReminderChannel::new();
        let mut history = vec![user_text("hi")];
        assert_eq!(ch.flush(&mut history), 0);
        assert_eq!(history.len(), 1, "empty flush must not add a message");
    }
}
