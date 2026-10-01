# .github/ agent rules

CI is [workflows/ci.yml](workflows/ci.yml). Supply-chain policy is
[../deny.toml](../deny.toml).

## Required legs

- `fmt`: `cargo fmt --all --check` on one runner.
- `msrv`: `cargo check --workspace --locked` on Rust 1.90.
- `supply-chain`: `cargo deny check`.
- `title`, on pull requests: the title matches
  `^(feat|fix|docs|refactor|chore|test|perf)(\([a-z0-9-]+\))?: .{1,50}$`
  and contains no `P` followed by a digit.
- `test`, on Linux, Windows, and macOS:
  `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked`,
  `cargo build --locked --bin wavecode`, then the e2e smoke
  (`--version`, `mcp serve` initialize, `plugin list`).
- `sdk`, from `sdk/typescript`: `pnpm install --no-frozen-lockfile`,
  `pnpm build`, `pnpm test`. Do not set `WAVECODE_SDK_LIVE`.

## Changes

- A new required command is added here and to `docs/development.md`
  plus `docs/development.zh.md` in the same change.
- Toolchain pins stay explicit: 1.90.0 for MSRV, and the fmt/clippy
  pin on the test legs.
- Workflow `permissions` stay `contents: read`.
- Concurrency cancels in-progress runs on the same ref.
- A new license or an advisory ignore is an explicit entry in
  `deny.toml`.
