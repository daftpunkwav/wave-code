---
type: concept
title: CI 与验证
description: 三 OS CI 矩阵门禁、OpenWiki 自动更新工作流与本地验证命令。
tags: [ci, testing, build, validation]
---

# CI 与验证

## CI 门禁（`.github/workflows/ci.yml`）

push main / PR 触发，**ubuntu / windows / macos 三 OS 矩阵**（terminal 工具跨平台的最低门槛——shell 工具、路径护栏的 Windows/Unix 差异需要三 OS 验证）：

1. `cargo fmt --check`（rustfmt，`max_width = 100`）；
2. `cargo clippy --workspace --all-targets --locked -- -D warnings`；
3. `cargo test --workspace --locked`。

工具链：dtolnay/rust-toolchain stable（含 rustfmt、clippy 组件）+ Swatinem/rust-cache。

## OpenWiki 自动更新（`.github/workflows/openwiki-update.yml`）

- 触发：`workflow_dispatch` + 每日 8 点 schedule（cron `0 8 * * *`）。
- `actions/checkout` 用 **fetch-depth: 0**（完整历史——`openwiki code --update` 需要 diff HEAD 与上次记录 commit 的差异；浅克隆会得到空变更摘要）。
- 安装 `openwiki@0.3.1` + `mermaid@11.16.0` + `jsdom@29.1.1`（mermaid/jsdom 提供 Mermaid 图的高保真校验；wiki 无图可移除）。
- `openwiki code --update --print`（provider openrouter / 模型 `z-ai/glm-5.2`）。
- peter-evans/create-pull-request 提交 `openwiki/`、`AGENTS.md`、`CLAUDE.md`、workflow 变更到 `openwiki/update` 分支并开 PR。

## 本地验证命令速查

| 意图 | 命令 |
|---|---|
| 全 workspace 单测（无网络，mock 模型驱动） | `cargo test --workspace --locked` |
| 单 crate 测试（如改 core） | `cargo test -p wavecode-core` |
| 单一二进制构建（产出 `wavecode`，含全部子命令） | `cargo build -p wavecode-cli --release` |
| lint 门禁 | `cargo clippy --workspace --all-targets -- -D warnings` |
| 格式门禁 | `cargo fmt --check` |
| 真实 API 冒烟（T7 路径） | 配好 `~/.wavecode/config.toml` 后 `wavecode exec "<prompt>"`（或 `--json`） |
| TS 侧安装 | `corepack enable` 后 `pnpm install`（packageManager 锁定 pnpm@9.15.0） |

## 测试策略（SPEC §18 落地现状）

- **单元测试与源码同文件/同目录**（`#[cfg(test)] mod tests`），纯逻辑（协议编解码、配置合并、上下文核算、规则匹配）100% 可单测，不依赖网络。
- **mock provider 驱动集成**：core（`MockModel` / `GatedModel` 脚本化流事件）与 app-server（门控 mock + `repeat_tail`）以录制/回放流式响应驱动完整 turn；golden 断言锁定协议事件序列（wire type tag 锁定、FIFO 三元素保序等）。
- **安全回归测试**：`redirect_is_not_followed_and_api_key_not_leaked`（本地双 TCP listener）、`symlink_escape_is_rejected`（Windows junction / Unix symlink）、`sanitize_env` 后缀模式、`sanitize_terminal` 全 ESC/C0/C1 剥离。
- **定时相关**：超时测试用 cmd 内建忙等（Windows 不留孙进程）；只读并行阈值测试（350ms 裕度）在过载 CI 有 flaky 观察（SPEC §17.5 M2 记录，阈值裕度 350/400ms）。
- **跨平台分支**：path_guard 的 symlink 测试在权限不足时打印提示跳过而非失败。
- 规划：cargo-nextest（CI 限制并发组、失败重试 1 次）、cargo-deny/cargo-audit（M2/M4）、generate-ts 产物 CI diff 校验、TUI insta snapshot、Web Playwright 组件测试。

## 相关页面

- 构建入口：[架构总览](../architecture/overview.md)（验证入口一节）
- 测试主体分布：[Agent 引擎（wavecode-core）](../engine/core.md)、[协议服务（wavecode-app-server）](../runtime/app-server.md)、[工具系统（wavecode-tools）](../engine/tools.md)、[命令行入口（wavecode-cli）](../runtime/cli.md)
