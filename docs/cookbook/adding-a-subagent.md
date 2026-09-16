# Cookbook: adding a subagent

Subagents are model-invoked through the `task` tool and defined by plain Markdown files with frontmatter. Everything below is implemented in `crates/operations/bootstrap/src/agent_task_tool.rs`; depth enforcement lives in `crates/runtime/child/src/lib.rs`.

## 1. Write the definition file

Create `.wavecode/agents/<file>.md` in the project (or `~`-level packs via `.claude/agents/`):

```markdown
---
name: explorer
description: Wide read-only codebase investigation; returns a summary of findings.
tools:
  - read
  - grep
  - glob
kind: explore
---

Optional body is ignored by the parser; keep the identity in `description`.
```

Frontmatter rules (parsed by `parse_agent_def`):

- `name` — required in practice; falls back to the file stem when empty.
- `description` — surfaces to the model as the delegation purpose; also becomes the child's identity preamble ("You are the `explorer` agent. Purpose: …").
- `tools` — restricted tool surface; comma-separated (`tools: read, grep`) or a `- item` list. Empty keeps the full session surface.
- `kind` — `explore`, `readonly`, or `read-only` select the read-only profile (`TaskKind::ReadOnly`). Anything else is `Standard`.
- A missing `---` frontmatter block makes the whole file unparseable; it is skipped.

## 2. How discovery runs

`discover_agent_defs(cwd)` scans `cwd/.wavecode/agents/*.md` then `cwd/.claude/agents/*.md` (cross-tool convention), sorting file paths within each directory. **The first definition of a name wins**, so a repo definition shadows a same-named global one. Unreadable or unparseable files are skipped silently — discovery is best-effort and must never fail the `task` call. There is no reload/cache step: definitions are re-read per `task` invocation, so edits apply immediately.

## 3. How the `task` tool applies the profile

`TaskTool::execute` resolves the invocation:

- `subagent_type: "explore"` → built-in read-only profile, no preamble.
- `subagent_type: "<name>"` → the discovered definition: `kind` maps to the child capability profile, the identity preamble is prepended to the caller's `prompt`, and the definition's `tools` become the child's allowlist.
- An explicit `allowed_tools` array in the call **overrides** the definition's surface.
- Unknown `subagent_type` → business error (`is_error`) listing `explore` plus every discovered name, so the model self-corrects.
- Missing `prompt` → business error.

The child runs its own conversation on the session driver, so it cannot interrupt the parent. The call blocks until the child finishes — bounded by `TASK_WAIT_TIMEOUT` (600 s, polled at 250 ms) — and returns the child's summary inline; a still-running child is reported with its id for `task_output` / `task_stop`. Per-run allowlist enforcement happens in the loop via `RunAllowlist` (`crates/runtime/runner/src/lib.rs`): restricted children never even *see* denied tools in `available_tools`, so they plan within their surface instead of hitting refusals.

## 4. Depth caps and the no-grandchildren rule

- `task` always spawns at `depth: 0` — a delegation from inside a child turn does not nest; it is a fresh top-level child on the shared service. Children get follow-ups only through the tracked continue path, which spawns with `depth + 1` and `parent` set (`crates/operations/bootstrap/src/child_service.rs`).
- `crates/runtime/child/src/lib.rs` enforces `MAX_CHILD_DEPTH = 3`: a spec past the cap is refused before its factory is even built ("max child depth 3 exceeded"), returning an explicit failed outcome rather than running unbounded work.
- Net effect: no unbounded agent trees — fan-out is wide but shallow, and every child is observable and stoppable through the task service.

## 5. Checklist

- [ ] File parses (leading `---`, keys `name` / `description` / `tools` / `kind`).
- [ ] Name does not collide with a global definition unless shadowing is intended.
- [ ] `tools` lists only names that exist (typo = silently missing tool in the child).
- [ ] Read-only intent expressed via `kind: explore` (not by hoping the model behaves).
- [ ] Verified by a real delegation: `task(prompt=..., subagent_type=<name>)` returns the child's summary, or `task_output` shows the still-running id.
