# Cookbook: recording and replaying a session

A "recording" is the session's wire event stream serialized as JSONL. Every `Event` (`crates/foundation/wire/src/lib.rs`) derives `Serialize`/`Deserialize` with a flattened `EventMsg` (`#[serde(tag = "type", rename_all = "snake_case")]`), so one event is exactly one JSON object with an `id` field and a `type` discriminator — the same shape committed under `benchmarks/fixtures/*.json`.

## 1. Record

The headless exec path already emits this format:

```sh
wavecode exec --json "inspect the build failure" > session.jsonl
```

`run_exec` (`crates/frontends/harness/src/main.rs`) serializes each event with `serde_json::to_string(&event)` and writes one per line to stdout while human rendering goes to stderr. Any driver works too: pass an `on_event` callback to `TurnDriver::drive_turn` / `RunLoop::run_turn` and append `serde_json::to_vec(&event)` per line — that is all `--json` does.

## 2. Load and validate

Read the file back as wire events. The fixture loader in `crates/operations/replay/tests/snapshot_replay.rs` shows the contract:

```rust
let events: Vec<Event> = serde_json::from_str(&text)?;   // one fixture = one JSON array
// or per line for JSONL: serde_json::from_str(&line)?
```

Validation has two layers. First, the wire contract: `operations_eval::replay::validate_contract(&events)` (`crates/operations/eval/src/replay.rs`) checks ordering, pairing, and settle-once per submission — deltas before their `AgentMessageComplete`, begin/end pairing, one `TokenCount`, nothing after `TurnCompleted` — returning human-readable violation lines. Second, structural sanity comes from the replay fold itself (unpaired ends, truncated calls, approvals). For JSONL recordings, `read_events_jsonl` loads one `Event` per line and fails loudly with the line number on malformed input.

## 3. Fold with the structural replay

`crates/operations/replay/src/lib.rs`:

```rust
use operations_replay::replay_to_trajectory;
let trajectory = replay_to_trajectory(&events);
for line in trajectory.replay() { println!("{line}"); }   // "#2 tool:shell: c1 started"
```

The fold is structural and read-only: deltas collapse away, `ToolCallBegin`/`ToolCallEnd` pair by `call_id` (unpaired ends stay visible; unclosed begins get a non-error `truncated: no end event` mark), approval requests and interruptions remain visible, and compactions become `compact` steps. It never re-executes anything — safe to run over untrusted recordings.

## 4. When to add a golden fixture

Add a file under `benchmarks/fixtures/` when:

- a **new user-visible structural signal** exists (a new `EventMsg` variant with trajectory semantics — e.g. a new approval or compaction shape) and no existing fixture exercises it;
- a **regression class** is worth pinning permanently (the current four pin the basic flow, approval visibility, interrupt truncation, and mixed truncation/error signals);
- a bug slipped through tier 1 and the recorded event sequence is the clearest expression of it.

Recipe: record or hand-write the minimal event array, drop it in `benchmarks/fixtures/<name>.json`, then extend `snapshot_replay.rs` with a test asserting the exact step/observation sequences and timeline text, and update the `replay_goldens_stay_fast_against_baseline` step-count assertion (`total_steps`) in the same change — it pins the fixture set's size. Fixtures stay offline and keyless; keep them minimal (fewer than a dozen events each). `benchmarks/run.rs` documents the fixture shapes and must stay in sync.

## 5. Limits

- Transcripts (the words) are not in the trajectory — the fold deliberately skips text deltas; pair the recording with the conversation journal if you need wording.
- `wavecode exec --json` writes JSONL, while committed fixtures are JSON arrays; convert with a one-line join when promoting a manual recording to a fixture.
- Real-API sessions can be recorded the same way, but tier-3 e2e remains manual — see `docs/subsystems/evals.md`.
