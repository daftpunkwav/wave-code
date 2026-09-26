# crates/operations/eval/ — 脚本化基准、录制会话回放与任务评分

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；只依赖 `runtime-runner`、`state-store`、`wavecode-wire` |
| `src/lib.rs` | `EvalCase`/`EvalResult`/`EvalReport` 与 `evaluate`：脚本化用例经 `TurnDriver` seam 运行，对照最终历史检查期望 |
| `src/replay.rs` | 录制会话回放：按 wire 契约（顺序、配对、settle-once）校验 JSONL 事件录制，再对助手文本评分——无模型、无工具 |
| `src/task.rs` | 任务级基准：`task.toml` 清单，`Assertion` 针对调用方提供的 `World`（文件系统读取 + 命令退出码）评分，套件报告可渲染为文本或 JSON |

这里没有任何执行 agent 的代码：用例经泛型 `TurnDriver` seam 运行，
回放是对录制事件的纯只读折叠，任务评分只裁判调用方经 `World` 交回
的观察结果。即使文本恰好匹配，turn 失败也算用例失败；没有断言的
任务不裁判任何东西。
