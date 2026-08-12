---
type: concept
title: 命令行入口（wavecode-cli）
description: wavecode 二进制、exec/REPL 模式、启动装配链、人类可读渲染与 markdown 终端渲染。
tags: [cli, repl, rendering, markdown]
---

# 命令行入口（wavecode-cli）

## 职责

`wavecode-cli` 是单二进制入口（`crates/cli/src/main.rs`，bin 名 `wavecode`）。M1 命令面：

- `wavecode`（无子命令）：基础交互 REPL（rustyline，流式渲染）；
- `wavecode exec "<prompt>"`：非交互单 turn；`--json` 时 stdout 输出 JSONL（每行一个 Event），人类可读渲染转 stderr；
- `wavecode --model <name>` / `--config <path>`：覆盖 config 项（`--model` 覆盖 config.model，`--config` 指定配置文件）。

TUI / app-server / mcp / login 等子命令随后续里程碑落地。模块：`bootstrap`（装配）、`render`（事件渲染）、`markdown`（终端 markdown 渲染）、`wave`（波形动画）。

## 启动装配（`bootstrap.rs`）

链路：`Config::load`（或 `--config` 指定）→ `resolve_provider`（[config](../engine/config.md)）→ `AnthropicClient::new(base_url, api_key)` → `SessionConfig { model_name: --model 覆盖 config.model, context_window, max_output_tokens, registry: Registry::builtin(), cwd: current_dir(), deny_env: env_key 注入 }`。

- `BootError`：`Config(ConfigError)`（退出码 2——配置缺失/解析失败/provider 或 api key 未定义）与 `Cwd(std::io::Error)`（退出码 1）须可区分。
- 配置缺失时打印中文指引 + `~/.wavecode/config.toml` 内容模板（env_key 方式推荐 + 内联 api_key 二选一）。
- **deny_env 注入**：config 的 `env_key` 自定义名（如 `MINIMAX_KEY`——敏感后缀模式挡不住的形态）注入 `SessionConfig.deny_env`，经 [core](../engine/core.md) 透传 `ToolCtx`（shell 工具的 env 剔除依赖此通道）；未配/空串则空。
- **http 明文警告**：base_url 为 `http://` 且目标非 loopback（localhost / 127.0.0.1 / ::1 / IPv6 字面量剥离端口判定）时 stderr 警告 api key 将明文传输；前缀形似 loopback 的域名（`127.0.0.1.evil.example.com`）不算 loopback。
- **日志（`init_tracing`）**：`tracing_subscriber` 走 stderr（stdout 留给 JSONL / 渲染输出），默认级别 `off`（用户侧错误经事件流呈现），`RUST_LOG` 可覆盖（T10 冒烟用 `RUST_LOG=off` 验证兼容）。

## exec 契约

- 退出码：`TurnCompleted{Completed}` 或断管（下游如 `| head` 提前关闭，用户已取走所需输出）→ 0；其余 stop_reason / 事件流意外结束 → 1。
- `--json`：stdout 只写 JSONL（每行一个完整 Event，无内嵌换行，flush 保证管道下游流式可见）；人类渲染转 stderr；不插入等待动画 tick。JSONL 输出走 `writeln!` + 显式 flush（`println!` 会在 EPIPE panic）；BrokenPipe 特判为干净结束。
- human 模式：stdout（anstream 按 TTY 自动去色/剥离），等待动画仅 TTY 开启。
- 结束前 submit `Op::Shutdown` 优雅关闭（不阻塞等待）；client 析构另有 abort 兜底。

## REPL

- 启动横幅：TTY 时播放 **12 帧滚动波形动画**（80ms/帧，相位步进 0.35，末帧相位定格横幅实现视觉无缝衔接）后打印横幅（品牌 WaveCode + 版本 + 模型 · cwd + 提示文案）；非 TTY 直接静态横幅（anstream 自动去色）。
- 提示符：亮青波形符 `∿ `；`/quit` / `/exit` 退出；空行跳过；Ctrl-D（Eof）退出；Ctrl-C（Interrupted）放弃当前输入行；历史经 `add_history_entry`。
- readline 是同步阻塞调用，只在无活动 turn（actor 空闲）时调用（未来支持 turn 中并发操作前须迁 spawn_blocking）。
- `consume_turn`：`select!` over `next_event()` 与 **80ms tick**（`MissedTickBehavior::Skip`——事件密集时丢弃积压 tick 不补帧）；`tick_frame` 内部按 animate/in_turn 自律，`--json` 不插入 tick。
- actor 意外死亡（事件流提前结束未收到 TurnCompleted）：补换行提示"会话已终止（agent 引擎意外退出）"后退出 REPL。

## 人类可读渲染（`render.rs`）

`HumanRenderer<W: Write>` 状态机：delta 经 sanitize 进缓冲，`AgentMessageComplete` / 中断时经 markdown **一次渲染**；工具行/告警实时打印（有半截消息缓冲也先渲染掉保持时序可读）。

- 事件映射：`TurnStarted` 置 in_turn/清缓冲；`AgentMessageDelta` sanitize 后入缓冲（不直接打印）；`AgentMessageComplete` flush_message；`ToolCallBegin` → `▸ {工具名亮青} {input摘要≤80字符暗灰}`；`ToolCallEnd{ok:false}` → `✗ {output≤200字符}` 红（成功安静）；`TokenCount` 记录本 turn 最近一次；`Warning` 黄 / `Error` 红；`TurnCompleted{Interrupted}` → `（已中断）` 黄；`TurnCompleted` 打印 tokens 行（仅本 turn 见过 TokenCount；**中断的 turn 可能整个没有 TokenCount**，渲染不得假设，也不得 panic）。
- **`sanitize_terminal`**：剥离 C0/C1 控制字符与 ESC 序列（保留 `\n`/`\t`）——防模型/工具来源文本携带 ANSI/OSC 序列（清屏 `\x1b[2J`、OSC 52 写剪贴板、BEL 等）擦除工具调用痕迹（M1 无审批，该摘要是用户唯一的实时线索）。快路径无控制字符时零拷贝借用返回。所有 sink（消息、工具行、告警、token 行）都走它。
- `truncate_chars`：按字符截断（非字节，防切断 UTF-8），超长末位替换 `…`；terminal_width 取 `terminal_size`，不可用回退 80。

## markdown 终端渲染（`markdown.rs`）

`render_markdown(input, term_width)`：pulldown-cmark（ENABLE_TABLES + ENABLE_STRIKETHROUGH）事件驱动渲染，纯函数、输出恰好以单个 `\n` 结尾；**term_width 钳制下限 20**（`term_width.max(20)`，表格预算同源）；ANSI 启用/剥离由输出侧 anstream 决定。支持的构造与样式：

- 标题（亮青加粗）、粗/斜/删（Inline 状态叠加）、行内码（黄）、链接（蓝下划线，URL 不附加）；
- 代码块：`│ ` 边线（`code_line_start` 每行前缀；**空行有意不加前缀**，保持零字符避免尾随空格）、水平线（`─` × min(width,40)）、引用（quote_depth 前缀）、列表栈（有序编号 + 缩进，`• `；嵌套列表弹栈恢复层级）；
- **表格**（`TableBuilder`）：单元格宽度按 unicode-width（CJK=2 格）；行格式 `│ c │ … │`，每列 3 格开销（`│ ` + 尾部空格）+ 结尾 `│` 1 格（overhead = `3*cols+1`）；超终端宽度时按比例压缩 `w * avail / sum`，保底 `(avail / cols).clamp(1, MIN_COL_WIDTH)`（`MIN_COL_WIDTH = 4`，极端超窄时 floor 总和可能略超宽，注释明确接受）；表头下分隔线 `├─┼─┤`；右/居中对齐按声明 pad（居中余量给右侧）；超宽单元格按 span 感知截断（宽字符预算补足，`rest` 剩余转填充，截断行仍填满 target）并以 `…` 收尾；未闭合表格在 `finish()` 兜底出表（模型输出被截断场景）；
- html/footnote/task-list 标记等 M1 不渲染忽略。
- 已知缺陷：loose list（`- 甲\n\n- 乙\n`）中 Item 内段落触发 block_gap 会产生破损——修复前既有缺陷，代码注释明确"勿在此误改"。

## 波形品牌（`wave.rs`）

`frame(n, phase)`：正弦采样八级块字符（`▁▂▃▄▅▆▇█`）+ 青→蓝→紫 RGB 渐变（`palette`：三段线性插值，周期 3），每格带 RGB ANSI、帧尾 reset——**确定性纯函数**，启动横幅动画与等待指示共用。`banner(model, cwd, version, phase)` 组装横幅（phase 取启动动画末帧相位，视觉无缝衔接）。

## 聚焦测试

| 测试 | 位置 | 锁定的行为 |
|---|---|---|
| `model_override_wins` / `missing_config_is_config_boot_error` / `env_key_injected_into_deny_env` / `no_env_key_gives_empty_deny_env` / `insecure_http_detection` | bootstrap.rs | `--model` 覆盖、NotFound→BootError::Config、deny_env 注入、http 警告判定面 |
| `jsonl_one_event_per_line` / `human_render_tool_call_truncates_input` / `truncate_multibyte_utf8_by_chars` / `interrupted_turn_renders_without_tokens` / `completed_turn_prints_latest_token_count` | render.rs | JSONL 单行契约、摘要截断、CJK/emoji 按字符截断、中断渲染无 tokens 行、tokens 行取最近一次 |
| markdown 快照/回归测试：`heading_is_bold_cyan_without_hash`、`bold_and_strike_render_as_effects`、`inline_code_is_yellow`、`code_block_lines_have_bar_prefix`、`code_block_then_paragraph_keeps_blank_line`、`unordered_and_ordered_lists`、`nested_list_pops_stack`、`blockquote_has_bar`、`link_is_underlined_text_only`、`rule_is_dim_line`、`paragraph_spacing_preserved`、`table_ascii_aligned_columns`、`table_cjk_width_counts_two_cells`、`table_compresses_over_terminal_width`、`compressed_table_rows_stay_equal_width`、`table_alignment_right_and_center`、`unclosed_table_still_renders` | markdown.rs | 渲染器各构造正确性、CJK 等宽对齐、超宽压缩、截断行等宽回归、未闭合表格出表 |
| `frame_is_deterministic_and_phase_moves` / `frame_contains_block_chars_rgb_and_reset` / `palette_cycles_through_three_stops` / `banner_contains_brand_model_cwd` | wave.rs | 波形确定性、RGB/reset、调色板周期、横幅内容 |

## 已知边界与规划

- REPL 为"基础行式"（ratatui TUI 在 M2，见 [feature-crates](../planned/feature-crates.md) tui 条目）；M2 起 TUI 渲染同样必须走 sanitize（SPEC §17.5）。
- 冒烟通过记录：SPEC §15.5 渲染契约与代码完全对账（review.md §11.1）；T7 真实 API 冒烟（MiniMax 端点）通过。
- 规划：app-server / mcp / login / resume 子命令。

## 相关页面

- 服务面：[协议服务（wavecode-app-server）](../runtime/app-server.md)、[前后端协议（wavecode-protocol）](../protocol/protocol.md)
- 装配：[配置系统（wavecode-config）](../engine/config.md)、[模型抽象层（wavecode-llm）](../engine/llm.md)
- 引擎：[Agent 引擎（wavecode-core）](../engine/core.md)
