# infrastructure/ agent rules

The crate map lives in [README.md](README.md).

## Boundaries

- These crates hold mechanisms: channel capacities, timeouts,
  truncation budgets, `InterruptHandle`, calendar-date and shell
  resolution, and token-bucket arithmetic.
- `infrastructure-base` has no workspace dependencies. It depends on
  `tokio` (the blocking pool that carries the off-worker process-tree
  kill), `encoding_rs` (the console code-page fallback; its types
  appear in the codec's cross-platform signatures), and, on unix,
  `libc` / on windows, `windows-sys` (the pid liveness probe and the
  cancel-path Job Object).
- `infrastructure-ratelimit` depends on `thiserror`. Its clock is
  supplied by the caller.
- No crate in this directory depends on another workspace crate.
- Upper layers use the constants defined here. Do not copy a limit
  into a caller.
