# 开发

[English](development.md) | 中文

本仓库的开发指南。crate 之间如何组合见 [architecture.zh.md](architecture.zh.md)。

## 前置条件

- Rust stable（edition 2024 要求 1.85+）。某些平台上构建依赖需要 C 工具链。
- 仅在改动 `sdk/typescript/` 包时需要 Node.js 与 pnpm。

## 命令

CI（`.github/workflows/ci.yml`）在 Linux、Windows 与 macOS 上运行的就是这一组：

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

推送之前先在本地跑同一组命令。性能基准另外位于 `crates/runtime/runner/tests/benchmarks.rs`；界限与夹具记录在 [benchmarks/README.md](../benchmarks/README.md)。

## 代码约定

- **注释与文档使用英文。** 所有 git 跟踪的文档、代码注释与 doc comment 默认用英文；仅在确有必要处使用其他语言。
- **每个 crate 入口文件带文件头**，声明 `@file`、`@description`、职责清单以及它不得依赖的层。职责变化时保持这些文件头最新：

  ```rust
  /*!
   * @file RunLoop
   * @description Execution orchestration for a single agent run.
   *
   * Responsibilities:
   * - Own the run state machine (sample -> decide -> execute -> recover).
   *
   * This module must not depend on: tools, sandbox, hooks, memory, ...
   */
  ```

- **遵守 [architecture.zh.md](architecture.zh.md) 中的依赖规则**：`runtime/runner` 停留在 trait 接缝上，只有 `operations/bootstrap` 引用具体能力 crate，策略消费工具属性而不是名字。
- **最小正确 diff。** 遵循周边风格；不要在无关行上重构能用的代码。先扩展现有抽象，再考虑新增平行抽象。
- **不留残余。** 不合入注释掉的代码、调试打印或占位 TODO。
- 格式化交给 `rustfmt.toml`（默认 profile）；没有需要手工保持的格式风格。

## 测试

- 新行为需要在同一个变更里带测试；bug 修复需要回归测试。
- 单元测试与代码同处（`#[cfg(test)] mod tests`）；跨 crate 行为放集成测试（`crates/*/tests/`，例如 `crates/runtime/runner/tests/`）。
- 测试不得依赖网络、模型 API key 或特定 OS 特性才能通过；脚本化模型与离线夹具是标准做法（见 `operations-eval` 与 `benchmarks/`）。
- Clippy 拒绝警告（`-D warnings`）；修原因，而不是放宽例外。

## 提交与分支

约定见 [AGENTS.md](../AGENTS.md)：Conventional Commits，祈使语气主题不超过 50 字符，一次提交只做一件事；分支命名为 `<type>/<kebab-case-description>`。提交信息描述变更本身，不描述任务清单或文档进度。

## 文档

- 文档随每个代码变更走：在同一个 PR 中更新受影响的 README 段落与文件头。
- 只写当前状态；不要在文档里叙述历史或评审理由。
- 受跟踪的文档位于 `README.md`、`docs/` 与各 crate/包的 README。`docs-local/` 是只在本地使用的草稿区，永不提交。
