# 子系统：工具

[English](tools.md) | 中文

工具框架位于 `crates/capabilities/tools`（包名 `wavecode-tools`）：`Tool` trait、`Registry`、路径围栏与内建工具集。执行编排（hooks、策略、审批）*不在*这里——由 RunLoop 驱动；见 `docs/subsystems/core-loop.md`（中文版 [core-loop.zh.md](core-loop.zh.md)）。

## `Tool` trait（`crates/capabilities/tools/src/lib.rs`）

```rust
fn name(&self) -> &str;              // 允许运行期名字（MCP 工具），不是 &'static str
fn description(&self) -> &str;       // 供模型消费
fn input_schema(&self) -> Value;     // 采样请求用的 JSON Schema
fn is_read_only(&self) -> bool;      // 只读工具可以并行
fn is_destructive(&self) -> bool;    // 默认 false；破坏性工具要走策略路径
async fn validate(&self, input) -> Result<()>;  // 语义预检，默认直通
async fn execute(&self, input, ctx: &ToolCtx) -> Result<ToolOutput>;
```

业务错误约定是承重的：**业务失败返回 `Ok(ToolOutput { is_error: true, content })` 且绝不 panic**；`Err` 保留给实现故障（io 错误）。`ToolAdapter`（bootstrap）给实现故障加 `tool fault:` 前缀，使转录能把它们与业务失败区分开，并把未知工具名也变成错误结果。`ToolCtx { cwd, deny_env }` 携带工作目录与环境变量剥离清单。

## Registry

`Registry` 是一个 `Mutex<HashMap<String, Arc<dyn Tool>>>`，带 `register(&self)`——内部可变性是刻意的，装配之后注册的工具（skill 工具、MCP 工具、子任务工具）经已共享的 `Arc` 到达 executor、策略与模型适配器，无需重建。辅助方法：`name_subset`（技能 `allowed-tools` fork；未知名字被静默跳过）、`read_only_subset`（explore 子 agent）、`specs()`（按名排序保证输出稳定）。

## 路径围栏（`src/path_guard.rs`）

每个文件系统工具都经 `path_guard::resolve(ctx, path)` 解析用户路径：

- 先做词法规范化（去掉 `.`，弹出 `..`），再在**canonicalize 后**的真实路径两侧做围栏检查（Windows `\\?\` 前缀必须一致）。
- 已存在的路径：解析出的 canonical 路径必须以 canonicalize 后的 `cwd` 开头——这会拒绝符号链接/junction 逃逸。
- 缺失的路径（`write` 的情形）：锚定到最近的现存祖先，canonicalize，重新检查前缀。
- 组件级前缀比较拒绝同缀目录混淆（`../abc` 内部的 `../abd`）。
- 已记录的限制：TOCTOU 窗口（检查与使用之间被调换的符号链接）为 M1 威胁模型所接受；fd 锚定访问是未来工作。

## 执行管线各阶段

对每个声明的调用，runner（`crates/runtime/runner/src/lib.rs::execute_calls`）按顺序施加：

1. **重复 id 准入**——首次出现获胜。
2. **按 run 允许清单**（`RunAllowlist`）——fork 范围的 `allowed-tools`；拒绝是业务错误，先于其他一切。
3. **`PreToolUse` 钩子**——可以阻止；被阻止的调用绝不抵达策略或审批。
4. **策略**——`PolicyAdapter` 从注册表读取 `is_read_only`/`is_destructive`（绝不从名字读取），向 `wavecode-sandbox` 请求裁决（deny 规则 → allow 规则 → 模式默认）。
5. **审批**（`Ask` 时）——挂在门上，超时即拒绝。
6. **执行**——只读、非破坏性的调用并发运行，上限 8 个在飞（`buffer_unordered`）；其余串行并逐项检查中断。
7. **`PostToolUse`**——只观察，从不阻塞。

关于 `validate()` 的说明：trait 把它作为 JSON Schema 之外的执行前语义检查（"第 0 阶段"）携带，但今天没有任何编排调用点调用它——它是留给未来接线的接口。工具不得依赖它保证安全；围栏与策略不依赖它。

## 内建工具

`Registry::builtin()` 注册（名称 — 源文件）：

- `read`、`write`、`edit` — `src/fs/mod.rs`（外加 `fs/read.rs`、`fs/write.rs`、`fs/edit.rs`）；写入是原子的（临时文件+改名），带大小上限与 edit 的精确匹配唯一性检查。`read` 支持经负 `offset` 的尾部读取（从文件末尾倒数，头部标注 `[showing lines X-Y of T]`），未命中路径时按同级文件名给出最接近的建议（有界扫描 + 有界 Levenshtein）。`read` 把每个文件的 (mtime, len) 指纹记入会话共享的 `FileLedger`，`write` / `edit` 在落盘指纹偏离会话最近一次所见时拒绝改写，外部变更由此强制先重读，而不是被悄悄覆盖。
- `grep`、`glob` — `src/search/`；同步遍历包在 `spawn_blocking` 里。
- `shell` — `src/shell_tool.rs`；经 `sanitize_env`（剥离 `deny_env` 名单及 `*_KEY`、`*_PAT`、`AWS_SECRET_ACCESS_KEY` 等敏感形态变量）与 OS sandbox 后端生成子进程。会话装配把 job 服务接为 shell 的 `RunHandoff`，因此超过超时的命令会被**晋升**为后台作业（进程在 `JobService` 下继续运行；turn 继续推进，由 `job_wait`/`job_output` 收集；完成通知只在晋升时才打开）——没有接缝时、或在 `WAVECODE_SANDBOX_OS` 隔离模式下，超时杀死进程并报告杀死前已产生的输出。完成运行的单流在捕获上限处被截断时会把全文落入 context 的 `SpillStore`，给出 `spill` 工具可读回的 `spill://` URI。捕获流先按 UTF-8 解码，逐行回退到 Windows ANSI 代码页（中文主机为 GBK），`cmd` 内建命令的输出不再变成 U+FFFD 乱码；`python`/`node` 走同一个解码器。
- `python`、`node` — `src/script.rs`（非只读）。
- `lsp_symbols`、`lsp_definition`、`lsp_hover`、`lsp_references` — `src/lsp.rs`；导航工具只读。
- `web_fetch` — `src/web_fetch.rs`；`web_search` — `src/web_search.rs`（DuckDuckGo 后端）；两者只读。
- `view`、`present` — `src/fs/image.rs`、`src/fs/present.rs`；`spill` — `src/spill_tool.rs`（读上下文 spill 存储）。

`todowrite`（`src/todo_tool.rs`）经 `with_todo_write` 单独注册，使工具与会话级 `TodoStore` 共享一个 `Arc`。`lsp_diagnostics`（`src/lsp.rs`）不在 `builtin()` 里；会话装配在 `crates/operations/bootstrap/src/session.rs` 中为 LSP 工具注册活的 provider（共享注册表上的迟注册）。`task`、`task_output`、`task_stop`（`src/task_tools.rs`、`src/agent_task_tool.rs`）、`ask_user`（`src/ask_user_tool.rs`）随本 crate 发布；`skill` 工具位于 `crates/capabilities/skills/src/tool.rs`，`memory_write` 位于 `crates/capabilities/memory/src/tool.rs`，`goal` 位于 `crates/state/goal/src/tool.rs`，`plan` 位于 `crates/state/plan/src/tool.rs`，`job_*` 家族位于 `crates/action/jobs/src/tools.rs`，`workflow_run` / `ralph_run` / `schedule` 位于 `crates/action/workflow/src/tools.rs`——会话装配以同样的方式从这些原籍注册它们全部。
