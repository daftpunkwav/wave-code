# crates/infrastructure/base/ — OS 运行时原语：常量、中断、共享渲染

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | 容量与超时常量（`EVENT_CHANNEL_CAP`、`CONTROL_CHANNEL_CAP`、`PENDING_QUEUE_CAP`、`SHUTDOWN_DRAIN`、`APPROVAL_POLL`、`HOOK_DEFAULT_TIMEOUT`）、截断预算（`EVENT_TEXT_TRUNCATION`、`APPROVAL_DETAIL_TRUNCATION`）、`InterruptHandle`、`truncate`、`shell_invocation`、`format_date`/`format_timestamp` |

每个容量、超时与截断预算都在这里集中定义并附上理由，上层不再自留
一份可能与其他处悄然不一致的魔法数字。`InterruptHandle` 是协作式的：
任务在两个工作单元之间轮询 `is_triggered`，而不是在操作中途被取消，
因此不会留下半写状态。`shell_invocation` 对平台命令字符串 shell
只解析一次（Windows 上 `cmd /C`，其他平台 `sh -c`，`WAVECODE_SHELL`
可覆盖），shell tool、PTY shell、hooks、jobs 等所有 spawn 路径遵循
同一覆盖。`format_date`/`format_timestamp` 是唯一的 UTC 渲染（无外部
依赖的公历日期算法），prompt 组装与事件头共用。本 crate 零依赖。
