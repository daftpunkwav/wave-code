# Cookbook: recording and validating a session

English | [中文](recording-and-replaying.zh.md)

A "recording" is the session's wire event stream serialized as JSONL. Every `Event` (`crates/foundation/wire/src/lib.rs`) derives `Serialize`/`Deserialize` with a flattened `EventMsg` (`#[serde(tag = "type", rename_all = "snake_case")]`), so one event is exactly one JSON object with an `id` field and a `type` discriminator.

One exception: a live `wavecode exec --json` recording opens with a single control line, `{"meta":"session",…}`, carrying the session id and its resume command (no `id`/`type`). It is session bookkeeping, not an event; `read_events_jsonl` skips it, and hand-rolled loaders should too.

## 1. Record

The headless exec path already emits this format:

```sh
wavecode exec --json "inspect the build failure" > session.jsonl
```

`run_exec` (`crates/frontends/harness/src/main.rs`) serializes each event with `serde_json::to_string(&event)` and writes one per line to stdout while human rendering goes to stderr. Any driver works too: pass an `on_event` callback to `TurnDriver::drive_turn` / `RunLoop::run_turn` and append `serde_json::to_vec(&event)` per line — that is all `--json` does.

## 2. Load and validate

Read the file back as wire events:

```rust
let events: Vec<Event> = serde_json::from_str(&text)?;   // one JSON array
// or per line for JSONL: serde_json::from_str(&line)?
```

Validation lives in `operations_eval::replay` (`crates/operations/eval/src/replay.rs`): `validate_contract(&events)` checks ordering, pairing, and settle-once per submission — deltas before their `AgentMessageComplete`, begin/end pairing, one `TokenCount`, nothing after `TurnCompleted` — returning human-readable violation lines instead of stopping at the first. For JSONL recordings, `read_events_jsonl` loads one `Event` per line and fails loudly with the line number on malformed input.

## 3. Score a recording

`evaluate_recorded` scores recorded assistant text against `must_contain` expectations (`ReplayReport::pass_rate`), and the behavioral harness in the same crate (`EvalCase` with `must_contain` / `must_not_contain` over final history through any `TurnDriver`) judges live driver behavior. This is the tier-2 machinery in `docs/subsystems/evals.md`.

## 4. Limits

- A recording carries protocol events, not transcripts: answer wording arrives through `AgentMessageDelta` / `AgentMessageComplete` events only, and pair the recording with the conversation journal if you need the full dialogue view.
- Real-API sessions can be recorded the same way, but tier-3 e2e remains manual — see `docs/subsystems/evals.md`.
