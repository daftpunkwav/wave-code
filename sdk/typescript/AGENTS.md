# sdk/typescript/ agent rules

The usage reference lives in [README.md](README.md).

## Contract

- The package runs `wavecode exec` as a child process and exposes the
  JSONL stream as a typed async iterator.
- Runtime dependencies are none. `engines.node` is `>=18`.
- `src/types.ts` mirrors `wavecode_wire::EventMsg`: an internal `type`
  tag and snake_case fields. A wire change updates this union in the
  same commit.
- Binary discovery is the `bin` option, then `WAVECODE_BIN`, then
  `wavecode` on `PATH`. On Windows the lookup is shell-less: the exact
  name, then `.com` and `.exe`, never `PATHEXT`; elsewhere the exact
  name is looked up on `PATH` as it is.
- A failed spawn arrives as the final `error` event of the stream. The
  event iterator is not rejected, and `wait()` rejects with the spawn
  error.
- Without `approvals: true`, approval requests deny.

## Checks

- From this directory: `pnpm build`, then `pnpm test`.
- Smoke tests skip unless `WAVECODE_SDK_LIVE=1` and a binary is
  available. CI does not set that variable.
