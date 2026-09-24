# Architecture

WaveCode is a headless-first AI coding agent in Rust. One `wavecode` binary serves single-turn execution (`exec`), an interactive REPL, a fullscreen TUI, and legacy session resume over a shared agent core: a multi-turn ReAct loop with tools, skills, memory, and MCP.

The workspace is a flat crate DAG at `crates/<group>/<crate>`. Dependencies point downward only; there are no cycles. This document describes the groups bottom-up, the dependency rules that hold the layering together, and the life of a turn.

## Dependency rules

1. **Everything may depend on `infrastructure/`.** These crates are leaf primitives with zero internal dependencies.
2. **`runtime/runner` depends only on trait seams and data transfer objects** (`state-store`, `wavecode-wire`, `infrastructure-base`). It must not depend on tools, sandbox, hooks, memory, skills, MCP, or transport; concrete implementations are injected from above.
3. **`operations/bootstrap` is the composition root.** It is the only crate allowed to name concrete capability crates and adapt them to the runner's trait seams. Policy lives in the capability crates; only wiring and mapping live in bootstrap. Nothing depends on bootstrap except the frontends (its tests excepted: the gateway's server tests use its hermetic assembly seam as a dev-dependency). The gateway's MCP-serve module is a serving skin that names `wavecode_tools` registry types only; executor construction stays in bootstrap.
4. **Policy never matches tool names.** Tools carry declarative attributes (`wavecode_tools::Tool::is_read_only` / `is_destructive`), and policy decisions consume those attributes (`operations_bootstrap::policy_adapter`), so adding a tool cannot silently drift the policy layer.
5. **New behavior lands on extension points, not loop changes.** Changing `runtime/runner` requires updating this document.

## Layer map

```
frontends      wavecode binary (exec / repl / resume / TUI launch) and the TUI client
                 │
operations     wire protocol, session actor (with the shared session
               contract), bootstrap composition root, RPC gateway (the
               live ACP / HTTP+SSE / MCP-serve surfaces), eval, observe,
               simulate
                 │
runtime        RunLoop, child turns, scheduler, prompt assembly, plugin seam,
               capability inventory, identity, skill routing
                 │
action         tool registry + attributes, child tasks, workflow engine,
               background jobs, browser seam, term retrieval
                 │
safety         tool policy, approval gate, injection guardrail, audit log,
               secrets vault, OS sandbox backends
state          conversation store, turn journal, trajectory, checkpoints,
               durable goals, plans, profiles, workspaces, artifacts
                 │
capabilities   tools, skills, memory, MCP client, sandbox policy, hooks,
               context pipeline          (the "wavecode-*" stack)
foundation     config, multi-provider LLM client, protocol vocabulary, auth
                 │
infrastructure channels + interrupts + limits, TTL/LRU cache, layered config,
               leases, token-bucket rate limit, model routing, JSON schema
```

## Crate inventory

Package names sometimes differ from directory names (the `wavecode-*` capability stack predates the layered layout); the table uses package names.

### `infrastructure/` — leaf primitives

| Crate | Responsibility |
| --- | --- |
| `infrastructure-base` | OS runtime primitives: channel capacities, cooperative interrupt handle, truncation budgets, shared calendar-date rendering |
| `infrastructure-ratelimit` | Token-bucket rate limiting with an explicit clock |

### Vocabulary and DTO layers

| Crate | Responsibility |
| --- | --- |
| `wavecode-wire` | Frontend/backend wire types for submissions and events; the model-facing system-reminder marker |
| `wavecode-protocol` | Shared frontend-protocol vocabulary: permission modes, approval kinds |

### `state/` — durable data

| Crate | Responsibility |
| --- | --- |
| `state-store` | Persisted conversation history and context budget checks |
| `state-persistence` | Append-only JSONL turn journal backing `resume` |
| `state-checkpoint` | Labelled state snapshots with rollback |
| `state-goal` | Durable per-session objective with CAS versioning, plus the model-invokable `goal` tool over it |
| `state-plan` | Reviewed plan-mode state machine |

### `safety/` — policy and isolation

| Crate | Responsibility |
| --- | --- |
| `safety-gate` | Permission modes, policy verdicts, and the approval gate |
| `safety-guardrail` | Heuristic prompt-injection screening and taint tracking |
| `safety-audit` | Append-only audit trail of security-relevant decisions |
| `safety-secrets` | Secret storage with redaction for logs and transcripts |

### `runtime/` — execution core

| Crate | Responsibility |
| --- | --- |
| `runtime-runner` | The RunLoop: owns the per-turn state machine (sample → decide → execute → recover), retry budgets, idempotency keys |
| `runtime-child` | Tracked background child tasks with depth accounting |
| `runtime-scheduler` | Priority queues, delayed tasks, cron matching, limits |
| `runtime-prompt` | System prompt assembly from named content slots |
| `runtime-plugin` | Minimal plugin system: service injection, middleware hooks, in-session lifecycle |

### `action/` — agent action surface

| Crate | Responsibility |
| --- | --- |
| `action-tasks` | Child task lifecycle behind a capability-neutral seam |
| `action-workflow` | Validated DAG execution and Ralph loops over `action-tasks` |
| `action-jobs` | Background shell jobs with wait/cancel/notice semantics |
| `action-browser` | Browser automation behind an async tab seam |
| `action-retrieval` | Term-overlap retrieval over chunked documents |

### `foundation/` + `capabilities/` — the capability stack

Legacy-named `wavecode-*` crates. They are consumed only through bootstrap adapters; new layers must not depend on them directly.

| Crate | Responsibility |
| --- | --- |
| `wavecode-config` | TOML config loading (`~/.wavecode/config.toml`) and provider resolution |
| `wavecode-llm` | Multi-provider abstraction: Anthropic/OpenAI adapters, SSE streaming, retry |
| `wavecode-auth` | Provider-scoped credentials for model access |
| `wavecode-tools` | Tool trait, registry, built-in tools (fs, search, shell, todo), path guarding |
| `wavecode-skills` | `SKILL.md` discovery, parsing, and catalog |
| `wavecode-memory` | Instruction memory (`WAVECODE.md`) and per-turn transcript distillation |
| `wavecode-mcp` | Model Context Protocol client and interface boundary: client/server traits, data types, server config, the `mcp__` naming convention, and the stdio/streamable-HTTP client bridge that injects external tools into the registry (byte-level framing lives in `transport-mcp`) |
| `wavecode-sandbox` | Permission and execution-safety layer for tool runs |
| `wavecode-hooks` | Lifecycle hooks (PreToolUse / PostToolUse / UserPromptSubmit / SessionStart) |
| `wavecode-context` | Context-management pipeline: compression, spill, budget stages |

### `operations/` — assembly and operations

| Crate | Responsibility |
| --- | --- |
| `operations-actor` | Serial session driver: submission routing plus turn driving; also owns the shared session contract (assembly options and failures, plus the `SessionSurface` the RPC servers consume) |
| `operations-bootstrap` | Composition root adapting concrete capabilities to runner traits |
| `operations-gateway` | The live RPC surfaces: ACP (JSON-RPC over stdio), the app server (REST + SSE over loopback HTTP), and MCP serve (tools over stdio), each a serving skin over the session contract |
| `operations-eval` | Behavioural benchmarks over any turn driver |
| `operations-observe` | Folds turn wire events into cumulative operations metrics |
| `operations-simulate` | Dry-run rendering of model-planned actions (plan preview) |

### `transport/` + `frontends/`

| Crate | Responsibility |
| --- | --- |
| `transport-mcp` | JSON-RPC framing over child-process stdio pipes |
| `harness-cli` | The `wavecode` binary: exec, REPL, resume; exit codes follow turn outcomes |
| `tui-engine` | Inline terminal rendering engine: components, editor, markdown, diff screen |
| `console-ui` | Themed console frontend over the wire + actor (transcript, dialogs, slash commands) |

### Wiring status

Wired means reachable from the `wavecode` binary through normal
dependencies. `cargo test --workspace` builds and tests every crate either
way, so a green suite is not evidence of wiring. As of 2026-09-24, **7 of
the 43 library crates are unwired** — they are not
reachable from the `wavecode` binary. Everything above describes what each
crate *does*, not what the product *offers*; this table is the correction.
Check a crate's dependents (`cargo tree -q -i <crate>`) before citing it as a
feature, and move it out of this table in the same change that wires it up.

| Unwired crate | Why it is not reachable |
| --- | --- |
| `operations-simulate` | Library-only: dry-run rendering of planned actions is not a product feature yet |
| `action-browser`, `action-retrieval` | The browser seam and term-overlap retrieval have no implementer or consumer; `wavecode-tools` owns the live registry |
| `safety-guardrail`, `safety-audit` | Live policy and approval flow is `safety-gate` + `wavecode-sandbox`; these overlap it and would need a boundary redraw before adoption, not a splice |
| `state-artifact` | The product's durable data rides `state-persistence` / `state-store` |
| `wavecode-auth` | Provider credentials resolve through `wavecode-config`'s `env_key` path |

### Reserved, unwired by intent

These crates are deliberate seeds for future work, not dead weight. They
compile and test in the workspace and stay out of the shipped binary until
their feature lands:

- `action-browser` — browser automation behind an async tab seam, for
  agent projects that need a browser.
- `action-retrieval` — term-overlap retrieval over chunked documents, for
  agent projects that need local corpus search.
- `safety-guardrail` — prompt-injection screening and taint tracking,
  reserved for a future trust-boundary redraw.
- `safety-audit` — append-only audit trail of security-relevant decisions,
  reserved for deployments that need one.
- `wavecode-auth` — provider-scoped credential storage, reserved for
  multi-provider setups beyond the `env_key` path.
- `operations-simulate` — dry-run rendering of model-planned actions
  (plan preview), for future frontends.
- `state-artifact` — versioned registry of run-produced artifacts, for
  agent projects that need one.

No duplicate-model watch items remain: every crate in the workspace is
either wired, a documented reserved seed, or the composition root itself.

## Life of a turn

1. A frontend (`harness-cli`, TUI, or an HTTP/SSE client through `wavecode serve`) submits a prompt as a wire submission.
2. `operations-actor` serializes submissions per session and drives `runtime-runner`.
3. The RunLoop samples the model through the injected `Model` trait (`wavecode-llm` adapter), decides on tool calls, executes them through the `Tool` seam, and recovers from failures — bounded by retry budgets and context budgets from `state-store`.
4. Tool executions pass the `safety-gate` approval flow; verdicts combine permission modes with the tool's declarative attributes (`is_read_only` / `is_destructive`), and OS-level isolation comes from `wavecode-sandbox`.
5. Every step emits `wavecode-wire` events; frontends render them as stdout JSONL, TUI rows, or gateway frames.
6. History mutations append to `state-persistence`'s block-level write-ahead journal (`<id>.history.jsonl`), which `resume` replays; completed turns additionally append a text snapshot to `<id>.jsonl` for the picker and for sessions written before the journal existed.

## Benchmarks

`benchmarks/` pins turn performance with offline, keyless benches; see [benchmarks/README.md](../benchmarks/README.md). Executable benches live in `crates/runtime/runner/tests/benchmarks.rs`.

## Subsystem pages

Per-subsystem documentation, a cookbook, defensive patterns, and postmortems live alongside this document:

- Subsystems: [core loop](subsystems/core-loop.md) · [tools](subsystems/tools.md) · [context engineering](subsystems/context-engineering.md) · [safety](subsystems/safety.md) · [extensibility](subsystems/extensibility.md) · [sessions and state](subsystems/sessions-state.md) · [evals](subsystems/evals.md) · [console ui](subsystems/ui.md)
- Cookbook: [adding a tool](cookbook/adding-a-tool.md) · [adding a subagent](cookbook/adding-a-subagent.md) · [recording and replaying](cookbook/recording-and-replaying.md)
- [Defensive patterns](defensive-patterns.md)
- Postmortems: [2026-09-13 bulk regex corruption](postmortem/2026-09-13-bulk-regex-corruption.md)
