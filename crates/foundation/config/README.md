# crates/foundation/config/ — TOML config loading and provider resolution

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | `Config` (top-level TOML: model, providers, permission mode, hooks, MCP servers, models catalog, `max_tool_rounds`), `ModelEntry`, `home_dir()`, `load`/`load_from`, `resolve_provider`/`resolve_named_provider` |
| `src/provider.rs` | `ProviderKind` (anthropic / OpenAI-compatible / OpenAI Responses, with parse aliases), `ProviderConfig` (redacted `Debug`, fallback chain, reasoning effort), `DEFAULT_CONTEXT_WINDOW`/`DEFAULT_MAX_OUTPUT_TOKENS` |
| `src/hooks.rs` | `HookRule`/`HookRuleSet` (single-table or array-of-tables forms) and `ConfigError` |
| `src/mcp.rs` | `McpServerRaw` — raw `[mcp_servers.<name>]` entries: stdio (`command`/`args`/`env`) and http (`url`/`headers`) fields plus optional OAuth client-credentials fields |
| `src/model_catalog.rs` | `ModelCatalog`/`ModelSpec`/`ApiKind`/`ReasoningSpec`/`ModalitiesSpec` — the `~/.wavecode/models.json` catalog, merged into a `Config` via `ModelCatalog::merge_into` |
| `src/permissions.rs` | `PermissionsConfig` — raw `[permissions]` allow/deny string entries |

This layer parses only; it owns no semantics. Entry validity is checked
upstream — hook event points and the MCP either-or during assembly, and
the permission rule syntax by the sandbox crate, which is the single
authority on verdicts. API-key resolution prefers the env var named by
`env_key` over the inline `api_key`, with empty or whitespace-only
values treated as unset; the home directory comes from `USERPROFILE`
then `HOME`, and loading never falls back to a relative path. The
`[permissions]` table is read from the user-level file only, by design:
the agent can write a repo-local config file, so widened authority
comes only from a human-edited home config. Dependencies are external
only (`serde`, `serde_json`, `toml`, `thiserror`); dependents include
`capabilities/context`, `capabilities/mcp`, the frontends,
`operations/actor`, `operations/bootstrap`, `operations/gateway`, and
`state/checkpoint`.
