# Subsystem: context engineering

English | [中文](context-engineering.zh.md)

Context management spans two crates plus glue: the persisted conversation and budget levels (`crates/state/store`), the pipeline passes (`crates/capabilities/context`), and the runner-facing compactor adapter (`crates/operations/bootstrap/src/compactor.rs`).

## Conversation and budget levels (`crates/state/store/src/lib.rs`)

`Conversation` is append-only with a single write entry (`push`). Readers get a frozen `Arc` snapshot (`snapshot()`), so sampling never races appends; `replace()` swaps the whole history (compaction) and the caller re-establishes the usage carry via `settle`. `normalize_history` merges adjacent same-role entries (some providers reject them); `find_pairing_violations` detects breaks.

Budgets are three levels of remaining tokens (`check_budget`):

| Level | Threshold (constant) | Effect in the loop |
| --- | --- | --- |
| `Warn` | `BUDGET_WARN_REMAINING = 20_000` | one-time warning per turn |
| `AutoCompact` | `BUDGET_AUTO_COMPACT_REMAINING = 13_000` | compact before the next sample |
| `Blocking` | `BUDGET_BLOCKING_REMAINING = 3_000` | compact or abort the turn |

`CONTEXT_OVERHEAD_TOKENS = 2_000` covers the system prompt on the fallback estimate path; `estimate_tokens` (chars / 4) is explicitly non-authoritative — provider usage always wins (`CONTEXT_OVERHEAD_TOKENS` matches `SYSTEM_OVERHEAD_TOKENS` in the context crate).

## Compaction strategy

`crates/capabilities/context/src/lib.rs` owns `Thresholds` (the same 20k/13k/3k margins, `check` probes deepest-first, `validate` rejects inverted margins) and `CompactionStrategy`. The only shipped strategy is `ModelSummary`: one model call producing a five-section summary with titles pinned verbatim — `## Goal` / `## Progress` / `## Key decisions` / `## File inventory` / `## Todo` — keeping concrete filenames, commands, and errors. The new history is the summary message plus the most recent `DEFAULT_KEEP_RECENT = 10` verbatim messages (`compact_history`), re-normalized for pairing.

`crates/operations/bootstrap/src/compactor.rs` adapts this to the runner's `Compactor` seam (`ContextCompactor`): filters empty texts, maps roles, reports failures as `CompactError::Failed`, and estimates summary tokens for `CompactCompleted`. Trigger timing (auto / blocking / reactive / manual) stays in the loop — one trigger pipeline, replaceable strategy.

## The eviction pass (`evict_old_tool_results`)

Cache-preserving micro-compaction, also in `crates/capabilities/context/src/lib.rs`: replace the payload of *old* tool results with a one-line stub, leave everything else verbatim.

- Never touched: the first `anchored_prefix = 4` messages (stable head ⇒ Anthropic prompt-cache prefix extends to the first change), the last `recent_window = 10` messages (aligned with full compaction's verbatim tail), and all non-tool-result content.
- Tool results are recognized structurally (`ContentBlock::ToolResult`), never parsed out of text; the stub keeps `tool_use_id` and `is_error`, so pairing checks still pass.
- Stubs are deterministic in `(tool_use_id, tool_name)` starting with `EVICTED_RESULT_MARKER_PREFIX = "[evicted tool result"` — running the pass twice reproduces the same bytes, which is what makes it **idempotent**.
- **Demand-driven, not all-or-nothing**: `DEFAULT_EVICTION_SOFT_THRESHOLD_TOKENS = 100_000` doubles as trigger and reclamation target, so the pass reclaims oldest-first only until the stubs free `total - soft_threshold` tokens. A history already under the line passes through byte-for-byte even when the caller invokes the pass, and recent evidence survives until it is actually needed.
- **Batch-aligned frontier**: the boundary only lands on a multiple of `batch_messages` (`DEFAULT_EVICTION_BATCH_MESSAGES = 12`) from the anchor, so the ordinary turn that appends one message changes no request bytes at all.
- Measured on synthetic growth (one `grep` with a ~1k-token result per turn, 159 request transitions): a per-message boundary diverged **62** times, the batched boundary **16**. Every divergence re-reads everything behind it at full input price, so this is a bill, not a nicety. Pinned by `crates/operations/bootstrap/tests/cache_prefix_stability.rs`, which also asserts zero divergence below the threshold and idempotency.
- Overlapping prefix/window on short histories leaves the evictable range empty, so the history passes through unchanged (saturating arithmetic, never panics).

## The `<system-reminder>` channel

`wrap_system_reminder` wraps text in the canonical `<system-reminder>` … `</system-reminder>` block — the single injection format for compaction notices, plan nudges, and similar meta text. `ReminderChannel` is a bounded FIFO (`DEFAULT_MAX_PENDING_REMINDERS = 8`; at the cap new reminders are dropped, never queued into unbounded growth). `enqueue` deduplicates against pending items and against a reminder still sitting in the trailing user entry; `flush` merges everything into the trailing user entry (or pushes a fresh one) right before the next user-role entry lands.

## Spill store (`crates/capabilities/context/src/spill.rs`)

Oversized tool outputs leave the history instead of being dropped: `prune_tool_output` replaces content above `DEFAULT_PRUNE_THRESHOLD_CHARS = 8192` with a 1024-char head plus `PRUNE_MARKER` (`"[pruned: output spilled to side-store]"`) and writes the full text to a `SpillStore` (`spill://` URIs, `SPILL_TOTAL_CAP_BYTES = 32 MiB` total cap, ids validated on read). The `spill` tool (`crates/capabilities/tools/src/spill_tool.rs`) reads a URI back on demand; `default_spill_store_root()` pins the on-disk location. Token accounting never sees the spilled tail — the marker text is all that remains in history.
