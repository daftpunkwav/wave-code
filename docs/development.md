# Development

English | [中文](development.zh.md)

Guide for working on the WaveCode repository. For how the crates fit together, see [architecture.md](architecture.md).

## Prerequisites

- Rust stable (edition 2024 requires 1.85+). A C toolchain is needed to build dependencies on some platforms.
- Node.js and pnpm only if you touch the `sdk/typescript/` package.

## Commands

CI (`.github/workflows/ci.yml`) runs exactly this on Linux, Windows, and macOS:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Run the same set before pushing. Performance benches additionally live in `crates/runtime/runner/tests/benchmarks.rs`; bounds and fixtures are documented in [benchmarks/README.md](../benchmarks/README.md).

## Code conventions

- **Comments and documentation are English.** All git-tracked docs, code comments, and doc comments use English by default; another language only where genuinely necessary.
- **Every crate entry point carries a file header** stating `@file`, `@description`, its responsibilities, and the layers it must not depend on. Keep these headers current when responsibilities change:

  ```rust
  /*!
   * @file RunLoop
   * @description Execution orchestration for a single agent run.
   *
   * Responsibilities:
   * - Own the run state machine (sample -> decide -> execute -> recover).
   *
   * This module must not depend on: tools, sandbox, hooks, memory, ...
   */
  ```

- **Respect the dependency rules** in [architecture.md](architecture.md): `runtime/runner` stays on trait seams, only `operations/bootstrap` names concrete capability crates, policy consumes tool attributes rather than names.
- **Minimal correct diffs.** Follow the surrounding style; do not refactor working code in unrelated lines. Extend existing abstractions before adding parallel ones.
- **No leftovers.** Do not land commented-out code, debug prints, or placeholder TODOs.
- Formatting is `rustfmt.toml` (default profile); there is no manual formatting style to preserve.

## Testing

- New behavior needs a test in the same change; bug fixes need a regression test.
- Unit tests live next to the code (`#[cfg(test)] mod tests`); cross-crate behavior lives in integration tests (`crates/*/tests/`, e.g. `crates/runtime/runner/tests/`).
- Tests must not require network access, model API keys, or specific OS features to pass; scripted models and offline fixtures are the pattern (see `operations-eval` and `benchmarks/`).
- Clippy denies warnings (`-D warnings`); fix the cause rather than widening exceptions.

## Commits and branches

Conventions live in [AGENTS.md](../AGENTS.md): Conventional Commits with an imperative subject of at most 50 characters, one change per commit; branches named `<type>/<kebab-case-description>`. Commits describe the change itself, not task-list or document progress.

## Documentation

- Docs accompany every code change: update affected README sections and file headers in the same PR.
- Current-state prose only; do not narrate history or review rationale in docs.
- Tracked documentation lives in `README.md`, `docs/`, and per-crate/package READMEs. `docs-local/` is a local-only scratch area and is never committed.
