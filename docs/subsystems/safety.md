# Subsystem: safety

Safety is layered: vocabulary (`crates/foundation/protocol`), static policy and the approval gate (`crates/safety/{policy,gate}`), the permission sandbox (`crates/capabilities/sandbox`), and OS confinement backends in the same crate. The runner consumes all of it through the `PolicyDecider` / `ApprovalSource` seams.

## Permission modes and policy

`crates/foundation/protocol/src/lib.rs` owns `PermissionMode` with locked wire strings: `plan` (read-only tools only; the rest deny straight back to the model; the system prompt nudges the model to propose via the plan tool), `guarded` (command execution and destructive tools ask per call; file edits and other non-exec writes flow through), `auto` (approve everything — deny rules still apply). A unit test locks the serde tags; `parse` rejects case drift and maps legacy names (`default`/`acceptEdits` → `guarded`, `bypassPermissions` → `auto`) onto their successors.

`crates/safety/policy/src/lib.rs` is pure data: `Rule` (exact or `prefix*` wildcard), `ToolPolicy::evaluate` with **deny-wins merging** (Deny > Ask > Allow regardless of rule order) and an unmatched default of `Ask` — a missing rule can never silently permit execution. `explain` cites the winning rules for audit output. `ExecutionPolicy` carries ceilings (32 rounds, 120 s approval timeout, 5 consecutive errors) and rejects zero ceilings at startup.

## Approval parking and timeout-deny

`crates/safety/gate/src/lib.rs` provides `ApprovalGate`: one waiter per call id (`GateError::DuplicateWaiter` otherwise), one-shot `decide` (late decisions for consumed ids return `false` and are dropped, so a stale UI click can never approve a future call), and `cancel` so an expired wait frees its id. `QuestionGate` mirrors it with free-text answers for the `ask_user` interactive-question flow.

`crates/operations/bootstrap/src/gate_adapter.rs` implements the runner's `ApprovalSource` over the gate: waits are bounded by a deadline; **expiry resolves to `Deny` with an explicit reason, never parks forever**, and a session interrupt ends a parked wait immediately as `Interrupted` (the reservation is withdrawn so the id can park fresh). A waiter dropped mid-wait (gate cleared) also denies. A `Headless` variant denies openly for non-interactive drivers. `clear_stale` runs at the start of every session-owned turn (child turns skip it).

## The sandbox decision (`crates/capabilities/sandbox/src/lib.rs`)

`Sandbox::decide(tool, input, read_only, destructive)` evaluates in a fixed order:

1. **Deny rules first** — no mode exempts them, not even `auto`. Both the whole compound Bash command and its segments match, so `echo hi\ncurl …` cannot prefix-disguise past `Bash(curl *)`.
2. **Allow rules** — bound to tool semantics via rule scope (loose input-key sniffing is not enough); for compound Bash commands only *literally exact* rules exempt, because a wildcard `*` spans command separators.
3. In-session state-tool exemptions (`todowrite` and the merged `goal` / `plan` tools need no approval in any mode — they write harness-owned coordination state, never the repo) and interactive-question routing (`ask_user`) — except `plan` with `action: "approve"`, which asks in every mode: only the user may approve a proposal.
4. The mode's default policy.

`allow_always` derives one exact, session-level allow rule from the approved call (shared through `Arc` so clones — including subagents — see it; deny-first is unaffected). Session allow rules are in-memory only; persisting them to config is not wired yet. Invalid rule entries fail construction at startup — explicit failure, never silent skips. `PolicyAdapter` (`crates/operations/bootstrap/src/policy_adapter.rs`) maps these verdicts onto the runner seam, sourcing attributes from the registry.

## OS sandbox: fail-closed chain (`crates/capabilities/sandbox/src/chain.rs`)

`SandboxBackend` exposes `is_available` / `backend_name` / `enforcement` / `spawn_confined`. `PROBE_ORDER` is `["bwrap", "landlock", "seatbelt", "job"]` — first available wins (`first_available`). When nothing is available, the chain terminates in `UnavailableBackend`, which **refuses every spawn**: fail-closed means unsure ⇒ refuse. `status_line` renders `SANDBOX_UNAVAILABLE` as a greppable token for unavailable backends.

Enforcement honesty (`EnforcementLevel::Full | Partial`):

| Backend | Level | Scope |
| --- | --- | --- |
| `bwrap` (`src/bwrap.rs`) | Full | bubblewrap with mount control |
| Landlock (`src/os.rs`) | Partial | kernel ruleset, known gaps |
| seatbelt (`src/seatbelt.rs`) | Partial | macOS `sandbox-exec` profile gaps documented |
| Windows job object (`src/windows.rs`) | Partial | **only** process-tree lifetime (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) + active-process cap |

The Windows backend states its limits explicitly (`WINDOWS_UNAVAILABLE_REASON`): no filesystem write boundary, no network policy, no integrity-level/AppContainer isolation — confinement requests beyond its scope fail closed rather than pretend. A `Partial` claim is the honest one.

## Env scrubbing and path guards

`crates/capabilities/tools/src/shell_tool.rs::sanitize_env` strips, before every spawn: (1) the explicit `ToolCtx::deny_env` list (assembly injects provider key names), and (2) a sensitive-shape fallback matching name segments and suffixes (`_KEY`, `_PAT`, `AWS_SECRET_ACCESS_KEY`-style) — a pure suffix list misses real shapes, so both are checked.

Filesystem containment is `path_guard::resolve` (canonicalize-based prefix checks; see `docs/subsystems/tools.md`). Secrets redaction for transcripts and logs lives in `crates/safety/secrets` (`redact`, longest value first) and `crates/foundation/auth` (`Credential`'s manual `Debug` prints `REDACTED`, never the material).
