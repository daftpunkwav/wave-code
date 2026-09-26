# Defensive patterns

English | [中文](defensive-patterns.zh.md)

Recurring guardrails in this codebase, each with a one-line anchor. New code should reuse these instead of inventing local variants.

| Pattern | Rule | Anchor |
| --- | --- | --- |
| Fail-closed verdicts | When unsure, refuse: the sandbox probe chain ends in a backend that refuses every spawn, and unavailable Windows confinement fails closed with an explicit reason. | `crates/capabilities/sandbox/src/chain.rs`, `src/windows.rs` |
| Business errors as data | Tool failures travel as `Ok(ToolOutput { is_error: true, .. })`, never panics; `Err` means implementation fault, surfaced with a `tool fault:` prefix. | `crates/capabilities/tools/src/lib.rs`, `crates/operations/bootstrap/src/tool_adapter.rs` |
| Atomic writes | Mutating file tools write temp + rename so a mid-write failure leaves the original intact. | `crates/capabilities/tools/src/fs/write.rs`, `fs/edit.rs` |
| Deny rules bind first | Deny rules apply in every mode and are evaluated before allow rules or the mode's default; an unmatched input never permits more than its mode allows. | `crates/capabilities/sandbox/src/lib.rs` |
| Approval timeout → deny | Parked approval waits resolve to `Deny` on expiry or dropped waiter; the loop never parks forever and late decisions are dropped. | `crates/operations/bootstrap/src/gate_adapter.rs`, `crates/safety/gate/src/lib.rs` |
| Append-only journals + versioned formats | Turn records append as JSONL with a `{"format":1}` header; migration happens on read only, loaders never rewrite user files. | `crates/state/persistence/src/lib.rs` |
| Warn-and-continue assembly | Broken plugins, skill packs, hooks, or agent-definition files warn with a reason and are skipped; assembly never fails on optional surface. | `crates/runtime/plugin/src/lib.rs`, `crates/capabilities/skills/src/plugin.rs`, `crates/capabilities/tools/src/agent_task_tool.rs` |
| Canonicalize-based containment | Path checks compare canonicalized real paths on both sides (component-level, not string prefix) to reject escapes and sibling confusion. | `crates/capabilities/tools/src/path_guard.rs`, `crates/capabilities/memory/src/instructions.rs` |
| Redacted Debug for secrets | `Credential`'s manual `Debug` prints the `REDACTED` placeholder; transcript redaction masks known values longest-first. | `crates/foundation/auth/src/lib.rs`, `crates/safety/secrets/src/lib.rs` |
| Deterministic stubs in outputs | Eviction stubs are a pure function of `(tool_use_id, tool name)`, so repeated passes are byte-identical — idempotency by construction. | `crates/capabilities/context/src/lib.rs` (`EVICTED_RESULT_MARKER_PREFIX`) |
| Dedup by canonical path | Memory/instruction assembly deduplicates files by canonicalized path so the same file is never concatenated twice. | `crates/capabilities/memory/src/instructions.rs` |
| Bounded channels, drop not grow | Reminder queue drops at its cap instead of growing silently; notification queues behave the same. | `crates/capabilities/context/src/lib.rs`, `crates/runtime/child/src/lib.rs` |
| Poison-recovery locks | Single-operation critical sections recover the guard after a panic (no half-written invariant to protect); the policy is centralized per crate. | `crates/capabilities/tools/src/lib.rs::lock`, `crates/safety/gate/src/lib.rs` |
| Total fallback slots | Every declared tool call gets exactly one result slot, with a trailing internal-error fallback — pairing can never break, even on future pipeline changes. | `crates/runtime/runner/src/lib.rs::execute_calls` |
| Explicit invalid-config failure | Invalid sandbox rules fail at startup (`Sandbox::new` rejects them); silent downgrades are forbidden. | `crates/capabilities/sandbox/src/lib.rs` |
| Interrupts preserve pairing | A pre-tool interrupt synthesizes interrupted results for every declared call instead of executing any — the model sees one result per call, always. | `crates/runtime/runner/src/lib.rs::run_turn` checkpoint 3 |
| One-shot approval decisions | `decide` takes the slot; late decisions for consumed ids return false and are dropped, so a stale UI click can never approve a future call. | `crates/safety/gate/src/lib.rs` |
| Frozen reader snapshots | Conversation readers hold an `Arc` snapshot that never changes under them; all mutation flows through `push`. | `crates/state/store/src/lib.rs` |
| Idempotency keys on side effects | `RunContext::idempotency_key(scope)` namespaces run-scoped effects so retries of the same submission deduplicate downstream. | `crates/runtime/runner/src/lib.rs` |
| Deterministic, locked wire output | `Registry::specs()` sorts by name; wire event tags are locked by a test; compaction summary section titles are pinned verbatim — output is reproducible. | `crates/capabilities/tools/src/lib.rs`, `crates/foundation/wire/src/lib.rs`, `crates/capabilities/context/src/lib.rs` |
| Drop empty inputs early | Empty steering/injection texts are dropped before entering history because providers reject empty user messages. | `crates/runtime/runner/src/lib.rs::steer` / `inject` |
| Corrupt journal lines counted, not fatal | JSONL loaders skip and count corrupt lines (`load_reported` returns the count) so strict callers can refuse partial loads while resume still works. | `crates/state/persistence/src/lib.rs` |
| Deduplicate pending reminders | `ReminderChannel::enqueue` refuses an identical reminder that is pending or still unconsumed in the trailing user entry. | `crates/capabilities/context/src/lib.rs` |
| Late registration reaches shared handles | The tool registry is interior-mutable, so tools registered after assembly are visible through every already-shared `Arc` without rebuilds. | `crates/capabilities/tools/src/lib.rs` |
| Attributes, never names | Policy and dispatch read `is_read_only` / `is_destructive` from the tool itself; unregistered names stay serial and destructive so unknowns take the cautious path. | `crates/operations/bootstrap/src/policy_adapter.rs`, `tool_adapter.rs` |
| Failure severity picks the recovery | Auto-compaction failure downgrades to a warning and continues; blocking-compaction failure aborts the turn — the trigger decides the blast radius. | `crates/runtime/runner/src/lib.rs::run_turn` |
| Labels validated before use | Checkpoint and snapshot labels pass a validator before touching the filesystem, and restore-time relative paths reject absolute paths and `..` — defense in depth against hand-edited manifests. | `crates/state/checkpoint/src/lib.rs` |
| Recordings are validated, not trusted | Replay eval checks the wire contract (ordering, pairing, settle-once) before scoring, and malformed JSONL fails loudly with the line number. | `crates/operations/eval/src/replay.rs` |
| Mechanical edits get compile + diff review | After any scripted multi-file change, `cargo check` plus a `git diff --stat` line-delta review is mandatory; on suspected corruption, stop editing and recover from `git fsck --unreachable` first. | `docs/postmortem/2026-09-13-bulk-regex-corruption.md` |

Two meta-rules govern when to reach for these:

1. **Prefer explicit failure over silent downgrade** (`Sandbox::new` rejecting bad rules) — except for *optional* surface (plugins, skills, agent defs), where the rule inverts to warn-and-skip because one bad pack must not take down a session.
2. **Determinism over cleverness** — eviction stubs, sorted `Registry::specs()`, and locked wire tags all choose reproducible output so caches, snapshots, and tests stay stable across runs.
