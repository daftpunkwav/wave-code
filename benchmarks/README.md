# Benchmarks and snapshot-replay quality machinery

Why this exists: unit tests can stay green while the product visibly
regresses (changed timeline text, dropped approval markers, slower turns).
This directory pins user-visible behavior and turn performance with
offline, keyless fixtures and benches. Everything here runs without
network access, model quota, or credentials.

## Layout

- `README.md` — this file.
- `run.rs` — plain documented harness (the human-readable spec, not a
  cargo test target). The executable form lives in integration tests
  (see below); keep the two in sync when changing rounds, bounds, or
  fixture shapes.
- `baseline.json` — committed medians plus `warn_beyond` / `fail_beyond`
  multipliers. Single source of truth for timing policy; tests read it
  with `include_str!` so updates never require test edits.
- `fixtures/*.json` — committed transcript fixtures. Each file is a JSON
  array of wire `Event` objects (`id` plus flattened `EventMsg`).

## Foci

(a) Continuation micro-bench — scripted model, 8 tool rounds through the
real `RunLoop`, asserts wall time under a generous bound and reports
tokens/rounds as JSON. Executable:
`crates/runtime/runner/tests/benchmarks.rs::continuation_micro_bench`.

(b) Session-open bench — offline assembly (loop seams + conversation +
system prompt) plus 5 scripted turns, same wall-time/report discipline.
No provider, no config file, no credentials. Executable:
`crates/runtime/runner/tests/benchmarks.rs::session_open_bench`.

(c) Replay goldens — each fixture replays through
`replay_to_trajectory`; tests assert exact step/observation sequences and
timeline text. Pins user-visible behavior without model quota.
Executable: `crates/operations/replay/tests/snapshot_replay.rs`
(fixtures: `basic`, `approval`, `interrupt`, `truncation`).

## Timing policy (never flaky-fail CI on noise)

- `PASS` when elapsed <= `warn_beyond` x median (prints PASS + JSON).
- `WARN` when elapsed is above that but within `fail_beyond` x median
  (prints WARN, test still passes).
- `FAIL` only beyond `fail_beyond` x median (10x) or on any correctness
  mismatch (wrong steps, missing observations, wrong stop reason).
- Medians are deliberately generous (hundreds of ms for work that takes
  low tens of ms), so only a real regression trips FAIL.

## Keyless / live split

Everything runs offline by default. One gated smoke test documents the
live path:

```
LIVE=1 cargo.exe test -p runtime-runner --test benchmarks live_gate_smoke -- --nocapture
```

- Without `LIVE=1`: prints SKIP, passes.
- With `LIVE=1` but no provider key (`ANTHROPIC_API_KEY`): prints SKIP
  with the reason, passes.
- With `LIVE=1` and a key: still never fails from here; it runs one
  scripted smoke turn and prints instructions for pointing a real
  provider adapter at it. A real single-turn live check stays a manual
  step until a hermetic provider fake lands.

## Running

```
cargo.exe test -p runtime-runner --test benchmarks
cargo.exe test -p operations-replay --test snapshot_replay
```

Meta-test `baseline_meta_test` (in the runner benchmarks file) asserts
`baseline.json` parses and that tolerances are sane
(`fail_beyond > warn_beyond >= 1`, every median > 0).

## Adding a fixture

1. Append a JSON event array under `fixtures/<name>.json`.
2. Add one test in `snapshot_replay.rs` asserting the exact steps,
   observations, and `replay()` timeline lines.
3. Run the two commands above; both must pass.
