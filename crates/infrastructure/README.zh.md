# crates/infrastructure/ — 集中管理上限值的运行时机制

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `base/` | `infrastructure-base`——OS 运行时原语：通道容量、超时、截断预算、协作式 `InterruptHandle`、共享的日历日期与 shell 解析 |
| `ratelimit/` | `infrastructure-ratelimit`——以调用方提供的时间做纯算术的 `TokenBucket` 限流 |

这些 crate 承载机制而非策略：散落的魔法数字在此集中并附上理由，
所有上层读取同一个常量，而不是各自发明。两者都不依赖任何 workspace
内 crate（`base` 连外部依赖都没有；`ratelimit` 仅依赖 `thiserror`），
并且只被上层消费——`base` 的消费方有 `action/jobs`、
`capabilities/hooks`、`capabilities/tools`、`operations/actor`、
`operations/bootstrap`、`operations/gateway`、`runtime/child`、
`runtime/runner` 与 `state/store`；`ratelimit` 的消费方是
`operations/bootstrap`。本目录没有任何反向依赖上层的代码。
