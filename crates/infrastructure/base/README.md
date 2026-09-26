# crates/infrastructure/base/ — OS runtime primitives: constants, interrupts, shared renderings

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | Capacity and timeout constants (`EVENT_CHANNEL_CAP`, `CONTROL_CHANNEL_CAP`, `PENDING_QUEUE_CAP`, `SHUTDOWN_DRAIN`, `APPROVAL_POLL`, `HOOK_DEFAULT_TIMEOUT`), truncation budgets (`EVENT_TEXT_TRUNCATION`, `APPROVAL_DETAIL_TRUNCATION`), `InterruptHandle`, `truncate`, `shell_invocation`, `format_date`/`format_timestamp` |

Every capacity, timeout, and truncation budget lives here with its
rationale, so no upper layer keeps its own copy of a magic number that
can silently disagree with the rest. `InterruptHandle` is cooperative:
tasks poll `is_triggered` between units of work instead of being
cancelled mid-operation, so no half-written state is left behind.
`shell_invocation` resolves the platform command-string shell once
(`cmd /C` on Windows, `sh -c` elsewhere, `WAVECODE_SHELL` override) so
every spawn path — shell tool, PTY shell, hooks, jobs — honors the
same override. `format_date`/`format_timestamp` are the single UTC
renderings (dependency-free civil-date math) shared by prompt assembly
and event headers. The crate declares zero dependencies.
