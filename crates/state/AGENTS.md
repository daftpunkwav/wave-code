# state/ agent rules

The crate map lives in [README.md](README.md).

## Persistence

- Durable files live under `<home>/.wavecode/`.
- Saves write a temp file and rename it into place.
- A missing file loads as empty state.
- Journal writes are append-only.
- Format migration runs on read via `migrate_record`. A load does not
  rewrite a journal to upgrade its format.
- The turn journal truncates a torn tail on load. The history journal
  reports a bad last line as `torn_tail` and a bad earlier line as
  `mid_gap`, and does not rewrite the file on read.
- Corrupt mid-file lines are counted. `load_all_checked` refuses them.
- Ids that become file names pass the existing whitelist. Restore
  paths reject absolute paths and `..`.

## Boundaries

- `state-artifact` depends on no workspace crate. It is unwired. Do
  not cite it as a product feature. Wiring it updates
  `docs/architecture.md`.
- `state-checkpoint` depends on `wavecode-config` for the home
  directory. Labels pass the validator before any filesystem write.
- `state-persistence` uses `wavecode-llm` for legacy message shapes.
  It also depends on `infrastructure-base`.
- `state-store` depends on `infrastructure-base`. Readers hold an
  `Arc` snapshot. Mutation goes through `push`.
- `state-goal` and `state-plan` may depend on `wavecode-tools` and
  `wavecode-protocol` to host their model tools. Those tools do not
  import drivers, actors, or sessions. Goal updates are
  compare-and-swap on `GoalState::version`.
- No crate here depends on runtime orchestration, action, safety,
  operations, or transport.
