# Cookbook：录制并校验一个会话

[English](recording-and-replaying.md) | 中文

"录制"（recording）是会话的 wire 事件流序列化成的 JSONL。每个 `Event`（`crates/foundation/wire/src/lib.rs`）都派生 `Serialize`/`Deserialize`，带一个扁平化的 `EventMsg`（`#[serde(tag = "type", rename_all = "snake_case")]`），因此一个事件恰好是一个 JSON 对象，含一个 `id` 字段和一个 `type` 判别符。

一个例外：活的 `wavecode exec --json` 录制以单独一行控制行开头，`{"meta":"session",…}`，携带会话 id 与它的恢复命令（无 `id`/`type`）。它是会话簿记，不是事件；`read_events_jsonl` 会跳过它，手写的加载器也应如此。

## 1. 录制

无头 exec 路径已经输出这种格式：

```sh
wavecode exec --json "inspect the build failure" > session.jsonl
```

`run_exec`（`crates/frontends/harness/src/main.rs`）用 `serde_json::to_string(&event)` 序列化每个事件并逐行写到 stdout，人类可读渲染走 stderr。任何驱动器也都可以：给 `TurnDriver::drive_turn` / `RunLoop::run_turn` 传一个 `on_event` 回调，每行追加 `serde_json::to_vec(&event)`——`--json` 做的就这么多。

## 2. 加载并校验

把文件读回为 wire 事件：

```rust
let events: Vec<Event> = serde_json::from_str(&text)?;   // 一个 JSON 数组
// 或按行读 JSONL：serde_json::from_str(&line)?
```

校验位于 `operations_eval::replay`（`crates/operations/eval/src/replay.rs`）：`validate_contract(&events)` 检查每个提交的顺序、配对与一次性结算——delta 先于其 `AgentMessageComplete`、begin/end 配对、一个 `TokenCount`、`TurnCompleted` 之后无任何事件——返回人类可读的违规行而不是在第一条就停下。对 JSONL 录制，`read_events_jsonl` 每行加载一个 `Event`，对畸形输入带行号大声失败。

## 3. 给录制打分

`evaluate_recorded` 按录制的助手文本对照 `must_contain` 期望打分（`ReplayReport::pass_rate`），同 crate 中的行为评测装置（`EvalCase`，经任意 `TurnDriver` 对最终历史做 `must_contain` / `must_not_contain`）评判活的驱动器行为。这就是 `docs/subsystems/evals.md`（中文版 [../subsystems/evals.zh.md](../subsystems/evals.zh.md)）中的第二层机制。

## 4. 局限

- 录制携带的是协议事件而非转录：回答措辞只经 `AgentMessageDelta` / `AgentMessageComplete` 事件到达；如果需要完整的对话视图，把录制与会话日志配对使用。
- 真实 API 的会话可以同样录制，但第三层 e2e 仍是手动的——见 `docs/subsystems/evals.md`。
