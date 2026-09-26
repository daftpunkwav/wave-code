# crates/runtime/prompt/ — pure system-prompt layout from named slots

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; no dependencies at all |
| `src/lib.rs` | `PromptSlots` / `build_system`: deterministic section assembly (identity first, summary last, empty slots skipped); `SourceBundle` / `assemble_budgeted`: character-budgeted assembly that drops lowest-priority sections first and reports every drop; `truncate_to_budget` / `Budget` |

Layout is the whole crate: the policy of what fills each slot (memory,
skills, tools, environment) stays with the driver, which composes these
primitives for dynamic per-turn assembly. Section order is contractual,
so snapshots and golden tests stay stable across refactors, and
over-budget assembly never silently loses a section — every drop is
reported.
