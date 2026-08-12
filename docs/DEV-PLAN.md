# WaveCode 开发阶段文档（DeepAgents 能力建设）

| 项目 | 内容 |
|---|---|
| 版本 | v0.1 |
| 状态 | 进行中（当前阶段：P10） |
| 上位文档 | [PRD.md](PRD.md) / [SPEC.md](SPEC.md)（架构决策以 SPEC 为准，本文档只排期与验收） |
| 能力参考 | [deepagents](https://github.com/langchain-ai/deepagents)（planning / subagents / filesystem / context quarantine / summarization / HITL / skills） |

---

## 0. 总目标与验收标准

**总目标**：以 Rust 实现 deepagents 全部核心能力（不使用 TS/Python 版本），完成 TUI 核心功能，MCP 预留接口。最终形态 = 除外部接入（MCP 真实 transport）外功能完整的 Claude Code 级 agent，且具备**优秀的长程任务能力**。

**总验收标准**（全部满足才算完成）：

1. 给 WaveCode 一个"搭建电商平台"级任务，agent 能自主完成：需求分析 → 任务分解（todo）→ 子代理分工 → 逐步实现 → 自测 → 验收汇报，全程无需人工纠偏。
2. 长程任务（目标 10 小时级）中上下文管理不丢失关键细节：目标、关键决策、文件清单、待办状态在任意多轮压缩后完整保留（压缩保留率有自动化测试锁定）。
3. 会话可中断、可恢复（rollout 持久化 + resume），崩溃后从断点继续。
4. TUI 核心功能完整：消息流、输入、状态栏、slash 命令、审批弹窗、中断。
5. MCP 接口边界就绪：配置 schema、工具命名注入点、crate trait 边界可编译、可用 mock server 验证；真实 stdio/http transport 留待后续。
6. 全部阶段 `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` 三门禁绿。

**诚实声明**：真实 10 小时长跑验收依赖真实 API 与真实任务，无法在 CI 内完成。本计划以"可自动化的代理指标"锁定能力（压缩信息保留率、长跑模拟回放、泄漏检查、断点恢复），并提供一个可人工执行的真实长跑验收脚本（P10）。代理指标通过 ≠ 真实 10 小时必然成功，这是已知的验收边界。

## 1. 现状基线（2026-08-11 对账）

已落地（M1 及界面增强 T1–T7）：

- `llm`：ChatModel 抽象、Anthropic 流式 SSE 客户端。
- `tools`：Tool trait（含 `is_destructive()` / `validate()` 默认实现，SPEC §11.1）、Registry、read_file/write_file/edit_file/list_dir/grep/glob/shell（path_guard 约束）；fs 工具 execute 已迁移 `tokio::fs`（grep/glob 的同步遍历内部包 `spawn_blocking`）；P4 新增 `todo_write`（deepagents write_todos 语义：整体重写 session 清单，状态经 `TodoStore` Arc 句柄由 Registry 持有并与 Session 共享，ToolCtx 保持纯数据）。
- `core`：Session turn 循环（流式采样 → 工具编排（只读并行/写串行）→ 结果回灌）、中断安全点、tool_use/tool_result 配对维护；P2 接入 AwaitApproval——非只读/破坏性工具执行前经 sandbox 判定，Ask 发 ApprovalRequested 并 park 等待（审批反向通道复用 interrupt_handle 模式：共享槽 + actor in-turn select! 路由），拒绝/plan 拦截以 is_error 回灌模型；P3 接入上下文管线——PreTurn 三级阈值（警告/自动压缩/阻塞）、reactive compact（prompt_too_long 压缩重试，连续 3 次熔断）、max_output_tokens 续写（≤2 次）、`/compact`（Session::compact）；历史改 `Arc<Vec<Message>>` 快照，每轮请求 O(1) 指针克隆（§17.5 M4 O(n²) 消除）；P4 落地 `prompt` 模块——系统提示词分层组装（静态层 const 字节稳定 + 动态层 cwd/git/平台集中，WAVECODE.md/skills/记忆槽位留占位）、清单非空时以 `<system-reminder>` 注入 system 尾部、stop steering（终态无 tool_use 且清单有未完成项时注入提醒继续 turn，连续 3 次放行）；P5 落地 `subagent` 模块——SubagentManager（派生/跟踪/停止）+ task/task_output/task_stop 工具（core 侧实现经 `Session::with_subagents` 注册，tools 不依赖 core 的矩阵约束下无新边），子代理 = 独立 Session（隔离消息历史，深度上限 1 由构造保证：子代理 registry 不含 task 工具），内置 general-purpose（全工具）/ explore（registry 只读子集）两型，后台终态以 `<task-notification>` 在 turn 循环头注入父会话。
- `sandbox`：PermissionMode 四档（protocol 线型）、allow/deny 规则解析与匹配（deny 优先，命中 allow 免审批）、`decide()` 审批判定；模式经 Arc 句柄可 turn 中切换；P4 新增 session 内状态工具豁免（`todo_write` 各模式免审批，deny 判定仍优先）。
- `context`：token 核算（usage 优先 + 字符估算回退）、三级阈值（margin 参数化，默认 window-20k/13k/3k）、`CompactionStrategy` trait + `ModelSummary`（五要素结构化摘要：目标/进展/关键决策/文件清单/待办）、`compact_history`（摘要 + 最近 N 条原文，默认 10）、`normalize_history`（空消息移除 / 孤儿 tool_use 补 is_error / 孤儿 tool_result 剔除）与 `find_pairing_violations` 断言函数。
- `protocol` / `app-server`：Submission/Event 基础协议与进程内服务；P2 新增 `Op::ExecApproval` / `Op::SetPermissionMode` / `EventMsg::ApprovalRequested` 与 actor 路由；P3 新增 `Op::Compact` / `EventMsg::CompactStarted{trigger}` / `EventMsg::CompactCompleted{summary_tokens}`（含 CompactTrigger 线型）与 actor 路由（turn 中排队、空闲立即执行）；P5 新增 `EventMsg::SubagentStarted/SubagentCompleted`（含 SubagentStatus 线型，wire tag 锁定测试登记），app-server 父会话切换 `Session::with_subagents`。
- `cli`：行式 REPL（波形横幅、markdown 渲染、exec --json）；P2 审批内联问答（y/n 可附拒绝原因），exec 非交互自动拒绝并回灌原因；`config.permission_mode` 接入 SessionConfig.sandbox；P3 `/compact` slash 命令与压缩事件渲染行；P5 task 工具行（子代理类型+描述）与子代理起止事件渲染行；P6 `/memory` 命令（列出持久记忆索引）与 bootstrap 记忆装配（WAVECODE.md 收集 + 索引快照）。
- `memory`：指令记忆（`.git` 向上定位项目根；用户级 → 项目根 → cwd 拼接，全局在前；`@path` 引用递归展开深度上限 5 + 防环；`.wavecode/rules/*.md` 并入）+ 持久记忆（`~/.wavecode/memories/` 四类条目文件 + MEMORY.md 索引，根目录可注入）+ 提取产出解析（`[category]` 线格式 → 条目）；core 侧落地 `memory_write` 工具（非只读经 sandbox 默认策略挂接审批：default Ask / plan Deny）、prompt builder 的 WAVECODE.md/记忆索引槽位注入（动态层之后、清单之前）、会话结束自动提取（SessionEnd 派生后台子代理提炼 `[category]` 条目追加，失败静默 warning；SPEC 24h+5 会话门控整合简化为纯追加式，参数留常量）；core→memory 边启用（SPEC §3 矩阵本已允许）。
- `skills`：SKILL.md 发现（builtin < 用户级 < 项目级，同名覆盖；单点坏文件警告跳过）+ frontmatter 解析（serde_yaml，SPEC §8.1 字段交集，kebab/snake 双拼写兼容）+ 清单注入（name+description+when_to_use 以 system-reminder 进 prompt 分层 builder skills 槽位，1% 窗口预算三级降级截断）；core 侧落地 `skill` 工具与 `/name [args]` slash（Op::SlashCommand 协议变体 + actor 路由）——inline 展开（$ARGUMENTS / ${WAVECODE_SKILL_DIR} 替换）为 ToolResult 回灌，fork 以 skill 正文为 preamble 派生后台子代理；allowed-tools：fork 构造级 registry 按名过滤（name_subset），inline turn 级白名单（Registry 共享句柄 ToolAllowlist，执行管道在 hook/审批前拦截，turn 入口清零）；core→skills 边启用。
- `hooks`：八事件点（PreToolUse/PostToolUse/UserPromptSubmit/SessionStart/SessionEnd/Stop/PreCompact/PostCompact）+ command hook 引擎（平台 shell 执行、stdin JSON 载荷、matcher `|`*多值、once 会话级、超时 kill_on_drop 强制终止）；阻塞语义：退出码 0 放行 / 2 阻塞 stderr 回传（仅可阻塞点，不可阻塞点降级警告）/ 其他非零警告放行；config `[hooks.<EventPoint>]` 原始解析（单表/表数组 untagged）经 core 转换 HookEngine（未知事件点显式报错，cli 降级警告）；挂接——PreToolUse/PostToolUse 进工具执行管道（SPEC §11.1 顺序）、UserPromptSubmit 挂 turn 入口、Stop 挂终态（先 todo steering 后 Stop hook，阻塞 stderr 回灌继续 turn 上限 3）、PreCompact/PostCompact 挂压缩管线、SessionStart/SessionEnd 挂 cli bootstrap/退出；prompt 类型 hook 留占位（SPEC 定为 M4 后）；core→hooks / core→config 边启用。

- `tui`：ratatui TUI（P8）——三段布局（消息流 / 输入框 / 状态栏：模型 / 权限模式 / tokens used/window / cwd）；markdown → ratatui 行渲染（语义对齐 SPEC §15.5：标题亮青加粗 / 行内码黄 / 代码块 `│ ` 边线，表格退化纯文本留待后续）；delta sanitize 入缓冲、complete/中断经 markdown 一次渲染（流式期间尾部纯文本预览）；工具行 ▸/✗、todo_write 清单 ☐▸✓ 与迁移标注、task 子代理行、压缩 ⟳✓、子代理 ⏚✓ 事件行（语义复用 cli render.rs）；slash 补全弹层（内置 /compact /memory /permissions /quit /exit + user-invocable skill，前缀过滤，Tab/Enter 补全、Up/Down 环绕、Esc 关闭）；审批内联弹窗（y 放行 / n 附原因拒绝 / Esc 空原因拒绝）；Esc 中断（Op::Interrupt）、Ctrl-C 退出、PageUp/Down 与滚轮翻页；经 InProcessClient 进程内 transport，Cargo.toml 只依赖 protocol + app-server（dependency_matrix_locked 测试锁定不依赖 core）；TestBackend 缓冲断言锁定启动布局 / 消息流+状态栏 / 审批弹窗 / slash 弹层四帧（选缓冲断言而非 insta，注释说明理由）。`cli`：TTY 默认进 TUI、非 TTY 或 `--repl` 回退行式 REPL；装配知识（模型名 / cwd / 初始权限模式 / 记忆索引路径 / 可直调 skill 名）经 TuiContext 注入；SessionStart/End hook 同 REPL 挂接。

- `mcp`（P9 预留接口）：client/server trait 边界（`McpClient`：`list_tools` / `call_tool` / `list_prompts`，对齐协议 tools/list·tools/call·prompts/list 能力面，Send+Sync 对象安全；`McpServerHandler` 镜像占位，`wavecode mcp serve` 留 P10 后）+ `mcp__{server}__{tool}` 命名约定常量与拼/拆函数 + `McpServerConfig`（stdio: command/args/env；http: url/headers）；core 侧 `mcp` 模块——config `[mcp_servers]` 原始表转换（stdio/http 二选一校验，非法条目警告跳过）、`McpToolBridge`（`Arc<dyn McpClient>` 工具逐个包装为 wavecode `Tool`，命名加 `mcp__` 前缀注册进 Registry，非只读默认经 sandbox 同一审批管道）、`/mcp` 状态行渲染（首版恒"未连接（transport 未实现）"，诚实展示）；config crate 解析 `[mcp_servers.<name>]`；skills 落地 `SkillSource::Mcp` 占位变体（prompt→inline skill 转换留注释占位，随真实 transport 接线）；cli bootstrap 解析+持有（Boot 产物），REPL 与 TUI `/mcp` 命令壳。**真实 stdio/http transport 未实现**（对齐 rmcp 能力面，留后续迭代）；mock client 经桥注册进 Registry 过 turn 循环调用成功、结果回灌、审批管道测试绿。

纯桩（≤7 行）：`auth`。

## 2. 阶段划分

> 纪律：每个阶段完成后——更新本文档状态列 → 三门禁绿 → commit（commit message 标注阶段号）→ 才进入下一阶段。不 push。

### P0 文档与基线 ✅ 本阶段

- 内容：本文档入库；现状对账；确认三门禁绿。
- 验收：`docs/DEV-PLAN.md` 存在；`cargo build --workspace`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` 通过。

### P1 工具集补全与 Tool trait 对齐

- 内容：
  - 新增 `grep` / `glob` 工具（只读，输出截断保护）；`web_search` / `web_fetch` 若引入外部依赖则先评估，允许降级为 P1 外。
  - Tool trait 增加 `is_destructive()` / `validate()` 默认实现（SPEC §11.1），不动既有内置 impl。
  - fs 工具迁移 `tokio::fs`（消除 execute 内阻塞 IO 的过渡态，SPEC §19.3）。
- 验收：新工具单测覆盖（含越界/截断）；mock turn 集成测试使用 grep/glob 完成一次任务；三门禁绿。

### P2 安全模型与审批（sandbox + HITL）

- 内容：
  - `sandbox` crate：PermissionMode 四档（default/plan/acceptEdits/bypassPermissions）、allow/deny 规则解析与匹配（deny 优先）。
  - 协议新增 `Op::ExecApproval` / `EventMsg::ApprovalRequested`（附 call_id/kind/detail）。
  - turn 循环接入 AwaitApproval 状态（park 等待审批 Submission，复用 interrupt_handle 模式）；破坏性工具默认需审批。
- 验收：审批 golden 测试（请求→放行/拒绝→结果回灌，拒绝原因回传模型）；plan 模式下写工具被拦截的测试；三门禁绿。

### P3 上下文管理工程（context crate，核心战役）

- 内容：
  - token 核算（基于 usage 回传 + 估算校准）；三级阈值（警告/自动压缩/阻塞，按窗口比例参数化，SPEC §6）。
  - `CompactionStrategy` trait + `ModelSummary` 实现（结构化摘要：目标/进展/关键决策/文件清单/待办）；摘要 + 最近 N 条原文组成新历史。
  - 历史 normalize：孤儿 tool_use 补全/剔除、空消息清理。
  - reactive compact（`prompt_too_long` 触发压缩重试，3 次熔断）；`max_output_tokens` 续写（≤2 次）；`/compact` 立即压缩（协议 Op::Compact）。
  - 每轮 `messages.clone()` O(n²) 消除（Arc/借用）。
- 验收：
  - **压缩信息保留率测试**：构造含目标/决策/文件清单/待办的长会话，压缩后摘要中五项要素逐项断言存在。
  - 压缩后 tool_use 配对完整性测试；threshold 边界测试；三门禁绿。

### P4 规划系统（deepagents planning）

- 内容：`todo` 工具（write_todos：session 内任务清单状态机 pending/in_progress/done）；系统提示词分层 builder 落地（SPEC §5.4，静态层/动态层分离，prompt cache 字节稳定前缀纪律）；任务清单注入上下文；长程 steering（未完成 todo 在 Stop 前提醒继续）。
- 验收：mock 长任务 golden 测试——模型经 todo 工具维护清单，事件流可观测状态迁移；系统提示词前缀稳定性测试（两次组装前缀字节相等）；三门禁绿。

### P5 子代理（subagents）

- 内容：`task` / `task_output` / `task_stop` 工具；子代理以独立 Session 运行（隔离上下文窗口），完成时以 `<task-notification>` 注入父会话；支持后台并行多个子代理；子代理系统提示词可定制（general-purpose / explore 等内置类型）。
- 验收：集成测试——父会话派生 2 个子代理并行执行，结果正确回注且父上下文不含子代理中间过程（上下文隔离断言）；三门禁绿。

### P6 记忆系统（memory crate）

- 内容：
  - 指令记忆：WAVECODE.md 发现（项目根向上定位、逐级拼接、@引用展开、`.wavecode/rules/`）。
  - 持久记忆：`~/.wavecode/memories/` 四类文件 + MEMORY.md 索引；索引注入上下文；`memory_write` 工具（需审批）；`/memory` 命令。
  - 自动提取：会话结束派生后台子代理提炼候选记忆（门控整合可简化为首版追加式，门控参数留配置）。
- 验收：跨会话记忆召回测试（会话 A 写入 → 会话 B 上下文含索引并可加载条目）；WAVECODE.md 拼接顺序与 @展开深度上限测试；三门禁绿。

### P7 Skills 与 Hooks

- 内容：
  - `skills` crate：SKILL.md 发现与 frontmatter 解析（SPEC §8.1 字段交集）、清单注入（1% 窗口预算）、inline 展开与 fork 执行、`skill` 工具。
  - `hooks` crate：事件点（PreToolUse/PostToolUse/UserPromptSubmit/SessionStart/SessionEnd/Stop/PreCompact/PostCompact）、command 类型 hook、阻塞语义与超时 kill。
- 验收：SPEC §8/§9 逐条场景测试（inline 展开、fork 派生、PreToolUse 阻塞回传 stderr、超时 kill 记 warning）；三门禁绿。

### P8 TUI 核心（tui crate）

- 内容：ratatui TUI——消息流（markdown 渲染复用 cli render 语义）、输入框、状态栏（模型/权限模式/token 用量/cwd）、slash 补全、Esc 中断、审批内联弹窗、工具调用折叠展示。经 app-server 进程内 transport 接入，不 import core。
- 验收：insta snapshot 测试关键帧；手工冒烟清单（启动/对话/工具展示/审批/中断/退出）逐项过；crate 边界检查（tui 不依赖 core）；三门禁绿。

**手工冒烟清单**（真实终端执行 `cargo run -p wavecode-cli` 逐项验证；自动化已覆盖的部分见括号）：

1. 启动：TTY 下 `wavecode` 进入 TUI 三段布局，状态栏显示模型/权限模式/tokens 占位/cwd；`wavecode --repl` 与非 TTY 回退行式 REPL（启动布局有 TestBackend 缓冲断言）。
2. 对话：输入文本 Enter 提交，delta 流式预览，complete 后 markdown 渲染（状态机 + 快照测试已锁定）。
3. 工具展示：工具行 `▸`、失败 `✗`、todo_write 清单符号与迁移标注、task 子代理行（状态机测试已锁定）。
4. 审批：弹窗出现，y 放行 / n 附原因拒绝 / Esc 空原因拒绝（状态机 + 快照测试已锁定）。
5. 中断：turn 中 Esc 发 Interrupt，消息流附（已中断）（状态机测试已锁定）。
6. slash：`/` 弹层前缀过滤、Tab/方向键补全、`/compact`、`/memory`、`/permissions` 四档循环（状态机 + 快照测试已锁定）。
7. 退出：Ctrl-C / `/quit` 退出并恢复终端原状（TerminalGuard Drop 恢复）。

> 冒烟验证备注（2026-08-12）：快照/状态机测试与三门禁全绿；交互项需人工在真实 TTY 过一遍清单（本阶段由自动化断言覆盖状态迁移与关键帧，人工冒烟待执行）。

### P9 MCP 预留接口

- 内容：`mcp` crate 定义 client/server trait 边界与配置 schema（`[mcp_servers]` 解析）；`mcp__{server}__{tool}` 命名约定与 Registry 注入点；`/mcp` 命令壳（展示已配置 server 及状态占位）。**不实现真实 transport**。
- 验收：mock McpClient 实现 trait 并注册工具进 Registry，经 turn 循环调用成功；配置解析测试；三门禁绿。

### P10 长程硬化与总验收

- 内容：
  - 会话持久化：rollout jsonl + SQLite 索引、`wavecode resume`、fork（SPEC §16）。
  - 长跑稳定性：压缩循环 100+ 轮模拟回放测试（录制/回放 mock provider）；内存/句柄泄漏检查；崩溃恢复测试（kill 后 resume 从断点继续）。
  - 端到端验收脚本：`scripts/acceptance/ecommerce.md`——真实 API 执行"搭建电商平台"任务的人工验收清单与代理指标对照表。
- 验收：总目标 1–6 逐条对账；模拟长跑测试绿；真实长跑验收脚本交付（人工执行项显式标注）。

## 3. 状态追踪

| 阶段 | 状态 | 完成日期 | 备注 |
|---|---|---|---|
| P0 | 已完成 | 2026-08-11 | 文档入库，三门禁基线绿 |
| P1 | 已完成 | 2026-08-12 | grep/glob + trait 对齐 + tokio::fs 迁移 |
| P2 | 已完成 | 2026-08-12 | sandbox 四档 + 规则匹配 + 审批管道（HITL）；SPEC §3 矩阵补录 sandbox→protocol / cli→sandbox 边 |
| P3 | 已完成 | 2026-08-12 | context crate（核算/三级阈值/ModelSummary 压缩/normalize）+ core 接入（PreTurn/reactive compact/续写/Arc 快照）+ `/compact`；core→context 边启用（SPEC §3 矩阵本已允许） |
| P4 | 已完成 | 2026-08-12 | todo_write（整体重写 + TodoStore 共享句柄）+ prompt 分层 builder（静态层字节稳定/动态层集中/槽位占位）+ 清单 system 尾部注入 + stop steering（上限 3）；cli 清单迁移渲染 |
| P5 | 已完成 | 2026-08-12 | SubagentManager + task/task_output/task_stop（core 侧工具实现，with_subagents 装配）+ 独立 Session 上下文隔离（深度上限 1 构造保证）+ `<task-notification>` 循环头注入 + SubagentStarted/Completed 事件；并行派生/隔离/stop/深度测试绿 |
| P6 | 已完成 | 2026-08-12 | memory crate（WAVECODE.md 发现/拼接/@展开深 5 防环/rules 并入 + 四类持久记忆 + 索引 + 提取解析）+ core 编排（memory_write 审批挂接 default Ask、prompt 槽位注入、SessionEnd 后台提取——门控整合简化纯追加式）+ cli `/memory` 与 bootstrap 装配；拼接顺序/@上限防环/rules/跨会话召回/审批/提取测试绿 |
| P7 | 已完成 | 2026-08-12 | skills crate（发现/覆盖优先级/frontmatter serde_yaml/清单 1% 预算截断）+ hooks crate（八事件点/command 执行/阻塞语义/超时 kill/once/matcher）+ core 编排（skill 工具 inline 回灌与 fork 派生、/name slash 经 Op::SlashCommand、allowed-tools fork 构造级过滤+inline turn 级白名单、八事件点挂接：管道/turn 入口/终态先 steering 后 Stop/压缩管线）+ cli（[hooks] 装配、SessionStart/End、/name 路由）；SPEC §8/§9 场景测试绿 |
| P8 | 已完成 | 2026-08-12 | ratatui TUI（三段布局 / markdown→ratatui 渲染对齐 §15.5 / slash 补全弹层 / 审批内联弹窗 y-n-Esc / Esc 中断 / 翻页跟随）；tui 仅依赖 protocol+app-server（测试锁定不依赖 core）；cli TTY 默认进 TUI、非 TTY 或 --repl 回退行式 REPL；TestBackend 缓冲断言四帧 + 交互状态机单测绿；交互冒烟清单已列、人工 TTY 冒烟待执行 |
| P9 | 已完成 | 2026-08-12 | mcp crate trait 边界（client/server + 命名约定 + McpServerConfig）+ core 桥（McpToolBridge 命名注入 Registry、非只读默认过 sandbox 审批管道）+ config `[mcp_servers]` 解析与 cli/core 装配（仅解析+持有）+ `/mcp` 命令壳（REPL/TUI，状态恒"未连接"诚实展示）+ `SkillSource::Mcp` 占位；真实 transport 未实现（注释对齐 rmcp 能力面）；mock client turn 循环调用/回灌/审批 + 命名拼拆 + 配置解析测试绿 |
| P10 | 未开始 | — | 总验收 |

## 4. 风险与边界

- **真实 10h 验收不可自动化**：以代理指标 + 人工验收脚本覆盖，见 §0 诚实声明。
- **web_search/web_fetch 依赖评估**：引入 HTTP 抓取需评估依赖与 SSRF 防护工作量，允许顺延，不阻塞主线。
- **模型能力依赖**：长程能力上限受所用模型影响；上下文工程保证"不丢信息"，不保证"模型不犯蠢"——验收场景需用能力足够的模型执行。
- **SPEC 对账**：每阶段完成后同步更新 SPEC/PRD 状态列（SPEC §19.6 要求）。
