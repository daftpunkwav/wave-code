# 子系统：会话与持久状态

[English](sessions-state.md) | 中文

持久状态刻意保持"笨"：追加式字节加版本化格式，结构由调用方拥有。涉及的 crate：`state/persistence`、`state/checkpoint`、`state/plan`、`state/goal`，外加 actor 的持久化辅助（`operations/actor/src/durable.rs`）。

## 回合日志（`crates/state/persistence/src/lib.rs`）

`JsonlJournal` 每回合以单个 JSON 行追加一条 `TurnRecord`（`run_id`、`input`、以 `(from_model, text)` 对表示的 `history`、`outcome`）。格式版本化：

- 新日志以 `{"format":1}` 头行开始（`JOURNAL_FORMAT_VERSION = 1`）。
- 没有头行的文件按 v0（`JOURNAL_FORMAT_V0`）读回；`is_header_value` 区分它们（数值 `format`，无 `run_id`）。
- `migrate_record` 在**读取时**把 v0 值抬升为当前形状——加载器从不改写用户的日志；未知或缺失字段降级为默认值。

加载器对损坏行计数而不是失败：`load_all` / `load_reported`（为严格的调用者返回跳过数）/ `load_all_checked` / `last_n` 供 resume 预览。日志在正常运行中是追加式的；唯一的例外是 `read_repaired`——当它切掉损坏尾部时会持久化这次截断——用户看到的文件可能缩小，但绝不静默（截断点处有文档说明缘由）。

## 会话注册表（`crates/state/persistence/src/sessions.rs`）

新栈的会话注册表给回合日志一个家和一个身份：

- `~/.wavecode/sessions/<session-id>.history.jsonl`：块级 **write-ahead 历史日志**（`crates/state/persistence/src/history.rs`）。每条已提交的会话变更一行 JSON——`{"k":"append","seq":n,"entry":…}` 或 `{"k":"replace","seq":n,"entries":…}`（压缩、回退）——每条都在 run 继续之前同步。记录带编号，因此丢失的写表现为序列跳变，而不是伪装成干净的尾部。`operations-bootstrap` 的 `history_journal` 模块拥有折叠与配对修复；日志本身只存已编码的 JSON，保持字节层无结构。
- `~/.wavecode/sessions/<session-id>.jsonl`：每会话一个 `JsonlJournal`；每个完成的 console 回合追加对话的完整文本快照（`record_turn`），它支撑选择器与**旧式** resume 路径（`load_session_history`）。`/undo` 以同样方式追加被截断的对话（`record_rewind`，outcome `Rewound`），因此回放呈现回退后的对话而不改写历史。
- `~/.wavecode/sessions/index.json`：每会话一条 `SessionMeta`（id、标题、cwd、创建/更新时间戳、回合数）；`list_sessions` 按最新优先读取，`set_title` 改名，`fork_session` 以调用方提供的快照播种新日志。损坏的索引文件降级为空并在下次写入时自愈。
- 会话 id 用与旧式 thread id 相同的白名单校验（`is_valid_session_id`）：来自 CLI 参数或选择器载荷的路径逃逸被拒绝。

消费者：console UI 在 `TurnCompleted` 时记日志，并驱动 `/sessions`（选择器）、`/resume`、`/fork` 与 `/title`；harness 把会话 id 向下传，`bootstrap::session::seed_conversation` 决定恢复的会话由什么构成。**只要历史日志存在，resume 就是块级的**——工具调用、工具结果、thinking 与图像都能在重启后存活，这正是让长会话的上下文可重采样而不是只能重新推导的原因。日志存在之前写入的会话（或无会话 id 运行的）回退到文本快照，且该播种会作为新基线记入日志，因此尾部永远不会被误当成完整历史。

工具的副作用与其被记录的结果之间崩溃，会留下一条没有配对 `tool_result` 的助手 `tool_use`；供应商拒绝这种配对，因此回放会用一条**声明结果未知的 `is_error` 结果**关闭每个未闭合的调用——模型被告知去验证，绝不要假设写入已发生或去重复它。装配把它变成点名受影响调用 id 的启动警告。

工具结果可以携带 `produced_at` 墙钟时间戳（epoch 秒）：循环在结果落地时打戳，因此恢复的或长程的 agent 能从持久历史推断新旧——文本视图把它渲染成紧凑的 `[YYYY-MM-DD HH:MM UTC]` 头（仅 UTC；时区数据库刻意不在范围内）。该字段可选，在打戳之前的日志记录上不存在（原样回放），在上述合成闭合结果上也不存在——它们的真实产生时间随丢失的记录一起消失了。

诚实的状态（更新于 2026-09-18）：该日志现在有了生产消费者（会话注册表）。旧式 `resume` 子命令仍读旧式导入路径——`state_persistence::legacy`（`crates/state/persistence/src/legacy.rs`）按最新优先列出并加载 `~/.wavecode/threads`（`THREADS_DIR`）。

## 检查点、快照与 actor 的持久化接缝

`crates/state/checkpoint/src/lib.rs`：

- `CheckpointStore`：带标签的内存快照；`rollback` 恢复目标并丢弃更新的标签；未知标签显式失败。
- `durable_save` / `durable_load` / `list_resume_labels`：根目录下的文件支撑的带标签状态；标签经校验（`validate_checkpoint_label`）。
- `SnapshotStore`：工作区文件快照，带上限（`SnapshotCaps`）、`create` / `create_with_caps` / `restore` / `list_labels` / `drop_label`，返回的报告其 `summary()` 是用户可见文本。

`crates/operations/actor/src/durable.rs` 定义 actor 使用的接缝：`CheckpointSink`（持久化落点）、`DurabilityConfig`（启用什么；测试用 `disabled()`）、`persist_checkpoint` 与 `persist_then_act`（在副作用之前持久化）、`render_snapshot`（会话 ⇒ 文本）以及 `turn_label`。

## 计划与目标状态

- `crates/state/plan/src/lib.rs`：`PlanState` 是经评审的状态机——`propose` → `approve` → `begin` → `complete` / `abandon`，`feedback` 回到评审；`PlanStatus::is_terminal` 约束转移；计划按会话存于 plans 根下（`plan_path_for_session`、`validate_session_id`）。
- `crates/state/goal/src/lib.rs`：每会话一个持久目标，带 **CAS 版本化**——每次变更递增 `GoalState::version`，goal 工具的 `update` 动作要求期望版本；不匹配时点名两个版本并让模型以 `status` 重新加载。状态：Active / Blocked / Paused / Completed（终态）。可选的 `sub_goals` 列表（文本 + in_progress/achieved）分解目标；新的 `set` 清空它。run 循环读取（从不写入）这个状态，以继续模型在目标仍为 `Active` 时结束的回合——见 `operations-bootstrap/src/goal_adapter.rs` 与 `docs/subsystems/core-loop.md`（中文版 [core-loop.zh.md](core-loop.zh.md)）中的续跑阶梯。

会话 wire 流的录制与事后校验见 `docs/subsystems/evals.md`（中文版 [evals.zh.md](evals.zh.md)，第 2 层）与 `docs/cookbook/recording-and-replaying.md`（中文版 [../cookbook/recording-and-replaying.zh.md](../cookbook/recording-and-replaying.zh.md)）。
