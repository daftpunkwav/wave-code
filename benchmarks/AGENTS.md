# benchmarks/ agent rules

The layout and timing policy live in [README.md](README.md).

## Offline benches

- `baseline.json` is the timing policy. Tests read it with
  `include_str!`. Do not hard-code multipliers in the test.
- `FAIL` is past `fail_beyond` times the median, or on a correctness
  mismatch. The `WARN` band still passes.
- `fail_beyond` stays greater than `warn_beyond`, and
  `warn_beyond` stays at least 1. Every median stays greater than 0.
- `run.rs` and `crates/runtime/runner/tests/benchmarks.rs` describe
  the same rounds and bounds. Change them together.
- Live smoke stays behind `LIVE=1` and passes when the variable or
  the provider key is absent.

## Tasks

- A task is `tasks/<id>/task.toml` plus `tasks/<id>/workspace/`.
  Only the filename `task.toml` marks a task.
- The prompt is self-contained. Assertions accept the intended end
  state.
- A cargo fixture carries its own `[workspace]` table.
- The pristine fixture fails at least one assertion. A known-good
  end state satisfies all of them.
- `file_unchanged` compares bytes with the pre-turn copy.
- Command assertions are an argv array, not a shell string.
- Do not add `AGENTS.md` inside a task `workspace/`. That directory
  is the fixture the evaluated agent sees.
- After adding or editing a task, `cargo test -p harness-cli task_eval`
  stays green.
