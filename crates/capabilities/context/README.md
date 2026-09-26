# crates/capabilities/context/ — context-management pipeline (accounting, thresholds, compaction)

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | The pipeline in one module: token accounting (`estimate_tokens`, `resolve_used_tokens`, split ASCII/non-ASCII estimate), three-level `Thresholds`/`BudgetLevel` (20k/13k/3k margins below the window top), the `CompactionStrategy` trait with the `ModelSummary` implementation, `compact_history` / `normalize_history` / `find_pairing_violations`, cache-preserving eviction (`evict_old_tool_results`, `EvictionConfig`), and the `ReminderChannel` injection queue |
| `src/spill.rs` | `SpillStore`: oversized tool outputs persisted to a home-scoped side-store (32 MB total cap, manifest, oldest-first eviction); `prune_tool_output` emits the `PRUNE_MARKER`, a `spill://` URI, and a head excerpt for the model |

There is exactly one compaction pipeline; trigger timing is orchestrated by
core, and the trigger kinds share the single `compact_history` entry point.
`CompactionStrategy` is the replaceable seam (the first implementation
summarizes in one model call into five pinned sections), while
`normalize_history` and `find_pairing_violations` form the pairing-integrity
contract shared by the compaction and restore paths. Dependencies point only
at foundation crates — `wavecode-llm` for the summary call, `wavecode-config`
for `home_dir`, `wavecode-wire` for the `system-reminder` markers — and
`spill.rs` states the boundary explicitly: no runtime, transport, or
UI-layer components.
