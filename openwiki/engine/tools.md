---
type: concept
title: 工具系统（wavecode-tools）
description: Tool trait 与注册表、五个内置工具、路径防逃逸与 shell 执行的安全边界。
tags: [tools, filesystem, shell, security]
---

# 工具系统（wavecode-tools）

## 职责

`wavecode-tools` 是工具框架与内置工具集（`crates/tools/src/lib.rs`）。M1 形态：`Tool` trait、`Registry` 注册表，四个内置文件工具（`read_file` / `write_file` / `edit_file` / `list_dir`）与 `shell` 工具。文件工具经 `path_guard` 把所有路径约束在 `ToolCtx::cwd` 之下，防 `..` 越界与绝对路径逃逸。执行管道（schema 校验、hook、权限审批）由 [core](core.md) 编排（M2+ 补 hooks/sandbox 环节）。

## Tool trait 与 Registry

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;              // 全局唯一（M3 MCP 动态工具需运行时命名，故非 &'static str）
    fn description(&self) -> &str;       // 英文，供模型消费
    fn input_schema(&self) -> serde_json::Value;
    fn is_read_only(&self) -> bool;      // 只读 → 可并行
    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}
```

- `ToolCtx { cwd: PathBuf, deny_env: Vec<String> }`：cwd 约定为绝对路径（文件工具的所有路径约束在其下）；deny_env 由装配层注入 provider 的 `env_key` 等显式名单，shell 工具另按敏感后缀模式自动剔除。
- `ToolOutput { content, is_error }`：**失败语义**——业务失败（文件不存在、匹配不唯一、参数缺失/类型错、路径逃逸、非零退出码、超时）返回 `Ok(is_error=true)` 把原因回灌给模型供其自我纠正；`Err` 仅用于实现级故障（io 错误等）。契约上 execute 不得 panic。
- `Registry`：`builtin()` 注册 5 工具；`register(tool)`（M3 起 MCP 等动态工具也经此）；`specs()` 按 name 排序输出稳定（注入采样请求）；`get(name)` 按名查找。
- `ToolsError`：`Io` / `InvalidInput { message }` / `PathEscape { path }`。

## 路径防逃逸（`path_guard.rs`）

`resolve(ctx, path)`：安全校验一律在 **canonicalize 后的真实路径**上进行（Windows canonicalize 带 `\\?\` 前缀，比较两侧须同态）；返回以未 canonicalize 的 cwd 锚定的词法规范化路径（便于展示与比较）。

- 空路径 → `InvalidInput`；`join` 后词法规范化消除 `.` / `..`（`..` 已到根无法弹出的保留，后续 starts_with 校验会拒绝）。
- **已存在**：解开 symlink 后的真实路径必须 starts_with cwd 真实路径（component 级前缀比对，防兄弟目录前缀混淆如 `abc` vs `abd`）。
- **不存在**（write_file 场景）：锚定最近的已存在祖先，canonicalize 后拼接剩余部分再做前缀校验。
- symlink 自身算"存在"（`symlink_metadata`），断链 symlink 在 canonicalize 处报错，避免顺着断链写到 cwd 之外。
- **TOCTOU 假设（显式接受）**：校验与实际使用之间 symlink 可能被替换，M1 威胁模型接受该窗口（fd 锚定 openat 语义为后续里程碑强化方向）。

## 内置文件工具（`fs_tools.rs`）

| 工具 | 只读 | 行为与护栏 |
|---|---|---|
| `read_file` | ✅ | 输出上限 2000 行 / 50 KB（截断追加换行前缀 `\n[truncated]`，字节截断回退字符边界无乱码）；输入侧硬上限 4 MB（超限提示分页）；offset/limit 分页（0-based，越界/limit=0 为业务失败；空文件 + offset>0 为业务失败非 panic）；非 UTF-8 报"binary file?"；目录/文件错配显式分流 |
| `write_file` | ❌ | 整文件覆盖，自动建父目录；content 超 10 MB 直接拒绝（不创建/覆盖） |
| `edit_file` | ❌ | 精确字符串替换：**old_string 必须非空**且唯一匹配（0 匹配 / 多匹配均为业务失败回灌）；输入侧 4 MB 护栏（edit 需全文匹配） |
| `list_dir` | ✅ | 按名排序、目录带 `/` 后缀；单目录 1000 条上限（超限追加 `\n[truncated: N more entries]`）；文件/目录错配显式分流（Windows 上对文件 read_dir 落 os error 267，kind 不稳定，故先取 metadata） |

所有路径经 `resolve_path`（PathEscape/InvalidInput 转业务失败输出，io 故障仍作 Err 传播）；`metadata` 与 `read` 之间的 TOCTOU 窗口保留 NotFound 分流。

## Shell 工具（`shell_tool.rs`）

- 平台 shell：Windows `cmd /C`、Unix `sh -c`；`WAVECODE_SHELL` 环境变量覆盖（值含 `cmd` 按 `/C`，否则 `-c`——启发式，对非常规命名可能猜错）。
- 超时：默认 60s、上限 300s（超过按上限钳制）；`tokio::time::timeout` 包裹 spawn + wait_with_output，超时后 run future 被 drop，`kill_on_drop(true)` 保证 shell 被杀；超时输出为业务失败 `timeout after {n}ms: {command}`（n 为实际 timeout_ms）。
- **非交互**：stdin 置空（防交互式命令如 read/pause/npm init 抢宿主终端输入）；stdout/stderr piped，`wait_with_output` 并发收两路防管道满死锁。
- 输出：stdout/stderr 各 30 KB 截断（UTF-8 边界安全，超限追加 `[truncated]`）；内容 `exit code: N` + 分节输出；Unix 信号杀死时 code 为 None 记 -1（仍非零）。
- **环境脱敏（`sanitize_env`）**：spawn 前剔除——`ctx.deny_env` 显式名单 + 敏感后缀模式兜底（`_API_KEY` / `_TOKEN` / `_SECRET` / `_PASSWORD` 结尾，大小写不敏感）。威胁模型边界：只防"经子进程环境继承泄密"；`type config.toml` 直读内联 api_key 属 M1 已接受面（review 记录在案）。
- **已知限制（M2 跟踪）**：`kill_on_drop` 只杀 shell 自身；孙进程继承管道句柄副本，shell 被杀后成孤儿存活（wavecode 退出 drop 运行时可能阻塞任意久，如孙进程是 dev server）；正解是进程组级回收（Windows Job Object / Unix killpg）。

## 聚焦测试

| 测试 | 位置 | 锁定的行为 |
|---|---|---|
| `registry_specs_sorted_and_have_schema` / `missing_param_is_error_output` | fs_tools.rs | 5 工具 specs 排序稳定 + schema 完整性、缺参 → is_error |
| `write_then_read_roundtrip` / `read_missing_file_is_error_output_not_err` / `edit_requires_unique_match` / `list_dir_marks_dirs` | fs_tools.rs | 写读往返、业务失败形态、edit 唯一匹配、目录标记 |
| `read_empty_file_with_offset_is_error_not_panic` | fs_tools.rs | 回归：空文件 + offset>0 曾触发 usize 下溢 panic；offset=0 读空文件返回空串 |
| `read_rejects_oversized_file` / `write_rejects_oversized_content` / `edit_rejects_oversized_file` | fs_tools.rs | 4 MB / 10 MB 输入护栏：超限拒绝且不创建/覆盖文件 |
| `read_truncates_at_line_cap` / `read_truncates_at_byte_cap_on_char_boundary` / `read_offset_limit_pages_correctly` / `list_dir_truncates_over_entry_cap` | fs_tools.rs | 2000 行 / 50 KB 截断标记 `[truncated]`、截断落字符边界无 U+FFFD、offset/limit 分页精确、1000 条上限 `[truncated: N more entries]` |
| `read_dir_path_is_error` / `list_file_path_is_error` / `invalid_offset_limit_is_error` | fs_tools.rs | 目录/文件错配分流、limit=0/负 offset/类型错 → is_error |
| `rejects_escape` / `resolves_nonexistent_file_under_cwd` / `symlink_escape_is_rejected` / `rejects_sibling_prefix_confusion` | path_guard.rs | `..`/绝对路径逃逸拒绝、write 目标不存在仍锚定 cwd 内、symlink/junction 指向 cwd 外零污染（Windows junction / Unix symlink 平台分支）、`abc` vs `abd` component 级前缀 |
| `captures_stdout_and_exit_code` / `nonzero_exit_is_error_but_captured` / `captures_stderr` / `runs_in_cwd` / `respects_timeout` / `missing_command_is_error_output` | shell_tool.rs | 输出与退出码捕获、非零为错误但内容保留、stderr 分节、cwd 执行、超时（Windows 用 cmd 内建忙等避免留孙进程）、缺参 |
| `truncate_cuts_at_char_boundary_without_mojibake` / `truncate_handles_invalid_utf8_without_panic` | shell_tool.rs | 30 KB 截断回退字符边界（`€` 不切碎）、无效字节经 lossy 替换不 panic |
| `shell_strips_sensitive_env_but_keeps_normal` | shell_tool.rs | 后缀模式（`FOO_API_KEY`）与 deny_env 名单（`FOO_PROVIDER_KEY`）均剔除、普通变量（`FOO_NORMAL`）子进程仍可见 |

## 规划

- grep / glob / web_search / web_fetch / todo / task / skill / browser_* 工具（SPEC §11.2，M2+）。
- 执行管道补齐：JSON Schema 校验 → `validate` → PreToolUse hook → 权限/审批（sandbox）→ execute → PostToolUse hook（SPEC §11.1）。
- `Tool` trait 对齐 SPEC §11.1：`is_destructive()` / `validate()` 用默认实现落地（M3，不动内置 impl）。
- Registry → `Arc<RwLock<…>>`（MCP 重连/热注册，SPEC §17.5 M3）。

## 相关页面

- 编排方：[Agent 引擎（wavecode-core）](core.md)（只读并行/写串行、ToolResult 配对）
- 模型侧：[模型抽象层（wavecode-llm）](llm.md)（ToolSpec 桥接）
- 装配：[命令行入口（wavecode-cli）](../runtime/cli.md)（Registry::builtin + deny_env 注入）
- 安全规划：[规划中的特性 crate（stub）](../planned/feature-crates.md)（sandbox 审批管道）
