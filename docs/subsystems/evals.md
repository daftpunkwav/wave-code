# Subsystem: evals

Quality assurance runs in four tiers, ordered by cost. Everything in tiers 1, 2, and 4 is offline and keyless; tier 3 is manual today — stated plainly so nobody mistakes CI green for live verification.

## Tier 1 — unit and integration tests

Unit tests sit next to the code (`#[cfg(test)] mod tests`); cross-crate behavior lives in `crates/*/tests/`. The rule set is in `docs/development.md`: no network, no model keys, no OS-specific requirements; scripted models and offline fixtures are the pattern. The scripted-model idiom in action: `crates/runtime/runner/tests/benchmarks.rs` drives the real `RunLoop` with a scripted `ModelGateway`, stub policy/hooks/approvals, and a fixed `RunConfig` — loop logic is tested against a deterministic model, never a live provider. CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked` on Linux, Windows, and macOS.

## Tier 2 — recorded-session replay

A recording is a JSON array of wire `Event` values (`id` plus a flattened `EventMsg`, snake_case `type` tags — the tag format is locked by a test in `crates/foundation/wire/src/lib.rs`). Committed fixtures live in `benchmarks/fixtures/`, each pinning one user-visible signal:

| Fixture | What it pins |
| --- | --- |
| `basic.json` | paired begin/end, one success observation, exact timeline text |
| `approval.json` | `ApprovalRequested` stays visible in order, call still pairs |
| `interrupt.json` | `TurnCompleted { interrupted: true }` noted; open call marked truncated |
| `truncation.json` | unpaired end visible, compact step, error observation carries `[error]` |

- **Snapshot goldens**: `crates/operations/replay/tests/snapshot_replay.rs` folds each fixture through `replay_to_trajectory` and asserts the exact step and observation sequences plus the exact timeline text (`"#2 tool:shell: c1 started"`-style). Display regressions, dropped approval markers, and lost truncation marks fail loudly here, not in front of a user.
- **Recording contract validation**: `crates/operations/eval/src/replay.rs` validates a recording against the wire protocol contract before scoring it — `validate_contract` checks per submission that `TurnStarted` comes first and fires once, deltas never follow their `AgentMessageComplete`, begin/end calls pair (unopened ends and never-ended calls are violations, except in interrupted turns), `TokenCount` settles once, and nothing arrives after `TurnCompleted`; violations accumulate as human-readable lines rather than stopping at the first. `evaluate_recorded` then scores recorded assistant text against `must_contain` expectations (`ReplayReport::pass_rate`), and `read_events_jsonl` loads a recording one `Event` per line (malformed lines fail loudly with the line number — a recording is a test fixture, not untrusted data). The behavioral harness in the same crate (`EvalCase` with `must_contain` / `must_not_contain` over final history through any `TurnDriver`) complements this for live driver behavior.

## Tier 3 — real-API e2e (manual)

There is no automated live-API test tier. The only live path is a gated smoke test:

```
LIVE=1 cargo test -p runtime-runner --test benchmarks live_gate_smoke -- --nocapture
```

Without `LIVE=1` it prints SKIP and passes; with `LIVE=1` but no provider key it also skips. Real end-to-end verification against providers is a manual step before releases — this is a known v1 gap, not a claim of coverage.

## Tier 4 — perf gates

Timing policy lives in `benchmarks/baseline.json` (committed medians plus `warn_beyond` / `fail_beyond` multipliers — single source of truth, read by tests so updates never require test edits):

- `PASS` at or under `warn_beyond × median`; `WARN` above that but within `fail_beyond × median` (test still passes); `FAIL` only beyond 10× or on any correctness mismatch. Medians are deliberately generous so CI noise warns instead of failing.
- Gates: `replay_goldens_stay_fast_against_baseline` (`crates/operations/replay/tests/snapshot_replay.rs`) times the four golden fixtures including rendering; `crates/runtime/runner/tests/benchmarks.rs` carries `continuation_micro_bench` (scripted model, 8 tool rounds through the real `RunLoop`) and `session_open_bench` (offline assembly + 5 scripted turns), both reporting JSON lines.
- `benchmarks/run.rs` is the plain-documented harness — the human-readable spec, kept in sync with the executable tests when rounds, bounds, or fixture shapes change. See `benchmarks/README.md`.

## Runtime measurement (what the tiers judge)

The tiers above verify structure; they cannot say whether a real session got
better. The runtime spine fills that gap:

- `runtime-runner` tags every `ToolCallEnd` with the dispatch exit that
  produced it (`ToolOutcome`) and the body's duration.
- `operations-observe` folds a session's event stream into `Metrics`, with
  per-tool `ToolStat` buckets and a `cache_read_share`.
- `operations-bootstrap`'s `metrics_tap` attaches to the actor client, so
  every journaled session appends one `TurnSample` per finished turn to
  `~/.wavecode/metrics/turns.jsonl` (a failed write warns; it never fails the
  turn it observes).
- `wavecode metrics [--session <id>] [--json]` aggregates the ledger into a
  per-model, per-tool table ranked by call volume. `success` covers executed
  calls only, so a tool that is mostly refused is not scored as broken.

Read-side numbers are the baseline for context-engineering and policy
changes: run the command before and after, and quote both.

## When to touch which tier

New user-visible structural behavior (a new `EventMsg` with trajectory semantics, a changed timeline rendering) → add or extend a tier-2 golden. A bug fix → tier-1 regression test next to the fix. Perf-sensitive loop changes → check the tier-4 gates print PASS, and re-baseline deliberately if the medians legitimately moved.
