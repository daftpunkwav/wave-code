# 基准测试与轮次性能质量机制

[English](README.md) | 中文

这个目录为何存在：单元测试可以保持全绿，而产品却明显退化（轮次变慢、
报告变化）。本目录用离线、无密钥的基准钉住轮次性能——只有一个刻意的
例外：`tasks/` 套件驱动真实的模型轮次，因此它消耗配额、按计划运行，
而不是每次提交都跑。

## 布局

- `README.md` — 本文件。
- `run.rs` — 纯文档化的 harness（人类可读的规格说明，不是 cargo
  test 目标）。可执行形式在集成测试里（见下）；修改轮数或界限时
  请保持两者同步。
- `baseline.json` — 已提交的中位数外加 `warn_beyond` / `fail_beyond`
  乘数。计时策略的唯一事实来源；测试用 `include_str!` 读取它，因此
  更新它不需要改测试。
- `tasks/<id>/` — 任务级基准套件：`task.toml`（清单）加
  `workspace/`（agent 要编辑的 fixture）。Live 层；见下文。

## 关注点

(a) Continuation 微基准 — 脚本化模型，经真实 `RunLoop` 走 8 个工具
轮，断言墙钟时间在宽松界限内，并以 JSON 报告 tokens/rounds。可执行
形式：
`crates/runtime/runner/tests/benchmarks.rs::continuation_micro_bench`。

(b) Session-open 基准 — 离线组装（loop seams + conversation +
system prompt）加 5 个脚本化轮次，同样的墙钟时间/报告纪律。
无 provider、无配置文件、无凭据。可执行形式：
`crates/runtime/runner/tests/benchmarks.rs::session_open_bench`。

## 计时策略（绝不在 CI 上因噪声而抖动失败）

- `PASS`：elapsed <= `warn_beyond` x 中位数（打印 PASS + JSON）。
- `WARN`：elapsed 超过前者但在 `fail_beyond` x 中位数之内（打印
  WARN，测试仍通过）。
- `FAIL`：仅当超过 `fail_beyond` x 中位数（10x），或任何正确性
  不匹配（步骤错误、观测缺失、停止原因错误）。
- 中位数刻意宽松（耗时几十毫秒的工作给几百毫秒），因此只有真正的
  退化才会触发 FAIL。

## 无密钥 / live 划分

默认一切离线运行。一个带门控的冒烟测试记录 live 路径：

```
LIVE=1 cargo.exe test -p runtime-runner --test benchmarks live_gate_smoke -- --nocapture
```

- 无 `LIVE=1`：打印 SKIP，通过。
- 有 `LIVE=1` 但无 provider 密钥（`ANTHROPIC_API_KEY`）：打印 SKIP
  及原因，通过。
- 有 `LIVE=1` 且有密钥：也绝不从这里失败；它运行一个脚本化冒烟轮，
  并打印把真实 provider adapter 指向它的说明。真实的单轮 live 检查
  仍是手动步骤，直到出现一个封闭的 provider fake。

## 运行

```
cargo.exe test -p runtime-runner --test benchmarks
```

元测试 `baseline_meta_test`（在 runner 的 benchmarks 文件里）断言
`baseline.json` 可解析且容差合理
（`fail_beyond > warn_beyond >= 1`，每个中位数 > 0）。

## 任务级套件（`tasks/`）

以上判断的是结构。这一层判断真实的会话是否把工作做完：一个任务是
一条 prompt 加一个小仓库，裁决来自轮次结束后的工作区，绝不来自模型
对它的总结。

```
cargo.exe build --bin wavecode
wavecode.exe eval tasks --permission-mode wave
wavecode.exe eval tasks --filter rust --json --out rust.json
```

请从仓库根运行——默认 `--dir` 是 `benchmarks/tasks`。每个任务把它的
`workspace/` 复制到 OS 临时目录下的一次性工作根（最后打印出来并被
保留，失败的任务可以按原样检查），然后在其中派生它自己的
`wavecode exec --json` 子进程。`--agent-bin <path>` 让套件指向另一个
构建，两个构建就是这样在同一批任务上被对比的。

清单（`tasks/<id>/task.toml`）；相对路径按清单自身所在目录解析，且
只有 `task.toml` 这个文件名标记一个任务，因此 fixture 可以自带
`Cargo.toml`：

```toml
id = "fix-config-timeout"
prompt = """README.md states the requirement the client config has to meet."""
fixture = "workspace"
tags = ["basic", "edit"]
agent_timeout = 900            # optional wall cap for the turn

[[assertion]]
kind = "file_contains"         # also: file_equals, file_not_contains,
path = "app.toml"              #       file_exists, file_absent, file_unchanged
text = "timeout = 30"

[[assertion]]
kind = "command"               # argv, no shell: same meaning on every platform
program = "cargo"
args = ["test", "--offline"]
timeout_secs = 300
```

轮次干净结束**且**每条断言都成立时任务通过。崩溃、被中断或被截断的
轮次一律失败，即使文件看起来是对的。`file_unchanged` 与轮次前的
副本逐字节比较——正是它让"修好了 bug 却踩坏了邻居"成为失败，而不是
带星号的成功。

`crates/frontends/harness/src/task_eval.rs` 里的三个离线门禁不花一个
token 就能保持套件诚实，且在普通 `cargo test` 中运行：

- `committed_tasks_are_well_formed` — 每个清单可加载、id 唯一且与
  目录匹配，cargo fixture 自带 `[workspace]` 表（没有它，外层
  workspace 会吞掉副本，构建断言会悄悄测错 crate）。
- `no_committed_task_is_already_solved` — 原始 fixture 必须至少挂在
  一条断言上，否则这个任务就是白送的分，什么也没测到。
- `every_committed_task_has_a_solution_that_passes` — 一个已知良好的
  终态必须满足这些断言，期望字符串里的笔误在这里被抓住，而不是等
  live 运行。

本套件是 nightly / 发布前的度量：每个任务一轮真实运行，外加每个
Rust 检查一次 `cargo test`。持续跟踪通过率与每任务轮数列；两者都在
`--json` 里。

## 添加任务

1. 创建 `tasks/<id>/task.toml` 加 `tasks/<id>/workspace/`，内含让请求
   成立的最小仓库形态（一个待修的 bug、一份陈述需求的 README、一个
   必须幸存的邻居文件）。
2. 保持 prompt 自包含：agent 只看到工作区，看不到别的。若某个文件
   不得触碰，请显式写明边界。
3. 写只有预期终态才能满足的断言。cargo fixture 需要自己的
   `[workspace]` 表。
4. `cargo.exe test -p harness-cli task_eval` — 三个门禁必须保持全绿，
   证明该任务既不白送也不无解。
5. 依赖它的数字之前，先 live 跑一次（`--filter <id>`）。
