---
type: concept
title: 文档地图
description: docs/ 目录下 PRD/SPEC/review/superpowers 文档的定位与用法，以及仓库其余非源码资产。
tags: [docs, prd, spec, review]
---

# 文档地图

## 设计权威文档（`docs/`）

| 文档 | 定位 | 用法 |
|---|---|---|
| `docs/PRD.md` | 产品需求文档（"做什么"）：愿景、四端同核、功能分级（P0/P1/P2）、里程碑、验收标准 | 产品意图与功能优先级的事实源；wiki 的 [planned](../planned/feature-crates.md) / [frontends](../planned/frontends.md) 页面按其描述规划形态 |
| `docs/SPEC.md` | 技术规格（"怎么实现"）：分层架构（§1）、crate 依赖矩阵与边界规则（§3）、协议规范（§4）、agent loop（§5）、上下文管线（§6）、记忆（§7）、skills（§8）、hooks（§9）、MCP（§10）、工具（§11）、安全模型（§12）、配置 schema（§13）、认证（§14）、前端规格（§15）、会话持久化（§16）、演进路线与 M2+ 准备项（§17）、测试策略（§18）、编码规范（§19） | 架构与实现方案的设计权威；wiki 各页面的设计与现状对账基准。注意 §17.5 含逐里程碑准备项（M2 transport/llm 加固、M3 工具与安全、M4 上下文与记忆、M5 Web/Desktop） |
| `docs/review.md`（+ `review.html` 渲染副本） | M1 全面审查报告（commit d11e685，2026-08-04）：八维度评分（综合 A-）、安全审查（§2）、代码质量、复用度、前端占位评估（§10）、文档与代码对账（§11，确认 SPEC §15.5 渲染契约完全落地）、改进优先级清单（§12.2） | 已知债务与改进优先级的事实源（P0：M3 闭环 sandbox 与 auth；P1：tui crate、复用主题与 mock；P2：theme 合一、terminal_width 缓存、EditFile 大小上限） |

`docs/review.html` 是 review.md 的 HTML 渲染副本，不单独开页。

## 实现计划与设计稿（`docs/superpowers/`）

- `docs/superpowers/plans/2026-08-02-m1-basic-cli.md`：M1 基础 CLI 实现计划（T1-T10 任务分解）。
- `docs/superpowers/plans/2026-08-03-cli-wave-rendering.md`：CLI 界面增强实现计划（T1 依赖 → T7 冒烟）。
- `docs/superpowers/specs/2026-08-03-cli-wave-rendering-design.md`：波形品牌与 markdown 渲染的设计稿（[cli](../runtime/cli.md) 的实现蓝本）。

## 仓库根 `skills/` 目录

两份 **OpenWiki 生成技能**（`<root>/skills/<name>/SKILL.md` 形态，frontmatter 含 `name` / `description`）：

- `mermaid-diagrams`：wiki 页嵌入 Mermaid 图的语法规范与纪律（本 wiki 各页面的 mermaid 图均按其规则撰写）；
- `write-connector`：新增 OpenWiki 内置 connector 的流程。

属本仓库生成工具链资产；其目录形态与 `docs/SPEC.md` §8.1 规划的 skills crate 发现格式同构，是 [planned/feature-crates.md](../planned/feature-crates.md) 中 skills 条目的格式参照。

## 其他非源码资产

- `demo/snake.html`：贪吃蛇网页 demo（单文件，无依赖），agent 网页自动化测试样例。
- `conversation_history/session_*.md`：717KB 会话日志（开发过程记录），非产品源码，wiki 不纳入。
- `.codegraph/codegraph.db`：代码图谱工具数据，wiki 不纳入。
- `.gitignore` / `rustfmt.toml`（`max_width = 100`）/ `pnpm-workspace.yaml`（`apps/*`、`sdk/*`）：工程配置。

## 与 wiki 的关系

PRD/SPEC 是设计权威，但 wiki 以**源码与测试为准**；SPEC 标注的"规划项"（M2+）与实现的差距在各页面"已知缺口/规划"小节中显式对账（典型：协议 Op 变体、generate-ts、sandbox/auth 等）。review.md §11 的文档-实现对账结论（渲染契约、依赖矩阵）已在 [cli](../runtime/cli.md) / [架构总览](../architecture/overview.md) 引用。
