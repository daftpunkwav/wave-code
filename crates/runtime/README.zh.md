# crates/runtime/ — 执行编排：run loop、子任务、插件、提示词与调度

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `child/` | `runtime-child` — 带结构化深度隔离的受管后台子任务 |
| `plugin/` | `runtime-plugin` — 按清单依赖序启动的插件注册表与类型擦除服务注入 |
| `prompt/` | `runtime-prompt` — 具名槽位之上的纯系统提示词排版 |
| `runner/` | `runtime-runner` — run loop 状态机，外加所有能力都要接入的防腐 trait seam |
| `scheduler/` | `runtime-scheduler` — 优先级/延迟队列、cron 匹配与持久化 cron 条目 |

这一层承载编排与被动机制，其 `Cargo.toml` 的依赖边即是证明：
`runner` 只依赖 `state-store`、`wavecode-wire` 与
`infrastructure-base`；`child`、`prompt`、`plugin`、`scheduler` 则
完全没有 workspace 依赖（或仅有 `infrastructure-base`）。这里不出现
任何能力 crate——工具、沙箱、钩子、模型都不在此层：具体能力只经
`runner` 的 trait seam 进入，由 `operations-bootstrap` 的组合根接线。
