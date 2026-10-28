# @wavecode/sdk

TypeScript SDK: runs `wavecode exec` as a child process and wraps the JSONL
event stream as a typed async iterator, for scripting and third-party
orchestration of agents. Zero runtime dependencies; Node >= 18.

## Usage

Observe a run:

```ts
import { execSession, isMetaLine } from "@wavecode/sdk";

const session = execSession({ prompt: "summarize this repo" });
for await (const event of session.events) {
  if (isMetaLine(event)) {
    console.log("session:", event.session_id, "→ resume via", event.resume);
  } else if (event.type === "agent_message_delta") {
    process.stdout.write(event.text);
  }
}
const { code, interrupted } = await session.wait();
```

Interrupting a run (`session.interrupt()`) unwinds the turn at its next
checkpoint and exits with code 130 on Unix. Windows has no per-process
interrupt signal: Node terminates the child outright, so the stream ends
without a `turn_completed` event and `interrupted` comes back `false`.

Drive approvals (opt-in; without `approvals: true` the run is fail-closed
and approvals deny):

```ts
const session = execSession({
  prompt: "fix the failing tests",
  approvals: true,
  permissionMode: "auto",
});
for await (const event of session.events) {
  if (event.type === "approval_requested") {
    // Human in the loop, policy engine, your call.
    session.answerApproval(event.call_id, "allow");
  } else if (event.type === "turn_completed") {
    break;
  }
}
await session.wait();
```

## Binary discovery

The SDK runs the binary named by the `bin` option, else `$WAVECODE_BIN` if
set, else `wavecode` from `PATH`. Build the CLI first (`cargo build --bin
wavecode`) and point `WAVECODE_BIN` at `target/debug/wavecode` during
development.

On Windows the lookup is shell-less: it tries the exact name, then the
executable extensions the runtime knows — `.com` and `.exe`, never
`PATHEXT`. A `.cmd` launcher is not among them, and it needs a shell to
run at all, so point `WAVECODE_BIN` at the executable behind it. Elsewhere
the exact name is looked up on `PATH` as it is. Without the `bin` option,
set `WAVECODE_BIN` whenever the binary's base name is not `wavecode`.

## Failure modes

A session that cannot start — a missing binary, say — hands the failure to
the consumer as the final `error` event of the stream, and the stream then
ends. The iterator is not rejected, so a `for await` consumer sees that
event and the end of the stream rather than an exception. `wait()` rejects
with the spawn error for the callers that await the exit.

## Types

`src/types.ts` mirrors the Rust `wavecode_wire::EventMsg` serialization
(`type` internal tag, snake_case fields) one-to-one. When the wire enum
changes, update the union in the same commit — the smoke test exercises the
real binary end to end and will catch drift in the exercised paths.

## Development

```sh
pnpm install
pnpm --filter @wavecode/sdk build
WAVECODE_BIN=../../target/debug/wavecode pnpm --filter @wavecode/sdk test
```

The smoke test skips when `WAVECODE_BIN` (or `../../target/debug/wavecode`)
does not exist, and when `WAVECODE_SDK_LIVE=1` is not set: both tests drive
real agent turns through a configured provider, so they stay opt-in (the
same convention as the benchmark `LIVE=1` tier) and never run on CI.
