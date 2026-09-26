# crates/capabilities/sandbox/ — permission policy and execution-safety layer

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | Policy core: `Rule` parsing and matching (`Bash(...)` / `File(...)` scopes, wildcard config rules vs exact-match session rules), `wildcard_match`, the `Verdict` enum (Allow / Ask / Deny) and `Sandbox::decide` (deny-first; sensitive credential paths always ask), approval-detail capping |
| `src/bash.rs` | tree-sitter-bash AST command segmentation for permission matching — each parsed command yields its full text plus a "bare" variant with env prefixes dropped; parse errors fall back conservatively to string segmentation |
| `src/os.rs` | The backend seam: `ConfinementProfile` (writable/readonly roots, network flag), the `SandboxBackend` trait over `tokio::process::Command`, `EnforcementLevel`, Linux `LinuxLandlockBackend`, `UnavailableBackend`, `detect_backend` |
| `src/bwrap.rs` | Linux bubblewrap backend: probes `bwrap --version` plus a user-namespace round-trip (fails closed without it) and rewrites the spawn into a `bwrap` isolation prefix; reports Full enforcement |
| `src/seatbelt.rs` | macOS `sandbox-exec` backend: generates a deny-by-default profile per cwd; Partial enforcement — network access is not restricted per-process |
| `src/windows.rs` | Partial Windows backend via Win32 Job Objects: process-tree lifetime control (kill on job close) plus a per-spawn process cap; no filesystem or network boundary, and requests beyond that scope fail closed |
| `src/chain.rs` | Probe chain: `PROBE_ORDER` (bwrap -> landlock -> seatbelt -> job), `first_available` fail-closed selection, and `status_line` carrying the greppable `SANDBOX_UNAVAILABLE` token |

Mechanism is separated from policy: `Sandbox::decide` rules on intent (core
passes `read_only`/`destructive` in, so this crate never depends back on
`tools`), while a backend enforces one spawn's `ConfinementProfile` — opt-in
per spawn through `WAVECODE_SANDBOX_OS`, refused when enabled and no backend
is available. The documented threat model is explicit about limits: no
containers, VMs, or syscall filtering; without confinement a spawned process
runs with the user's own OS privileges. `PermissionMode`/`ApprovalKind` come
from `wavecode-protocol`; `tools` depends on this crate in production, and a
dev-dependency test locks the tool-name classification vocabulary.
