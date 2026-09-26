# crates/infrastructure/ratelimit/ — 基于显式时间的令牌桶限流

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | `TokenBucket`（经 `try_new` 校验的构造、`try_acquire`/`try_acquire_n`、`time_until_available` 重试提示、`available`）与 `BucketConfigError` |

这里的限流是对调用方提供的时间戳做纯算术：桶自身从不读时钟，因此
测试下行为完全确定，调用方也可以用任意时间源驱动它。补充（refill）
是惰性的并在容量处截断；时钟回拨永远不会让桶超量。`try_new` 拒绝
零容量、非有限或负的补充速率、非有限的时间戳，而不是默默钳制。
`time_until_available` 是纯查询——从不改动状态——并且当请求永远无法
满足时（超过突发容量，或桶永不补充）返回 `None`。本 crate 只依赖
`thiserror`；当前 workspace 内的消费者是 `operations/bootstrap`。
