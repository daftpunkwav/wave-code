//! TUI 应用状态机：协议事件与键盘输入 → 界面状态 + 待发 [`Op`]。
//!
//! 全部迁移逻辑为纯函数形态（不进终端、不碰 client），单测直接驱动；
//! 事件渲染语义复用 SPEC §15.5 / cli render.rs：工具行 `▸`/`✗`、压缩
//! `⟳`/`✓`、审批 `⚠` 黄、子代理 `⏚`/`✓`、todo 清单 `☐▸✓` 与状态迁移
//! 标注、中断 `（已中断）`。delta 经 sanitize 入缓冲，complete / 中断时
//! 经 markdown 一次性渲染（流式期间消息流尾部追加纯文本预览）。

use wavecode_protocol::{Op, PermissionMode};

/// turn 进行中的等待动画帧（状态栏，100ms tick 推进）。
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 主题色（与 cli 渲染同一色系）。
mod types;

/// 行为层子模块（阶段拆分）：协议事件 / 键盘粘贴 / slash 命令 / 审批弹窗。
mod approval;
mod event;
mod input;
mod slash;

pub use self::types::TuiContext;
use self::types::{ApprovalPopup, Item, dim, err};

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;
    use wavecode_protocol::{ApprovalDecision, ApprovalKind, Event, EventMsg, StopReason};

    fn ctx() -> TuiContext {
        TuiContext {
            model_name: "m".into(),
            cwd: PathBuf::from("/tmp/x"),
            permission_mode: PermissionMode::Default,
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

    /// follow_tail 语义：仅内容类事件（条目入流 / 缓冲增长）恢复跟随；
    /// TokenCount、TurnStarted（清缓冲）等无内容事件不拽底——turn 内
    /// delta 高频，否则用户翻历史阅读会被立即拉回。
    #[test]
    fn follow_tail_follows_only_content_events() {
        let mut app = App::new(ctx());
        app.handle_key(key(KeyCode::PageUp));
        assert!(!app.follow_tail, "翻页后应离开跟随");
        app.handle_event(&ev(EventMsg::TokenCount { used: 1, window: 2 }));
        assert!(!app.follow_tail, "TokenCount 不应拽回底部");
        app.handle_event(&ev(EventMsg::TurnStarted {
            turn_id: "t".into(),
        }));
        assert!(!app.follow_tail, "TurnStarted 仅清缓冲,非内容");
        app.handle_event(&ev(EventMsg::AgentMessageDelta { text: "hi".into() }));
        assert!(app.follow_tail, "delta 增长缓冲应恢复跟随");
        app.handle_key(key(KeyCode::PageUp));
        assert!(!app.follow_tail);
        app.handle_event(&ev(EventMsg::Warning {
            message: "w".into(),
        }));
        assert!(app.follow_tail, "告警条目应恢复跟随");
    }

    /// paste 路由：弹窗打开时与按键同路由——reason 录入态粘贴进原因，
    /// 非 reason 态忽略（主输入框被弹窗遮挡，静默写入不可见）；弹窗
    /// 关闭时粘贴进主输入框。
    #[test]
    fn paste_routes_to_approval_popup_when_open() {
        let mut app = App::new(ctx());
        app.paste("主输入");
        assert_eq!(app.input, "主输入");
        app.handle_event(&ev(EventMsg::ApprovalRequested {
            call_id: "c1".into(),
            kind: ApprovalKind::Exec,
            detail: "d".into(),
        }));
        app.paste("xyz");
        assert_eq!(app.input, "主输入", "弹窗非 reason 态粘贴应忽略");
        app.handle_key(key(KeyCode::Char('n')));
        app.paste("非常危险\n的第二行");
        let popup = app.approval.as_ref().unwrap();
        assert_eq!(popup.reason, "非常危险\n的第二行");
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
