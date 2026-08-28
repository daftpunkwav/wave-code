//! 人类可读渲染（阶段 4 拆分自 render.rs）：HumanRenderer 状态机与各类
//! human_* 行渲染（工具/todo/task/事件）。

use super::*;
use crate::render::sanitize::sanitize_terminal;
use crate::render::theme::{
    terminal_width, theme_dim, theme_err, theme_tool, theme_warn, truncate_chars,
};

pub fn human_tool_begin(tool: &str, input: &serde_json::Value) -> String {
    // 工具来源文本同样须 sanitize（防注入擦除痕迹）；`▸ 工具名` 同一色段，
    // 保证 strip 后仍连续可读。
    let summary = truncate_chars(&sanitize_terminal(&input.to_string()), TOOL_INPUT_MAX_CHARS);
    let accent = theme_tool();
    let dim = theme_dim();
    format!(
        "{}▸ {}{} {}{}{}",
        accent.render(),
        tool,
        accent.render_reset(),
        dim.render(),
        summary,
        dim.render_reset(),
    )
}

/// 工具失败输出摘要：`✗ {output ≤200字符}` 红色。
fn human_tool_error(output: &str) -> String {
    let summary = truncate_chars(&sanitize_terminal(output), TOOL_OUTPUT_MAX_CHARS);
    let err = theme_err();
    format!("{}✗ {}{}", err.render(), summary, err.render_reset())
}

/// 按字符数截断（非字节，防切断 UTF-8），超长时末位替换为省略号 `…`。
/// 人类渲染状态机：delta 进缓冲，complete / 中断 / 工具行 / 告警前经
/// markdown 一次渲染；等待动画由 main 的 tick 驱动（`tick_frame`）。
///（REPL / exec 默认 stdout；`exec --json` 时为 stderr，保证 stdout 纯 JSONL）。
pub struct HumanRenderer<W: Write> {
    pub(crate) out: W,
    /// 等待动画开关（human 模式 && TTY）
    animate: bool,
    /// 当前助手消息缓冲（delta 累积，complete/中断时渲染）
    msg_buf: String,
    /// 本 turn 最近一次 TokenCount（用于 tokens 行；中断的 turn 可能整个没有）
    last_usage: Option<(u64, u64)>,
    /// 波形相位（tick_frame 推进）
    phase: f32,
    /// 等待指示当前是否显示在终端上（下次输出前需 \r\x1b[K 清除）
    indicator_on: bool,
    /// 是否处于 turn 内
    in_turn: bool,
    /// 最近一次 todo_write 展示的清单（content, status），用于渲染状态迁移（P4）
    last_todos: Vec<(String, String)>,
}

/// todo 状态符号（对齐渲染风格：✓ 完成、▸ 进行中、☐ 待办）。
fn todo_symbol(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "in_progress" => "▸",
        _ => "☐",
    }
}

/// todo_write 的人类可读展示（P4）：逐行渲染新清单的状态符号；与上次
/// 清单同名条目状态变化时附 `（旧 → 新）` 弱化标注，呈现清单状态迁移。
fn human_todo_begin(input: &serde_json::Value, last_todos: &[(String, String)]) -> String {
    let accent = theme_tool();
    let dim = theme_dim();
    let mut out = format!("{}▸ todo_write{}", accent.render(), accent.render_reset());
    let empty = Vec::new();
    let items = input
        .get("todos")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    for item in items {
        let content = item.get("content").and_then(|c| c.as_str()).unwrap_or("");
        let status = item
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending");
        let transition = match last_todos.iter().find(|(c, _)| c == content) {
            Some((_, old)) if old != status => format!("（{old} → {status}）"),
            _ => String::new(),
        };
        out.push_str(&format!(
            "\n  {} {}{}{}{}",
            todo_symbol(status),
            sanitize_terminal(content),
            dim.render(),
            transition,
            dim.render_reset()
        ));
    }
    out
}

/// 从 todo_write 输入提取清单状态（content, status），供下次渲染比对。
fn parse_todo_input(input: &serde_json::Value) -> Vec<(String, String)> {
    input
        .get("todos")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .map(|i| {
                    (
                        i.get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_owned(),
                        i.get("status")
                            .and_then(|s| s.as_str())
                            .unwrap_or("pending")
                            .to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// task 工具的人类可读展示（P5）：子代理类型 + 任务描述（后台形态附标记）。
pub(crate) fn human_task_begin(input: &serde_json::Value) -> String {
    let accent = theme_tool();
    let dim = theme_dim();
    let subagent_type = input
        .get("subagent_type")
        .and_then(|t| t.as_str())
        .unwrap_or("general-purpose");
    let description = input
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let background = input
        .get("run_in_background")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let bg_label = if background { "（后台）" } else { "" };
    let kind = format!("{}{}", sanitize_terminal(subagent_type), bg_label);
    format!(
        "{}▸ task{} {}{}{} {}",
        accent.render(),
        accent.render_reset(),
        dim.render(),
        kind,
        dim.render_reset(),
        sanitize_terminal(description),
    )
}

impl<W: Write> HumanRenderer<W> {
    pub fn new(out: W, animate: bool) -> Self {
        Self {
            out,
            animate,
            msg_buf: String::new(),
            last_usage: None,
            phase: 0.0,
            indicator_on: false,
            in_turn: false,
            last_todos: Vec::new(),
        }
    }

    /// 是否处于 turn 内（等待模型产出）：main 的 tick 据此决定是否重绘
    pub fn is_waiting_on_model(&self) -> bool {
        self.in_turn
    }

    /// 渲染单个事件；IO 错误向上传播（如管道关闭）。
    pub fn handle(&mut self, ev: &Event) -> io::Result<()> {
        use wavecode_protocol::EventMsg::*;
        match &ev.msg {
            TurnStarted { .. } => {
                self.in_turn = true;
                self.msg_buf.clear();
                self.last_usage = None;
                self.phase = 0.0;
            }
            // delta 先剥控制字符（防终端注入）再进缓冲，不直接打印。
            AgentMessageDelta { text } => {
                let clean = sanitize_terminal(text);
                self.msg_buf.push_str(&clean);
            }
            AgentMessageComplete { .. } => self.flush_message()?,
            // 工具行实时打印；即便有半截消息缓冲也先渲染掉，保持时序可读。
            // todo_write 展示清单状态迁移（P4），并记录清单供下次比对；
            // task 展示子代理类型与描述（P5）。
            ToolCallBegin { tool, input, .. } => {
                self.flush_message()?;
                if tool == "todo_write" {
                    writeln!(self.out, "{}", human_todo_begin(input, &self.last_todos))?;
                    self.last_todos = parse_todo_input(input);
                } else if tool == "task" {
                    writeln!(self.out, "{}", human_task_begin(input))?;
                } else {
                    writeln!(self.out, "{}", human_tool_begin(tool, input))?;
                }
            }
            // 仅失败回显输出摘要；成功保持安静。
            ToolCallEnd {
                ok: false, output, ..
            } => {
                self.flush_message()?;
                writeln!(self.out, "{}", human_tool_error(output))?;
            }
            ToolCallEnd { .. } => {}
            TokenCount { used, window } => {
                self.last_usage = Some((*used, *window));
            }
            // 压缩事件（P3）：弱化提示行，对齐 tokens 行风格。
            CompactStarted { .. } => {
                self.flush_message()?;
                let dim = theme_dim();
                writeln!(
                    self.out,
                    "{}⟳ 正在压缩上下文…{}",
                    dim.render(),
                    dim.render_reset()
                )?;
            }
            CompactCompleted { summary_tokens } => {
                self.flush_message()?;
                let dim = theme_dim();
                writeln!(
                    self.out,
                    "{}✓ 上下文已压缩（摘要 {summary_tokens} tokens）{}",
                    dim.render(),
                    dim.render_reset()
                )?;
            }
            // 审批请求（P2）：黄色提示行；实际问答由 main 的审批处理完成
            //（REPL 内联提示 y/n；exec 非交互自动拒绝）。
            ApprovalRequested { kind, detail, .. } => {
                self.flush_message()?;
                let warn = theme_warn();
                let kind_label = match kind {
                    wavecode_protocol::ApprovalKind::Exec => "执行命令",
                    _ => "写入文件",
                };
                writeln!(
                    self.out,
                    "{}⚠ 审批请求（{kind_label}）：{}{}",
                    warn.render(),
                    sanitize_terminal(detail),
                    warn.render_reset()
                )?;
            }
            // 子代理起止（P5）：弱化提示行，对齐压缩事件风格；子代理
            // 中间过程不进父会话事件流（上下文隔离），前端只见起止。
            SubagentStarted {
                task_id,
                subagent_type,
                description,
            } => {
                self.flush_message()?;
                let dim = theme_dim();
                writeln!(
                    self.out,
                    "{}⏚ 子代理 {task_id} 启动（{}）{}{}",
                    dim.render(),
                    sanitize_terminal(subagent_type),
                    sanitize_terminal(description),
                    dim.render_reset()
                )?;
            }
            SubagentCompleted {
                task_id, status, ..
            } => {
                self.flush_message()?;
                let dim = theme_dim();
                let label = match status {
                    wavecode_protocol::SubagentStatus::Completed => "完成",
                    wavecode_protocol::SubagentStatus::Failed => "失败",
                    wavecode_protocol::SubagentStatus::Stopped => "已停止",
                    _ => "结束",
                };
                writeln!(
                    self.out,
                    "{}✓ 子代理 {task_id} {label}{}",
                    dim.render(),
                    dim.render_reset()
                )?;
            }
            Warning { message } | Error { message, .. } => {
                self.flush_message()?;
                let style = if matches!(ev.msg, Warning { .. }) {
                    theme_warn()
                } else {
                    theme_err()
                };
                writeln!(
                    self.out,
                    "{}{}{}",
                    style.render(),
                    sanitize_terminal(message),
                    style.render_reset()
                )?;
            }
            TurnCompleted { stop_reason } => {
                self.flush_message()?; // 中断路径的残余缓冲
                writeln!(self.out)?;
                if *stop_reason == StopReason::Interrupted {
                    let warn = theme_warn();
                    writeln!(
                        self.out,
                        "{}（已中断）{}",
                        warn.render(),
                        warn.render_reset()
                    )?;
                // 仅本 turn 见过 TokenCount 才打印 tokens 行（take 顺带清理状态）。
                } else if let Some((used, window)) = self.last_usage.take() {
                    let dim = theme_dim();
                    writeln!(
                        self.out,
                        "{}tokens: {used}/{window}{}",
                        dim.render(),
                        dim.render_reset()
                    )?;
                }
                self.in_turn = false;
            }
            // EventMsg 标注 non_exhaustive：未来新增事件 M1 不渲染。
            _ => {}
        }
        Ok(())
    }

    /// 渲染缓冲消息（markdown）并清空；空缓冲 no-op。
    /// 内容输出路径统一在此先清除等待指示（幂等），调用方无需记配对。
    fn flush_message(&mut self) -> io::Result<()> {
        self.clear_indicator()?;
        if self.msg_buf.is_empty() {
            return Ok(());
        }
        let text = std::mem::take(&mut self.msg_buf);
        let width = terminal_width();
        write!(
            self.out,
            "{}",
            crate::markdown::render_markdown(&text, width)
        )?;
        self.out.flush()
    }

    /// 清除等待指示行（若显示中）
    fn clear_indicator(&mut self) -> io::Result<()> {
        if self.indicator_on {
            write!(self.out, "\r\x1b[K")?;
            self.indicator_on = false;
        }
        Ok(())
    }

    /// 等待动画一帧（main 的 80ms tick 驱动）
    pub fn tick_frame(&mut self) -> io::Result<()> {
        if !self.animate || !self.in_turn {
            return Ok(());
        }
        self.phase += 0.35;
        write!(self.out, "\r{}", crate::banner::frame(14, self.phase))?;
        self.out.flush()?;
        self.indicator_on = true;
        Ok(())
    }
}
