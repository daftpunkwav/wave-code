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

The SDK runs `$WAVECODE_BIN` if set, else `wavecode` from `PATH`. Build the
CLI first (`cargo build --bin wavecode`) and point `WAVECODE_BIN` at
`target/debug/wavecode` during development.

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
does not exist.
