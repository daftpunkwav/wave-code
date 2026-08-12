//! 事件渲染：exec 与 REPL 共用。
//!
//! 两种输出：
//! - [`render_jsonl`]：事件 → 单行 JSON（`exec --json` 的 stdout 契约）；
//! - [`HumanRenderer`]：人类可读渲染状态机（delta 进缓冲、complete/中断时
//!   经 markdown 一次渲染；工具行/告警着色；等待动画帧由 tick 驱动）。
//!
//! 中断路径（M1-T7 审查结论）：`TurnCompleted{Interrupted}` 之前没有
//! `AgentMessageComplete` 与 `TokenCount`，渲染状态机不得假设每个 turn
//! 都有 TokenCount；Error 事件后 turn 也可能直接结束，渲染不得 panic。

use std::borrow::Cow;
use std::io::{self, Write};

use wavecode_protocol::{Event, StopReason};

/// 工具调用输入摘要的字符上限。
const TOOL_INPUT_MAX_CHARS: usize = 80;
/// 工具失败输出摘要的字符上限。
const TOOL_OUTPUT_MAX_CHARS: usize = 200;

/// 是否为需剥离的控制字符：C0（保留 `\n` / `\t`）、DEL、C1（U+0080–U+009F）。
fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// 终端输出净化：剥离 C0/C1 控制字符与 ESC 序列（保留 `\n`、`\t`），
/// 防模型 / 工具来源文本携带 ANSI / OSC 序列（清屏 `\x1b[2J`、OSC 52 写
/// 剪贴板、BEL 等）擦除工具调用痕迹——M1 无审批，该摘要是用户唯一的
/// 实时线索。无控制字符时零拷贝返回借用。
fn sanitize_terminal(s: &str) -> Cow<'_, str> {
    // 快路径：无需剥离的字符直接借用。
    if !s.chars().any(is_control) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            // ESC 序列整体跳过：CSI（ESC [ … 终字节 0x40–0x7E）、
            // OSC（ESC ] … 终止于 BEL 或 ESC \）、其余按 ESC+单字符。
            match it.next() {
                Some('[') => {
                    for c in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for c in it.by_ref() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\x1b' {
                            // 假定 ST（ESC \）：多吞一字符（截断的 OSC 同样
                            // 保守剥离，ESC 绝不进终端）。
                            it.next();
                            break;
                        }
                    }
                }
                // ESC+单字符序列（含孤立 ESC \）：跳过的字符已消费。
                _ => {}
            }
            continue;
        }
        if !is_control(c) {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// 事件 → 单行 JSON（JSONL 契约：每行一个完整 Event，无内嵌换行）。
pub fn render_jsonl(ev: &Event) -> String {
    // Event 全字段可序列化，to_string 不会失败。
    serde_json::to_string(ev).expect("Event 序列化不会失败")
}

/// 工具调用开始的人类可读摘要：`▸ {tool}` 亮青，input JSON 摘要暗灰（≤80字符）。
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
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// 终端宽度：terminal_size 不可用时回退 80
fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

/// 工具名/波形主题色（亮青）
fn theme_tool() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::BrightCyan)))
}

/// 弱化文本（input 摘要、tokens 行）
fn theme_dim() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::BrightBlack)))
}

/// 警告（黄）
fn theme_warn() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Yellow)))
}

/// 错误（红）
fn theme_err() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)))
}

/// 人类渲染状态机：delta 进缓冲，complete / 中断 / 工具行 / 告警前经
/// markdown 一次渲染；等待动画由 main 的 tick 驱动（`tick_frame`）。
///（REPL / exec 默认 stdout；`exec --json` 时为 stderr，保证 stdout 纯 JSONL）。
pub struct HumanRenderer<W: Write> {
    out: W,
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
fn human_task_begin(input: &serde_json::Value) -> String {
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
        write!(self.out, "\r{}", crate::wave::frame(14, self.phase))?;
        self.out.flush()?;
        self.indicator_on = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 去掉 ANSI 序列，便于断言可见文本（测试辅助）
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // 跳过 CSI：ESC [ ... 终字节 @-~
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('@'..='~').contains(&c2) {
                            break;
                        }
                    }
                    continue;
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn jsonl_one_event_per_line() {
        let ev = wavecode_protocol::Event {
            id: "s-1".into(),
            msg: wavecode_protocol::EventMsg::AgentMessageDelta { text: "hi".into() },
        };
        let line = render_jsonl(&ev);
        assert!(line.starts_with(r#"{"id":"s-1","msg":{"type":"agent_message_delta""#));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn human_render_tool_call_truncates_input() {
        let long = "x".repeat(200);
        let s = human_tool_begin("write_file", &serde_json::json!({"content": long}));
        // T5 起输出带 ANSI 样式：strip 后断言可见字符数
        assert!(strip(&s).chars().count() <= 100);
    }

    /// 多字节 UTF-8 输入按字符截断（非字节）：不切断码点、不 panic，
    /// 结果在上限内且以 `…` 收尾。
    #[test]
    fn truncate_multibyte_utf8_by_chars() {
        // CJK（3 字节码点）。
        let t = truncate_chars(&"汉".repeat(200), 80);
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
        // emoji（4 字节码点）与 CJK 混合。
        let t = truncate_chars(&"🦀汉".repeat(100), 80);
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
        // 短于上限原样返回。
        assert_eq!(truncate_chars("短", 80), "短");
    }

    /// P3：压缩事件渲染为弱化提示行（开始 / 完成带摘要 token 数）。
    #[test]
    fn compact_events_render_dim_lines() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::CompactStarted {
            trigger: wavecode_protocol::CompactTrigger::Manual,
        }))
        .unwrap();
        r.handle(&ev(M::CompactCompleted { summary_tokens: 42 }))
            .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("⟳ 正在压缩上下文"), "缺少开始行: {out:?}");
        assert!(
            out.contains("✓ 上下文已压缩（摘要 42 tokens）"),
            "缺少完成行: {out:?}"
        );
    }

    /// P2：审批请求渲染为黄色提示行（detail 经 sanitize 防终端注入）。
    #[test]
    fn approval_requested_renders_warning_line() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = wavecode_protocol::Event {
            id: "s-1".into(),
            msg: wavecode_protocol::EventMsg::ApprovalRequested {
                call_id: "c1".into(),
                kind: wavecode_protocol::ApprovalKind::Exec,
                detail: "shell: rm -rf build/\x1b[2J".into(),
            },
        };
        r.handle(&ev).unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(
            out.contains("⚠ 审批请求（执行命令）"),
            "应有审批提示行: {out:?}"
        );
        assert!(out.contains("shell: rm -rf build/"));
        assert!(!out.contains("\x1b[2J"), "注入序列应被剥离: {out:?}");
    }

    /// 中断路径（M1-T7 审查结论）：TurnCompleted{Interrupted} 前没有
    /// AgentMessageComplete / TokenCount —— 渲染不得假设有 tokens 行，
    /// 且须打印（已中断）标记、先补换行（delta 是裸 print!）。
    #[test]
    fn interrupted_turn_renders_without_tokens() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t-1".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta { text: "hi".into() }))
            .unwrap();
        r.handle(&ev(M::TurnCompleted {
            stop_reason: wavecode_protocol::StopReason::Interrupted,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("（已中断）"), "缺少中断标记: {out:?}");
        assert!(
            !out.contains("tokens:"),
            "无 TokenCount 不应打印 tokens 行: {out:?}"
        );
        // delta 之后必须先补换行再输出标记。
        assert!(out.contains("hi\n"), "TurnCompleted 前未补换行: {out:?}");
    }

    /// 正常路径：tokens 行取本 turn 最近一次 TokenCount。
    #[test]
    fn completed_turn_prints_latest_token_count() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t-1".into(),
        }))
        .unwrap();
        r.handle(&ev(M::TokenCount {
            used: 100,
            window: 200_000,
        }))
        .unwrap();
        r.handle(&ev(M::TokenCount {
            used: 120,
            window: 200_000,
        }))
        .unwrap();
        r.handle(&ev(M::TurnCompleted {
            stop_reason: wavecode_protocol::StopReason::Completed,
        }))
        .unwrap();
        let out = String::from_utf8_lossy(&r.out).into_owned();
        assert!(
            out.contains("tokens: 120/200000"),
            "tokens 行应取最近一次: {out:?}"
        );
        assert!(!out.contains("（已中断）"));
        // 下一个 turn 开始前状态已清理：无 TokenCount 的 turn 不残留上一 turn 数据。
        r.handle(&ev(M::TurnStarted {
            turn_id: "t-2".into(),
        }))
        .unwrap();
        r.handle(&ev(M::TurnCompleted {
            stop_reason: wavecode_protocol::StopReason::Completed,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert_eq!(out.matches("tokens:").count(), 1, "tokens 行残留: {out:?}");
    }

    /// 工具失败输出截断到 200 字符以内并带 ✗ 前缀；成功不输出。
    #[test]
    fn tool_call_end_failure_truncates_output() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::ToolCallEnd {
            call_id: "c1".into(),
            ok: true,
            output: "fine".into(),
        }))
        .unwrap();
        let long = "e".repeat(300);
        r.handle(&ev(M::ToolCallEnd {
            call_id: "c1".into(),
            ok: false,
            output: long,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        // T5 起 ✗ 行带红色样式：strip 后再断言
        let line = strip(out.lines().next().unwrap_or(""));
        assert!(line.starts_with('✗'), "失败输出应带 ✗ 前缀: {out:?}");
        // ✗(3字节) + 空格 + 199字符 + …(3字节) = 206 字节上限，且不含完整 300 字符。
        assert!(line.len() <= 206, "输出未截断: {} 字节", line.len());
        assert!(!out.contains(&"e".repeat(300)));
    }

    /// sanitize 单元面：CSI / OSC / BEL 剥离；正常中文 / emoji / 换行 /
    /// 制表符不受影响；无控制字符时零拷贝借用。
    #[test]
    fn sanitize_strips_control_sequences() {
        // CSI：清屏、带参数的颜色序列。
        assert_eq!(sanitize_terminal("a\x1b[2Jb"), "ab");
        assert_eq!(sanitize_terminal("\x1b[1;31m红\x1b[0m"), "红");
        // OSC：52 写剪贴板（BEL 与 ESC \ 两种终止形态）。
        assert_eq!(sanitize_terminal("x\x1b]52;;cGF5bG9hZA==\x07y"), "xy");
        assert_eq!(sanitize_terminal("x\x1b]0;title\x1b\\y"), "xy");
        // 孤立 BEL 与其他 C0（\n \t 除外）。
        assert_eq!(sanitize_terminal("p\x07q\x08r"), "pqr");
        // C1 控制字符（U+0080–U+009F，UTF-8 双字节形态）：字符本身剥离，
        // 其后参数字节按普通文本留存（终端已无法解释为序列）。
        assert_eq!(sanitize_terminal("a\u{9b}1;31mb"), "a1;31mb");
        // 正常文本不受影响；无控制字符走 Borrowed 零拷贝路径。
        let s = "正常中文🦀\n换行\t制表符";
        let sanitized = sanitize_terminal(s);
        assert_eq!(sanitized, s);
        assert!(matches!(sanitized, Cow::Borrowed(_)), "应零拷贝借用");
    }

    /// 渲染路径面：模型 / 工具来源文本中的控制序列不得进终端输出——
    /// delta、工具失败摘要、Warning / Error 四处应用点逐一锁定。
    #[test]
    fn render_strips_escape_sequences() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::AgentMessageDelta {
            text: "hi\x1b[2J\x07".into(),
        }))
        .unwrap();
        r.handle(&ev(M::ToolCallEnd {
            call_id: "c1".into(),
            ok: false,
            output: "boom\x1b]52;;cGF5bG9hZA==\x07".into(),
        }))
        .unwrap();
        r.handle(&ev(M::Warning {
            message: "warn\x1b[2J".into(),
        }))
        .unwrap();
        r.handle(&ev(M::Error {
            message: "err\x07".into(),
            recoverable: false,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        // T5 起渲染自行注入样式（含 ESC）：改为断言"注入的载荷"未进入输出
        assert!(!out.contains("\x1b[2J"), "注入的 CSI 进入输出: {out:?}");
        assert!(
            !out.contains("cGF5bG9hZA=="),
            "注入的 OSC 载荷进入输出: {out:?}"
        );
        assert!(!out.contains('\x07'), "输出含 BEL: {out:?}");
        assert!(out.contains("hi"), "正常文本被误剥: {out:?}");
        assert!(out.contains("boom"), "正常文本被误剥: {out:?}");
    }

    /// P4：todo_write 渲染清单状态符号与状态迁移标注（pending→completed 等）。
    #[test]
    fn todo_write_renders_status_migration() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |todos: serde_json::Value| wavecode_protocol::Event {
            id: "s-1".into(),
            msg: wavecode_protocol::EventMsg::ToolCallBegin {
                call_id: "c".into(),
                tool: "todo_write".into(),
                input: serde_json::json!({"todos": todos}),
            },
        };
        r.handle(&ev(serde_json::json!([
            {"content": "设计", "status": "in_progress"},
            {"content": "实现", "status": "pending"}
        ])))
        .unwrap();
        r.handle(&ev(serde_json::json!([
            {"content": "设计", "status": "completed"},
            {"content": "实现", "status": "pending"}
        ])))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("▸ todo_write"));
        assert!(out.contains("▸ 设计"), "首轮 in_progress 符号: {out:?}");
        assert!(out.contains("☐ 实现"), "pending 符号: {out:?}");
        assert!(
            out.contains("✓ 设计") && out.contains("（in_progress → completed）"),
            "状态迁移标注: {out:?}"
        );
    }

    /// P5：task 工具行展示子代理类型与描述（后台形态附标记）；子代理
    /// 起止事件渲染为弱化提示行。
    #[test]
    fn task_tool_and_subagent_events_render() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::ToolCallBegin {
            call_id: "c".into(),
            tool: "task".into(),
            input: serde_json::json!({
                "description": "调查认证模块",
                "prompt": "…",
                "subagent_type": "explore",
                "run_in_background": true
            }),
        }))
        .unwrap();
        r.handle(&ev(M::SubagentStarted {
            task_id: "task-1".into(),
            subagent_type: "explore".into(),
            description: "调查认证模块".into(),
        }))
        .unwrap();
        r.handle(&ev(M::SubagentCompleted {
            task_id: "task-1".into(),
            status: wavecode_protocol::SubagentStatus::Completed,
            summary: "结论".into(),
        }))
        .unwrap();
        let out = strip(&String::from_utf8(r.out).unwrap());
        assert!(out.contains("▸ task"), "task 工具行: {out:?}");
        assert!(out.contains("explore") && out.contains("调查认证模块"));
        assert!(out.contains("（后台）"), "后台标记: {out:?}");
        assert!(
            out.contains("⏚ 子代理 task-1 启动（explore）调查认证模块"),
            "启动行: {out:?}"
        );
        assert!(out.contains("✓ 子代理 task-1 完成"), "完成行: {out:?}");
        // 默认类型与无后台标记的回退形态。
        let line = human_task_begin(&serde_json::json!({"description": "d"}));
        let line = strip(&line);
        assert!(line.contains("general-purpose"));
        assert!(!line.contains("（后台）"));
    }

    /// T5：delta 进缓冲不直接输出；complete 时一次性经 markdown 渲染
    ///（记号被渲染掉、注入样式）。
    #[test]
    fn delta_is_buffered_until_complete_then_rendered_markdown() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta {
            text: "**你好**".into(),
        }))
        .unwrap();
        assert!(r.out.is_empty(), "delta 不得直接输出");
        r.handle(&ev(M::AgentMessageComplete {
            text: "**你好**".into(),
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("你好"));
        assert!(!out.contains("**"), "markdown 记号应被渲染掉：{out}");
        assert!(out.contains("\x1b["), "应有样式：{out}");
    }

    /// T5：中断 turn 的残余缓冲照常渲染，附（已中断）且无 tokens 行。
    #[test]
    fn interrupted_turn_renders_residual_buffer() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta {
            text: "写了一半".into(),
        }))
        .unwrap();
        r.handle(&ev(M::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("写了一半"), "中断残余应渲染：{out}");
        assert!(out.contains("（已中断）"));
        assert!(!out.contains("tokens:"), "中断无 tokens 行");
    }

    /// T5：工具开始/失败行着色（`▸ 工具名` 同色段、`✗` 红）。
    #[test]
    fn tool_lines_are_colored() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.handle(&ev(M::ToolCallBegin {
            call_id: "1".into(),
            tool: "write_file".into(),
            input: serde_json::json!({"path":"a.txt"}),
        }))
        .unwrap();
        r.handle(&ev(M::ToolCallEnd {
            call_id: "1".into(),
            ok: false,
            output: "permission denied".into(),
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("▸ write_file"));
        assert!(out.contains("✗ permission denied"));
        assert!(out.contains("\x1b["), "工具行应着色：{out}");
    }

    /// T5：非 animate 时 tick_frame 为 no-op；turn 内处于等待模型状态。
    #[test]
    fn tick_frame_noop_when_not_animate() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        assert!(r.is_waiting_on_model());
        r.tick_frame().unwrap();
        assert!(r.out.is_empty(), "非 animate 时 tick 为 no-op");
    }

    /// animate=true 时 Complete 先清波形指示再渲染消息（flush_message 内聚清除）。
    #[test]
    fn complete_clears_indicator_before_rendering() {
        let mut r = HumanRenderer::new(Vec::new(), true);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.tick_frame().unwrap(); // 波形上屏（indicator_on = true）
        r.handle(&ev(M::AgentMessageDelta {
            text: "正文".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageComplete {
            text: "正文".into(),
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        let clear = out.find("\x1b[K").expect("应有清行序列：{out}");
        let msg = out.find("正文").expect("消息应渲染：{out}");
        assert!(clear < msg, "消息应在波形清除之后输出：{out:?}");
    }

    /// 交错时序回归锁：delta → 工具行 → delta → complete，输出保持
    /// 前半 → 工具行 → 后半 的可读顺序（工具行前先把半截消息渲染掉）。
    #[test]
    fn interleaved_delta_tool_delta_keeps_order() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta {
            text: "前半".into(),
        }))
        .unwrap();
        r.handle(&ev(M::ToolCallBegin {
            call_id: "1".into(),
            tool: "read_file".into(),
            input: serde_json::json!({}),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta {
            text: "后半".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageComplete {
            text: "后半".into(),
        }))
        .unwrap();
        let out = strip(&String::from_utf8(r.out).unwrap());
        let first = out.find("前半").expect("前半应输出：{out}");
        let tool = out.find("▸ read_file").expect("工具行应输出：{out}");
        let second = out.find("后半").expect("后半应输出：{out}");
        assert!(first < tool && tool < second, "时序错乱：{out}");
    }
}
