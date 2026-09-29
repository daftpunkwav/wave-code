# 架构

[English](architecture.md) | 中文

WaveCode 是一个无头优先（headless-first）的 Rust AI 编程 agent。单一 `wavecode` 二进制同时服务单回合执行（`exec`）、交互式 REPL、内嵌 console UI（主屏幕全视口帧，保留原生回滚缓冲）以及旧式会话恢复，它们共享同一个 agent 核心：带工具、技能、记忆与 MCP 的多回合 ReAct 循环。

工作区是位于 `crates/<group>/<crate>` 的扁平 crate DAG。依赖只朝下指，不存在环。本文自底向上描述各分组、维系分层的依赖规则，以及一个回合（turn）的生命周期。

## 依赖规则

1. **任何 crate 都可以依赖 `infrastructure/`。** 这些 crate 是零内部依赖的叶子原语。
2. **`runtime/runner` 只依赖 trait 接缝（trait seams）和数据传输对象**（`state-store`、`wavecode-wire`、`infrastructure-base`）。它不得依赖 tools、sandbox、hooks、memory、skills、MCP 或 transport；具体实现从上层注入。
3. **`operations/bootstrap` 是组合根（composition root）。** 它是唯一允许把具体能力适配到 runner 的 trait 接缝上的 crate。策略放在能力 crate 里；bootstrap 只承载接线与映射。除前端外没有任何 crate 依赖 bootstrap（其测试除外：gateway 的服务器测试以 dev-dependency 的方式使用它的密封装配接缝）。gateway 的 MCP-serve 模块是一个只引用 `wavecode_tools` 注册表类型的服务表皮；executor 的构造仍留在 bootstrap。bootstrap 独占能力消费的已记录例外有：模型工具与其引擎同居——`state-goal`、`state-plan`、`action-jobs`、`action-workflow`、`wavecode-skills` 与 `wavecode-memory` 直接引用 `wavecode_tools`，以便在共享的 `Tool` 抽象上托管各自的工具（bootstrap 仍逐一注册它们）；`wavecode-tools` 消费词汇层的 `action-tasks` 接缝来实现委派工具；`wavecode-tools` 还引用 `wavecode-context` 以使用共享的 `SpillStore`（shell 工具把截断输出落盘到其中，bootstrap 构造并注入唯一的共享实例）。
4. **策略永不匹配工具名。** 工具携带声明式属性（`wavecode_tools::Tool::is_read_only` / `is_destructive` / `kind`，即 `wavecode-protocol` 的 `ToolKind` 分类），策略决策消费这些属性（`operations_bootstrap::policy_adapter`），因此新增一个工具不会让策略层悄悄漂移。
5. **新行为落在扩展点上，而不是改循环。** 修改 `runtime/runner` 必须同步更新本文档。

## 分层图

这张分组图是示意性的，不是严格的 DAG 渲染——依赖规则 3 例外清单里记录的
跨组边与 `cargo metadata` 才是两者不一致时的依据。

```
frontends      wavecode 二进制（exec / repl / resume / console 启动）与
                 console 客户端
                 │
operations     会话 actor（含共享会话契约）、bootstrap 组合根、
               RPC gateway（活跃的 ACP / HTTP+SSE / MCP-serve 表面）、
               eval、observe、simulate
                 │
runtime        RunLoop、子回合、调度器、提示词组装、插件接缝、
               能力清单、身份、技能路由
                 │
action         子任务、工作流引擎、后台作业、浏览器接缝、
               术语检索
                 │
safety         工具策略、审批门、注入防护、审计日志、
               密钥保管
state          会话存储、回合日志、检查点、持久目标、
               计划、产物
                 │
capabilities   tools、skills、memory、MCP 客户端、sandbox 策略、hooks、
               context 管线            （"wavecode-*" 栈）
foundation     wire 类型、配置、多供应商 LLM 客户端、protocol 词汇、
               auth
                 │
infrastructure channels + interrupts + limits、TTL/LRU 缓存、分层配置、
               leases、令牌桶限流、模型路由、JSON schema
```

## Crate 清单

包名有时与目录名不同（`wavecode-*` 能力栈先于分层布局存在）；下表使用包名。

### `infrastructure/` — 叶子原语

| Crate | 职责 |
| --- | --- |
| `infrastructure-base` | OS 运行时原语：channel 容量、协作式 interrupt 句柄、截断预算、共享的日历日期渲染 |
| `infrastructure-ratelimit` | 带显式时钟的令牌桶限流 |

### 词汇与 DTO 层

| Crate | 职责 |
| --- | --- |
| `wavecode-wire` | 前后端之间提交与事件的 wire 类型；面向模型的 system-reminder 标记 |
| `wavecode-protocol` | 共享的前端协议词汇：权限模式、审批种类 |

### `state/` — 持久数据

| Crate | 职责 |
| --- | --- |
| `state-store` | 持久化的会话历史与上下文预算检查 |
| `state-persistence` | 支撑 `resume` 的追加式 JSONL 回合日志 |
| `state-checkpoint` | 带标签的状态快照与回滚 |
| `state-goal` | 带 CAS 版本化的每会话持久目标，以及其上的模型可调用 `goal` 工具 |
| `state-plan` | 经评审的 plan 模式状态机，以及其上的模型可调用 `plan` 工具 |

### `safety/` — 策略与隔离

| Crate | 职责 |
| --- | --- |
| `safety-gate` | 权限模式、策略裁决与审批门 |
| `safety-guardrail` | 启发式提示注入筛查与污点追踪 |
| `safety-audit` | 安全相关决策的追加式审计轨迹 |
| `safety-secrets` | 面向日志与转录脱敏的密钥存储 |

### `runtime/` — 执行核心

| Crate | 职责 |
| --- | --- |
| `runtime-runner` | RunLoop：拥有每回合状态机（sample → decide → execute → recover）、重试预算、幂等键 |
| `runtime-child` | 带深度记账的受追踪后台子任务 |
| `runtime-scheduler` | 优先级队列、延迟任务、cron 匹配、限额 |
| `runtime-prompt` | 从命名内容槽组装系统提示词 |
| `runtime-plugin` | 极简插件系统：服务注入、middleware 钩子、会话内生命周期 |

### `action/` — agent 行动面

| Crate | 职责 |
| --- | --- |
| `action-tasks` | 能力中立接缝背后的子任务生命周期（层级上与 `runtime-child` 同为词汇层的被动机制） |
| `action-workflow` | 基于 `action-tasks` 的校验型 DAG 执行与 Ralph 循环，外加模型可调用的 `workflow_run` / `ralph_run` / `schedule` 工具 |
| `action-jobs` | 带 wait/cancel/notice 语义的后台 shell 作业，以及其上的模型可调用 `job_*` 工具 |
| `action-browser` | 异步 tab 接缝背后的浏览器自动化 |
| `action-retrieval` | 分块文档上的词项重叠检索 |

### `foundation/` + `capabilities/` — 能力栈

旧式命名的 `wavecode-*` crate。bootstrap 是它们的默认消费者；已记录的例外是依赖规则 3 中列出的工具宿主引擎——它们消费 `wavecode-tools`（并经由它消费 `action-tasks` 接缝）来托管各自的模型工具；另有 `wavecode-tools` 自身指向 `wavecode-context` 的共享 spill 存储边。

| Crate | 职责 |
| --- | --- |
| `wavecode-config` | TOML 配置加载（`~/.wavecode/config.toml`）与供应商解析 |
| `wavecode-llm` | 多供应商抽象：Anthropic/OpenAI 适配器、SSE 流式、重试 |
| `wavecode-auth` | 面向模型访问的按供应商凭据存储 |
| `wavecode-tools` | Tool trait、注册表、内建工具（fs、search、shell、todo、子任务委派、ask-user）、路径守卫 |
| `wavecode-skills` | `SKILL.md` 的发现、解析与目录，外加模型可调用的 `skill` 工具 |
| `wavecode-memory` | 指令记忆（`AGENTS.md` + `AGENTS.local.md` 分层，子目录首次触碰时发现）与每回合转录蒸馏，外加模型可调用的 `memory_write` 工具 |
| `wavecode-mcp` | Model Context Protocol 客户端与接口边界：client/server trait、数据类型、服务器配置、`mcp__` 命名约定，以及把外部工具注入注册表的 stdio/streamable-HTTP 客户端桥（字节级组帧在 `transport-mcp`） |
| `wavecode-sandbox` | 工具运行的权限与执行安全层 |
| `wavecode-hooks` | 生命周期钩子（PreToolUse / PostToolUse / UserPromptSubmit / SessionStart / SessionEnd / Stop / PreCompact / PostCompact） |
| `wavecode-context` | 上下文管理管线：压缩、spill、预算阶段 |

### `operations/` — 装配与运维

| Crate | 职责 |
| --- | --- |
| `operations-actor` | 串行会话驱动器：提交路由加回合驱动；同时拥有共享会话契约（装配选项与失败，以及 RPC 服务器消费的 `SessionSurface`） |
| `operations-bootstrap` | 把具体能力适配到 runner trait 的组合根 |
| `operations-gateway` | 活跃的 RPC 表面：ACP（stdio 上的 JSON-RPC）、app server（回环 HTTP 上的 REST + SSE）、MCP serve（stdio 上的工具），各自都是会话契约之上的服务表皮 |
| `operations-eval` | 面向任意回合驱动器的行为基准 |
| `operations-observe` | 把回合 wire 事件折叠为累计的运维指标 |
| `operations-simulate` | 模型计划动作的干跑渲染（plan 预览） |

### `transport/` + `frontends/`

| Crate | 职责 |
| --- | --- |
| `transport-mcp` | 子进程 stdio 管道上的 JSON-RPC 组帧 |
| `harness-cli` | `wavecode` 二进制：exec、REPL、resume；退出码跟随回合结果 |
| `tui-engine` | 内嵌终端渲染引擎：组件、编辑器、markdown（数学、mermaid 图、fenced-block 接缝）、diff 屏幕 |
| `console-ui` | wire + actor 之上的主题化 console 前端（转录、对话框、slash 命令） |

### 接线状态

"已接线"（wired）指可以从 `wavecode` 二进制经普通依赖到达。无论是否接线，`cargo test --workspace` 都会构建并测试每个 crate，因此测试全绿不是接线的证据。截至 2026-09-24，43 个库 crate 中有 7 个**未接线**——它们无法从 `wavecode` 二进制到达。上文描述的是每个 crate *做什么*，而不是产品*提供什么*；本表就是那道更正。在把某个 crate 当作特性引用之前，先查它的依赖方（`cargo tree -q -i <crate>`）；接线它的同一个变更里应把它移出本表。

| 未接线的 crate | 为什么不可达 |
| --- | --- |
| `operations-simulate` | 仅库：计划动作的干跑渲染还不是产品特性 |
| `action-browser`、`action-retrieval` | 浏览器接缝与词项重叠检索没有实现者或消费者；`wavecode-tools` 拥有活跃注册表 |
| `safety-guardrail`、`safety-audit` | 活跃的策略与审批流是 `safety-gate` + `wavecode-sandbox`；这两者与之重叠，采用前需要重画边界，而不是直接拼接 |
| `state-artifact` | 产品的持久数据走 `state-persistence` / `state-store` |
| `wavecode-auth` | 供应商凭据经 `wavecode-config` 的 `env_key` 路径解析 |

### 有意保留、未接线

这些 crate 是面向未来工作的有意播种，不是死重。它们在工作区中编译、测试，并在其特性落地之前不进入发布的二进制：

- `action-browser` — 异步 tab 接缝背后的浏览器自动化，面向需要浏览器的 agent 项目。
- `action-retrieval` — 分块文档上的词项重叠检索，面向需要本地语料搜索的 agent 项目。
- `safety-guardrail` — 提示注入筛查与污点追踪，保留给未来的信任边界重画。
- `safety-audit` — 安全相关决策的追加式审计轨迹，保留给需要它的部署。
- `wavecode-auth` — 按供应商的凭据存储，保留给超出 `env_key` 路径的多供应商场景。
- `operations-simulate` — 模型计划动作的干跑渲染（plan 预览），保留给未来的前端。
- `state-artifact` — 运行产物的版本化注册表，面向需要它的 agent 项目。

没有遗留的重复模型观察项：工作区中每个 crate 要么已接线、要么是已记录的保留种子、要么就是组合根本身。

## 一个回合的生命周期

1. 前端（`harness-cli`、TUI，或经 `wavecode serve` 接入的 HTTP/SSE 客户端）把一次提示作为 wire 提交（submission）发出。
2. `operations-actor` 按会话串行化提交，并驱动 `runtime-runner`。
3. RunLoop 经注入的 `Model` trait（`wavecode-llm` 适配器）采样模型，决定工具调用，经 `Tool` 接缝执行它们，并从失败中恢复——受重试预算和来自 `state-store` 的上下文预算约束。
4. 工具执行经过 `safety-gate` 的审批流；裁决由权限模式与工具的声明式属性（`is_read_only` / `is_destructive` / `kind`）合成，OS 级隔离来自 `wavecode-sandbox`。
5. 每一步都发出 `wavecode-wire` 事件；前端把它们渲染为 stdout JSONL、TUI 行或 gateway 帧。
6. 历史变更追加到 `state-persistence` 的块级 write-ahead 日志（`<id>.history.jsonl`），`resume` 会重放它；完成的回合另外向 `<id>.jsonl` 追加一份文本快照，供选择器使用，也供日志存在之前的旧会话使用。

## 基准

`benchmarks/` 用离线、免密钥的基准测试钉住回合性能；见 [benchmarks/README.md](../benchmarks/README.md)。可执行的基准位于 `crates/runtime/runner/tests/benchmarks.rs`。

## 子系统页面

按子系统划分的文档、cookbook、防御模式与事故复盘与本文档放在一起：

- 子系统：[核心循环](subsystems/core-loop.zh.md) · [工具](subsystems/tools.zh.md) · [上下文工程](subsystems/context-engineering.zh.md) · [安全](subsystems/safety.zh.md) · [扩展性](subsystems/extensibility.zh.md) · [会话与状态](subsystems/sessions-state.zh.md) · [评测](subsystems/evals.zh.md) · [console UI](subsystems/ui.zh.md)
- Cookbook：[添加一个工具](cookbook/adding-a-tool.zh.md) · [添加一个 subagent](cookbook/adding-a-subagent.zh.md) · [录制与回放](cookbook/recording-and-replaying.zh.md)
- [防御模式](defensive-patterns.zh.md)
- 事故复盘：[2026-09-13 批量 regex 损坏](postmortem/2026-09-13-bulk-regex-corruption.md)
