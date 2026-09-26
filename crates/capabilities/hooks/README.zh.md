# crates/capabilities/hooks/ — 生命周期钩子

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | 整个 crate 在一个模块里：`HookEventPoint`（八个事件点，其中三个可阻断）、`HookDef` / `HookInput` / `HookVerdict` / `HookReport`，以及 `HookEngine`——来自 `[hooks.<EventPoint>]` 配置的 `command` 钩子经平台 shell 执行、事件载荷写入 stdin；以编程方式注册的 `prompt` 钩子捕获 stdout（64 KB 上限、截断标记）作为注入上下文，且从不阻断 |

阻断语义由退出码驱动：0 放行；2 在可阻断点（`PreToolUse` /
`UserPromptSubmit` / `Stop`）阻断并把 stderr 回喂给模型；2 出现在其他点、
以及任何其他非零码都降级为放行加警告；超时强制杀死并记录警告。信任边界
有意区别于 shell 工具：钩子命令来自用户自己的配置文件——属于已授权配置，
因此不做环境变量剥离、也不施加路径约束。该 crate 只依赖
`infrastructure-base`（共享的 `shell_invocation` 解析）、`serde_json` 与
`tokio`；钩子的触发与 verdict 接入回合循环都留在 core 侧。
