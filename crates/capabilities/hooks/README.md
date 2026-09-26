# crates/capabilities/hooks/ — lifecycle hooks

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | The whole crate in one module: `HookEventPoint` (eight event points, three of them blockable), `HookDef` / `HookInput` / `HookVerdict` / `HookReport`, and `HookEngine` — `command` hooks from `[hooks.<EventPoint>]` config run via the platform shell with the event payload on stdin; `prompt` hooks registered programmatically capture stdout (64 KB cap, truncation marker) as injected context and never block |

Blocking semantics are exit-code driven: 0 allows, 2 blocks on the blockable
points (`PreToolUse` / `UserPromptSubmit` / `Stop`) with stderr fed back to
the model, 2 elsewhere and any other nonzero code degrade to
allow-with-warning, and timeouts force-kill with a logged warning. The trust
boundary differs from the shell tool by design: hook commands come from the
user's own config file — authorized configuration, so no env-var stripping
or path constraints apply. The crate depends only on `infrastructure-base`
(the shared `shell_invocation` resolution), `serde_json`, and `tokio`;
firing hooks and wiring verdicts into the turn loop stay core-side.
