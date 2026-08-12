//! TUI 应用状态机：协议事件与键盘输入 → 界面状态 + 待发 [`Op`]。
//!
//! 全部迁移逻辑为纯函数形态（不进终端、不碰 client），单测直接驱动；
//! 事件渲染语义复用 SPEC §15.5 / cli render.rs：工具行 `▸`/`✗`、压缩
//! `⟳`/`✓`、审批 `⚠` 黄、子代理 `⏚`/`✓`、todo 清单 `☐▸✓` 与状态迁移
//! 标注、中断 `（已中断）`。delta 经 sanitize 入缓冲，complete / 中断时
//! 经 markdown 一次性渲染（流式期间消息流尾部追加纯文本预览）。

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use wavecode_protocol::{ApprovalDecision, ApprovalKind, Event, Op, PermissionMode, StopReason};

use crate::markdown::render_markdown;
use crate::text::{sanitize_terminal, truncate_chars};

/// 工具调用输入摘要的字符上限（与 cli 一致）。
const TOOL_INPUT_MAX_CHARS: usize = 80;
/// 工具失败输出摘要的字符上限（与 cli 一致）。
const TOOL_OUTPUT_MAX_CHARS: usize = 200;

/// 内置 slash 命令（补全候选与路由共用；skill 名由装配侧注入）。
const BUILTIN_COMMANDS: &[&str] = &["compact", "memory", "mcp", "permissions", "quit", "exit"];

/// turn 进行中的等待动画帧（状态栏，100ms tick 推进）。
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 主题色（与 cli 渲染同一色系）。
fn accent() -> Style {
    Style::default().fg(Color::LightCyan)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

fn err() -> Style {
    Style::default().fg(Color::Red)
}

/// TUI 启动上下文（装配侧 cli 从 SessionConfig 提取后传入——tui 不能
/// 依赖 core，凡 core 拥有的知识（记忆索引路径、skill 清单、初始权限
/// 模式）都经本结构注入）。
pub struct TuiContext {
    /// 模型名（状态栏）。
    pub model_name: String,
    /// 会话工作目录（状态栏）。
    pub cwd: PathBuf,
    /// 初始权限模式（`/permissions` 在此基础上循环）。
    pub permission_mode: PermissionMode,
    /// 持久记忆索引文件路径（`/memory` 读取面；无记忆能力时为 None）。
    pub memory_index_path: Option<PathBuf>,
    /// 可直调 skill 名清单（slash 补全候选与路由判定）。
    pub skill_names: Vec<String>,
    /// 已配置 MCP server 的状态行（P9，`/mcp` 展示面；core 预渲染，
    /// 首版状态恒为"未连接（transport 未实现）"）。空 = 未配置。
    pub mcp_server_lines: Vec<String>,
}

/// 消息流中的一个条目（已提交、不可变；行集含样式）。
pub struct Item {
    pub lines: Vec<Line<'static>>,
}

impl Item {
    fn plain(text: String, style: Style) -> Self {
        let lines = text
            .split('\n')
            .map(|l| Line::from(Span::styled(l.to_string(), style)))
            .collect();
        Self { lines }
    }

    fn user(text: &str) -> Self {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let lines = text
            .split('\n')
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 { "> " } else { "  " };
                Line::from(Span::styled(format!("{prefix}{l}"), bold))
            })
            .collect();
        Self { lines }
    }

    fn assistant(text: &str) -> Self {
        Self {
            lines: render_markdown(text),
        }
    }
}

/// todo 状态符号（与 cli 一致：✓ 完成、▸ 进行中、☐ 待办）。
fn todo_symbol(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "in_progress" => "▸",
        _ => "☐",
    }
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

/// 审批内联弹窗状态。
pub struct ApprovalPopup {
    pub call_id: String,
    pub kind: ApprovalKind,
    pub detail: String,
    /// false：y/n 选择态；true：拒绝原因录入态。
    pub reason_mode: bool,
    pub reason: String,
}

/// TUI 应用状态（事件与按键的唯一事实源）。
pub struct App {
    ctx: TuiContext,
    /// 已提交的消息流条目。
    pub items: Vec<Item>,
    /// 流式助手消息缓冲（delta 累积，complete / 工具行前提交）。
    msg_buf: String,
    /// 是否处于 turn 内。
    pub in_turn: bool,
    /// 输入框文本与光标（字符索引）。
    pub input: String,
    pub cursor: usize,
    /// 消息流滚动（行偏移；follow_tail 时由 ui 收敛到底部）。
    pub scroll: usize,
    pub follow_tail: bool,
    /// 审批弹窗（Some 时按键全部路由给弹窗）。
    pub approval: Option<ApprovalPopup>,
    /// Esc 手动关闭 slash 弹层（输入再变化时复位）。
    slash_dismissed: bool,
    slash_selected: usize,
    /// 最近一次 TokenCount（状态栏 used/window）。
    pub tokens: Option<(u64, u64)>,
    /// 当前权限模式（`/permissions` 循环后本地同步）。
    pub permission_mode: PermissionMode,
    /// 等待动画相位。
    pub spinner: usize,
    /// 待投递的协议 Op（run 循环取出后经 client 发送）。
    outbox: Vec<Op>,
    quit: bool,
    /// 最近一次 todo_write 展示的清单（渲染状态迁移用）。
    last_todos: Vec<(String, String)>,
}

impl App {
    pub fn new(ctx: TuiContext) -> Self {
        let permission_mode = ctx.permission_mode;
        let mut app = Self {
            ctx,
            items: Vec::new(),
            msg_buf: String::new(),
            in_turn: false,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow_tail: true,
            approval: None,
            slash_dismissed: false,
            slash_selected: 0,
            tokens: None,
            permission_mode,
            spinner: 0,
            outbox: Vec::new(),
            quit: false,
            last_todos: Vec::new(),
        };
        app.items.push(Item::plain(
            "WaveCode TUI — Enter 提交 · / 命令补全 · Esc 中断 · Ctrl-C 退出".into(),
            dim(),
        ));
        app
    }

    pub fn model_name(&self) -> &str {
        &self.ctx.model_name
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.ctx.cwd
    }

    /// 流式缓冲内容（ui 在消息流尾部追加纯文本预览）。
    pub fn streaming_buffer(&self) -> &str {
        &self.msg_buf
    }

    pub fn is_quit(&self) -> bool {
        self.quit
    }

    /// 取出全部待发 Op。
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.outbox)
    }

    /// 100ms tick：仅 turn 内推进等待动画相位。
    pub fn tick(&mut self) {
        if self.in_turn {
            self.spinner = (self.spinner + 1) % SPINNER.len();
        }
    }

    /// 事件流提前结束（actor 退出）：红色提示并退出。
    pub fn actor_died(&mut self) {
        self.items.push(Item::plain(
            "会话已终止（agent 引擎意外退出）".into(),
            err(),
        ));
        self.quit = true;
    }

    // ── 协议事件 ────────────────────────────────────────────────

    /// 协议事件 → 状态迁移（渲染语义对齐 cli render.rs 的 HumanRenderer）。
    pub fn handle_event(&mut self, ev: &Event) {
        use wavecode_protocol::EventMsg as M;
        match &ev.msg {
            M::TurnStarted { .. } => {
                self.in_turn = true;
                self.msg_buf.clear();
                self.spinner = 0;
            }
            // delta 先剥控制字符（防终端注入）再进缓冲。
            M::AgentMessageDelta { text } => {
                let clean = sanitize_terminal(text);
                self.msg_buf.push_str(&clean);
            }
            M::AgentMessageComplete { .. } => self.flush_message(),
            // 工具行实时提交；半截消息先渲染掉，保持时序可读。
            M::ToolCallBegin { tool, input, .. } => {
                self.flush_message();
                self.tool_begin_item(tool, input);
            }
            // 仅失败回显输出摘要；成功保持安静。
            M::ToolCallEnd {
                ok: false, output, ..
            } => {
                self.flush_message();
                let summary = truncate_chars(&sanitize_terminal(output), TOOL_OUTPUT_MAX_CHARS);
                self.items.push(Item::plain(format!("✗ {summary}"), err()));
            }
            M::ToolCallEnd { .. } => {}
            M::TokenCount { used, window } => {
                self.tokens = Some((*used, *window));
            }
            M::CompactStarted { .. } => {
                self.flush_message();
                self.items
                    .push(Item::plain("⟳ 正在压缩上下文…".into(), dim()));
            }
            M::CompactCompleted { summary_tokens } => {
                self.flush_message();
                self.items.push(Item::plain(
                    format!("✓ 上下文已压缩（摘要 {summary_tokens} tokens）"),
                    dim(),
                ));
            }
            // 审批请求：黄色提示行 + 内联弹窗（决策见 handle_key）。
            M::ApprovalRequested {
                call_id,
                kind,
                detail,
            } => {
                self.flush_message();
                let kind_label = match kind {
                    ApprovalKind::Exec => "执行命令",
                    _ => "写入文件",
                };
                let detail = sanitize_terminal(detail).into_owned();
                self.items.push(Item::plain(
                    format!("⚠ 审批请求（{kind_label}）：{detail}"),
                    warn(),
                ));
                self.approval = Some(ApprovalPopup {
                    call_id: call_id.clone(),
                    kind: *kind,
                    detail,
                    reason_mode: false,
                    reason: String::new(),
                });
            }
            // 子代理起止：弱化提示行；中间过程不进父会话事件流。
            M::SubagentStarted {
                task_id,
                subagent_type,
                description,
            } => {
                self.flush_message();
                let ty = sanitize_terminal(subagent_type);
                let desc = sanitize_terminal(description);
                self.items.push(Item::plain(
                    format!("⏚ 子代理 {task_id} 启动（{ty}）{desc}"),
                    dim(),
                ));
            }
            M::SubagentCompleted {
                task_id, status, ..
            } => {
                self.flush_message();
                use wavecode_protocol::SubagentStatus as S;
                let label = match status {
                    S::Completed => "完成",
                    S::Failed => "失败",
                    S::Stopped => "已停止",
                    _ => "结束",
                };
                self.items
                    .push(Item::plain(format!("✓ 子代理 {task_id} {label}"), dim()));
            }
            M::Warning { message } => {
                self.flush_message();
                let msg = sanitize_terminal(message).into_owned();
                self.items.push(Item::plain(msg, warn()));
            }
            M::Error { message, .. } => {
                self.flush_message();
                let msg = sanitize_terminal(message).into_owned();
                self.items.push(Item::plain(msg, err()));
            }
            M::TurnCompleted { stop_reason } => {
                self.flush_message(); // 中断路径的残余缓冲
                if *stop_reason == StopReason::Interrupted {
                    self.items.push(Item::plain("（已中断）".into(), warn()));
                }
                self.in_turn = false;
            }
            // EventMsg 标注 non_exhaustive：未来新增事件不渲染。
            _ => {}
        }
        self.follow_tail = true;
    }

    /// 渲染缓冲消息（markdown）并清空；空缓冲 no-op。
    fn flush_message(&mut self) {
        if self.msg_buf.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.msg_buf);
        self.items.push(Item::assistant(&text));
    }

    /// 工具调用开始条目：todo_write 展示清单状态迁移，task 展示子代理
    /// 类型与描述，其余 `▸ {tool}` + input 摘要（与 cli 同语义）。
    fn tool_begin_item(&mut self, tool: &str, input: &serde_json::Value) {
        if tool == "todo_write" {
            let mut lines = vec![Line::from(Span::styled("▸ todo_write", accent()))];
            let empty = Vec::new();
            let todos = input
                .get("todos")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty);
            for item in todos {
                let content = item.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let status = item
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending");
                let transition = match self.last_todos.iter().find(|(c, _)| c == content) {
                    Some((_, old)) if old != status => format!("（{old} → {status}）"),
                    _ => String::new(),
                };
                lines.push(Line::from(vec![
                    Span::raw(format!("  {} ", todo_symbol(status))),
                    Span::raw(sanitize_terminal(content).into_owned()),
                    Span::styled(transition, dim()),
                ]));
            }
            self.last_todos = parse_todo_input(input);
            self.items.push(Item { lines });
        } else if tool == "task" {
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
            self.items.push(Item {
                lines: vec![Line::from(vec![
                    Span::styled("▸ task", accent()),
                    Span::raw(" "),
                    Span::styled(
                        format!("{}{}", sanitize_terminal(subagent_type), bg_label),
                        dim(),
                    ),
                    Span::raw(format!(" {}", sanitize_terminal(description))),
                ])],
            });
        } else {
            let summary =
                truncate_chars(&sanitize_terminal(&input.to_string()), TOOL_INPUT_MAX_CHARS);
            self.items.push(Item {
                lines: vec![Line::from(vec![
                    Span::styled(format!("▸ {tool}"), accent()),
                    Span::styled(format!(" {summary}"), dim()),
                ])],
            });
        }
    }

    // ── slash 补全 ──────────────────────────────────────────────

    /// 当前输入派生的 slash 候选（内置命令 + 可直调 skill，按前缀过滤）。
    pub fn slash_candidates(&self) -> Vec<String> {
        let Some(prefix) = self.input.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(char::is_whitespace) {
            return Vec::new();
        }
        BUILTIN_COMMANDS
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| format!("/{name}"))
            .chain(
                self.ctx
                    .skill_names
                    .iter()
                    .filter(|name| name.starts_with(prefix))
                    .map(|name| format!("/{name}")),
            )
            .collect()
    }

    /// slash 弹层是否可见：`/` 起始、无参数空白、未被 Esc 关闭、有候选。
    pub fn slash_visible(&self) -> bool {
        !self.slash_dismissed && !self.slash_candidates().is_empty()
    }

    pub fn slash_selected(&self) -> usize {
        self.slash_selected
    }

    /// 将选中候选填入输入框。
    fn complete_slash(&mut self) {
        let candidates = self.slash_candidates();
        let idx = self.slash_selected.min(candidates.len().saturating_sub(1));
        if let Some(c) = candidates.get(idx) {
            self.input = c.clone();
            self.cursor = self.input.chars().count();
        }
    }

    // ── 键盘输入 ────────────────────────────────────────────────

    /// 按键 → 状态迁移（审批弹窗打开时按键全部路由给弹窗）。
    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.approval.is_some() {
            self.handle_approval_key(key);
            return;
        }
        match key.code {
            KeyCode::Enter => {
                // 弹层打开且输入不等于选中候选：先补全；否则提交。
                if self.slash_visible()
                    && self
                        .slash_candidates()
                        .get(self.slash_selected)
                        .is_some_and(|c| *c != self.input)
                {
                    self.complete_slash();
                } else {
                    self.submit_input();
                }
            }
            KeyCode::Esc => {
                if self.slash_visible() {
                    self.slash_dismissed = true;
                } else if self.in_turn {
                    self.outbox.push(Op::Interrupt);
                }
            }
            KeyCode::Tab => {
                if self.slash_visible() {
                    self.complete_slash();
                }
            }
            KeyCode::Up => {
                if self.slash_visible() {
                    let n = self.slash_candidates().len();
                    self.slash_selected = (self.slash_selected + n - 1) % n;
                }
            }
            KeyCode::Down => {
                if self.slash_visible() {
                    let n = self.slash_candidates().len();
                    self.slash_selected = (self.slash_selected + 1) % n;
                }
            }
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let byte = Self::char_to_byte(&self.input, self.cursor - 1);
                    self.input.remove(byte);
                    self.cursor -= 1;
                    self.on_input_changed();
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let byte = Self::char_to_byte(&self.input, self.cursor);
                    self.input.remove(byte);
                    self.on_input_changed();
                }
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) => {
                let byte = Self::char_to_byte(&self.input, self.cursor);
                self.input.insert(byte, c);
                self.cursor += 1;
                self.on_input_changed();
            }
            _ => {}
        }
    }

    /// 粘贴事件：净化后插入（ bracketed paste 防注入）。
    pub fn paste(&mut self, s: &str) {
        let clean = sanitize_terminal(s);
        let byte = Self::char_to_byte(&self.input, self.cursor);
        self.input.insert_str(byte, &clean);
        self.cursor += clean.chars().count();
        self.on_input_changed();
    }

    /// 鼠标滚轮：滚动消息流。
    pub fn scroll_by(&mut self, delta: isize) {
        if delta < 0 {
            self.follow_tail = false;
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.scroll = self.scroll.saturating_add(delta as usize);
            // 到达底部由 ui 绘制时判定并恢复 follow_tail（需要可视高度）。
        }
    }

    fn on_input_changed(&mut self) {
        self.slash_dismissed = false;
        self.slash_selected = 0;
    }

    /// 字符索引 → 字节索引（cursor 按字符计，防切断 UTF-8）。
    fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(s.len())
    }

    /// 审批弹窗按键：y 放行；n 进入原因录入态（Enter 确认拒绝）；
    /// Esc 直接拒绝（不留 park 悬挂）。
    fn handle_approval_key(&mut self, key: KeyEvent) {
        let Some(popup) = &mut self.approval else {
            return;
        };
        if popup.reason_mode {
            match key.code {
                KeyCode::Enter => {
                    let popup = self.approval.take().expect("上面已判定 Some");
                    self.outbox.push(Op::ExecApproval {
                        call_id: popup.call_id,
                        decision: ApprovalDecision::Deny {
                            reason: popup.reason,
                        },
                    });
                }
                KeyCode::Esc => {
                    let popup = self.approval.take().expect("上面已判定 Some");
                    self.outbox.push(Op::ExecApproval {
                        call_id: popup.call_id,
                        decision: ApprovalDecision::Deny {
                            reason: String::new(),
                        },
                    });
                }
                KeyCode::Backspace => {
                    popup.reason.pop();
                }
                KeyCode::Char(c) => popup.reason.push(c),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let popup = self.approval.take().expect("上面已判定 Some");
                self.outbox.push(Op::ExecApproval {
                    call_id: popup.call_id,
                    decision: ApprovalDecision::AllowOnce,
                });
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                popup.reason_mode = true;
            }
            KeyCode::Esc => {
                let popup = self.approval.take().expect("上面已判定 Some");
                self.outbox.push(Op::ExecApproval {
                    call_id: popup.call_id,
                    decision: ApprovalDecision::Deny {
                        reason: String::new(),
                    },
                });
            }
            _ => {}
        }
    }

    /// Enter 提交：入消息流（`> ` 前缀）并按 slash 路由产生 Op。
    fn submit_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.slash_dismissed = false;
        self.slash_selected = 0;
        self.items.push(Item::user(&text));
        match text.strip_prefix('/') {
            None => self.outbox.push(Op::UserInput { text }),
            Some(rest) => self.route_slash(rest),
        }
        self.follow_tail = true;
    }

    /// slash 路由：内置命令本地处置；其余按 skill 名查找后直调。
    fn route_slash(&mut self, rest: &str) {
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        match name {
            "quit" | "exit" => self.quit = true,
            "compact" => self.outbox.push(Op::Compact),
            "memory" => self.show_memory(),
            "mcp" => self.show_mcp(),
            "permissions" => self.cycle_permission_mode(),
            _ => {
                if self.ctx.skill_names.iter().any(|n| n == name) {
                    self.outbox.push(Op::SlashCommand {
                        name: name.to_owned(),
                        args: args.to_owned(),
                    });
                } else {
                    self.items.push(Item::plain(
                        format!(
                            "未知命令：/{name}（内置：{}；其余 / 前缀为 skill 直调）",
                            BUILTIN_COMMANDS
                                .iter()
                                .map(|c| format!("/{c}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                        warn(),
                    ));
                }
            }
        }
    }

    /// `/memory`：读取持久记忆索引文件（路径由装配侧注入）并展示。
    fn show_memory(&mut self) {
        match &self.ctx.memory_index_path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(index) if index.trim().is_empty() => {
                    self.items.push(Item::plain(
                        format!("（暂无持久记忆；索引文件：{}）", path.display()),
                        dim(),
                    ));
                }
                Ok(index) => {
                    let content = sanitize_terminal(index.trim_end()).into_owned();
                    self.items.push(Item::plain(content, Style::default()));
                }
                Err(e) => self
                    .items
                    .push(Item::plain(format!("读取记忆索引失败：{e}"), err())),
            },
            None => self.items.push(Item::plain(
                "记忆能力不可用（启动时无法解析用户主目录）".into(),
                warn(),
            )),
        }
    }

    /// `/mcp`：展示已配置 server 状态行（core 预渲染，P9；首版状态恒为
    /// "未连接（transport 未实现）"——诚实展示，不伪造在线状态）。
    fn show_mcp(&mut self) {
        if self.ctx.mcp_server_lines.is_empty() {
            self.items.push(Item::plain(
                "（未配置 MCP server；在 config.toml 添加 [mcp_servers.<name>] 段）".into(),
                dim(),
            ));
        } else {
            for line in &self.ctx.mcp_server_lines {
                self.items
                    .push(Item::plain(sanitize_terminal(line).into_owned(), dim()));
            }
        }
    }

    /// `/permissions`：四档循环切换，Op 同步 core 侧、本地立即生效。
    fn cycle_permission_mode(&mut self) {
        let next = match self.permission_mode {
            PermissionMode::Default => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::BypassPermissions,
            _ => PermissionMode::Default,
        };
        self.permission_mode = next;
        self.outbox.push(Op::SetPermissionMode { mode: next });
        self.items.push(Item::plain(
            format!("权限模式切换为 {next}（写 / 执行工具的审批策略随之变化）"),
            dim(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use wavecode_protocol::EventMsg;

    fn ctx() -> TuiContext {
        TuiContext {
            model_name: "m".into(),
            cwd: PathBuf::from("/tmp/x"),
            permission_mode: PermissionMode::Default,
            memory_index_path: None,
            skill_names: vec!["commit".into()],
            mcp_server_lines: vec![],
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn ev(msg: EventMsg) -> Event {
        Event {
            id: "s-1".into(),
            msg,
        }
    }

    /// 输入 → Enter：用户行进消息流、Op::UserInput 出队、输入框清空。
    #[test]
    fn typing_and_enter_submits_user_input() {
        let mut app = App::new(ctx());
        type_str(&mut app, "你好");
        app.handle_key(key(KeyCode::Enter));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::UserInput { text }] if text == "你好"),
            "应产出 UserInput: {ops:?}"
        );
        assert!(app.input.is_empty() && app.cursor == 0);
        let user = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("> 你好"));
        assert!(user, "用户行应进消息流");
    }

    /// Esc 优先级：弹层打开先关弹层；否则 turn 内发 Interrupt；空闲无操作。
    #[test]
    fn esc_priority_slash_then_interrupt() {
        let mut app = App::new(ctx());
        // turn 内：Esc → Interrupt
        app.handle_event(&ev(EventMsg::TurnStarted {
            turn_id: "t".into(),
        }));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(&app.take_ops()[..], [Op::Interrupt]));
        // 空闲：Esc 无操作
        app.handle_event(&ev(EventMsg::TurnCompleted {
            stop_reason: StopReason::Completed,
        }));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.take_ops().is_empty());
        // 弹层打开：Esc 只关弹层，不发 Interrupt
        app.handle_event(&ev(EventMsg::TurnStarted {
            turn_id: "t2".into(),
        }));
        type_str(&mut app, "/c");
        assert!(app.slash_visible());
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.slash_visible(), "Esc 应关闭弹层");
        assert!(app.take_ops().is_empty(), "关弹层不应发 Interrupt");
    }

    /// 审批流：事件开弹窗；y → AllowOnce；n → 原因态，录入后 Enter → Deny{原因}。
    #[test]
    fn approval_flow_allow_and_deny_with_reason() {
        let mut app = App::new(ctx());
        let req = |id: &str| {
            ev(EventMsg::ApprovalRequested {
                call_id: id.into(),
                kind: ApprovalKind::Exec,
                detail: "d".into(),
            })
        };
        // y 放行
        app.handle_event(&req("c1"));
        assert!(app.approval.is_some());
        app.handle_key(key(KeyCode::Char('y')));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::ExecApproval { call_id, decision: ApprovalDecision::AllowOnce }] if call_id == "c1"),
            "y 应放行: {ops:?}"
        );
        assert!(app.approval.is_none());
        // n → 原因录入 → Enter 拒绝带原因
        app.handle_event(&req("c2"));
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.approval.as_ref().is_some_and(|p| p.reason_mode));
        type_str(&mut app, "危险");
        app.handle_key(key(KeyCode::Enter));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::ExecApproval { call_id, decision: ApprovalDecision::Deny { reason } }] if call_id == "c2" && reason == "危险"),
            "n+原因 应拒绝: {ops:?}"
        );
        // Esc 直接拒绝（空原因，不留 park 悬挂）
        app.handle_event(&req("c3"));
        app.handle_key(key(KeyCode::Esc));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::ExecApproval { decision: ApprovalDecision::Deny { reason }, .. }] if reason.is_empty()),
            "Esc 应空原因拒绝: {ops:?}"
        );
    }

    /// slash 补全状态迁移：前缀过滤、Up/Down 环绕、Tab 补全、
    /// Enter 在输入≠候选时先补全、等于候选时提交。
    #[test]
    fn slash_completion_state_machine() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/c");
        assert_eq!(
            app.slash_candidates(),
            vec!["/compact".to_string(), "/commit".to_string()]
        );
        // Down 移动选中并环绕
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.slash_selected(), 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.slash_selected(), 0);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.slash_selected(), 1);
        // Tab 补全选中项
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.input, "/commit");
        // 输入恰为候选：Enter 提交（skill 直调）
        app.handle_key(key(KeyCode::Enter));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::SlashCommand { name, args }] if name == "commit" && args.is_empty()),
            "应直调 skill: {ops:?}"
        );
    }

    /// Enter 在弹层打开且输入为前缀时先补全不提交。
    #[test]
    fn enter_with_popup_completes_before_submit() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/c");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty(), "应先补全不提交");
        assert_eq!(app.input, "/compact");
        app.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(&app.take_ops()[..], [Op::Compact]),
            "补全后 Enter 才提交"
        );
    }

    /// /permissions：四档循环，Op 与本地状态同步。
    #[test]
    fn permissions_cycles_four_modes() {
        let mut app = App::new(ctx());
        let expect = [
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::Default,
        ];
        for want in expect {
            type_str(&mut app, "/permissions");
            app.handle_key(key(KeyCode::Enter));
            let ops = app.take_ops();
            assert!(
                matches!(&ops[..], [Op::SetPermissionMode { mode }] if *mode == want),
                "循环档位: {ops:?}"
            );
            assert_eq!(app.permission_mode, want);
        }
    }

    /// 未知 slash：黄色提示行进消息流，不产生 Op。
    #[test]
    fn unknown_slash_warns_without_op() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/nope");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty());
        let has_warn = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("未知命令：/nope"));
        assert!(has_warn);
    }

    /// /quit：置退出标志（run 循环据此外层收尾 Shutdown）。
    #[test]
    fn quit_command_sets_flag() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/quit");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.is_quit());
    }

    /// /mcp（P9）：本地展示 server 状态行（不产生 Op）；未配置时给指引。
    #[test]
    fn mcp_lists_configured_servers_locally() {
        let mut app = App::new(TuiContext {
            mcp_server_lines: vec![
                "playwright — stdio: npx @playwright/mcp@latest — 未连接（transport 未实现）"
                    .into(),
            ],
            ..ctx()
        });
        type_str(&mut app, "/mcp");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty(), "/mcp 为本地展示面");
        let has_line = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("playwright") && s.content.contains("未连接"));
        assert!(has_line);
        // 未配置：指引行。
        let mut app = App::new(ctx());
        type_str(&mut app, "/mcp");
        app.handle_key(key(KeyCode::Enter));
        let has_hint = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("未配置 MCP server"));
        assert!(has_hint);
    }

    /// 事件流：delta 缓冲 → complete 经 markdown 渲染；中断残余照常
    /// 渲染并附（已中断）；TokenCount 进状态栏。
    #[test]
    fn event_flow_markdown_and_interrupt() {
        let mut app = App::new(ctx());
        use wavecode_protocol::EventMsg as M;
        app.handle_event(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }));
        assert!(app.in_turn);
        app.handle_event(&ev(M::AgentMessageDelta {
            text: "**好**".into(),
        }));
        assert!(app.items.len() == 1, "delta 不直接进条目");
        app.handle_event(&ev(M::TokenCount {
            used: 5,
            window: 100,
        }));
        app.handle_event(&ev(M::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        }));
        assert!(!app.in_turn);
        assert_eq!(app.tokens, Some((5, 100)));
        let text: String = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("好"), "中断残余应渲染: {text}");
        assert!(!text.contains("**"), "markdown 记号应被渲染掉: {text}");
        assert!(text.contains("（已中断）"), "中断标记: {text}");
    }

    /// todo_write 条目：状态符号与迁移标注（与 cli 同语义）。
    #[test]
    fn todo_write_renders_status_migration() {
        let mut app = App::new(ctx());
        let todo = |content: &str, status: &str| {
            ev(EventMsg::ToolCallBegin {
                call_id: "c".into(),
                tool: "todo_write".into(),
                input: serde_json::json!({"todos": [{"content": content, "status": status}]}),
            })
        };
        app.handle_event(&todo("设计", "in_progress"));
        app.handle_event(&todo("设计", "completed"));
        let last = app.items.last().unwrap();
        let text: String = last
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("✓ 设计"), "完成符号: {text}");
        assert!(
            text.contains("（in_progress → completed）"),
            "迁移标注: {text}"
        );
    }

    /// 光标编辑：多字节字符插入 / 删除 / 左右移动不切断 UTF-8。
    #[test]
    fn cursor_editing_multibyte_safe() {
        let mut app = App::new(ctx());
        type_str(&mut app, "甲丙");
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Char('乙')));
        assert_eq!(app.input, "甲乙丙");
        assert_eq!(app.cursor, 2);
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "甲丙");
        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.input, "丙");
    }
}
