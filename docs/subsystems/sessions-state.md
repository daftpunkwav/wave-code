# Subsystem: sessions and durable state

Durable state is deliberately dumb: append-only bytes plus versioned formats, with structure owned by the callers. Crates: `state/persistence`, `state/checkpoint`, `state/plan`, `state/goal`, plus the replay fold in `operations/replay` and the actor's durability helpers in `operations/actor/src/durable.rs`.

## Turn journal (`crates/state/persistence/src/lib.rs`)

`JsonlJournal` appends one `TurnRecord` per turn as a single JSON line (`run_id`, `input`, `history` as `(from_model, text)` pairs, `outcome`). Format versioning:

- New journals start with a `{"format":1}` header line (`JOURNAL_FORMAT_VERSION = 1`).
- Files without a header read back as v0 (`JOURNAL_FORMAT_V0`); `is_header_value` distinguishes them (numeric `format`, no `run_id`).
- `migrate_record` lifts v0 values into the current shape on **read only** — loaders never rewrite the user's journal; unknown or missing fields degrade to defaults.

Loaders count skipped corrupt lines instead of failing: `load_all` / `load_reported` (returns the skipped count for strict callers) / `load_all_checked` / `last_n` for resume previews. The journal is append-only; nothing truncates it.

## Session registry (`crates/state/persistence/src/sessions.rs`)

The new-stack session registry gives the turn journal a home and an identity:

- `~/.wavecode/sessions/<session-id>.jsonl`: one `JsonlJournal` per session; every completed console turn appends a full text snapshot of the dialogue (`record_turn`), so the **latest record alone suffices for resume** (`load_session_history`). `/undo` appends the truncated dialogue the same way (`record_rewind`, outcome `Rewound`), so resume replays the rewound conversation without rewriting history.
- `~/.wavecode/sessions/index.json`: one `SessionMeta` (id, title, cwd, created/updated timestamps, turn count) per session; `list_sessions` reads it newest-first, `set_title` renames, `fork_session` seeds a fresh journal with a caller-supplied snapshot. Broken index files degrade to empty and heal on the next write.
- Session ids validate against the same whitelist as legacy thread ids (`is_valid_session_id`): path escapes from CLI arguments or picker payloads are rejected.

Consumers: the console UI journals on `TurnCompleted` and drives `/sessions` (picker), `/resume`, `/fork`, and `/title`; the harness assembles resumed sessions from `load_session_history` (`--session <id>`, `--continue`). Resume is text-level by design: tool blocks are not represented in the journal shape.

Honest status (updated 2026-09-18): the journal now has a production consumer (the session registry). The legacy `resume` subcommand still reads the legacy import path — `state_persistence::legacy` (`crates/state/persistence/src/legacy.rs`) lists and loads `~/.wavecode/threads` (`THREADS_DIR`), newest first.

## Checkpoints, snapshots, and the actor's durability seam

`crates/state/checkpoint/src/lib.rs`:

- `CheckpointStore`: labelled in-memory snapshots; `rollback` restores the target and drops newer labels; unknown labels fail explicitly.
- `durable_save` / `durable_load` / `resume_checkpoint`: file-backed labelled state under a root; labels validated (`validate_checkpoint_label`).
- `SnapshotStore`: workspace file snapshots with caps (`SnapshotCaps`), `create` / `create_with_caps` / `restore` / `list_labels` / `drop_label`, returning reports whose `summary()` is user-visible text.

`crates/operations/actor/src/durable.rs` defines the seam the actor uses: `CheckpointSink` (where durability lands), `DurabilityConfig` (what is enabled; `disabled()` for tests), `persist_checkpoint` and `persist_then_act` (persist before the side effect), `render_snapshot` (conversation ⇒ text), and `turn_label`.

## Plan and goal state

- `crates/state/plan/src/lib.rs`: `PlanState` is a reviewed state machine — `propose` → `approve` → `begin` → `complete` / `abandon`, with `feedback` returning to review; `PlanStatus::is_terminal` gates transitions; plans live per session under a plans root (`plan_path_for_session`, `validate_session_id`).
- `crates/state/goal/src/lib.rs`: one durable objective per session with **CAS versioning** — every mutation bumps `GoalState::version`, and the goal tool's `update` action requires the expected version; a mismatch names both versions and tells the model to reload with `status`. Statuses: Active / Blocked / Paused / Completed (terminal). An optional `sub_goals` list (text + in_progress/achieved) decomposes the objective; a fresh `set` clears it.

## Replay: the structural fold (`crates/operations/replay/src/lib.rs`)

`replay_to_trajectory(events) -> Trajectory` folds recorded wire events into steps and observations. It is **structural, never re-executing**:

- Text deltas are intentionally skipped (replay answers "what ran, in what order, with what outcome"; transcripts keep the words). `AgentMessageComplete` ⇒ a `sample` step.
- `ToolCallBegin` opens a call; `ToolCallEnd` closes it with an `is_error` observation. Unpaired ends stay visible (marked `unpaired end`); begins that never closed get a non-error `truncated: no end event` mark, so truncated recordings are distinguishable from success.
- `ApprovalRequested` becomes a visible note — safety-relevant pauses must not disappear.
- `CompactCompleted` ⇒ a `compact` step; `Warning`/`Error` become observations; `TurnCompleted { interrupted: true }` is noted, clean completions add nothing.

Snapshot tests pin this fold against committed fixtures — see `docs/subsystems/evals.md` and `docs/cookbook/recording-and-replaying.md`.
