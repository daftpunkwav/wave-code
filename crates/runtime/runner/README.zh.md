# crates/runtime/runner/ — run loop 状态机及其防腐 trait seam

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；只依赖 `state-store`、`wavecode-wire`、`infrastructure-base` |
| `src/lib.rs` | `RunLoop<E, P, H, M, A, T, C>`：sample → decide → execute → recover 状态机，外加 DTO 词汇（`RunContext`、`StopReason`、`ToolCall`/`ToolResult`、`PolicyVerdict`、`SampleRequest`/`SampleResponse`）与全部防腐 trait（`ToolExecutor`、`PolicyDecider`、`HookGateway`、`ModelGateway`、`ApprovalSource`、`PlanTracker`、`GoalTracker`、`Compactor`、`TurnDriver`） |
| `tests/benchmarks.rs` | 对照 `benchmarks/baseline.json` 钉住 turn 性能与形态的离线基准；真实 provider 冒烟仅在 `LIVE=1` 下运行且从不失败 |

loop 对七个 seam trait 加 goal tracker 与 compactor 保持泛型；具体
能力（工具、策略、钩子、模型）由组合根接到这些 trait 之后，因此本
crate 永远无法 import 它们。终止是结构性的：硬性 tool-round 上限
（开放会话目标可重新武装）、响应式 compact 限额与重复调用熔断都以
停止 turn 代替报错。`TurnDriver` 是会话 actor 消费的 blanket
implementation seam，使传输层与 actor 无需接触各个具体能力。
