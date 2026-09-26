# Cookbook：添加一个 subagent

[English](adding-a-subagent.md) | 中文

Subagent 由模型经 `task` 工具调用，由带 frontmatter 的纯 Markdown 文件定义。下述一切实现于 `crates/capabilities/tools/src/agent_task_tool.rs`；深度限制在 `crates/runtime/child/src/lib.rs`。

## 1. 编写定义文件

在项目中创建 `.wavecode/agents/<file>.md`（或经 `.claude/agents/` 使用 `~` 级别的包）：

```markdown
---
name: explorer
description: Wide read-only codebase investigation; returns a summary of findings.
tools:
  - read
  - grep
  - glob
kind: explore
---

可选的正文会被解析器忽略；把身份信息放在 `description` 里。
```

Frontmatter 规则（由 `parse_agent_def` 解析）：

- `name` — 实际上是必填；为空时回退到文件词干。
- `description` — 作为委派目的呈现给模型；同时成为子 agent 的身份前导词（"You are the `explorer` agent. Purpose: …"）。
- `tools` — 受限的工具面；逗号分隔（`tools: read, grep`）或 `- item` 列表。留空则保留完整会话工具面。
- `kind` — `explore`、`readonly` 或 `read-only` 选择只读配置（`TaskKind::ReadOnly`）。其他任何值都是 `Standard`。
- 缺失 `---` frontmatter 块会让整个文件不可解析；该文件被跳过。

## 2. 发现机制如何运行

`discover_agent_defs(cwd)` 先扫描 `cwd/.wavecode/agents/*.md` 再扫描 `cwd/.claude/agents/*.md`（跨工具兼容约定），并在每个目录内按文件路径排序。**同名的第一个定义获胜**，因此仓库级定义会遮蔽同名的全局定义。不可读或不可解析的文件被静默跳过——发现是尽力而为的，绝不能让 `task` 调用失败。没有重载/缓存步骤：每次 `task` 调用都重新读取定义，因此编辑立即生效。

## 3. `task` 工具如何应用配置

`TaskTool::execute` 解析调用：

- `subagent_type: "explore"` → 内置只读配置，无前导词。
- `subagent_type: "<name>"` → 发现的定义：`kind` 映射到子 agent 的能力配置，身份前导词被前置到调用方的 `prompt`，定义的 `tools` 成为子 agent 的允许清单。
- 调用中的显式 `allowed_tools` 数组**覆盖**定义的工具面。
- 未知的 `subagent_type` → 业务错误（`is_error`），列出 `explore` 以及所有已发现的名称，让模型自我纠正。
- 缺失 `prompt` → 业务错误。

子 agent 在会话驱动器上运行自己的会话，因此它无法中断父会话。调用阻塞直到子 agent 结束——受 `TASK_WAIT_TIMEOUT`（600 秒，250 毫秒轮询）约束——并内联返回子 agent 的摘要；仍在运行的子 agent 会带 id 上报，供 `task_output` / `task_stop` 使用。按 run 的允许清单在循环内经 `RunAllowlist`（`crates/runtime/runner/src/lib.rs`）执行：受限的子 agent 在 `available_tools` 里根本*看不到*被拒绝的工具，因此它们在自己的工具面内规划，而不是撞上拒绝。

## 4. 深度上限与"无孙辈"规则

- `task` 总是以 `depth: 0` 生成——子回合内部的委派不会嵌套，它在共享服务上是一个全新的顶层子 agent。子 agent 只能通过受追踪的 continue 路径获得后续回合，该路径以 `depth + 1` 和 `parent` 生成（`crates/operations/bootstrap/src/child_service.rs`）。
- `crates/runtime/child/src/lib.rs` 强制 `MAX_CHILD_DEPTH = 3`：超过上限的 spec 在其工厂被构建之前就被拒绝（"max child depth 3 exceeded"），返回显式的失败结果而不是无界运行。
- 净效果：不会出现无界的 agent 树——扇出可以宽，但很浅，且每个子 agent 都可经任务服务观察和停止。

## 5. 检查清单

- [ ] 文件可解析（开头的 `---`，键 `name` / `description` / `tools` / `kind`）。
- [ ] 名称不与全局定义冲突，除非有意遮蔽。
- [ ] `tools` 只列出存在的名字（拼错 = 子 agent 里悄悄缺一个工具）。
- [ ] 只读意图经 `kind: explore` 表达（而不是指望模型自觉）。
- [ ] 经一次真实委派验证：`task(prompt=..., subagent_type=<name>)` 返回子 agent 的摘要，或 `task_output` 显示仍在运行的 id。
