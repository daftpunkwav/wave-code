# Subsystem: safety

English | [中文](safety.zh.md)

Safety is layered: vocabulary (`crates/foundation/protocol`), the approval gate (`crates/safety/gate`), the permission sandbox with rule evaluation (`crates/capabilities/sandbox`), and OS confinement backends in the same crate. The runner consumes all of it through the `PolicyDecider` / `ApprovalSource` seams.

## Permission modes and policy

`crates/foundation/protocol/src/lib.rs` owns `PermissionMode` with locked wire strings: `plan` (read-only tools only; the rest deny straight back to the model; the system prompt nudges the model to propose via the plan tool), `auto` (command execution and destructive tools ask per call; file edits and other non-exec writes flow through), `wave` (approve everything — deny rules still apply). A unit test locks the serde tags; `parse` rejects case drift and maps legacy names (`guarded`/`default`/`acceptEdits` → `auto`, `bypassPermissions`/`yolo` → `wave`) onto their successors. Note the rename hazard: `auto` used to mean "approve everything" (what `wave` means now), so old config files land one notch more conservative, never less.

Permission rules are pure data in `crates/capabilities/sandbox/src/lib.rs`: `Rule` parses exact and `prefix*` wildcard entries (`Rule::parse`), and evaluation is deny-first — deny rules bind in every mode before allow rules or the mode's default are consulted, so an unmatched input can never permit more than its mode allows (the full decision order is in "The sandbox decision" below). `is_covered_by` conservatively detects allow rules a deny rule shadows, for `wavecode doctor`.

## Approval parking and timeout-deny

`crates/safety/gate/src/lib.rs` provides `ApprovalGate`: one waiter per call id (`GateError::DuplicateWaiter` otherwise), one-shot `decide` (late decisions for consumed ids return `false` and are dropped, so a stale UI click can never approve a future call), and `cancel` so an expired wait frees its id. `QuestionGate` mirrors it with free-text answers for the `ask_user` interactive-question flow.

`crates/operations/bootstrap/src/gate_adapter.rs` implements the runner's `ApprovalSource` over the gate: waits are bounded by a deadline; **expiry resolves to `Deny` with an explicit reason, never parks forever**, and a session interrupt ends a parked wait immediately as `Interrupted` (the reservation is withdrawn so the id can park fresh). A waiter dropped mid-wait (gate cleared) also denies. A `Headless` variant denies openly for non-interactive drivers. `clear_stale` runs at the start of every session-owned turn (child turns skip it).

## The sandbox decision (`crates/capabilities/sandbox/src/lib.rs`)

`Sandbox::decide(tool, input, read_only, destructive)` evaluates in a fixed order:

1. **Deny rules first** — no mode exempts them, not even `auto`. Both the whole compound Bash command and its segments match, so `echo hi\ncurl …` cannot prefix-disguise past `Bash(curl *)`. Segments are quoting-aware when the command parses: tree-sitter-bash extraction ignores commands only *mentioned* inside quotes and adds an assignment-stripped view, so `X=1 curl evil` no longer slips past `Bash(curl *)`; unparseable input falls back to string segmentation (coverage never drops below the pre-parser level).
2. **Sensitive credential files ask** (`.env` family with documentation variants exempt, SSH private keys, `.aws`/`.gcp` credential stores) — in every mode including `wave`, since this ask is the only line of defense against an injected prompt quietly reading secrets. Exact allow rules naming the very path (session "always allow") exempt; wildcard allows cannot.
3. **Dangerous commands ask** (`crates/capabilities/sandbox/src/risk.rs`) — in `auto` and `wave` modes, a shell command carrying an inherently destructive construct asks with the reason named: block-device writes (`dd of=/dev/*`), filesystem destruction (`mkfs`, `wipefs`, partition editors), power control (`shutdown`, `systemctl reboot`), recursive forced deletes of system roots (`rm -rf /`, `Remove-Item -Recurse -Force C:\`), downloads piped into a shell (`curl … | sh`), and fork bombs. Detection runs over the tree-sitter AST (quoting and indirection wrappers like `sudo`/`env`/`nohup` are seen through); an unparseable command degrades to a raw-text screen of the most distinctive tokens — fail toward asking, never toward silence. This is the one gate that survives `wave` mode: a denylist only covers what the user foresaw, while a prompt-injected destructive command should need a human even when the mode allows everything. Plan mode skips the guard (its denial is stricter than any ask), and an exact session allow of the same command text still exempts (step 2's asymmetry applies: the grant re-loads as non-exact, so the ask returns next session). The table is deliberately small and high-signal — broad categories belong to the user's denylist, not to a guard that nags on routine work.
4. **Allow rules** — bound to tool semantics via rule scope (loose input-key sniffing is not enough); for compound Bash commands only *literally exact* rules exempt, because a wildcard `*` spans command separators.
5. In-session state-tool exemptions (`todowrite` and the merged `goal` / `plan` tools need no approval in any mode — they write harness-owned coordination state, never the repo) and interactive-question routing (`ask_user`) — except `plan` with `action: "approve"`, which asks in every mode: only the user may approve a proposal.
6. The mode's default policy.

`allow_always` derives one exact, session-level allow rule from the approved call (shared through `Arc` so clones — including subagents — see it; deny-first is unaffected) and hands the same rule to the grant sink, which stores it for later sessions.

## Where the startup rules come from (`operations-bootstrap`, `session::load_permissions`)

Allow has two sources, deny two. Widened authority is only ever human-authored:

| Source | File | Contents |
| --- | --- | --- |
| `[permissions] allow` / `deny` | `~/.wavecode/config.toml` | rule entries a human wrote, wildcards allowed (`Bash(cargo test *)`) |
| persisted grants | `~/.wavecode/grants.jsonl` | literal entries appended when a human answers "always allow" |
| `permission_mode` | `~/.wavecode/console-settings.json` | the saved mode shown at startup |
| `wave_denylist` | `~/.wavecode/wave-denylist.json` | bare command fragments / `Bash(pattern)` rules, Bash-scoped on load; every session surface loads this store |

Two bounds keep the persistent half from becoming a way to widen authority by accident:

- **User-level only.** The planned project layer (`.wavecode/config.toml` inside the working directory) stays unwired for these tables: the agent can write that file, so a repo-scoped allow table would let a session grant its own future exemptions.
- **Grants are literals.** `add_grant` refuses any entry carrying `*` or `?`. A derived rule compared *literally* in the session it was approved in; storing it and re-parsing it as a config entry on the next load would silently promote the approved text into a wildcard allow surface. Approved commands that happen to contain glob characters still exempt for the rest of that session (with a `tracing::warn!`), and a human who wants a wildcard writes one in the config file.

Entries validate one at a time (`Rule::parse` per line): an invalid entry costs only itself and surfaces as a startup finding, never as a dropped table — losing the deny table over a typo in an allow line would widen authority silently. One asymmetry is deliberate: a persisted `File(...)` grant re-loads as a non-exact rule, so it never exempts the sensitive-credential ask (step 2 above requires an *exact* rule) — approving one read of `.env` does not buy a permanent one, and the ask returns next session. Grants and rules are reported by `wavecode doctor` (`permissions: …`), which also flags an allow rule a deny rule provably shadows (`Rule::is_covered_by`, conservative: it can miss a dead rule, never invent one), and `wavecode grants list|remove <i>|clear` reads and revokes the grant table (revocation rewrites the file write-then-rename, so a crash cannot leave it truncated into "no grants"). `PolicyAdapter` (`crates/operations/bootstrap/src/policy_adapter.rs`) maps verdicts onto the runner seam, sourcing attributes from the registry.

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
