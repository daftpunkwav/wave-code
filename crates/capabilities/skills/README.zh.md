# crates/capabilities/skills/ — SKILL.md 发现、目录注入与调用

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | 技能模型：`Skill`/`SkillMeta`（YAML frontmatter；字段名的 kebab 与 snake 两种拼写都接受，未知字段忽略）、`SkillSource` 优先级覆盖（builtin < user < project）、`discover` / `Discovery::refresh`（坏文件逐个警告并跳过）、带分级字符预算降级的 `SkillSet::catalog`，以及 `$ARGUMENTS` / `${WAVECODE_SKILL_DIR}` 展开 |
| `src/plugin.rs` | `~/.wavecode/plugins/<dir>/plugin.toml` 下的插件包（`{name, version, skills_dir?, hooks_file?}` 加内联 `[mcp_servers]` 表）：技能经发现管线加载；MCP 条目与 hook 规则以原始形态返回，交装配层转换；非法包警告并跳过 |
| `src/tool.rs` | `SkillTool`——模型可调用的 `skill` 工具：inline 技能原地展开；fork 技能经 `action-tasks` 的 `TaskService` 接缝派生后台子任务，经既有的子任务通知回报结果 |

渲染走共享 `Tool` trait（`wavecode-tools`——与 `wavecode-mcp` 同层的取用
方式），fork 走零依赖的 `action-tasks` crate；这就是全部的工作区依赖边。
目录（catalog）预算（上下文窗口的 1%）由调用方以字符配额传入，按字符而非
字节比较，因此 CJK 描述不会被提前截断。`user_invocable` 只约束前端斜杠
调用，绝不约束模型调用——目录本身就在邀请模型自行触发技能。
