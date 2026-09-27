# WaveCode

[English](README.md) | 中文

Rust 编写的 headless-first AI 编码代理：一个 `wavecode` 二进制覆盖单轮
执行、交互式 REPL、内联控制台 UI（主屏上的整视口帧、保留原生
scrollback）以及旧版会话恢复，全部运行在共享的 agent 核心之上（多轮
ReAct 循环、工具、技能、记忆、MCP）。

> 🚧 积极开发中；尚无稳定版本。

## 致谢

WaveCode 的控制台体验源自对多个优秀终端编码代理的研究——尤其是
[Kimi Code CLI](https://github.com/MoonshotAI/kimi-code)
（MIT；其 pi-tui 前端启发了差分渲染的控制台），以及 codex、opencode
和 Claude Code。此处全部 Rust 代码均为全新实现。

## 安装

每个 `v*` release 都附带预编译二进制（经 sha256 校验）：

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/daftpunkwav/wave-code/main/scripts/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/daftpunkwav/wave-code/main/scripts/install.ps1 | iex
```

或从源码构建：

```bash
cargo build --bin wavecode

wavecode exec "fix the failing test"   # 单轮，退出码跟随结果
wavecode exec --json "summarize"       # stdout 上的 JSONL 事件
wavecode repl                          # 交互式多轮会话
wavecode resume                        # 列出历史会话 / 恢复某个会话
wavecode                               # TTY 上是内联控制台，否则 REPL
wavecode --plan                        # 以 plan 模式启动（-y 为 auto 模式）
wavecode --debug                       # debug 级文件日志（~/.wavecode/logs）
wavecode metrics                       # 按模型、按工具的质量报告
wavecode grants list                   # "always allow" 一直在批准什么
wavecode eval tasks                    # 任务级套件：真实轮次、被评判的工作区
wavecode doctor                        # 校验本地环境，不联系任何 provider
wavecode update                        # 检查是否有更新的已发布版本
wavecode update --install              # 下载 + 校验 + 换上更新的 release
```

首次无配置运行时，WaveCode 会打印带配置模板的创建指南。创建
`~/.wavecode/config.toml`：

```toml
model = "your-model"
model_provider = "your-provider"

[model_providers.your-provider]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
env_key = "YOUR_API_KEY_ENV"  # key read from this env var (wins over inline api_key)
# rpm_limit = 30               # optional local throttle (requests/minute)
# fallback_providers = ["backup-provider"]  # ordered failover: each provider
# retries transient errors (429/5xx, honoring Retry-After) before the next
# one takes over; auth and quota errors fail fast
# prompt_cache_ttl = "1h"      # Anthropic 缓存条目存活 1 小时而非 5 分钟：
# 写入按基础输入价 2 倍计费（而非 1.25 倍），但一段安静期（长构建、停顿）
# 不再把整个提示前缀过期成全价重读

# Wire dialect per provider, set with `type`:
#   "anthropic"         -> Anthropic Messages   (POST {base_url}/v1/messages)
#   "openai-compatible" -> OpenAI Chat Completions (POST {base_url}/chat/completions)
#   "openai-responses"  -> OpenAI Responses     (POST {base_url}/responses)
# Chat Completions is what most third-party gateways speak; Responses is the
# endpoint that serves models with no chat route (o1-pro, gpt-5-codex) and the
# recommended one for the newer reasoning families. All three stream, carry
# tools and images, and pair tool calls with their results across rounds.

# Optional: entries for the `/model` picker (alias -> provider + wire model).
[models.fast]
provider = "your-provider"
model = "your-model-fast"
# reasoning_effort = "low"   # OpenAI-compatible providers only

# The same picker also reads `~/.wavecode/models.json`, a standalone model
# catalog holding full provider specs (endpoint, API kind, context/output
# limits, thinking variants, modalities). Edit it in the console with
# `/model list` / `add` / `set` / `remove`, or by hand; its models merge
# into `[models]` at startup, and a config.toml provider with the same id
# wins over a catalog one.

# Optional: cheap model for routine side sessions. `/btw` answers sample
# through this [models] entry instead of the primary model; an explicit
# /model choice in the session still wins, and a dangling alias degrades
# to the primary with a warning (doctor reports it).
# secondary_model = "fast"

# Optional: tool-round ceiling per turn. Unset uses the runner default
# (256 — wavecode targets super-long-horizon work); an open session goal
# re-arms the ceiling up to 7 more times (8 ceilings per turn). `0`
# stops every turn before its first tool round.
# max_tool_rounds = 256

# Optional: permission rules, `Scope(pattern)` syntax. Allow entries skip
# approval; deny entries refuse in every mode (deny always wins). `*` matches
# any run of characters, `?` one. Read from this home file only — never from a
# repo-local config, because the working directory is the agent's to write in.
# [permissions]
# allow = ["Bash(cargo test *)", "File(docs/**)"]
# deny  = ["Bash(curl *)"]
```

## Surfaces（命令界面）

- `exec`：一个 prompt，一轮。答案流式输出到 stdout；工具活动、审批与
  用量走 stderr。`--json` 切换为 stdout 上的 JSONL 事件、stderr 上的
  人类可读渲染——开头的 `{"meta":"session",…}` 行携带会话 id 及其
  `wavecode --session` 恢复命令，完成的轮次会记入 journal，因此会话
  可恢复。Ctrl-C 中断本轮（exit 130）。
- `exec --image <path>`：为支持视觉的模型给 prompt 附带一张图片
  （PNG/JPEG/WebP/GIF，≤5 MB）；可重复。不支持视觉的 provider 会以
  可见错误拒绝。每个请求只携带最新的两张图片；更早的以
  `[image … omitted]` 文本占位符随行，因此截图密集的会话不会撑爆
  预算（存储的会话保留每张图片，占位符会指明省略了什么）。
- `exec --approvals`：选择从 stdin 应答挂起的审批。默认关闭，因此
  无人值守的运行保持 fail-closed 拒绝。JSON 方言用一行 stdin 应答
  某个具体请求：`<call_id> allow|always|deny[:reason]`（call id 来自
  `approval_requested` 事件）。文本方言在 stderr 上提示，接受
  `y` / `a` / `n`。stdin 关闭（EOF）会立即拒绝所有仍挂起的请求——
  任何运行都不会因等待应答而挂死。
- `repl`：一条对话之上的多轮会话。斜杠命令：`/compact`（立即压缩
  上下文）、`/memory`（显示记忆索引）、`/mcp`（列出服务器）、
  `/permissions`（审批模式）、`/quit`（结束会话）、`/help`。
  `/skill-name args` 按名称调用用户可调用技能。
- Console：同一会话套上整视口的内联界面（保留原生 scrollback）——
  审批提示内联渲染（文件写入审批把受影响的行显示为彩色 diff）、
  `/mcp` 状态行、`/btw` 旁路提问（只读，答案流式汇入面板而不触碰
  对话），以及 `/model`（provider 标签页、搜索、仅本会话的 Alt+S、
  OpenAI 兼容 provider 的 thinking 档位行，外加 `/model list|add|set|remove`
  编辑 `~/.wavecode/models.json` catalog）、`/permissions`、`/theme`
  （内置主题加自定义主题）、`/help`（可滚动的按键 + 命令参考）等
  对话框。会话命令：`/sessions`（别名 `/resume`）原位恢复已记录的
  会话，`/fork` 快照出可恢复副本，`/title` 改名，`/new` 开新会话，
  `/init` 请 agent 写 AGENTS.md，`/status` 概览会话，`/undo [n]`
  （或双击 Esc）按整轮回退对话，`/compact [instruction]` 压缩上下文
  并可附带引导，`/export [path]` 与 `/copy` 导出对话，`/usage` 显示
  token 分布，`/editor <cmd>` 设置 Ctrl+G 外部编辑器。完成的轮次
  journal 到 `~/.wavecode/sessions/`；`wavecode --session <id>` 与
  `wavecode --continue` 从 CLI 恢复。恢复是块级的——工具调用、工具
  结果与图片完整回来——因为每个会话在其轮次快照旁保留一份
  write-ahead 历史 journal；结果因崩溃丢失的工具会被报告为
  unresolved，而不是重新执行。在该 journal 出现之前写入的会话从其
  文本快照恢复。当 compaction 替换早期轮次时，摘要以一条
  `## Context Recovery` 附注结尾，指明该 journal（以及活动任务
  列表），agent 因此可以查到确切的早期输出而不是猜测；每个 `task`
  子代理把自己的轮次记录在 `sessions/children/<parent>/` 下。
- `resume`：`wavecode resume` 按新到旧列出旧版会话；
  `wavecode resume <thread-id>` 将其历史作为文本导入并继续交互。
  工具调用导入为 `[tool:name]` / `[error:...]` 标记（文本导入是
  文档化的范围；replay 绝不重新执行）。
- `serve`：跑在活动会话之上的本地 HTTP app server（REST + SSE）。
  只绑定 `127.0.0.1`，要求每次运行一个 bearer token（启动时打印，
  `--token` 可覆盖）。`POST /sessions` 组装支持 parking 的会话；
  `GET /sessions/{id}/events` 以 SSE 流式输出 wire 事件；
  `POST /sessions/{id}/prompt` 提交一轮；挂起的审批与提问经
  `POST .../approvals/{call_id}` 与 `.../questions/{call_id}` 应答；
  `POST /shutdown` 停止服务器。
- `metrics`：把本地 metrics 台账（`~/.wavecode/metrics/`）聚合成
  按模型、按工具的表格——已执行调用及其成功率，refusal 与 denial
  分列，prompt-cache 读取占比，轮次，审批。离线：只读本地文件，
  绝不联系 provider。`--session <id>` 收窄到单个会话；`--json`
  输出合并后的总计供脚本使用。
- `grants`：列出先前会话持久化的 "always allow" 决定
  （`~/.wavecode/grants.jsonl`）及 `wavecode grants remove <i>`
  所需的索引；`grants clear` 全部撤销。这里的每个动作都只会收紧
  权限——被撤销的 grant 回到询问。当撤销无法执行或索引不在表中时
  exit 1。
- `doctor`：不联系任何 provider 校验本地环境——config 解析、
  provider api key 解析（绝不打印）、`[models]` 条目、权限规则
  （无效条目，以及被 deny 遮蔽的 allow）、OS 隔离后端、控制台
  设置、自定义主题与会话记录。任一检查失败即 exit 1。
- `eval tasks`：运行 `benchmarks/tasks/` 下的任务级套件（请从仓库
  根运行）。每个任务把 fixture 复制进一次性工作根，在那里驱动一轮
  真实 `exec`，然后用断言评判工作区——一条按 argv 运行的检查命令，
  或一个必须匹配、包含、不再包含、存在或保持逐字节不变的文件。
  只有轮次干净结束**且**每条断言都成立，任务才算通过，因此崩溃的
  步骤不能靠别人留下的文件过关。消耗真实 token：这是 nightly 级
  度量，不是每个 PR 的门禁。`--filter`、`--tag`、`--json`、
  `--out <path>` 与 `--agent-bin <path>`（对比两个构建）可收窄一次
  运行；`--permission-mode wave` 是无人值守套件能动手的前提。
  只要有一个选中任务未通过即 exit 1。

诊断：每个 surface 的日志都写入 `~/.wavecode/logs/` 下的按日滚动
文件（保留 14 天），绝不写 stdout/stderr，因此 `exec --json` 流保持
干净。级别：`--debug` 优先于 `WAVECODE_LOG` 环境变量，后者优先于
`warn` 默认值（`WAVECODE_LOG` 接受完整的 env-filter 语法——
`wavecode=debug` 只取本 crate 的 debug 日志；裸单词按 target 过滤，
会把一切静音）。活动 UI 中的 panic 会在报告打印前恢复终端模式。

- `update`：将运行中的版本与最新 GitHub release 对比（10 秒探测）。
  有新版本时打印 release 页面，否则打印 "up to date"，首个 tag 之前
  打印 "no published release yet"。探测失败 exit 1，脚本绝不会把它
  误认为"无更新"。TUI 启动时探测一次，有新 release 时在 footer 显示
  `update available: <tag>`。

## 权限

三种模式：`plan`（只读探索；模型被引导提出计划，也可以直接回答）、
`auto`（仅命令执行与破坏性工具需要询问）、`wave`（全自动；deny 规则
仍然生效）。来源按优先级：

1. `--permission-mode <mode>` CLI 标志（全局，胜过配置），
2. 配置中的 `permission_mode`，
3. 内置 `auto`。

旧名称仍可解析（`guarded`/`default`/`acceptEdits` → `auto`、
`bypassPermissions`/`yolo` → `wave`），并带一条启动警告。未知值警告
并回退到 `auto`。Shift+Tab（或 REPL 中的 `/permissions`）为运行中的
会话轮换模式。

规则位于模式之下：`deny` 条目在任何模式下都拒绝，`allow` 条目跳过
询问（home 配置中的 `[permissions]`，见上方模板）。写规则时有两个
边界值得知道：通配 allow 永不豁免复合 Bash 命令——`git status && curl …`
仍会询问，因为 `*` 跨越命令分隔符——而且一条语法错误的条目只损失它
自己，以启动警告的形式报告，而不是丢弃它所在的整张表。

回答 **always allow** 会把该条确切命令或路径存入
`~/.wavecode/grants.jsonl`，下个会话对同一调用直接豁免而不再询问。
存储的 grant 有意保持字面量：携带 `*` 或 `?` 的条目会被重新解析为
通配符，批准的范围会超出人类当初同意的，因此这类条目只在批准它们的
会话内保存在内存里，并在该会话的日志中警告。用 `wavecode grants
list` / `grants remove <i>` / `grants clear` 撤销。

审批是意图，不是围栏。shell 派生的 OS 隔离经 `WAVECODE_SANDBOX_OS=1`
选择性开启；启用时第一个可用的后端生效
（bwrap → Landlock → seatbelt → Windows job object），没有任何后端的
平台 fail closed 而不是不受限地运行。注意各平台上的含义：Linux 与
macOS 后端约束文件系统（cwd + 临时目录可写，shell 派生无网络），而
Windows job object 只约束进程生命周期与数量——没有文件系统或网络
边界，因此那里获批准的命令以你自己账户的权限运行。`wavecode doctor`
打印当前生效的后端及其约束范围。

## 主题

默认的深色外观是 **synthwave** 视觉（霓虹青 primary、琥珀色用户
输入、深紫暗底）；先前的青色 **deepwave** 视觉仍可选择。一次选择
同时为界面 chrome 与代码块着色：synthwave 搭配内置的 SynthWave '84
语法主题，deepwave 搭配 `base16-ocean.dark`，light 搭配
`base16-ocean.light`。颜色深度在启动时探测一次——默认 truecolor，
较朴素的终端上退到最近的 256 色或 16 个经典 ANSI 色。

`/theme light|dark|deepwave|auto` 实时切换（`auto` 重新探测终端
背景）；`/theme <name>` 从 `~/.wavecode/themes/<name>.json` 加载
自定义主题：

```json
{
  "base": "dark",
  "syntax_theme": "ocean-dark",
  "colors": { "primary": "#ff0000" }
}
```

`base` 是 `dark`、`light` 或 `deepwave`；`colors` 用 `#rrggbb` 值
覆盖任意子集的 20 个语义 token；`syntax_theme` 是可选别名
（`synthwave-84`、`ocean-dark`、`ocean-light`）。未知 token 名、
畸形颜色与未知别名在加载时被拒绝——打错字绝不会悄悄渲染成 base。

## 长时间运行的工作

- **目标让轮次继续。** 经 `goal` 工具记录的目标（`/goal` 显示其
  状态；按 home，CAS 版本化）从两个方向驱动循环：当模型不再发起
  工具调用而目标仍是 `Active` 时，轮次继续——至多 8 次；当轮次撞上
  工具轮上限而目标仍开放时，上限重新武装——每轮至多 8 次上限
  （默认配置下 2048 轮），与 goal 驱动器为自己设的上限一致。每一次
  这样的推动都会说明上下文、轮次、重新武装与继续预算还剩多少，模型
  因此可以收尾，而不是开一段它完不成的工作。把目标标为 `completed`、
  `blocked` 或 `paused` 会立刻停止所有驱动；这些是"工作应当停止"的
  声明，循环从不自行写入 goal 状态。
- **历史按变更持久化。** 每次对话变更都向
  `~/.wavecode/sessions/<id>.history.jsonl` 追加一条同步记录，因此
  崩溃至多损失进行中的那一步，恢复的会话保得住工具调用与结果而不只
  是文字，结果从未落地的工具会被报告为 unresolved 而不是被悄悄
  重跑。

## 技能、记忆、MCP

- Skills：带 frontmatter 的 Markdown 文件，从 home 与项目目录发现。
  内联技能展开进本轮；fork 技能作为后台子任务运行，遵守其
  `allowed-tools` 面，完成时以通知形式回注。`task_output` /
  `task_stop` 让模型轮询和停止它们。
- Memory：逐轮的 transcript 蒸馏向按 home 作用域的存储追加
  `[category]` 条目；下一个会话把它们读回作为自己的索引。
- MCP：配置在 `[mcp_servers.<name>]` 下的 stdio 服务器（`command`
  加可选 `args`/`env`）与 streamable-HTTP 服务器（`url`，可选 OAuth
  client 凭据）在启动时以 `initialize` + `tools/list` 连接，并把每个
  工具桥接为 `mcp__<server>__<tool>`（遵守
  `annotations.readOnlyHint`；受能力门控的 `read_resource` /
  `get_prompt` 发现工具同样桥接）。不可达的服务器降级为警告，绝不
  导致启动失败。Prompts 到 skills 的转换是未来工作。

## SDK

[`@wavecode/sdk`](sdk/typescript) 从 JavaScript 驱动 agent：
`execSession({ prompt })` 启动 `wavecode exec --json`，把带类型的
JSONL 事件流暴露为 async iterator，并且在 `approvals: true` 时让你的
代码应答审批请求（`session.answerApproval(callId, "allow")`）。零运行
时依赖；`src/types.ts` 中的 wire 类型与 Rust 的 `EventMsg` 一一对应。
测试对着真实二进制运行（见 SDK README）。

## 文档

| 文档 | 内容 |
|----------|------|
| [`docs/architecture.zh.md`](docs/architecture.zh.md) | crate 依赖方向图与组装根 |
| [`docs/development.zh.md`](docs/development.zh.md) | 构建、测试与 CI 命令；目录布局约定 |
| [`docs/themes.zh.md`](docs/themes.zh.md) | theme JSON 文件编写指南 |
| [`docs/subsystems/`](docs/subsystems/) | 各子系统契约（核心循环、工具、UI、安全、记忆、会话、评测、扩展） |
| [`docs/cookbook/`](docs/cookbook/) | 任务指南：添加工具、添加 subagent、录制与回放 |

`crates/` 下每个目录都有自己的 `README.md`（文件地图与逐 crate 契约）；
每份英文文档旁都有中文镜像（`README.zh.md` / `*.zh.md`）。

## 开发

```bash,
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --check
```

布局：workspace 是 `crates/<group>/<crate>` 下的扁平 crate DAG，依赖
只指向下方——`infrastructure`（叶子原语）、`foundation`（wire 类型、
config、llm、协议词汇、auth）与 `capabilities`（tools、skills、
memory、mcp、sandbox、hooks、context）构成能力栈，`state`（store、
persistence、checkpoint、goal、plan）与 `safety`（gate、guardrail、
audit、secrets）持有持久数据与策略，`action`（tasks、workflow、jobs、
browser、retrieval）与 `runtime`（run loop、child turns、scheduler、
prompt assembly、plugin seam）执行，`operations`（actor、bootstrap
组合根、gateway、eval）组装，`transport` 承载 MCP 字节帧，
`frontends` 承载 `wavecode` 二进制与控制台 UI。TypeScript SDK 位于
`sdk/typescript`。见 [docs/architecture.md](docs/architecture.md)。
DAG 中的每个 crate 未必都能从发布的二进制到达：架构文档的
[Wiring status](docs/architecture.md#wiring-status) 表列出未接线的
那些，磁盘上存在一个 crate 并不构成功能声明。

文档：[架构总览](docs/architecture.md)、
[开发指南](docs/development.md)、
[贡献指南](CONTRIBUTING.md)，以及面向编码代理的
[AGENTS.md](AGENTS.md)。

提交遵循 Conventional Commits（`feat:`/`fix:`/`docs:`/`refactor:`/…，
祈使句主题，一次提交一件事）。

## 许可证

MIT，见 [LICENSE](LICENSE)。
