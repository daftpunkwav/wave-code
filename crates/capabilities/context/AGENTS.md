# capabilities/context/ agent rules

The pipeline map lives in [README.md](README.md).

## Compaction and eviction

- Compaction enters through `compact_history`. Trigger timing stays
  in the run loop.
- `normalize_history` and `find_pairing_violations` are the pairing
  contract for compaction and restore.
- Eviction recognizes tool results as `ContentBlock::ToolResult`.
- A stub keeps `tool_use_id` and `is_error`. It is a pure function of
  `(tool_use_id, tool name)` and starts with
  `EVICTED_RESULT_MARKER_PREFIX`.
- The anchored prefix and the recent window are not evicted.
- A history under the soft threshold passes through unchanged.
- The frontier uses `step = batch_messages.max(1)`. Each group ends
  at `(frontier + step).min(end)`. It stops once freed tokens reach
  `total - soft_threshold`. A history already under the soft
  threshold leaves the frontier at the start index.
- `operations/bootstrap/tests/cache_prefix_stability.rs` stays green.
- Section titles of a model summary stay pinned verbatim.

## Dependencies

- Workspace dependencies are `infrastructure-base`, `wavecode-llm`,
  `wavecode-config`, and `wavecode-wire`.
- Spill files live outside the tool cwd. `spill.rs` does not depend
  on runtime, transport, or a UI crate.
