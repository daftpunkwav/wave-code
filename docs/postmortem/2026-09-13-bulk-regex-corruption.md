# Postmortem: bulk regex edit truncates three Rust files

English | [中文](2026-09-13-bulk-regex-corruption.zh.md)

Date: 2026-09-13 · Status: resolved · Severity: low (no data loss)

## Summary

During a large multi-feature landing, a bulk Python script (regex plus brace matching) used to append struct fields across the workspace truncated three Rust files — `acp.rs`, `model_adapter.rs`, and `session.rs` — losing roughly 500–900 lines each. Detection was same-session via `cargo check` parse errors and implausible `git diff --stat` line deltas; recovery came from `git fsck --unreachable`, which still held the day's `WIP on main` stash snapshot. Total rework was about one hour; no commits were harmed and no data was permanently lost.

## Timeline

1. A multi-feature change was in progress with large uncommitted edits in `crates/operations/bootstrap/src/`.
2. To add the same field to several structs, a Python script edited multiple files in one pass using regex matching and brace counting instead of targeted per-site edits.
3. The script's brace-matching heuristic mis-tracked string literals and nested blocks, writing truncated replacements: `bootstrap/src/acp.rs`, `bootstrap/src/model_adapter.rs`, and `bootstrap/src/session.rs` each lost 500–900 lines.
4. The next `cargo check` reported parse errors far from any edit site — the first hard signal.
5. `git diff --stat` showed three files with large negative line deltas inconsistent with an append-only change — confirming truncation rather than a code mistake.
6. Editing stopped immediately (no further writes over the damaged tree).
7. `git fsck --unreachable` located dangling objects from the day's `WIP on main` stash; the pre-corruption content of all three files was recovered from that snapshot.
8. The feature edits were re-applied with precise per-site edits; `cargo check` and the test suite passed.

## Impact

- ~1 hour of rework re-applying both the lost file content and the in-flight feature edits.
- No permanent data loss: no commits were affected, and the stash snapshot provided full recovery.
- No user-facing impact: the corruption never built or ran.

## Root cause

A scripted bulk edit applied a syntactically blind transformation (regex + brace counting) to Rust source. Brace counting cannot see strings, comments, or macros, so the script's notion of "end of struct" diverged from the parser's, and whole trailing regions of files were overwritten. The blast radius was amplified by doing this during an already-large change, where a broken intermediate state was easy to miss until the next compile.

## What went well

- Verification discipline caught it immediately: `cargo check` plus a `git diff --stat` review turned "weird parse error" into "file truncated" within minutes.
- Editing stopped at suspicion instead of layering more changes over the damage.
- Git's unreachable-object store preserved a same-day snapshot; knowing to look there (`git fsck --unreachable`) avoided reconstructing the files by hand.

## Action items

1. **Prefer precise per-site edits over scripted bulk edits.** Regex/brace-matching scripts must not rewrite Rust source; when a change spans many sites, do them individually or extend the compiler-checked abstraction instead.
2. **After any mechanical multi-file change, run `cargo check` AND review `git diff --stat`.** Parse errors plus implausible line deltas (especially large deletions from an append-only change) are the corruption signature; either one alone is not sufficient.
3. **When corruption is suspected, stop editing and recover from unreachable objects before overwriting.** `git fsck --unreachable` (and stashes) hold the pre-corruption state; further edits destroy exactly the history needed for recovery.
