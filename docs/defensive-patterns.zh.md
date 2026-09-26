# 防御模式

[English](defensive-patterns.md) | 中文

本代码库中反复出现的防护手段，每条配一行代码锚点。新代码应复用这些模式，而不是另造局部变体。

| 模式 | 规则 | 锚点 |
| --- | --- | --- |
| Fail-closed 裁决 | 拿不准就拒绝：sandbox 探测链的末端是一个拒绝所有 spawn 的后端，Windows 上不可用的隔离手段会带明确原因 fail-closed。 | `crates/capabilities/sandbox/src/chain.rs`、`src/windows.rs` |
| 业务错误即数据 | 工具失败以 `Ok(ToolOutput { is_error: true, .. })` 传递，绝不 panic；`Err` 只表示实现故障，以 `tool fault:` 前缀暴露。 | `crates/capabilities/tools/src/lib.rs`、`crates/operations/bootstrap/src/tool_adapter.rs` |
| 原子写 | 有写副作用的文件工具先写临时文件再改名，中途失败不会破坏原文件。 | `crates/capabilities/tools/src/fs/write.rs`、`fs/edit.rs` |
| Deny 规则先绑定 | Deny 规则在所有模式下都生效，并且先于 allow 规则或模式默认值求值；未被匹配的输入绝不会获得超过其模式允许的权限。 | `crates/capabilities/sandbox/src/lib.rs` |
| 审批超时 → 拒绝 | 挂起中的审批等待在到期或 waiter 被丢弃时解析为 `Deny`；循环永不无限挂起，迟到的决策被丢弃。 | `crates/operations/bootstrap/src/gate_adapter.rs`、`crates/safety/gate/src/lib.rs` |
| 追加式日志 + 版本化格式 | 回合记录以 JSONL 追加，带 `{"format":1}` 头行；迁移只发生在读取时，加载器从不改写用户文件。 | `crates/state/persistence/src/lib.rs` |
| 警告并继续的装配 | 损坏的插件、技能包、hooks 或 agent 定义文件会带原因警告并被跳过；装配从不在可选面上失败。 | `crates/runtime/plugin/src/lib.rs`、`crates/capabilities/skills/src/plugin.rs`、`crates/capabilities/tools/src/agent_task_tool.rs` |
| 基于 canonicalize 的围栏 | 路径检查对两侧都比较 canonicalize 后的真实路径（按组件级比较，不是字符串前缀），以拒绝逃逸与同缀目录混淆。 | `crates/capabilities/tools/src/path_guard.rs`、`crates/capabilities/memory/src/instructions.rs` |
| 密钥的脱敏 Debug | `Credential` 的手写 `Debug` 输出 `REDACTED` 占位符；转录脱敏按已知值从长到短依次掩蔽。 | `crates/foundation/auth/src/lib.rs`、`crates/safety/secrets/src/lib.rs` |
| 输出中的确定性桩 | 逐出（eviction）桩是 `(tool_use_id, 工具名)` 的纯函数，因此重复执行逐出得到的字节完全一致——构造上的幂等。 | `crates/capabilities/context/src/lib.rs`（`EVICTED_RESULT_MARKER_PREFIX`） |
| 按 canonical path 去重 | 记忆/指令装配按 canonicalize 后的路径去重文件，同一文件绝不会被拼接两次。 | `crates/capabilities/memory/src/instructions.rs` |
| 有界 channel，丢弃而非增长 | Reminder 队列在达到容量上限时丢弃而不是无声增长；通知队列行为相同。 | `crates/capabilities/context/src/lib.rs`、`crates/runtime/child/src/lib.rs` |
| 中毒恢复锁 | 单操作临界区在 panic 后恢复 guard（无需保护半写不变量）；该策略在每个 crate 内集中管理。 | `crates/capabilities/tools/src/lib.rs::lock`、`crates/safety/gate/src/lib.rs` |
| 全量的回退槽位 | 每个声明的工具调用都恰好获得一个结果槽位，末尾带内部错误回退——配对永不破裂，即使未来管线发生变化。 | `crates/runtime/runner/src/lib.rs::execute_calls` |
| 显式的非法配置失败 | 非法的 sandbox 规则在启动时失败（`Sandbox::new` 拒绝它们）；禁止无声降级。 | `crates/capabilities/sandbox/src/lib.rs` |
| 中断保持配对 | 工具执行前的中断会为每个声明的调用合成 interrupted 结果而不是执行任何一个——模型看到的永远是一次调用一个结果。 | `crates/runtime/runner/src/lib.rs::run_turn` 检查点 3 |
| 一次性审批决策 | `decide` 取走槽位；已消费 id 的迟到决策返回 false 并被丢弃，因此过期的 UI 点击绝不可能批准未来的调用。 | `crates/safety/gate/src/lib.rs` |
| 冻结的读取快照 | 会话读取者持有的是永不变化的 `Arc` 快照；所有变更都经由 `push` 流入。 | `crates/state/store/src/lib.rs` |
| 副作用上的幂等键 | `RunContext::idempotency_key(scope)` 为 run 范围的效应划分命名空间，使同一提交的重试在下游去重。 | `crates/runtime/runner/src/lib.rs` |
| 确定性、锁定的 wire 输出 | `Registry::specs()` 按名称排序；wire 事件标签由测试锁定；压缩摘要的节标题逐字固定——输出可复现。 | `crates/capabilities/tools/src/lib.rs`、`crates/foundation/wire/src/lib.rs`、`crates/capabilities/context/src/lib.rs` |
| 提前丢弃空输入 | 空的 steering/注入文本在进入历史之前被丢弃，因为供应商拒绝空的用户消息。 | `crates/runtime/runner/src/lib.rs::steer` / `inject` |
| 损坏日志行计数而非致命 | JSONL 加载器跳过并计数损坏行（`load_reported` 返回计数），严格的调用者可以拒绝部分加载，同时 resume 仍然可用。 | `crates/state/persistence/src/lib.rs` |
| 待处理 reminder 去重 | `ReminderChannel::enqueue` 拒绝与待处理项、或与尾部用户条目中尚未消费的 reminder 完全相同的重复项。 | `crates/capabilities/context/src/lib.rs` |
| 迟注册对共享句柄可见 | 工具注册表是内部可变的，装配之后注册的工具经已共享的 `Arc` 对所有持有者可见，无需重建。 | `crates/capabilities/tools/src/lib.rs` |
| 属性而非名字 | 策略与分发从工具自身读取 `is_read_only` / `is_destructive`；未注册的名字保持串行且视为破坏性，未知工具走谨慎路径。 | `crates/operations/bootstrap/src/policy_adapter.rs`、`tool_adapter.rs` |
| 失败严重度决定恢复方式 | 自动压缩失败降级为警告并继续；阻塞式压缩失败中止回合——触发器决定爆炸半径。 | `crates/runtime/runner/src/lib.rs::run_turn` |
| 标签先验证后使用 | 检查点与快照标签在触碰文件系统前先过验证器，restore 时相对路径拒绝绝对路径与 `..`——针对手改清单的纵深防御。 | `crates/state/checkpoint/src/lib.rs` |
| 录制经验证而非信任 | 回放评测先检查 wire 契约（顺序、配对、一次性结算）再打分，畸形 JSONL 会带行号大声失败。 | `crates/operations/eval/src/replay.rs` |
| 机械修改必须编译 + diff 复查 | 任何脚本化的多文件修改之后，`cargo check` 加一行 `git diff --stat` 行数增量复查是强制的；疑似损坏时停止编辑，先从 `git fsck --unreachable` 恢复。 | `docs/postmortem/2026-09-13-bulk-regex-corruption.md` |

两条元规则决定何时动用它们：

1. **显式失败优于无声降级**（`Sandbox::new` 拒绝坏规则）——但对*可选*面（插件、技能、agent 定义）规则反转：警告并跳过，因为一个坏包不应拖垮整个会话。
2. **确定性优于取巧**——逐出桩、按名排序的 `Registry::specs()`、锁定的 wire 标签都选择可复现的输出，使缓存、快照与测试跨运行保持稳定。
