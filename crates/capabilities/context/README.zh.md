# crates/capabilities/context/ — 上下文管理流水线（记账、阈值、压缩）

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | 整条流水线在一个模块里：token 记账（`estimate_tokens`、`resolve_used_tokens`，ASCII/非 ASCII 分开的估算）、三级 `Thresholds`/`BudgetLevel`（窗口顶部下方 20k/13k/3k 边距）、`CompactionStrategy` trait 及其 `ModelSummary` 实现、`compact_history` / `normalize_history` / `find_pairing_violations`、保缓存的驱逐（`evict_old_tool_results`、`EvictionConfig`）、`ReminderChannel` 注入队列 |
| `src/spill.rs` | `SpillStore`：超限的工具输出持久化到 home 作用域的侧存储（总量上限 32 MB、manifest、最旧先逐出）；`prune_tool_output` 产出 `PRUNE_MARKER`、一个 `spill://` URI 与给模型看的头部摘录 |

压缩流水线只有一条；触发时机由 core 编排，各触发方式共用 `compact_history`
这一个入口。`CompactionStrategy` 是可替换的接缝（首个实现一次模型调用产出
五个固定小节的结构化摘要），而 `normalize_history` 与
`find_pairing_violations` 构成压缩与恢复两条路径共享的配对完整性契约。
依赖只指向 foundation 层 crate——`wavecode-llm`（摘要调用）、
`wavecode-config`（`home_dir`）、`wavecode-wire`（`system-reminder` 标记）——
且 `spill.rs` 明确写出边界：不依赖 runtime、transport 或 UI 层组件。
