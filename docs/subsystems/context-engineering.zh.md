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

`crates/operations/bootstrap/src/compactor.rs` 把它适配到 runner 的 `Compactor` 接缝（`ContextCompactor`）：过滤空文本、映射角色、把失败上报为 `CompactError::Failed`、为 `CompactCompleted` 估算摘要 token。触发时机（auto / blocking / reactive / manual / model）留在循环里——一条触发管线，可替换的策略。

## 模型请求的压缩（`compact_context`）

模型可以主动申请压缩，而不必等待阈值：`compact_context` 工具（`crates/operations/bootstrap/src/compaction_tool.rs`）把理由排入共享的 `CompactionRequests` 槽（`crates/runtime/runner`），循环在下一个 loop head 评审每一条申请——绝不在工具执行内部评审，因为在那里批准会重写工具结果尚未落位的历史。

评审门（`review_model_compact`）按递进顺序拒绝，每条拒绝以瞬态 `<system-reminder>` note 加一条 `Warning` 事件送达下一个样本：会话批准上限（`MAX_MODEL_COMPACTS = 4`）、与阈值路径共享的每回合一次旗标（`compact_context` 的批准与预算压缩取自同一个"每回合一次"的预算，掐断"填满再压"的循环）、以及占用下限（`MODEL_COMPACT_MIN_USED_PCT = 50`——近乎空窗的申请会被带着实测比例拒绝）。批准则走标准 `do_compact`，触发器为 `CompactTrigger::Model`（上报为 `compact_started { trigger: "model" }`），并重启 loop head——重写使本次迭代的用量数字失效。压缩失败降级为一条拒绝 note；回合继续。未接线时（未调用 `RunLoop::with_compaction_requests`）模型完全没有压缩通道。

## 瞬态样本附注（`SampleRequest.notes`）

每个采样请求携带 `notes`：由 harness 每次迭代从活状态重建、投影为末尾一条 user 消息、永不落库的文本——投影就是 note 的全部生命周期。成员按序为：服务中的模型及其实时窗口加今天的日期（一行；`/model` 切换或跨午夜即时生效，无需触碰已冻结的环境段——该段现在只含会话内稳定事实：OS、shell、工作目录）、`Context usage: {pct}% ({used}/{window} tokens)` 行（与预算门同一个 `used` 数字）、重复 streak 运行期间的逐级熔断提醒、以及各条压缩评审拒绝。只需要被读一次的反馈属于这里，而不是历史。提供方提示缓存不受影响：尾部断点本来就落在最新的消息上，无论有没有 notes，它每回合都会变。

## 嵌套指令发现（`AGENTS.md`）

会话装配加载全局（`~/.wavecode/AGENTS.md`）、项目根与启动目录三层，每层带各自的 `AGENTS.local.md` 补充与 `.wavecode/rules/*.md`（`wavecode_memory::collect`）；不再读取 `WAVECODE.md`/`CLAUDE.md`。更深的目录按需加载：`AgentsInstructionsExecutor` 装饰器（`crates/operations/bootstrap/src/agents_instructions.rs`）读取每个文件工具调用的 `path` 输入，从其目录向上走到项目根，把每层尚未加载的最近 `AGENTS.md` 提供给循环的 `DirectoryInstructions` 槽；下一个循环头将其落为持久用户条目，包在 `<system-reminder>` 里。持久而非瞬态：一个目录的约束对之后在该目录的每次操作都持续相关。项目根层永不重复提供（装配已有它），且每个目录每会话至多落一次。

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
