# crates/safety/gate/ — approval and question gates

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; `thiserror`, `tokio` (oneshot channels) |
| `src/lib.rs` | `ApprovalGate` (`wait_for`/`decide`/`cancel`/`clear`) and `QuestionGate` (free-text answers), `ApprovalDecision` (AllowOnce/AllowAlways/Deny), `GateError`, deprecated `ApprovalKind` alias |

Each call id admits exactly one parked waiter, and delivery is
one-shot: the first decision takes the slot and late decisions return
`false`, so a stale UI click can never approve a later call reusing the
id. `cancel` frees expired waits so their call ids can park again, and
a withdrawn waiter's receiver resolves as dropped, never as approved.
Mutex poisons are recovered by taking the inner guard, because these
short critical sections leave no half-written invariants behind. The
crate decides policy only — OS-level isolation is out of scope, and
canonical permission types live in the protocol crates, which makes the
`ApprovalKind` alias here deprecated.
