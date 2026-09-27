# 子系统：上下文工程

[English](context-engineering.md) | 中文

上下文管理横跨两个 crate 外加胶水层：持久化的会话与预算分级（`crates/state/store`）、管线各 pass（`crates/capabilities/context`），以及面向 runner 的压缩器适配器（`crates/operations/bootstrap/src/compactor.rs`）。

## 会话与预算分级（`crates/state/store/src/lib.rs`）

`Conversation` 是追加式的，只有一个写入入口（`push`）。读取者拿到冻结的 `Arc` 快照（`snapshot()`），因此采样永远不会与追加竞争；`replace()` 整体替换历史（压缩），调用方经 `settle` 重新建立用量结转。`normalize_history` 合并相邻同角色条目（部分供应商拒绝它们）；`find_pairing_violations` 检测配对破坏。

预算是剩余 token 的三个分级（`check_budget`）：

| 分级 | 阈值（常量） | 循环中的效果 |
| --- | --- | --- |
| `Warn` | `BUDGET_WARN_REMAINING = 20_000` | 每回合一次的警告 |
| `AutoCompact` | `BUDGET_AUTO_COMPACT_REMAINING = 13_000` | 下一次采样前压缩 |
| `Blocking` | `BUDGET_BLOCKING_REMAINING = 3_000` | 压缩或中止回合 |

`CONTEXT_OVERHEAD_TOKENS = 2_000` 覆盖回退估算路径上的系统提示词；`estimate_tokens`（字符数 / 4）明确是非权威的——供应商的用量数据永远优先（`CONTEXT_OVERHEAD_TOKENS` 与 context crate 中的 `SYSTEM_OVERHEAD_TOKENS` 相同）。

## 压缩策略

`crates/capabilities/context/src/lib.rs` 拥有 `Thresholds`（同样的 20k/13k/3k 余量，`check` 从最深优先探测，`validate` 拒绝倒置的余量）和 `CompactionStrategy`。目前唯一发布的策略是 `ModelSummary`：一次模型调用产出五节摘要，节标题逐字固定——`## Goal` / `## Progress` / `## Key decisions` / `## File inventory` / `## Todo`——保留具体文件名、命令与错误。新历史是摘要消息加上最近 `DEFAULT_KEEP_RECENT = 10` 条逐字消息（`compact_history`），并重新规范化以保证配对。

`crates/operations/bootstrap/src/compactor.rs` 把它适配到 runner 的 `Compactor` 接缝（`ContextCompactor`）：过滤空文本、映射角色、把失败上报为 `CompactError::Failed`、为 `CompactCompleted` 估算摘要 token。触发时机（auto / blocking / reactive / manual）留在循环里——一条触发管线，可替换的策略。

## 逐出 pass（`evict_old_tool_results`）

保缓存命中的微压缩，同样在 `crates/capabilities/context/src/lib.rs`：把*旧的*工具结果的载荷替换为单行桩，其余一切逐字保留。

- 永不触碰：开头 `anchored_prefix = 4` 条消息（稳定头部 ⇒ Anthropic 提示缓存前缀延伸到第一处变化）、末尾 `recent_window = 10` 条消息（与完整压缩的逐字尾部对齐），以及所有非工具结果内容。
- 工具结果按结构识别（`ContentBlock::ToolResult`），绝不从文本中解析；桩保留 `tool_use_id` 和 `is_error`，配对检查依然通过。
- 桩是 `(tool_use_id, tool_name)` 的确定性函数，以 `EVICTED_RESULT_MARKER_PREFIX = "[evicted tool result"` 开头——同一 pass 跑两次产出相同字节，这正是它**幂等**的原因。
- **按需驱动，不是全有或全无**：`DEFAULT_EVICTION_SOFT_THRESHOLD_TOKENS = 100_000` 同时充当触发器和回收目标，pass 只按最旧优先回收，直到桩释放出 `total - soft_threshold` 的 token。已经在线以下的历史即使被调用也逐字节原样通过，近期的证据得以留存到真正需要时。
- **批次对齐的前沿**：边界只落在距锚点 `batch_messages`（`DEFAULT_EVICTION_BATCH_MESSAGES = 12`）倍数的位置，因此只追加一条消息的普通回合完全不会改变请求字节。
- 在合成增长上的实测（每回合一个 `grep`、约 1k token 结果，159 次请求转移）：按消息的边界发散 **62** 次，批次化边界 **16** 次。每一次发散都会让它背后的所有内容按全价重新读取，所以这是一笔账单，不是锦上添花。由 `crates/operations/bootstrap/tests/cache_prefix_stability.rs` 钉住，它还断言阈值以下零发散与幂等性。
- 短历史上前缀/窗口重叠会使可逐出区间为空，历史原样通过（饱和算术，绝不 panic）。

## 断点生命周期（适配器侧）

应用层保证请求前缀字节级稳定；Anthropic 适配器（`crates/foundation/llm/src/anthropic.rs`）决定提供方把它记多久。断点（`system` 块 / 最后一个工具 / 最后一条消息）默认是提供方的五分钟条目，每次命中即刷新。在 provider 配置上设置 `prompt_cache_ttl = "1h"` 可切换为一小时条目（`cache_control.ttl: "1h"`；适配器还会附带历史上的 beta 头 `anthropic-beta: extended-cache-ttl-2025-04-11`——现行 API 自 2025 年 8 月起已不再要求，保留是为兼容 beta 时期的 Anthropic 协议网关）：写入按基础输入价的两倍计费（而非 1.25 倍），换来超过五分钟的一段安静——一次长构建、多日运行里的一夜停顿——不再把整个前缀过期成一次全价重读。长时运行会话正是它回本的场景；突发式交互用默认值即可。配置了未知值时告警并回落默认。

## `<system-reminder>` 通道

`wrap_system_reminder` 把文本包进规范的 `<system-reminder>` … `</system-reminder>` 块——压缩通知、plan 提醒与类似元文本的唯一注入格式。`ReminderChannel` 是有界 FIFO（`DEFAULT_MAX_PENDING_REMINDERS = 8`；达到上限时新 reminder 被丢弃，绝不排入无界增长）。`enqueue` 对待处理项、以及仍停留在尾部用户条目中的 reminder 去重；`flush` 在下一条 user 角色条目落位之前，把一切合并进尾部用户条目（或推入新的一条）。

## Spill 存储（`crates/capabilities/context/src/spill.rs`）

超大工具输出离开历史而不是被丢弃：`prune_tool_output` 把超过 `DEFAULT_PRUNE_THRESHOLD_CHARS = 8192` 的内容替换为 1024 字符的头部加 `PRUNE_MARKER`（`"[pruned: output spilled to side-store]"`），并把全文写入 `SpillStore`（`spill://` URI，`SPILL_TOTAL_CAP_BYTES = 32 MiB` 总上限，id 读取时校验）。`spill` 工具（`crates/capabilities/tools/src/spill_tool.rs`）按需读回 URI；`default_spill_store_root()` 固定磁盘位置。token 计数永远看不到被 spill 的尾部——历史里剩下的只有标记文本。
