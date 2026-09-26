# crates/capabilities/tools/ — 工具框架与内建工具集

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | 框架本体：`Tool` trait（read-only / destructive 属性、业务失败语义）、`Registry`（内建集合、`name_subset` / `read_only_subset`、延迟注册）、`ToolCtx`（cwd + `deny_env`）、`ToolOutput`、`ToolAllowlist`（skills 的 `allowed-tools` 工具面）、`is_sensitive_env_name`、`TOOL_FAULT_PREFIX` 实现级故障标记 |
| `src/fs/` | 限定在 `ToolCtx::cwd` 之下的文件工具：`read`（`read.rs`，2000 行 / 50 KB 预算）、`write`（`write.rs`，原子写，10 MB 上限）、`edit`（`edit.rs`，精确匹配唯一性检查）、`view`（`image.rs`，按魔数嗅探的图片附件，5 MB）、`present`（`present.rs`，交付物记入共享 `PresentStore`）；`mod.rs` 持有共享上限与辅助函数，还承载 `FileLedger`——`read` 记录每个文件的 (mtime, len) 指纹，`write` / `edit` 在落盘指纹偏离会话最近一次所见时拒绝改写，外部变更（格式化器、git checkout、其他进程）由此强制先重读，而不是被悄悄覆盖 |
| `src/search/` | 只读搜索：`grep`（`grep.rs`，正则内容搜索，500 条匹配上限）与 `glob`（`glob.rs`，路径模式匹配，1000 条路径上限）；阻塞遍历放进 `spawn_blocking`，命中路径在 canonicalize 后复检以防符号链接逃逸（`mod.rs`） |
| `src/path_guard.rs` | 路径逃逸守卫：所有检查都在 canonicalize 后的真实路径上进行（符号链接交换的 TOCTOU 窗口有明文记载），返回词法归一化的路径供展示 |
| `src/shell_tool.rs` | `shell`：经共享 `shell_invocation` 解析的跨平台命令执行、敏感环境变量清洗、单流 30 KB 输出上限、默认 60 s 超时（上限 300 s）、经 `WAVECODE_SANDBOX_OS` 选择性开启的 OS 级隔离 |
| `src/script.rs` | `python` / `node`：以内联方式非交互执行脚本，解释器按 PATH 探测，cwd 隔离，超时钳制，失败语义与 shell 一致 |
| `src/lsp.rs` | 极简 stdio LSP 客户端加 `lsp_symbols` / `lsp_definition` / `lsp_hover` / `lsp_references`（每次调用显式给 `server_command`，或经惰性启动、跨调用复用的注册表后端）与 `lsp_diagnostics`（渲染记录下来的 `publishDiagnostics` 推送）；不捆绑任何服务器 |
| `src/web_fetch.rs` | `web_fetch`：仅 http/https，手工跟随重定向（最多 5 跳），流式大小上限（默认 256 KB / 硬上限 1 MB）加 `[truncated]` 标记，HTML 渲染为 Markdown（`raw=true` 可关闭） |
| `src/websearch.rs` | `web_search`：可插拔的 `SearchBackend`；默认的 `DuckDuckGoBackend` 抓取免密钥的 HTML 端点，解析失败作为业务错误上抛 |
| `src/html.rs` | 服务于 `web_fetch` 的宽松单遍 HTML -> Markdown 转换器：结构性标记转 Markdown，script/style 丢弃，畸形输入退化为纯文本、绝不失败 |
| `src/spill_tool.rs` | `spill`：经 context crate 的 `SpillStore` 只读读回 `spill://` URI（spill 存放在 `cwd` 之外，`read` 够不到） |
| `src/todo_tool.rs` | `todowrite`：全量重写语义的会话任务列表；`TodoStore` 句柄存于会话配置、装配时注入 |
| `src/agent_task_tool.rs` | `task`：经 `action-tasks` 接缝的自由格式子代理委派，`.wavecode/agents/` 中的具名 agent 定义解析为工具面限制与身份前导词 |
| `src/task_tools.rs` | `task_output` / `task_stop` / `task_continue`：观察、停止、续跑子任务；未知 id 一律作为业务错误返回 |
| `src/ask_user_tool.rs` | `ask_user`：交互式提问表面；合法调用由 sandbox 按工具名路由进提问流程，工具体只做校验、并在闸门被绕过时如实上报 |

两条契约贯穿每个工具：业务失败返回 `Ok(ToolOutput { is_error: true, .. })`、
附上模型可自行纠正的原因，`Err` 只留给实现级故障；且全部执行都是真异步
（`tokio::fs` / `tokio::process`，阻塞遍历包进 `spawn_blocking`）。本 crate
是这一层的接缝枢纽——`wavecode-mcp`、`wavecode-memory`、`wavecode-skills`
都实现它的 `Tool` trait——而它自己向下触及 `wavecode-sandbox`（策略词汇）、
`wavecode-context`（spill 存储根）、`infrastructure-base`（shell 解析）与
`action-tasks`（委派）。Schema 校验、钩子与权限审批由 core 侧编排，
不在这里。
