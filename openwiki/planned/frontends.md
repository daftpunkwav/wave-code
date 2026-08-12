---
type: concept
title: 前端与 SDK 占位包
description: apps/web、apps/desktop、sdk/typescript 三个 TypeScript 占位包的规划形态。
tags: [planned, web, desktop, sdk, typescript]
---

# 前端与 SDK 占位包

## 状态说明

`apps/web`、`apps/desktop`、`sdk/typescript` 三个 TypeScript 包在 M1 仅为 **README.md + package.json 占位**（无源码、无锁定的依赖版本），`pnpm-workspace.yaml` 已声明 workspace（`apps/*`、`sdk/*`）。它们的类型共享依赖 `wavecode app-server generate-ts` 产物（[protocol](../protocol/protocol.md) 规划），因此 M3 之前的 TS 实现不能开始（review.md §10.1 风险提示）。占位 README 中未标注里程碑状态，按 `docs/SPEC.md` §15 与 PRD §4 的产品形态表确定规划。

## 逐包规划形态

| 包 | 规划技术形态 | 协议接入 | 里程碑 |
|---|---|---|---|
| `@wavecode/web` | Vite + React + TypeScript；状态管理 Zustand；协议客户端封装为 `WavecodeClient`（WebSocket，自动重连 + 事件重同步）；页面：会话列表（多会话并行）、对话视图（流式渲染、工具调用折叠卡、diff 视图）、审批弹窗、设置页（模型/provider/权限/规则）；UI 组件库独立为内部包，Desktop 复用 | WebSocket（JSON-RPC） | M4+（SPEC §15.2） |
| `@wavecode/desktop` | Electron；主进程以 stdio spawn `wavecode app-server`；渲染进程加载 Web UI 组件树；**核心差异能力**：内置浏览器视图（独立 `BrowserView`/`webContents` 标签页），主进程经 `webContents.debugger.attach("1.3")` 建 CDP 通道；`browser_*` 工具桥（navigate / click / type / scroll / screenshot / snapshot / console）：core 发起工具调用 → app-server 路由到 Desktop → CDP 执行 → 结果回灌；用户手动操作与 agent 操作共存（检测"用户活跃中"最近 2s 有输入则等待/询问）；写操作默认走审批；截图不离开本机 | stdio 子进程 | M5+（SPEC §15.3） |
| `@wavecode/sdk` | 以子进程方式运行 `wavecode exec`，将 stdin/stdout 的 JSONL 事件流封装为 typed async iterator，供脚本与第三方工具编排 agent | stdio 子进程（JSONL） | M7+（SPEC §15.4 / PRD §4） |

## 对代理的指引

- 这些包中找不到源码属预期；改动它们前无需担心与 Rust 侧的类型漂移——`generate-ts` 落地后生成物由 CI diff 校验。
- Desktop 的 `browser_*` 工具需要协议层"外部工具注册 op"扩展（SPEC §17.5 M3 记录：M5 前把扩展方案写入 SPEC §4.1）。
- Web UI 的多客户端会话订阅需要事件 fan-out（broadcast，SPEC §17.5 M5）。

## 相关页面

- 协议契约：[前后端协议（wavecode-protocol）](../protocol/protocol.md)
- 服务面：[协议服务（wavecode-app-server）](../runtime/app-server.md)
- 规划总览：[架构总览](../architecture/overview.md)
