---
type: concept
title: 规划中的特性 crate（stub）
description: auth/context/hooks/mcp/memory/sandbox/skills/tui 八个占位 crate 的规划职责与依赖位置。
tags: [planned, roadmap, architecture]
---

# 规划中的特性 crate（stub）

## 状态说明

八个 crate（`auth` / `context` / `hooks` / `mcp` / `memory` / `sandbox` / `skills` / `tui`）在 M1 为**仅模块级文档注释占位（零实现）**——stub 显式标注 YAGNI 理由，不假装实现、不留 TODO 噪声（review.md §12.1）。**代理在这些 crate 中找不到实现属预期**；改动这些 crate 的依赖矩阵前对照 [架构总览](../architecture/overview.md) 的 SPEC §3 边界规则（特性层互不依赖，仅 context→llm、tools→llm 例外；tui 不得依赖 core）。

## 逐 crate 规划范围（证据：各 crate `src/lib.rs` 模块文档 + docs/SPEC.md 对应章节）

| crate | 依赖（workspace） | 规划职责（一句话） | 规格章节 | 优先里程碑 |
|---|---|---|---|---|
| `wavecode-context` | llm | 上下文管理单一管线：token 预算核算 → 三级阈值（警告线 `window-20k` / 自动压缩线 `window-13k` / 阻塞线 `window-3k`）→ 摘要压缩（`CompactionStrategy` trait，首版 `ModelSummary`：结构化摘要 + 最近 N 条原文默认 10）；历史 normalize（移除中断空消息、合并连续工具结果、孤儿 tool_use 补全/剔除） | SPEC §6 | M4 |
| `wavecode-memory` | — | 指令记忆：`WAVECODE.md` 分层发现（用户级 `~/.wavecode/WAVECODE.md` → 项目根 → cwd，全局在前局部在后），`@path` 递归展开（深度上限 5 防环），`.wavecode/rules/*.md` 并入，fallback 文件名（CLAUDE.md/AGENTS.md）；持久记忆：`~/.wavecode/memories/` 下 `MEMORY.md` 索引 + user/feedback/project/reference 四类，自动提取与整合（门控 ≥24h 且 ≥5 新会话） | SPEC §7 | M4 |
| `wavecode-skills` | — | `SKILL.md`（YAML frontmatter + Markdown 正文）发现/解析/清单注入；frontmatter 字段：`description`（必填）/`when_to_use`/`allowed-tools`/`context: inline\|fork`/`user-invocable`/`argument-hint`/`paths`；来源优先级 builtin < `~/.wavecode/skills` < `.wavecode/skills` < MCP 暴露的 skill；inline 展开进当前上下文（`$ARGUMENTS` 占位与 `${WAVECODE_SKILL_DIR}` 变量）、fork 以独立 subagent 运行；清单注入预算 = 上下文窗口 1% | SPEC §8 | M4+ |
| `wavecode-hooks` | — | 9 事件点：PreToolUse（可阻塞）/PostToolUse/UserPromptSubmit（可阻塞）/SessionStart/SessionEnd/Stop（可阻塞，goal 模式）/PreCompact/PostCompact/Notification；hook 类型：`command`（shell，退出码 0 放行、2 阻塞且 stderr 回传模型、其他非零警告放行）与 `prompt`（模板调用模型裁决，M4 后）；字段 matcher/timeout_ms/once；来源用户/项目配置 + SKILL.md frontmatter；超时强制 kill 记 warning | SPEC §9 | M4+ |
| `wavecode-mcp` | — | 客户端：stdio（spawn 子进程）与 streamable-http（含 OAuth 2.0 + PKCE）transport，连接失败指数退避重连，外部工具以 `mcp__{server}__{tool}` 命名注入注册表，server 暴露的 prompt 自动转 inline skill；服务端（P1）：`wavecode mcp serve` 经 stdio 暴露 WaveCode 能力，鉴权默认仅本机 + client 白名单 | SPEC §10 | M3+ |
| `wavecode-sandbox` | — | 四档权限模式：`default`（写/执行/破坏性逐次审批）/`plan`（仅只读）/`acceptEdits`（文件编辑自动放行，shell 仍审批）/`bypassPermissions`（全放行，进入需确认短语）；规则语法 `allow`/`deny`（如 `Bash(git *)`、`File(src/**)`，deny 优先）；审批流：`ApprovalRequested` 事件 → 前端展示 → `ExecApproval` 回填（本次放行/始终放行写规则/拒绝附原因回传模型）；OS 级沙箱（landlock/seatbelt/Windows ACL）为 P2 | SPEC §12 | **M3（P0，review §12.2）** |
| `wavecode-auth` | — | API key：`env_key` 指向环境变量，或 `wavecode login <provider>` 交互录入存系统 keyring（Windows 凭据管理器/macOS Keychain/Linux Secret Service）；OAuth（P1）：PKCE + localhost 回调，refresh token 存 keyring 自动刷新；凭据永不写日志、`debug-config` 输出自动脱敏 | SPEC §14 | M3 |
| `wavecode-tui` | protocol, app-server | ratatui 终端 UI，作为 `wavecode_app_server` 的**进程内客户端**实现全部终端交互：消息流/输入框/状态栏（模型、权限模式、token 用量、cwd）、slash 补全（按 feature flag 过滤）、Esc 中断、`!` 前缀直接执行 shell、审批内联弹窗、diff 语法高亮视图、会话恢复；**不 import core**（能力与远端前端等价）；TUI 渲染必须走 cli render.rs 同款 sanitize 语义 | SPEC §15.1 | M2 |

## 关键跨系统衔接

- **sandbox 是 M3 P0**：审批门落地的最迟时点（review.md §12.2）；工具执行管道（specs → 校验 → hook → 审批 → execute）由 [core](../engine/core.md) 编排，sandbox 提供规则与审批状态；M3 还需审批反向通道（复用 interrupt_handle 模式 + actor in-turn select! 路由，SPEC §17.5）。
- **tui 复用面**：sanitize_terminal（现位于 [cli](../runtime/cli.md) render.rs）与品牌主题（cli 内三处独立 theme 为已知复用债，review §9.1）；MockModel/GatedModel 抽 dev-only 共享 fixture（core 与 app-server 同构重复）。
- **skills 与根目录 `skills/`**：本仓库根 `skills/` 下已有两份 OpenWiki 生成技能（`mermaid-diagrams` / `write-connector`），其 `<root>/skills/<name>/SKILL.md` 形态即 SPEC §8.1 规划的发现格式（见 [文档地图](../operations/docs.md)）。
- **mcp 与 registry**：M3 起 Registry → `Arc<RwLock<…>>` 支持 MCP 重连/热注册；外部工具经 `mcp__{server}__{tool}` 命名注入，参与相同权限审批管道。

## 相关页面

- 架构位置：[架构总览](../architecture/overview.md)
- 已实现对应物：[Agent 引擎（wavecode-core）](../engine/core.md)（特性编排方）、[工具系统（wavecode-tools）](../engine/tools.md)（执行管道）、[命令行入口（wavecode-cli）](../runtime/cli.md)（sanitize/theme 复用源）
