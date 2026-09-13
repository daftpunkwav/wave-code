# Contributing

Thank you for your interest in contributing to WaveCode!

WaveCode is under active development and pre-stable; expect compatibility-breaking changes.

## Getting started

1. Read the [README](README.md) and the [architecture overview](docs/architecture.md).
2. Build and test your checkout:

   ```sh
   cargo test --workspace --locked
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo fmt --check
   ```

## Submitting changes

- Branch from `main` using `<type>/<kebab-case-description>` (e.g. `feat/context-compaction`, `fix/memory-dedup`).
- One change per commit, with a Conventional Commits subject: `feat:` / `fix:` / `docs:` / `refactor:` / `chore:` / `test:` / `perf:` plus an imperative subject of at most 50 characters.
- Follow the conventions in [docs/development.md](docs/development.md) — English comments and docs, file headers on crate entry points, dependency rules, tests with new behavior.
- Run the three commands above before opening a pull request; CI runs the same set on Linux, Windows, and macOS.

## Reporting issues

When reporting a bug, include the WaveCode version (`wavecode --version` or the commit), the invocation, expected versus actual behavior, and relevant logs (`--debug` output or JSONL events). Redact secrets and personal data.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
