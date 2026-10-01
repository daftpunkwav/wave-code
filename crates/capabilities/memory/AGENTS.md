# capabilities/memory/ agent rules

The store map lives in [README.md](README.md).

## Instruction files

- The instruction filename is `AGENTS.md`. A directory without it has
  no tier.
- `AGENTS.local.md` is concatenated only when `AGENTS.md` exists in
  the same directory, and only after that file.
- Collection does not read `CLAUDE.md`, `WAVECODE.md`, or any other
  fallback name.
- Collection order is the user file `~/.wavecode/AGENTS.md`, then the
  project root, then the cwd. Each of those tiers also reads
  `.wavecode/rules/*.md`, sorted by filename.
- The project root is the nearest ancestor containing `.git`, whether
  `.git` is a directory or a file.
- `@path` expansion stops at depth 5. A missing, unreadable, or
  cyclic target stays literal.
- The same canonical path is concatenated once.
- Per-directory discovery beyond the cwd is implemented in
  `operations/bootstrap/src/agents_instructions.rs`. Keep the filename
  and the "no fallback" rule aligned with that walker.

## Store

- `MemoryStore` appends Markdown. `plan_merges` is pure. IO stays in
  `MemoryStore::consolidate`.
- Extract parsing drops unknown tags, empty entries, and pre-tag
  chatter.
- `memory_write` validates category and content before any write.
- `tool.rs` may depend on `wavecode-tools`. The crate does not run
  extraction itself.
