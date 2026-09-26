# crates/capabilities/memory/ — instruction and persistent memory

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | Crate map and re-exports (`collect`, `find_project_root`, `MemoryStore`, `plan_merges`, `parse_extracted_entries`, ...) |
| `src/instructions.rs` | Instruction memory: layered `WAVECODE.md` discovery (user -> project root -> cwd; the root is located upward via `.git`) with recursive `@path` expansion (depth cap 5, cycle-safe), per-tier fallback filenames (the `AGENTS.md` convention first, then one further cross-tool fallback; first existing wins), merged with `.wavecode/rules/*.md` rule dirs |
| `src/store.rs` | `MemoryStore`: a `MEMORY.md` index plus four category entry files (`user` / `feedback` / `project` / `reference`) in Markdown bullet form; append-only, root injectable (production `~/.wavecode/memories/`, tests tempfile) |
| `src/extract.rs` | `parse_extracted_entries`: parses a subagent's `[category] content` line format back into `(category, content)` pairs; unknown tags, empty entries, and pre-tag chatter are dropped — model output is untrusted |
| `src/consolidate.rs` | Heuristic consolidation: the pure `plan_merges` planner folds near-duplicates (Jaccard >= 0.6 on lowercased word sets, within one category) into their newest wording; all IO lives in the thin `MemoryStore::consolidate` wrapper |
| `src/tool.rs` | `MemoryWrite` — the `memory_write` tool (shared `Tool` trait) for explicit durable writes; validates category and content before touching the filesystem |

The store is plain, user-editable Markdown and this crate only appends to
it. Apart from the `wavecode-tools` edge (`tool.rs` implements the shared
`Tool` trait — the same-tier edge `wavecode-mcp` also takes), the crate has
no workspace-internal dependencies: `memory_write` approval wiring, prompt
injection, and extraction orchestration live core-side. Writes are small
appends the caller moves off the executor with `spawn_blocking`; reads
happen once at startup assembly.
