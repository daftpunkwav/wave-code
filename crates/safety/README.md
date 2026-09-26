# crates/safety/ — policy-level safety primitives

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `audit/` | `safety-audit` — in-memory append-only audit trail of allow/ask/deny/error decisions |
| `gate/` | `safety-gate` — race-free one-shot parking of approval and question requests keyed by call id |
| `guardrail/` | `safety-guardrail` — heuristic prompt-injection screening and taint tracking |
| `secrets/` | `safety-secrets` — named secret storage with redaction for logs and transcripts |

This layer decides policy only: OS-level isolation (landlock, seatbelt,
ACLs) is out of scope, and permission modes are wire vocabulary owned by
`wavecode-protocol`, never redefined here. Dependency edges are minimal
by design — `audit`, `guardrail`, and `secrets` depend on nothing, while
`gate` needs only `tokio` (oneshot channels) and `thiserror`. The crates
are explicit about their limits: guardrail heuristics are speed bumps,
not walls, and secret redaction is log hygiene, not a security boundary.
