# capabilities/sandbox/ agent rules

The policy map lives in [README.md](README.md).

## Decide order

`Sandbox::decide` keeps this order:

1. Deny rules, including a deny matched against a language-server
   command. Deny wins over every later step.
2. Sensitive credential paths ask, unless an exact file allow matches.
   This step skips `ToolKind::Shell`.
3. Outside plan mode, a dangerous command asks, unless an exact bash
   allow matches.
4. Allow rules. On a compound bash command, only an exact allow
   matches.
5. `ToolKind::SessionState` allows, except input `action = "approve"`,
   which asks.
6. `ask_user` with a valid question payload returns the question
   verdict. `is_user_question` matches that name only. Do not add
   another tool-name match.
7. A language-server command is command execution even when the tool
   attribute is read-only. Plan mode denies the spawn. Auto mode asks.
   A dangerous server command asks in Wave mode unless an exact bash
   allow matches; that allow does not stop Plan from denying or Auto
   from asking. Wave mode falls through when the command is not
   dangerous, and when an exact bash allow skipped the dangerous ask.
8. Mode policy. `Plan` denies a tool that is not read-only or is
   destructive. `Wave` allows. A read-only non-destructive tool
   allows. `FileEdit` and `Present` allow when they are not
   destructive. Everything else asks.

- `PermissionMode` and `ApprovalKind` come from `wavecode-protocol`.
- `Sandbox::new` rejects invalid rules.
- The normal workspace dependency is `wavecode-protocol`.
  `wavecode-tools` stays a dev-dependency for the classification lock
  test.

## Confinement

- OS backends are opt-in per spawn through `WAVECODE_SANDBOX_OS`.
- When confinement is requested and no backend is available, the
  spawn is refused. The status line keeps the `SANDBOX_UNAVAILABLE`
  token.
- Probe order is bubblewrap, then Landlock, then seatbelt, then Job
  Objects.
- When `is_available()` is true, `spawn_confined` ignores the
  filesystem and network fields of the profile. It returns
  `SandboxError::ConfineFailed` when `create_confined_job` fails or
  the assignment watcher has not started. Otherwise it returns
  `Ok(ArmedSpawn)`. It enforces process-tree lifetime and
  `MAX_PROCESSES_PER_JOB`. `enforcement()` is `Partial`.
  `status_appendix()` is `JOB_STATUS_GAP`.
  `WINDOWS_UNAVAILABLE_REASON` is the long-form disclosure pinned
  against that gap. It is not the spawn error.
- When the Job Object backend is unavailable, its `spawn_confined`
  returns `SandboxError::Unavailable`.
- The seatbelt backend's `enforcement()` is `Partial`. It does not
  apply a per-process network boundary.
- Bubblewrap's `enforcement()` is always `Full`. `is_available()` is
  true only on Linux, after `bwrap --version` and the user-namespace
  probe both succeed. When bubblewrap is unavailable, its
  `spawn_confined` returns `SandboxError::Unavailable`.
