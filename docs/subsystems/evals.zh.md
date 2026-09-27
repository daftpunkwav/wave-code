# 子系统：评测

[English](evals.md) | 中文

质量保障分五层（tier），按成本排序。第 1、2、5 层全部离线且免密钥；第 3、4 层需要真实模型，由人有意运行——直说了，免得有人把 CI 全绿误当成线上验证。

## 第 1 层——单元与集成测试

单元测试与代码同处（`#[cfg(test)] mod tests`）；跨 crate 行为位于 `crates/*/tests/`。规则集在 `docs/development.md`（中文版 [../development.zh.md](../development.zh.md)）：无网络、无模型 key、无 OS 特定要求；脚本化模型与离线夹具是标准做法。脚本化模型惯用法的实际应用：`crates/runtime/runner/tests/benchmarks.rs` 用脚本化的 `ModelGateway`、stub 的策略/钩子/审批和固定的 `RunConfig` 驱动真实的 `RunLoop`——循环逻辑对着确定性模型测试，从不对活体供应商。CI（`.github/workflows/ci.yml`）在 Linux、Windows 与 macOS 上运行 `cargo fmt --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings` 和 `cargo test --workspace --locked`。

## 第 2 层——录制会话打分

录制是 wire `Event` 值的 JSON 数组（`id` 加扁平化的 `EventMsg`，snake_case 的 `type` 标签——标签格式由 `crates/foundation/wire/src/lib.rs` 中的一个测试锁定）。录制来自 `wavecode exec --json` 会话（见 `docs/cookbook/recording-and-replaying.md`，中文版 [../cookbook/recording-and-replaying.zh.md](../cookbook/recording-and-replaying.zh.md)）。

- **录制契约校验**：`crates/operations/eval/src/replay.rs` 在打分之前先按 wire 协议契约校验录制——`validate_contract` 检查每个提交：`TurnStarted` 最先且只触发一次，begin/end 调用配对（未开启的 end 与从未结束的调用是违规，被中断的回合除外），`TurnCompleted` 之后无任何事件；违规累积为人类可读的行而不是在第一条就停下。同一提交 id 下重复的 `AgentMessageComplete` / `AgentMessageDelta` / `TokenCount` 是合法的——运行循环**每个 sample** 完成并结算一次，因此带工具调用的回合会携带多组这类事件。`evaluate_recorded` 随后按录制的助手文本对照 `must_contain` 期望打分（`ReplayReport::pass_rate`），`read_events_jsonl` 每行加载一个 `Event`（畸形行带行号大声失败——录制是测试夹具，不是不受信数据）。同 crate 中的行为评测装置（`EvalCase`，经任意 `TurnDriver` 对最终历史做 `must_contain` / `must_not_contain`）为活的驱动器行为补足这一层。

## 第 3 层——真实 API e2e（手动）

没有自动化的活体 API 测试层。唯一的活体路径是一个带门禁的冒烟测试：

```
LIVE=1 cargo test -p runtime-runner --test benchmarks live_gate_smoke -- --nocapture
```

没有 `LIVE=1` 时它打印 SKIP 并通过；有 `LIVE=1` 但没有供应商 key 时同样跳过。对供应商的真实端到端验证是发布前的手动步骤——这是已知的 v1 缺口，不是覆盖声明。

## 第 4 层——任务级套件（`wavecode eval tasks`）

第 1-2 层评判结构；这一层评判真实会话是否把工作做完了。一个任务是一条提示加一个小型仓库，裁决来自回合之后的 workspace，从不来自模型自己的总结。

```
wavecode eval tasks --permission-mode wave                 # 整个套件
wavecode eval tasks --filter rust --json --out rust.json   # 一个切片，JSON 输出
```

布局：`benchmarks/tasks/<id>/` 下每个任务一个目录，内含
`task.toml`（清单——正是这个文件名标记了一个任务）和
`workspace/`（拷贝进一次性工作根的夹具）。每个任务在该拷贝中生成自己的
`wavecode exec --json` 子进程，因此 agent 在隔离环境中工作，
提交的夹具永不被修改。

评判由 `operations-eval` 的 `task` 模块承担：断言要么是按 argv 运行的
命令（不走 shell，因此一份清单在所有平台通用），要么是文件条件——
`file_equals`、`file_contains`、`file_not_contains`、
`file_exists`、`file_absent`、`file_unchanged`。任务的 agent 步骤干净结束*且*每条断言都成立才算通过；`interrupted`、触顶或非零退出的步骤即使文件看起来正确也算失败——崩溃之后没有信任这些文件的依据。

`crates/frontends/harness/src/task_eval.rs` 中的离线闸口不花一个 token 就让套件保持诚实：

- `committed_tasks_are_well_formed` — 每份清单可加载，id 唯一且与其目录匹配，任何 cargo 夹具自带 `[workspace]` 表（否则外层工作区会吞掉拷贝，构建断言就会说谎）。
- `no_committed_task_is_already_solved` — 原始夹具必须至少让一条断言失败，否则该任务就是白送的分数，什么也测不出来。
- `every_committed_task_has_a_solution_that_passes` — 每个任务有一个已知良好的终态必须满足断言，因此期望字符串里的拼写错误在这里被抓住，而不是留给一次活体运行。

成本与调度：每个任务是一次真实回合，Rust 夹具每次检查要跑一次
`cargo test`。这是 nightly / 发布前的度量，不是每 PR 的闸门。通过率是要
长期跟踪的数字，连同每任务的轮数（`--json` 两者都报）。

## 第 5 层——性能闸口

计时策略位于 `benchmarks/baseline.json`（提交的 median 加 `warn_beyond` / `fail_beyond` 乘数——单一事实来源，由测试读取，更新时从不需要改测试）：

- 等于或低于 `warn_beyond × median` 为 `PASS`；高于它但在 `fail_beyond × median` 内为 `WARN`（测试仍然通过）；只有超过 10× 或任何正确性不匹配才是 `FAIL`。Median 有意定得宽松，让 CI 噪声表现为警告而不是失败。
- 闸口：`crates/runtime/runner/tests/benchmarks.rs` 携带 `continuation_micro_bench`（脚本化模型，经真实 `RunLoop` 跑 8 个工具轮）和 `session_open_bench`（离线装配 + 5 个脚本化回合），都输出 JSON 行。
- `benchmarks/run.rs` 是纯文档化的装置——人类可读的规格，在轮数、界限或夹具形状变化时与可执行测试保持同步。见 `benchmarks/README.md`。

## 运行时度量（各层评判的对象）

以上各层验证结构；它们说不出真实会话是否变好了。运行时主干填补这个缺口：

- `runtime-runner` 给每个 `ToolCallEnd` 打上产生它的分发出口（`ToolOutcome`）与主体时长的标签。
- `operations-observe` 把会话的事件流折叠为 `Metrics`，含按工具的 `ToolStat` 桶和 `cache_read_share`。
- `operations-bootstrap` 的 `metrics_tap` 挂到 actor 客户端上，每个被记录日志的会话在每个完成的回合后向 `~/.wavecode/metrics/turns.jsonl` 追加一条 `TurnSample`（写入失败只是警告，绝不会让它观察的回合失败）。
- `wavecode metrics [--session <id>] [--json]` 把台账聚合成按调用量排名的每模型、每工具表格。`success` 只覆盖已执行的调用，因此一个大部分被拒绝的工具不会被记成坏的。

读侧数字是上下文工程与策略变更的基线：变更前后各跑一次命令，并引用两个数字。

## 何时动哪一层

新的用户可见结构行为（新的 `EventMsg` 变体、变化的 wire 形状）→ 扩展第 2 层契约校验，并在 `docs/cookbook/recording-and-replaying.md` 中注明。bug 修复 → 修复处的第 1 层回归测试。上下文工程、逐出或工具面的变更 → 跑第 4 层任务套件，引用前后的通过率。性能敏感的循环变更 → 检查第 5 层闸口打印 PASS，若 median 确实移动了则有意重新定基。
