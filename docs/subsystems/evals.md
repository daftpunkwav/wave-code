# Subsystem: evals

Quality assurance runs in five tiers, ordered by cost. Everything in tiers 1, 2, and 5 is offline and keyless; tiers 3 and 4 need a real model and are run by a human on purpose — stated plainly so nobody mistakes CI green for live verification.

## Tier 1 — unit and integration tests

Unit tests sit next to the code (`#[cfg(test)] mod tests`); cross-crate behavior lives in `crates/*/tests/`. The rule set is in `docs/development.md`: no network, no model keys, no OS-specific requirements; scripted models and offline fixtures are the pattern. The scripted-model idiom in action: `crates/runtime/runner/tests/benchmarks.rs` drives the real `RunLoop` with a scripted `ModelGateway`, stub policy/hooks/approvals, and a fixed `RunConfig` — loop logic is tested against a deterministic model, never a live provider. CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked` on Linux, Windows, and macOS.

## Tier 2 — recorded-session scoring

A recording is a JSON array of wire `Event` values (`id` plus a flattened `EventMsg`, snake_case `type` tags — the tag format is locked by a test in `crates/foundation/wire/src/lib.rs`). Recordings come from `wavecode exec --json` sessions (see `docs/cookbook/recording-and-replaying.md`).

- **Recording contract validation**: `crates/operations/eval/src/replay.rs` validates a recording against the wire protocol contract before scoring it — `validate_contract` checks per submission that `TurnStarted` comes first and fires once, deltas never follow their `AgentMessageComplete`, begin/end calls pair (unopened ends and never-ended calls are violations, except in interrupted turns), `TokenCount` settles once, and nothing arrives after `TurnCompleted`; violations accumulate as human-readable lines rather than stopping at the first. `evaluate_recorded` then scores recorded assistant text against `must_contain` expectations (`ReplayReport::pass_rate`), and `read_events_jsonl` loads a recording one `Event` per line (malformed lines fail loudly with the line number — a recording is a test fixture, not untrusted data). The behavioral harness in the same crate (`EvalCase` with `must_contain` / `must_not_contain` over final history through any `TurnDriver`) complements this for live driver behavior.

## Tier 3 — real-API e2e (manual)

There is no automated live-API test tier. The only live path is a gated smoke test:

```
LIVE=1 cargo test -p runtime-runner --test benchmarks live_gate_smoke -- --nocapture
```

Without `LIVE=1` it prints SKIP and passes; with `LIVE=1` but no provider key it also skips. Real end-to-end verification against providers is a manual step before releases — this is a known v1 gap, not a claim of coverage.

## Tier 4 — task-level suite (`wavecode eval tasks`)

Tier 1-2 judge structure; this tier judges whether a real session got the
work done. A task is a prompt plus a tiny repository, and its verdict comes
from the workspace after the turn, never from the model's own summary.

```
wavecode eval tasks --permission-mode wave                 # whole suite
wavecode eval tasks --filter rust --json --out rust.json   # a slice, as JSON
```

Layout: one directory per task under `benchmarks/tasks/<id>/`, holding
`task.toml` (the manifest — that exact file name is what marks a task) and
`workspace/` (the fixture copied into a throwaway work root). Each task
spawns its own `wavecode exec --json` child in that copy, so the agent works
in isolation and the committed fixture is never modified.

Judging is `operations-eval`'s `task` module: an assertion is either a
command run by argv (no shell, so one manifest works on every platform), or
a file condition — `file_equals`, `file_contains`, `file_not_contains`,
`file_exists`, `file_absent`, `file_unchanged`. A task passes
when its agent step ended cleanly *and* every assertion holds; an
`interrupted`, capped, or non-zero-exiting step fails even if the files look
right, because a crash leaves no basis for trusting them.

Offline gates in `crates/frontends/harness/src/task_eval.rs` keep the suite
honest without spending a token:

- `committed_tasks_are_well_formed` — every manifest loads, ids are unique
  and match their directory, and any cargo fixture carries its own
  `[workspace]` table (without it the outer workspace swallows the copy and
  the build assertions lie).
- `no_committed_task_is_already_solved` — the pristine fixture must fail at
  least one assertion, or the task is a free point that measures nothing.
- `every_committed_task_has_a_solution_that_passes` — a known-good end state
  per task must satisfy the assertions, so a typo in an expected string is
  caught here instead of by a live run.

Cost and scheduling: each task is one real turn, and the Rust fixtures run a
`cargo test` per check. This is a nightly / before-release measurement, not
a per-PR gate. The pass rate is the number to track over time, along with
rounds per task (`--json` reports both).

## Tier 5 — perf gates

Timing policy lives in `benchmarks/baseline.json` (committed medians plus `warn_beyond` / `fail_beyond` multipliers — single source of truth, read by tests so updates never require test edits):

- `PASS` at or under `warn_beyond × median`; `WARN` above that but within `fail_beyond × median` (test still passes); `FAIL` only beyond 10× or on any correctness mismatch. Medians are deliberately generous so CI noise warns instead of failing.
- Gates: `crates/runtime/runner/tests/benchmarks.rs` carries `continuation_micro_bench` (scripted model, 8 tool rounds through the real `RunLoop`) and `session_open_bench` (offline assembly + 5 scripted turns), both reporting JSON lines.
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

New user-visible structural behavior (a new `EventMsg` variant, a changed wire shape) → extend the tier-2 contract validation and note it in `docs/cookbook/recording-and-replaying.md`. A bug fix → tier-1 regression test next to the fix. A change to context engineering, eviction, or the tool surface → run the tier-4 task suite and quote the pass rate before and after. Perf-sensitive loop changes → check the tier-5 gates print PASS, and re-baseline deliberately if the medians legitimately moved.
