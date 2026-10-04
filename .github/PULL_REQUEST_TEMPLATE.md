<!--
The PR title becomes the subject of the squashed merge commit:
  <type>(<scope>): <subject> — imperative, lowercase, ≤ 72 chars, no trailing
  period. One PR does one thing; merge requirements live in the branch ruleset
  (2 approvals, threads resolved, up to date with main).
-->

## What

<!-- A few sentences: what changes? Lead with the behavior, not the file list. -->

## Why

<!-- The problem or motivation. Link the issue (`Closes #123`), or state why
     there is no issue: bug fixes and behavior changes need one; typos and
     doc corrections may skip it. -->

## How

<!-- What a reviewer should know: key decisions, alternatives considered and
     rejected, boundaries touched (Rust core, SDK surface, docs). Delete this
     section if the diff is self-explanatory. -->

## Verification

<!-- How you proved it works and did not break anything else. The list
     below is the CI gate set: fmt, MSRV, clippy, test, the e2e smoke,
     cargo-deny, the title check, gitleaks, the SDK audit, and the docs
     pair check. Local runs catch failures before the push. -->
- [ ] Tests added or updated — a bug fix ships a regression test that fails
      before the fix and passes after it
- [ ] `cargo fmt --all --check`
- [ ] `cargo check --workspace --locked` (MSRV, Rust 1.90)
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo build --locked --bin wavecode`
- [ ] E2E smoke after the build: `--version`, `mcp serve` initialize, `plugin list`
- [ ] `cargo deny check` (supply-chain)
- [ ] Security: gitleaks history scan; SDK changes also run
      `pnpm --dir sdk/typescript audit --prod`
- [ ] SDK changes: `pnpm install --no-frozen-lockfile` · `pnpm build` · `pnpm test`
- [ ] Docs changes: `python3 scripts/ci/check_docs_pairs.py` (bilingual pairs)
- [ ] PR title matches Conventional Commits and has no phase number (`title` gate)
- [ ] Anything the tests cannot reach was verified manually (describe below)

<!-- Manual steps, before/after output. Delete if empty. -->

## Compatibility impact

<!-- Breaking changes to the SDK surface, commands, or output formats?
     "None" is a valid answer — state it explicitly. If breaking: what breaks,
     who is affected, and the migration path. -->

## Security and supply chain

<!-- Does the change touch the sandbox or process-runner boundaries,
     Cargo.toml/Cargo.lock or the SDK manifests, or GitHub workflows?
     security.yml scans on every PR regardless of paths — use this section to
     give the reviewer context the scans cannot infer. Otherwise write "N/A". -->

## Reviewer notes

<!-- Non-obvious trade-offs, known follow-ups, areas that deserve extra
     scrutiny. Delete if empty. -->
