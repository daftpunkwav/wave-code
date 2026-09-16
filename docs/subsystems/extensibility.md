# Subsystem: extensibility

Five extension surfaces: skills, hooks, MCP, plugins, and named agent definitions. All of them ride the existing seams (registry, hooks gateway, task service) instead of loop changes.

## Skills (`crates/capabilities/skills/src/lib.rs`)

A skill is `<root>/skills/<name>/SKILL.md` — YAML frontmatter plus a Markdown body. Discovery walks roots in ascending priority — builtin < `~/.wavecode/skills` < `<cwd>/.wavecode/skills` — with same-name skills overridden by the higher-priority root (`standard_roots`); skills exposed over MCP are a specified fourth source that is still only a placeholder. Required field is `description`; optional fields: `when_to_use`, `allowed-tools`, `context: inline | fork`, `user-invocable`, `argument-hint`, `paths` (serde aliases accept kebab-case and snake_case).

Frontmatter parsing uses `serde_yaml` rather than a hand-rolled scanner — a deliberate tradeoff, since skill values may hold colons, lists, and multi-line strings, and a minimal parser's edge cases degrade silently. Frontmatter fields take the SPEC section 8.1 table intersection.

- **Inline** (default): the body expands into the current session's result text, with `$ARGUMENTS` replaced by the call arguments.
- **Fork**: the skill runs in a dedicated subagent via the task service; its `allowed-tools` restrict the child's tool surface (forks without a surface get the registry surface minus the child-spawning tools, so no fork can ever re-spawn children).

`${WAVECODE_SKILL_DIR}` expands to the skill directory so bundled reference files resolve. The model-facing skill catalog is budgeted — 1% of the context window as a character quota, downgraded truncation past the limit (drop `when_to_use` first, then truncate descriptions) — so a large skill pack cannot eat the prompt. The model invokes skills through the `skill` tool (`crates/operations/bootstrap/src/skill_tool.rs`), registered late because it needs the child service; `task_output` / `task_stop` observe and stop what forks spawned.

## Hooks (`crates/capabilities/hooks/src/lib.rs`)

Eight lifecycle points: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `SessionStart`, `SessionEnd`, `Stop`, `PreCompact`, `PostCompact`. Only three are blockable — `PreToolUse`, `UserPromptSubmit`, `Stop` (exit code 2 vetoes and feeds stderr back to the model); on non-blockable points a veto degrades to a warning so a broken hook cannot stall the loop.

Two hook types share one execution shape (matcher / command / stdin payload / timeout):

- `command` — shell command from config (`matcher` is `|`-separated tool names; a matcher on a non-tool point never fires; timeout force-kills with a warning). Config entries carry `matcher` / `command` / `timeout_ms` / `once` (a table or table array).
- `prompt` — registered programmatically via `HookEngine::register_prompt_hook`; stdout is captured (capped, `PROMPT_CONTEXT_TRUNCATED` marker on overflow) and **injected as context** for the model. Prompt hooks never block.

Structurally dead entries are rejected at load (empty commands, blank matchers) rather than silently never firing.

## MCP client (`crates/capabilities/mcp/src/lib.rs`, `crates/operations/bootstrap/src/mcp_bridge.rs`)

Transports: child-process **stdio** and **streamable HTTP** (the HTTP client re-initializes once when the server expires the session with 404; connection loss reconnects with exponential backoff). List methods (`tools/list`, `resources/list`, `prompts/list`) are walked page by page under a bound, so a misbehaving server cannot loop the client forever. Tools discovered via paginated `tools/list` bridge into the registry as `mcp__{server}__{tool}` (`MCP_TOOL_PREFIX`), so server-side names never collide with builtins. When the server advertises the capabilities, two discovery tools bridge resources and prompts — `mcp__{server}__read_resource` (`resources/list` + `resources/read`) and `mcp__{server}__get_prompt` (`prompts/list` + `prompts/get`) — with the server's catalog embedded in the tool description.

Known limits, stated in the code: interactive browser/PKCE OAuth is out of scope (static headers only), and MCP prompts do not yet auto-convert into inline skills — the skills crate carries the `SkillSource::Mcp` placeholder and the core-side conversion wiring is deferred to a later iteration.

## Plugins (`crates/runtime/plugin/src/lib.rs`, `crates/capabilities/skills/src/plugin.rs`)

The runtime `Registry` starts plugins in manifest-dependency order and stops them in reverse; `ServiceMap` injects typed services with type-id keyed lookup. Discovery reads `runtime.toml` manifests (`{name, version, depends?}` identity) and degrades **warn-and-skip**: an invalid manifest warns with its reason and is skipped, never failing assembly. The `PluginLoader` in the skills crate loads user plugin packs (`plugin.toml`: `name`, `version`, optional `skills_dir`, hooks, MCP server entries) with the same warn-and-skip contract, feeding their skills/hooks/MCP servers into normal assembly.

## Named agent definitions (`crates/operations/bootstrap/src/agent_task_tool.rs`)

The `task` tool delegates free-form prompts to child agents. `discover_agent_defs(cwd)` scans `.wavecode/agents/*.md` then `.claude/agents/*.md` (interop with the cross-tool convention); **the first definition of a name wins**, so a repo definition shadows a global one. Unparseable files are skipped — discovery is best-effort and must never fail the call.

Frontmatter fields: `name` (falls back to the file stem), `description`, `tools` (comma-separated or `- item` list), `kind` (`explore` / `readonly` / `read-only` select the read-only profile). The definition becomes an identity preamble plus a tool-surface restriction on the spawned child. See `docs/cookbook/adding-a-subagent.md` for the recipe and its depth-cap section (`MAX_CHILD_DEPTH = 3` in `crates/runtime/child/src/lib.rs`).
