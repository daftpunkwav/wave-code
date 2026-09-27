# 子系统：核心循环

[English](core-loop.md) | 中文

RunLoop 位于 `crates/runtime/runner/src/lib.rs`，端到端地拥有一个回合：sample → decide → dispatch → recover。它只依赖 trait 接缝与 DTO；具体能力由组合根（`crates/operations/bootstrap`）注入。修改这个文件必须同步更新 `docs/architecture.md`（中文版 [../architecture.zh.md](../architecture.zh.md)）。

## 接缝

`RunLoop<E, P, H, M, A, T, C>` 对七个 trait 泛型，全部在同一个文件里（goal 接缝骑在默认的 `dyn GoalTracker` 字段上，而不是类型参数）：

| 接缝 | 角色 | 生产适配器 |
| --- | --- | --- |
| `ModelGateway` | 一次采样；`sample_streaming` 输送 delta；`PromptTooLong` 触发压缩 | `wavecode-llm` 适配器 |
| `ToolExecutor` | 执行一次调用；暴露 `is_read_only` / `is_destructive` / `available_tools` | `ToolAdapter`（`crates/operations/bootstrap/src/tool_adapter.rs`） |
| `PolicyDecider` | 执行前对每次调用给出 Allow / Ask / Deny | `PolicyAdapter`，基于 `wavecode-sandbox`（`crates/operations/bootstrap/src/policy_adapter.rs`） |
| `HookGateway` | 运行生命周期钩子；只有阻塞点可以否决 | `hook_adapter.rs` |
| `ApprovalSource` | 挂起等待用户决策或超时 | `gate_adapter.rs`，基于 `safety-gate` |
| `PlanTracker` | 未完成的计划项 + 提醒文本（plan 提示） | plan 工具 |
| `GoalTracker` | 持久目标是否仍然开放 + 提醒文本（goal 续跑） | `goal_adapter.rs`，基于 goal store（默认 `NoGoal`） |
| `Compactor` | 触发时摘要历史 | `ContextCompactor`（`crates/operations/bootstrap/src/compactor.rs`） |

Actor 从不引用这八个类型；它们驱动 `TurnDriver`（对每个 `RunLoop` 与 `Arc<T>` 有 blanket impl），后者还暴露 `set_permission_mode`、`set_model`、`end_session` 与共享的 `inbox_handle`。按 run 的中断覆盖放在循环的 `RunInterrupts` 注册表上（`RunLoop::run_interrupts()`），而不是 driver trait 上：子任务服务为每个子 agent 注册一个句柄，因此 `task_stop` 恰好桥接到那个回合。

## 回合生命周期（`run_turn`）

1. 会话自有的回合（在该回合 run id 下没有 run 范围的中断注册）执行 `approvals.clear_stale()` + `interrupt.reset()`；子回合跳过两者，因此子 agent 的启动永远不会抹掉父会话正挂起等待的审批，也不会吞掉与它启动竞争的用户中断。
2. **准入**：`PromptSubmit` 钩子运行在 `TurnStarted` *之前*。被阻止的输入不进历史、不被采样；回合以 `Error` + `TurnCompleted` 与 `StopReason::Completed` 结束。
3. 推入用户条目，发出 `TurnStarted`，进入轮循环。
4. **检查点 1——循环头**：被触发的中断结算用量，发出 `TurnCompleted { interrupted: true }`，并在采样前返回 `StopReason::Interrupted`。
5. 从 inbox 排出 `NextTurn` steering，作为用户历史；跨越午夜的会话注入一次日历滚动通知（环境段在装配时渲染，日期必须重新宣告）。
6. **轮上限**：`state.rounds_exhausted(max_tool_rounds × (round_rearms + 1))` 发出 `Warning`，结算回合，返回 `StopReason::MaxToolRounds`——硬停止，绝不是错误，但可以与自然完成区分。默认 `DEFAULT_MAX_TOOL_ROUNDS = 256`——wavecode 面向超长程工作；`max_tool_rounds` 配置键按会话覆盖它，警告会点名该键。**开放的会话目标会重新武装上限**而不是停下（`MAX_GOAL_REARMS = 7`，即一个回合跨 8 个上限 = 有效上限 2048 轮）；每次 re-arm 注入带预算行的 goal 提醒并计入上限，被阻塞 / 暂停 / 完成或为空的目标不 re-arm 任何东西。
7. **预算行**（`state-store` 分级）：`Warn` 发出每回合一次的警告；`AutoCompact`/`Blocking` 每回合运行一次 `do_compact`（blocking 失败中止回合；auto 失败降级为警告）。所有闸口都以**有效窗口**推理，在每个循环头从 `ModelGateway::context_window()` 重新读取（`None` 回退到装配时冻结的窗口），因此 `/model` 切换到更小窗口的模型时会在下一次采样*之前*经这条路径压缩，而不是以硬性供应商溢出浮出；更大的窗口不改变任何行为。
8. 在预算行*之后*排出 `NextStep` steering + 直接注入，然后采样。
9. `PromptTooLong` → 反应式压缩（`CompactTrigger::Reactive`）。一次成功采样会重置计数器；`MAX_REACTIVE_COMPACTS = 3` 次连续失败会以错误终结回合。传输/超时错误在结算后使回合失败。
10. 发出 `AgentMessageComplete`，然后：
    - 无调用 + `truncated` → 推入 `CONTINUATION_PROMPT` 并继续（`MAX_CONTINUATIONS = 2`）；
    - 有调用 + `truncated` → 让整批调用以说明性结果失败并在同一续跑预算内继续：流式参数 JSON 由尽力而为的解析收尾，在输出上限处被截断的调用可能携带不完整参数，而半成品的命令绝不能执行；
    - 有未完成的计划项 → 提醒（`MAX_PLAN_NUDGES = 3`）；
    - 有开放的会话目标（`Active` 且目标非空）→ 以 goal 自身的状态渲染加循环的实时**预算行**（上下文用量 / 上限内的工具轮次 / 续跑计数）继续，每回合 `MAX_GOAL_CONTINUATIONS = 8` 次后照停。这正是把人的"继续"从长任务中移除的机制；`Blocked` / `Paused` / `Completed` 的 goal 永不推进，因为那些是"工作应当停止"的声明。子 agent 运行与没有 goal store 的会话用 `NoGoal`（不续跑，和以前一样）；
    - `Stop` 钩子阻止 → 把原因喂回并继续（`MAX_STOP_BLOCKS = 3`，之后照常进行）；
    - 否则以 `Completed` 跳出。
11. **检查点 3——工具执行前**：此处中断会为每个声明的调用合成 `interrupted` 结果，在不执行任何调用的情况下保持调用/结果配对。
12. **重复熔断**（`max_repeat_streak > 0`，默认 `MAX_REPEAT_STREAK`）：同一签名的工具调用一轮接一轮重复是卡死循环的标志失败模式。到达每个 `REPEAT_REMINDER_AT` 阈值时注入逐级升级的提醒（每阈值一次）；到达上限时拒绝执行——每个调用都得到携带文本交接提示的错误结果——回合结算并以 `StopReason::RepeatBreaker` 结束，而不是烧轮次直到上限熔化。
13. 分发（`execute_calls`，见下），把结果作为用户条目追加，`bump_tool_round`，继续循环。

`settle` 在每个采样出口运行（每个 sample 一次，因此多 sample 回合会发出多个 `TokenCount`）；未上报 input 计费的采样跳过结算并发出警告，使用量 carry 永不被零覆盖。`TokenCount` 携带有效上下文窗口与驻留上下文估算 `context_used`（最后一次 prompt 加该次 sample 自身的输出——更早 sample 的输出已在被计费的 prompt 内），供前端上下文表使用；`TurnCompleted` 恰好发出一次且在最后。

## 分发管线（`execute_calls`）

固定顺序：**所有 `ToolCallBegin` 事件按声明顺序先发；所有 `ToolCallEnd` 事件在其后按声明顺序发。** 每个声明的调用恰好获得一个结果槽位，配对不可能破裂（末尾回退槽用内部错误填补不可能的空缺）。每个 `ToolCallEnd` 携带有界的输出预览（`ToolCallPreview`，上限 4 KiB，字符边界安全），前端无需完整转录正文即可渲染结果头部。

`TurnStarted` 写明本回合采样的模型（`model: String`，driver 没有模型时为空），因为 `/model` 可以在会话中途切换模型，而按回合归因正是让指标表能回答"是这个工具弱，还是这个模型不擅长它"的关键。

每个 `ToolCallEnd` 还携带产生它的管线出口（`outcome: ToolOutcome`）与主体的墙钟成本（`duration_ms`）。`is_error` 表示一次调用没有成功；`outcome` 表示是谁停下了它——工具自身、策略、钩子、run 的工具面、审批提示处的用户，或一次中断。非 `Executed` 的出口一律上报 `duration_ms: 0`，因此拒绝永远不会被读成慢工具。`operations-observe` 把这些折叠为按工具的计数器（`Metrics::tools`）；出口分类是封闭的（`ToolOutcome` 覆盖 `execute_calls` 的每个分支），所以新增一个出口意味着新增一个变体，指标拆分无需进一步接线即随之生效。

每次调用：重复调用 id 准入（首次出现执行；重复者获得错误槽位而不执行——否则配对查找会把一个槽位消费两次）→ 按 run 允许清单（`RunAllowlist`，fork 范围的 `allowed-tools`；拒绝是业务错误，受限的 run 在 `available_tools` 里根本看不到被拒工具）→ `PreToolUse` 钩子（阻止 ⇒ 错误槽位，绝不抵达策略）→ `PolicyDecider`：

- `Allow` → 只读（`is_read_only && !is_destructive`）调用加入并发批次，上限 8 个在飞（`buffer_unordered`）；其余串行执行。
- `Deny` → 带原因的错误结果。
- `Ask` → 发出 `ApprovalRequested` 事件，然后在 `ApprovalSource` 上串行挂起。`AllowOnce`/`AllowAlways` 执行；`Deny` 返回原因；`Interrupted` 填充中断槽位。串行循环对每一项重新检查中断；剩余项填充中断槽位而不是中止整批。

`PostToolUse` 只对已执行的调用触发且从不阻塞。一个与旧快速路径的刻意分歧：策略看到*每一个*调用，包括只读的——旧的 deny 直通已移除，只保留了并行性。

## Steering inbox

`InboxHandle` 是共享、可克隆的句柄，带三条队列：`NextTurn`（在循环头应用）、`NextStep`（在下一次采样前应用）、`inject`（与 `NextStep` 同点）。被 steering 的消息作为用户条目走普通历史，因此预算、压缩与配对把它们当普通输入对待；空文本被丢弃（供应商拒绝空的用户消息）。`cancel(keep_next_turn)` 在中断时丢弃待处理项。前端经 actor 的客户端推进 steering，客户端持有循环的 `inbox_handle`。

## 事件顺序契约

Delta（`AgentMessageDelta` / `AgentThinkingDelta`）严格先于其采样的 `AgentMessageComplete`；一个批次内所有 `ToolCallBegin` 先于所有 `ToolCallEnd`；`CompactStarted`/`CompactCompleted` 括起压缩（四周有 `PreCompact`/`PostCompact` 钩子）；`TurnCompleted` 恰好一次且在最后。回放与前端依赖这一点——见 `docs/subsystems/sessions-state.md`（中文版 [sessions-state.zh.md](sessions-state.zh.md)）。
