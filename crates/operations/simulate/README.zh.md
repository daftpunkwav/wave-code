# crates/operations/simulate/ — 模型计划动作的只读 dry-run 渲染

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；唯一依赖是 `runtime-runner` |
| `src/lib.rs` | `render_plan` / `PlanSummary`：把采样得到的 `SampleBlock` 逐块渲染成 dry-run 行——文本作为发言，工具块作为带原始输入的调用 |

这里的模拟是诚实的预览：只打印"将会运行什么"，绝不执行任何东西。
result、image 与 thinking 块不会出现在计划里，也不接触 executor、
policy 或审批——本模块与 run loop seam 只共享块词汇表。
