# WaveCode 产品需求文档（PRD）

| 项目 | 内容 |
|---|---|
| 版本 | v0.1（M0 基线） |
| 状态 | 已评审草案 |
| License | MIT |
| 技术细节 | 见 [SPEC.md](SPEC.md)（本文档只描述"做什么"，不描述"怎么实现"） |
| 参考实现 | OpenAI Codex（本地源码）、Claude Code（本地反混淆源码 cc-haha） |

---

## 1. 愿景与定位

WaveCode 是一个**完整的、多平台的 AI coding agent**，对标 Claude Code 与 OpenAI Codex 的完整能力集：

- **四端同核**：CLI（非交互）/ TUI（终端交互）/ Web UI / Desktop 四种形态，共享同一个 agent 核心，行为与能力完全一致。
- **现代 agent 工具链全集**：上下文管理、记忆系统、goal 模式、plan 模式、slash 指令、subagents、skills、hooks、MCP 双向支持、权限审批。
- **Desktop 内置浏览器**：agent 可通过 CDP 完整操控 Desktop 内嵌的浏览器，完成 Web 自动化任务（前端自测、数据抓取、UI 走查）。
- **多 provider**：不限定单一模型厂商，Anthropic 与 OpenAI 兼容 API 均可接入（含 DeepSeek / Qwen / Kimi 等兼容端点）。

### 1.1 设计原则

1. **单一核心，协议统摄**：agent 逻辑只实现一次；所有前端经统一 JSON-RPC 协议接入，杜绝多端行为漂移。
2. **简单优先（YAGNI）**：不做超出需求的抽象。特性做成核心模块，不做插件框架；插件化是明确的演进方向而非当前实现。
3. **安全默认**：危险操作默认需要审批；权限模式显式切换；凭据存系统 keyring。
4. **为 prompt cache 与流式而设计**：系统提示词静态/动态分层；事件流是一等公民。
5. **可验证**：每个里程碑有明确验收标准；协议 schema 可由代码生成，文档与实现可对账。

## 2. 目标用户与核心场景

**目标用户**：使用 AI 辅助编程的开发者，从个人开发者到小团队。

核心场景：

1. **终端内结对编程**：在终端中以自然语言驱动 agent 读写代码、跑测试、修 bug（TUI）。
2. **脚本化/CI 集成**：`wavecode exec "修复 lint 错误"` 在管道或 CI 中非交互运行，JSONL 事件流供下游消费。
3. **浏览器内长任务**：在 Web UI 中管理多个会话、查看 diff、审批操作。
4. **桌面全流程**：Desktop 中一边写代码一边让 agent 在内置浏览器里验证前端效果、自动化 Web 操作。
5. **跨工具互操作**：作为 MCP client 接入外部工具生态；同时作为 MCP server 被其他 agent / IDE 调用。

## 3. 竞品分析

| 能力 | Claude Code | OpenAI Codex | WaveCode 目标 |
|---|---|---|---|
| 终端 TUI | 有 | 有（ratatui） | 有（P0） |
| 非交互 exec | 有（`-p`） | 有（`codex exec`） | 有（P0） |
| Web UI | 有（Web 版） | 有 | 有（P1） |
| Desktop | 有（Electron） | 有 | 有，**且内置浏览器可被 agent 自动化**（P1） |
| slash 指令 | ~90+ | ~50 | 核心集 25+（P0/P1 分批） |
| 上下文管理 | 五级压缩策略 | 多套并存 | 单一管线 + 策略 trait（P0） |
| 记忆 | CLAUDE.md 分层 + 自动记忆 | AGENTS.md + memories | WAVECODE.md 分层 + 持久记忆 + 自动提取整合（P0/P1） |
| subagents | 有（4 条生成路径） | 有 | 有（P1） |
| skills | 有（frontmatter 丰富） | 有 | 有，字段集取交集（P1） |
| hooks | 28 事件点 | 生命周期 hooks | 9 个核心事件点（P1） |
| MCP | client | client + server | **双向**（P0 client / P1 server） |
| 权限模式 | 4+ 档 + ML 分类器 | 审批策略 + 沙箱 | 4 档 + 规则语法（P0），OS 沙箱（P2） |
| goal 模式 | 有（/goal） | 有（/goal） | 有（P1） |
| 模型 provider | Anthropic | OpenAI 系 | **多 provider**（P0） |

## 4. 产品形态

| 形态 | 命令/入口 | 协议接入方式 | 优先级 |
|---|---|---|---|
| TUI | `wavecode`（默认） | 进程内 JSON-RPC | P0 |
| CLI 非交互 | `wavecode exec "<prompt>"` | 进程内 | P0 |
| app-server | `wavecode app-server` | stdio / WebSocket 服务 | P0 |
| Web UI | 浏览器访问 | WebSocket | P1 |
| Desktop | Electron 应用 | stdio 子进程 | P1 |
| MCP server | `wavecode mcp serve` | MCP stdio | P1 |
| TS SDK | `@wavecode/sdk` | spawn `exec`，JSONL | P2 |

## 5. 功能需求

优先级定义：**P0** = 没有就不是可用的 coding agent；**P1** = 对标产品的完整能力；**P2** = 差异化与演进。

### 5.1 Agent 核心（P0）

- **F1.1 多 provider 对话**：支持 Anthropic API 与 OpenAI 兼容 API；流式输出；可在 config 中定义自定义 provider（base_url、headers、模型别名）并随时 `/model` 切换。
- **F1.2 工具集**：文件读/写/精确编辑、shell 执行、grep、glob、web 搜索/抓取、todo 列表；工具自声明只读/破坏性属性；只读工具并行执行、写入工具串行。
- **F1.3 turn 循环**：状态机式 agent loop；用户可随时中断（Esc）；中断后可续写。
- **F1.4 会话管理**：会话持久化；`/resume` 恢复历史会话；`/new` `/rename` `/fork`；`/status` 查看 token 与配置状态。

### 5.2 上下文管理（P0）

- **F2.1 token 预算**：实时统计上下文 token 占用并展示（`/context`）。
- **F2.2 自动压缩**：接近上下文窗口上限时自动摘要压缩历史，压缩前后提示用户；压缩失败有熔断与降级策略。
- **F2.3 手动压缩**：`/compact [聚焦指令]` 立即压缩，可带保留重点。
- **F2.4 清空**：`/clear` 开新对话保留会话外壳。

### 5.3 记忆系统（P0 指令记忆 / P1 持久记忆）

- **F3.1 指令记忆**：自动加载 `WAVECODE.md`（用户级 → 项目根 → 子目录分层拼接），支持 `@path` 引用与 `.wavecode/rules/*.md` 规则目录。
- **F3.2 `/init`**：分析代码库并生成项目级 `WAVECODE.md`。
- **F3.3 持久记忆**（P1）：user / feedback / project / reference 四类记忆，跨会话生效；`/memory` 查看与编辑；`#` 前缀快速记录。
- **F3.4 记忆自动提取与整合**（P1）：会话结束后后台 subagent 自动提炼候选记忆条目并写入；满足门控（距上次整合 ≥24h 且期间 ≥5 个新会话）时自动整合——合并重复、剔除失效、精简索引。全部条目经 `/memory` 可见、可编辑、可删除。
- **F3.5 兼容导入**（P2）：可导入 CLAUDE.md / AGENTS.md。

### 5.4 Slash 指令（核心集）

- **P0**：`/help` `/init` `/compact` `/clear` `/model` `/status` `/context` `/permissions` `/resume` `/new` `/quit` `/mcp` `/memory` `/diff` `/review`
- **P1**：`/goal` `/plan` `/agents` `/skills` `/hooks` `/usage` `/config` `/fork` `/rename` `/export` `/login` `/logout`
- 指令体系在协议层通用（TUI/Web/Desktop 一致），支持按 feature flag 过滤实验指令。

### 5.5 Goal 模式（P1）

- `/goal <目标>` 进入目标驱动模式：agent 围绕可验证的完成条件自主多轮推进；每轮注入 steering 上下文；目标未达成时阻止提前结束；支持暂停/恢复/放弃；预算限制（轮数/token/时间）。

### 5.6 Plan 模式（P1）

- 只读探索 + 计划审批：`/plan` 或 Shift+Tab 切换；agent 只能执行只读工具，产出实施计划经用户批准后退出 plan 模式执行。

### 5.7 Subagents 与后台任务（P1）

- Task 工具派生子代理（独立上下文窗口）；内置 `explore`（只读代码探索）、`plan`（方案设计）、`general` 三类；支持后台运行与完成通知注入；`/agents` 管理自定义 agent 定义。

### 5.8 Skills（P1）

- `SKILL.md`（YAML frontmatter + Markdown）即技能；来源：builtin / 用户级 / 项目级；支持模型自动触发（`when_to_use`）与用户 `/skill-name` 直调；inline（展开进当前上下文）与 fork（独立子代理）两种执行模式；`/skills` 管理。

### 5.9 Hooks（P1）

- 9 个事件点：PreToolUse / PostToolUse / UserPromptSubmit / SessionStart / SessionEnd / Stop / PreCompact / PostCompact / Notification。
- command hook（shell，退出码 0 放行 / 2 阻塞并回传原因）与 prompt hook；`/hooks` 管理。

### 5.10 MCP（P0 client / P1 server）

- **F10.1 客户端**：配置 `[mcp_servers]` 接入 stdio 与 streamable-http（含 OAuth）server；工具以 `mcp__server__tool` 命名空间进入工具表；`/mcp` 查看状态与重连。
- **F10.2 服务端**（P1）：`wavecode mcp serve` 把 WaveCode 能力暴露为 MCP server。

### 5.11 权限与安全（P0）

- 四档权限模式：`default`（危险操作逐次审批）/ `plan`（只读）/ `acceptEdits`（编辑自动放行，shell 仍审批）/ `bypassPermissions`（全放行，需显式确认风险）。
- 规则语法：`Bash(git *)`、`File(src/**)` 细粒度放行/拒绝规则；审批 UI 清晰展示命令、影响范围与差异。

### 5.12 Web UI（P1）

- 会话列表与多会话并行；流式消息渲染（Markdown / 代码高亮 / diff 视图）；审批弹窗；token 用量展示；移动端自适应布局。

### 5.13 Desktop 与内置浏览器（P1）

- Electron 壳复用 Web UI 全部界面与会话能力。
- **内置浏览器**：应用内嵌 Chromium 浏览器视图（地址栏、标签页、devtools）。
- **agent 浏览器自动化**：agent 通过工具调用操控内置浏览器——导航、点击、输入、滚动、截图、DOM 快照、控制台日志读取；用户可随时接管手动操作；自动化操作与手动操作不互相阻塞。
- 本地文件对话框、系统通知等桌面集成。

### 5.14 认证（P0）

- API key 登录（各 provider）与 OAuth（PKCE）登录；凭据存系统 keyring；`/login` `/logout`。

### 5.15 配置系统（P0）

- `~/.wavecode/config.toml` 用户级 + `.wavecode/config.toml` 项目级分层合并；`profiles` 命名配置档 `-p` 切换；`model_providers` 自定义端点；按项目目录覆盖。

## 6. 非功能需求

| 类别 | 要求 |
|---|---|
| 性能 | TUI 首 token 渲染 < 1s（取决于 provider 延迟外的开销 < 100ms）；进程内 transport 零序列化拷贝；冷启动 < 300ms |
| 兼容性 | Windows 10+ / macOS 13+ / Linux（x64、arm64）；单二进制分发 CLI |
| 安全 | 凭据仅存系统 keyring；危险操作默认审批；hook 脚本超时强制终止；MCP OAuth 遵循 PKCE |
| 可维护性 | 低耦合高内聚：crate 依赖单向无环（见 SPEC §3）；公开接口最小化；注释中文、术语保留英文 |
| 可观测性 | 结构化日志；token 用量统计；`/feedback` 问题反馈 |
| 可测试性 | 单测与源码同目录；协议 golden 测试；UI snapshot 测试 |

## 7. 里程碑

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| **M0** 骨架 | git 仓库、15 crate 可编译骨架、PRD/SPEC | `cargo check/fmt/clippy` 全绿；文档评审通过 |
| **M1** 核心打通 | protocol + llm（单 provider）+ core 最小 loop + tools（fs/shell）+ `exec` 非交互 + 基础行式 REPL（ratatui TUI 在 M2） | `wavecode exec "创建 hello.txt 并列出目录"` 端到端完成；JSONL 事件流可消费（已完成，2026-08-03） |
| **M2** TUI | app-server 进程内 transport + TUI 基础（流式渲染、输入、中断） | TUI 完成多轮对话；Esc 中断生效 |
| **M3** 工具与安全 | 工具全集 + 权限模式 + 审批 UI + MCP client + config 分层 | 危险命令触发审批；MCP server 工具可调用 |
| **M4** 上下文与记忆 | context 管线（auto compact）+ WAVECODE.md + `/init` + slash P0 全集 + Web UI 基础 | 长会话自动压缩不丢任务；`/init` 生成可用文档 |
| **M5** Web UI 完整 | 多会话、diff、审批、用量 | 浏览器完成完整 coding 会话 |
| **M6** Desktop | Electron 壳 + 内置浏览器 + 浏览器自动化工具 | agent 在内置浏览器完成"打开页面→填写表单→截图"链路 |
| **M7** 完整能力 | goal / plan / subagents / skills / hooks / MCP server / 持久记忆（含自动提取整合） | §5 全部 P1 功能验收；TS SDK alpha |

## 8. 验收标准（产品级）

1. 四端完成同一任务（"修复一个 failing test"）的核心事件序列语义一致（协议级 golden 测试）。
2. 权限模式为 `default` 时，任何写文件/执行 shell 都经过审批或规则放行。
3. 上下文接近窗口上限时自动压缩，任务不中断、关键约束不丢失（以 compaction benchmark 场景验证）。
4. Desktop 内置浏览器可被 agent 完整操控，且用户可随时无缝接管。
5. 更换 provider（Anthropic ↔ OpenAI 兼容）仅改配置，无需重启会话之外的任何操作。

## 9. 明确不做（本期）

- 语音输入/实时语音会话。
- 云端任务（cloud tasks）与团队协作功能。
- 插件市场与第三方插件框架（核心预留扩展点，见 SPEC §17 演进路线）。
- 移动端原生应用。

## 10. 已决议事项（2026-08-02）

1. 对标产品范围聚焦 Claude Code 与 OpenAI Codex，不纳入 pi。
2. License 采用 MIT（已落实 `LICENSE` 文件与各 manifest 的 license 字段）。
3. 记忆自动提取与整合纳入 P1（F3.4），机制见 SPEC §7.2。
