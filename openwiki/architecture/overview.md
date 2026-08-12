---
type: concept
title: 架构总览
description: WaveCode 分层架构、15 个 crate 的依赖矩阵与 M1 里程碑实现状态、演进路线。
tags: [architecture, rust, workspace]
---

# 架构总览

## 仓库定位

<!-- openwiki: broken internal link [operations/docs.md] file "operations/docs.md" does not exist. Fix the href or restore the target, then delete this comment. -->
WaveCode 是一个**多平台 AI coding agent**（CLI / TUI / Web / Desktop 四端同核），Rust workspace（15 crate）+ 3 个 TypeScript 占位包，经统一 JSON-RPC 协议（app-server）接入，对标 Claude Code 与 OpenAI Codex 的能力集。当前为 **M1 里程碑**：7 个 crate 已实现、8 个 crate 仅文档注释占位、3 个 TS 包仅 README + package.json 占位（[文档地图](operations/docs.md) 的 review.md §1.1 确认）。

## 分层架构

<!-- openwiki: mermaid parse failed and this diagram was converted to a text fence so it does not break rendering. Fix the diagram source and restore the mermaid fence. Parser error: Heuristic: an unescaped angle bracket inside a label breaks rendering; rephrase the label. -->
```text
flowchart TB
    subgraph 前端层
        CLI[wavecode 二进制<br/>exec / REPL]
        TUI[wavecode-tui<br/>规划 M2]
        WEB[@wavecode/web<br/>规划 M4]
        DESK[@wavecode/desktop<br/>规划 M5]
        SDK[@wavecode/sdk<br/>规划 M7]
    end
    subgraph 服务与协议层
        AS[wavecode-app-server<br/>InProcessClient 进程内]
        PROTO[wavecode-protocol<br/>Submission / Event]
    end
    subgraph 引擎层
        CORE[wavecode-core<br/>Session · turn 状态机]
    end
    subgraph 特性层
        TOOLS[wavecode-tools]
        CTX[wavecode-context<br/>stub]
        MEM[wavecode-memory<br/>stub]
        SK[wavecode-skills<br/>stub]
        HK[wavecode-hooks<br/>stub]
        MCP[wavecode-mcp<br/>stub]
        SB[wavecode-sandbox<br/>stub]
    end
    subgraph 基础设施层
        LLM[wavecode-llm]
        CFG[wavecode-config]
        AUTH[wavecode-auth<br/>stub]
    end
    CLI -->|进程内| AS
    TUI -->|进程内| AS
    WEB -->|WebSocket 规划| AS
    DESK -->|stdio 规划| AS
    SDK -->|stdio 规划| AS
    AS --> PROTO
    AS --> CORE
    CORE --> TOOLS
    CORE --> LLM
    CORE --> CFG
    CORE -.M2+ 引回.-> CTX
    CORE -.M2+ 引回.-> MEM
    CORE -.M2+ 引回.-> SK
    CORE -.M2+ 引回.-> HK
    CORE -.M2+ 引回.-> MCP
    CORE -.M2+ 引回.-> SB
    TOOLS --> LLM
```

M1 实际依赖链（已实现 crate）：`cli → app-server → core → {protocol, llm, tools}`；`cli → {config, llm, tools}`（bootstrap 装配）；`core → {protocol, llm, tools}`；`tools → llm`（ToolSpec 桥接）。其余特性层 crate 按 SPEC §3 矩阵在 M2+ 引回。

## crate 依赖矩阵（SPEC §3）与边界规则

| crate | 状态 | 允许的 workspace 依赖 | 职责一句话 |
|---|---|---|---|
| `wavecode-protocol` | ✅ 实现 | — | Submission/Event 类型，serde，TS schema 导出源（规划） |
| `wavecode-config` | ✅ 实现 | — | 分层 TOML 解析与合并（M1：用户级单层） |
| `wavecode-llm` | ✅ 实现 | — | provider 抽象与流式客户端（M1：Anthropic） |
| `wavecode-tools` | ✅ 实现 | llm | Tool trait、注册表、内置工具 |
| `wavecode-context` | ⬜ stub | llm | token 预算、压缩管线 |
| `wavecode-memory` | ⬜ stub | — | WAVECODE.md 与持久记忆 |
| `wavecode-skills` | ⬜ stub | — | SKILL.md 发现/解析/注入 |
| `wavecode-hooks` | ⬜ stub | — | 事件点与 hook 执行 |
| `wavecode-mcp` | ⬜ stub | — | MCP client/server |
| `wavecode-sandbox` | ⬜ stub | — | 权限模式、审批、命令策略 |
| `wavecode-auth` | ⬜ stub | — | 登录与 keyring 凭据 |
| `wavecode-core` | ✅ 实现 | protocol, llm, tools（M2+ 引回其余） | agent 引擎 |
| `wavecode-app-server` | ✅ 实现 | protocol, core | JSON-RPC 服务与 transport（M1：进程内） |
| `wavecode-tui` | ⬜ stub | protocol, app-server | 终端 UI |
| `wavecode-cli` | ✅ 实现 | protocol, config, llm, tools, core, app-server | 二进制入口 |

边界规则（review 强制）：**特性层 crate 互不依赖**（仅 context→llm、tools→llm 两例外）；**tui 不得依赖 core**（只能经 app-server 与 protocol 交互，保证与 Web/Desktop 能力等价）；第三方依赖统一在 workspace 根 `[workspace.dependencies]` 定版本。`Cargo.toml`：`resolver = "3"`、`edition = 2024`；`rustfmt.toml`：`max_width = 100`。

## 关键设计决策

1. **协议统摄**：所有前端只讲一套协议，core 不知晓前端形态（参考 Codex app-server，但只做一套协议，不引入 Codex 双轨历史包袱）。
2. **crate 收敛**：15 个 crate，一个特性域一个 crate（Codex ~109 crate 的导航成本是反面教材）。
3. **单一上下文管线**：压缩策略以 trait 插拔，只有一条触发与执行管线（SPEC §6，规避 Codex 5+ 套 compact 并存的演进事故）。
4. **特性即模块，非插件**：goal/skills/memory 等为核心 crate 内模块；插件化扩展点是演进方向（SPEC §17 路线 1）。
5. **安全默认**：危险操作默认审批（sandbox 规划）、凭据 Debug 脱敏、shell env 剔除、禁 HTTP 重定向防 key 泄露、终端 sanitize。

## 里程碑状态与演进路线

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M1 | protocol/config/llm/tools/core/app-server/cli 七 crate + wire 锁定 + CLI exec/REPL + 渲染契约 | ✅ 完成（commit a656bbb 起，43 commits） |
| M2 | TUI（tui crate）、stdio/WS transport、llm 加固、MockModel 共享 fixture、fs 工具 tokio::fs | 规划（SPEC §17.5） |
| M3 | sandbox 与 auth 闭环（审批门最迟时点）、Registry Arc 化、Tool trait 对齐、config 分层 | 规划（review.md §12.2 P0） |
| M4 | 上下文与记忆（context/memory 落地）、system prompt 分层组装 builder、CI 加固 | 规划 |
| M5 | Web UI、事件 fan-out、generate-ts、browser_* 工具桥扩展方案 | 规划 |
| M6/M7 | Desktop（内置浏览器 + CDP）、TS SDK | 规划 |

<!-- openwiki: broken internal link [runtime/cli.md] file "runtime/cli.md" does not exist. Fix the href or restore the target, then delete this comment. -->
近 20 个 commit 全部集中在 CLI 界面增强（T1-T7：markdown 渲染、波形品牌、渲染状态机）与 M1 审查修复——cli 是当前改动最活跃区域（见 [命令行入口（wavecode-cli）](runtime/cli.md)）。

## 验证入口

<!-- openwiki: broken internal link [operations/ci.md] file "operations/ci.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- 门禁：`cargo fmt --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`（三 OS 矩阵，见 [CI 与验证](operations/ci.md)）。
- 构建：`cargo build -p wavecode-cli --release` → 产出 `wavecode`（内含全部子命令）。

## 相关页面

<!-- openwiki: broken internal link [engine/core.md] file "engine/core.md" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [engine/llm.md] file "engine/llm.md" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [engine/tools.md] file "engine/tools.md" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [engine/config.md] file "engine/config.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- 引擎：[Agent 引擎（wavecode-core）](engine/core.md)、[模型抽象层（wavecode-llm）](engine/llm.md)、[工具系统（wavecode-tools）](engine/tools.md)、[配置系统（wavecode-config）](engine/config.md)
<!-- openwiki: broken internal link [protocol/protocol.md] file "protocol/protocol.md" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [runtime/app-server.md] file "runtime/app-server.md" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [runtime/cli.md] file "runtime/cli.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- 协议与服务：[前后端协议（wavecode-protocol）](protocol/protocol.md)、[协议服务（wavecode-app-server）](runtime/app-server.md)、[命令行入口（wavecode-cli）](runtime/cli.md)
<!-- openwiki: broken internal link [planned/feature-crates.md] file "planned/feature-crates.md" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [planned/frontends.md] file "planned/frontends.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- 规划：[规划中的特性 crate（stub）](planned/feature-crates.md)、[前端与 SDK 占位包](planned/frontends.md)
