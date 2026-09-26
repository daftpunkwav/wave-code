# crates/frontends/harness/ — `wavecode` 二进制：CLI 入口与 app-server 界面（harness-cli）

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | 清单；声明 `wavecode` 二进制目标 |
| `src/main.rs` | clap CLI：参数/子命令解析、会话组装与全部 surface——headless `exec`（文本或 `--json` JSONL）、`repl`、TTY 上的内联控制台（否则 REPL）、`resume`、`mcp serve`、`plugin list`、`acp`、`doctor`、`metrics`、`update`、`grants`、`eval tasks`，以及 `serve`（本地 HTTP app-server，REST + SSE，经 `operations-gateway` 运行） |
| `src/logging.rs` | `~/.wavecode/logs` 下的按日滚动文件日志；只写文件，协议流与 TUI 因此保持干净 |
| `src/task_eval.rs` | 任务级 eval 执行：fixture 隔离、经 `AgentStep` seam 走 `exec` surface 的 agent 步骤、真实进程的 `World`、文本/JSON 报告；评分在 `operations-eval` |
| `src/update.rs` | 对照 GitHub releases 的版本检查与校验和验证的自安装，保留 `.bak` 回滚副本 |

二进制本身是薄入口：它通过 `operations-bootstrap` 组装会话，经 actor
客户端驱动，把交互式渲染交给 `console-ui`。在 headless `exec` 中，未
显式 `--approvals` 时挂起的审批公开拒绝；所有 surface 只写文件日志，
从不写 stdout/stderr。
