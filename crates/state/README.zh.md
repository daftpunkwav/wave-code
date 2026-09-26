# crates/state/ — 每会话状态及其持久化

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `artifact/` | `state-artifact`——运行产出物的版本化登记表 |
| `checkpoint/` | `state-checkpoint`——带回滚的命名检查点以及文件内容快照 |
| `goal/` | `state-goal`——持久的每会话 goal 状态机与可被模型调用的 `goal` 工具 |
| `persistence/` | `state-persistence`——turn journal、history 写前日志、grant 表、session registry、legacy 导入 |
| `plan/` | `state-plan`——受评审的 plan 模式状态机与可被模型调用的 `plan` 工具 |
| `store/` | `state-store`——内存中的会话历史,带冻结快照与上下文预算分级 |

除 `store` 外——它把历史保存在内存中,把持久化委托给自己的 `HistorySink`——这一层的每个 crate 都把状态持久化在 `<home>/.wavecode/` 之下(`goals/`、`plans/`、`snapshots/`、`sessions/`、`grants.jsonl`),写入都先写临时文件再 rename 原子落盘,文件缺失一律按全新/空状态读取。依赖边保持很窄:`artifact` 零依赖,`checkpoint` 仅用 `wavecode-config` 取 home 目录,`persistence` 仅用 `wavecode-llm` 解析 legacy 消息形状,`store` 仅用 `infrastructure-base` 做时间戳格式化;只有 `goal` 和 `plan` 跨出本层——为了 `wavecode-tools` 中共享的 `Tool` trait。它们都不依赖 runtime、action、safety、operations、transport 或任何编排层 crate。
