# crates/frontends/harness/ — `wavecode` 二进制：CLI 入口与 app-server 界面（harness-cli）

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | 清单；声明 `wavecode` 二进制目标 |
| `src/main.rs` | clap CLI：参数/子命令解析、会话组装与全部 surface 的分发 |
| `src/exec.rs` | headless `exec` surface：单回合管线、文本或 `--json` JSONL 事件渲染、stdin 审批方言，以及可续跑的 `ExecSession` journal |
| `src/repl.rs` | 交互式 REPL：slash 命令文法、多轮输入循环，以及旧线程 `resume` surface |
| `src/tui.rs` | 全屏 TUI 组装（resume seed、`/model` 选择器目录、UI 接线），跑在实时 harness 会话之上 |
| `src/serve.rs` | 服务器形态 surface：MCP stdio、ACP stdio、插件清单，以及本地 HTTP app-server（REST + SSE，经 `operations-gateway` 运行） |
| `src/surfaces/` | 从不组装会话的自包含命令 surface，只作用于本地状态（`grants` 可撤销已持久化的授权）；`update_cmd` 是唯一联网的一个（自更新） |
| `src/logging.rs` | `~/.wavecode/logs` 下的按日滚动文件日志；只写文件，协议流与 TUI 因此保持干净 |
| `src/task_eval.rs` | 任务级 eval 执行：fixture 隔离、经 `AgentStep` seam 走 `exec` surface 的 agent 步骤、真实进程的 `World`、文本/JSON 报告；评分在 `operations-eval` |
| `src/update.rs` | 对照 GitHub releases 的版本检查与校验和验证的自安装，保留 `.bak` 回滚副本 |

二进制本身是薄入口：它通过 `operations-bootstrap` 组装会话，经 actor
客户端驱动，把交互式渲染交给 `console-ui`。在 headless `exec` 中，未
显式 `--approvals` 时挂起的审批公开拒绝；所有 surface 只写文件日志，
从不写 stdout/stderr。
