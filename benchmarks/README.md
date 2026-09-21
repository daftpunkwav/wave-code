# Benchmarks and snapshot-replay quality machinery

Why this exists: unit tests can stay green while the product visibly
regresses (changed timeline text, dropped approval markers, slower turns).
This directory pins user-visible behavior and turn performance with
offline, keyless fixtures and benches — with one deliberate exception: the
`tasks/` suite drives real model turns, so it costs quota and runs on
purpose, not on every commit.

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
- `tasks/<id>/` — task-level benchmark suite: `task.toml` (manifest) plus
  `workspace/` (the fixture an agent edits). Live tier; see below.

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

## Task-level suite (`tasks/`)

Everything above judges structure. This tier judges whether a real session
got the work done: a task is a prompt plus a tiny repository, and the verdict
comes from the workspace after the turn, never from the model's summary of it.

```
cargo.exe build --bin wavecode
wavecode.exe eval tasks --permission-mode wave
wavecode.exe eval tasks --filter rust --json --out rust.json
```

Run it from the repository root — the default `--dir` is
`benchmarks/tasks`. Each task copies its `workspace/` into a throwaway work
root under the OS temp directory (printed at the end, and kept, so a failing
task can be inspected as it stands), then spawns its own `wavecode exec
--json` child there. `--agent-bin <path>` points the suite at a different
build, which is how two builds get compared on the same tasks.

Manifest (`tasks/<id>/task.toml`); relative paths resolve against the
manifest's own directory, and only the file name `task.toml` marks a task, so
a fixture may hold its own `Cargo.toml`:

```toml
id = "fix-config-timeout"
prompt = """README.md states the requirement the client config has to meet."""
fixture = "workspace"
tags = ["basic", "edit"]
agent_timeout = 900            # optional wall cap for the turn

[[assertion]]
kind = "file_contains"         # also: file_equals, file_not_contains,
path = "app.toml"              #       file_exists, file_absent, file_unchanged
text = "timeout = 30"

[[assertion]]
kind = "command"               # argv, no shell: same meaning on every platform
program = "cargo"
args = ["test", "--offline"]
timeout_secs = 300
```

A task passes when its turn ended cleanly **and** every assertion holds. A
crashed, interrupted or capped turn fails even if the files look right.
`file_unchanged` compares bytes against the pre-turn copy, which is what
makes "fixed the bug and trampled a neighbour" a failure rather than a
success with an asterisk.

Three offline gates in `crates/frontends/harness/src/task_eval.rs` keep the
suite honest without spending a token, and run in ordinary `cargo test`:

- `committed_tasks_are_well_formed` — every manifest loads, ids are unique
  and match their directory, and a cargo fixture carries its own
  `[workspace]` table (without it the outer workspace swallows the copy and
  the build assertions quietly test the wrong crate).
- `no_committed_task_is_already_solved` — the pristine fixture must fail at
  least one assertion, or the task is a free point measuring nothing.
- `every_committed_task_has_a_solution_that_passes` — a known-good end state
  must satisfy the assertions, so a typo in an expected string is caught
  here rather than by a live run.

The suite is a nightly / before-release measurement: one real turn per task,
plus a `cargo test` per Rust check. Track the pass rate and the rounds-per-task
column over time; both are in `--json`.

## Adding a fixture

1. Append a JSON event array under `fixtures/<name>.json`.
2. Add one test in `snapshot_replay.rs` asserting the exact steps,
   observations, and `replay()` timeline lines.
3. Run the two commands above; both must pass.

## Adding a task

1. Create `tasks/<id>/task.toml` plus `tasks/<id>/workspace/` holding the
   smallest repository shape that makes the request real (a bug to fix, a
   README that states the requirement, a neighbour file that must survive).
2. Keep the prompt self-contained: the agent sees the workspace and nothing
   else. Name the boundary explicitly if a file must not be touched.
3. Write assertions that only the intended end state satisfies. A cargo
   fixture needs its own `[workspace]` table.
4. `cargo.exe test -p harness-cli task_eval` — the three gates must stay
   green, which proves the task is neither free nor unsolvable.
5. Run it live once (`--filter <id>`) before relying on its number.
