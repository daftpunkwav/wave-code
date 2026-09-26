# crates/capabilities/skills/ — SKILL.md discovery, catalog injection, invocation

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | The skill model: `Skill`/`SkillMeta` (YAML frontmatter; kebab- and snake-case field spellings both accepted, unknown fields ignored), `SkillSource` priority override (builtin < user < project), `discover` / `Discovery::refresh` (each bad file warns and skips), `SkillSet::catalog` with a stepwise character-budget downgrade, and `$ARGUMENTS` / `${WAVECODE_SKILL_DIR}` expansion |
| `src/plugin.rs` | Plugin packs under `~/.wavecode/plugins/<dir>/plugin.toml` (`{name, version, skills_dir?, hooks_file?}` plus inline `[mcp_servers]` tables): skills load through the discovery pipeline; MCP entries and hook rules return raw for the assembly layer to convert; invalid packs warn-and-skip |
| `src/tool.rs` | `SkillTool` — the model-invokable `skill` tool: inline skills expand in place; fork skills spawn a background child task through the `action-tasks` `TaskService` seam and report back via the existing child-task notifications |

Rendering goes through the shared `Tool` trait (`wavecode-tools` — the
same-tier edge `wavecode-mcp` also takes) and forks go through the
zero-dependency `action-tasks` crate; those are the only workspace edges.
The catalog budget (1% of the context window) arrives from the caller as a
character quota and is compared in characters, not bytes, so CJK
descriptions are not truncated early. `user_invocable` gates frontend slash
invocation only, never model invocation — the catalog itself invites the
model to trigger skills.
