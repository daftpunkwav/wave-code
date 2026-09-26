# crates/runtime/prompt/ — 具名槽位之上的纯系统提示词排版

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；没有任何依赖 |
| `src/lib.rs` | `PromptSlots` / `build_system`：确定性分节组装（identity 在前、summary 在后，空槽位整体跳过）；`SourceBundle` / `assemble_budgeted`：按字符预算组装，超预算时先丢弃低优先级分节并逐一上报；`truncate_to_budget` / `Budget` |

排版就是这个 crate 的全部：每个槽位填什么（memory、skills、tools、
environment）的策略留在 driver 侧，由其组合这些原语完成动态的逐
turn 组装。分节顺序是契约性的，因此快照与 golden 测试在重构中保持
稳定；超预算组装绝不静默丢失分节——每次丢弃都会上报。
