# frontends/harness/ agent rules

The command map lives in [README.md](README.md).

## The binary

- This crate owns the `wavecode` binary: argument parsing, session
  assembly through `operations-bootstrap`, and process exit codes
  that follow turn outcomes.
- Interactive rendering is delegated to `console-ui`. Turns are
  driven through the actor client.
- `file_appender()` writes under `~/.wavecode/logs` when it can open
  a daily rolling file there. It returns none when `home_dir()` is
  missing, the directory cannot be created, or
  `RollingFileAppender` fails to build. `init` then installs the
  subscriber on stderr. Do not write tracing logs to stdout.
- A `ModelCatalog` load failure is printed to stderr and the session
  continues on `config.toml`.
- Headless `exec` denies parked approvals unless `--approvals` is
  set.
- `--plan` starts plan mode. `-y` / `--yolo` starts auto mode. Each
  conflicts with the other and with `--permission-mode`.
- Scoring stays in `operations-eval`. `task_eval.rs` owns fixture
  isolation and the agent step.
- These tests stay green: `committed_tasks_are_well_formed`,
  `no_committed_task_is_already_solved`, and
  `every_committed_task_has_a_solution_that_passes`.
- `dependency_matrix_locked` in `src/main.rs` locks the
  `[dependencies]` path edges. `FRONTEND_ALLOWLIST` locks the same
  set. Update both in the same change.
