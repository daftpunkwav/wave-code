//! wavecode-tui — ratatui 终端用户界面（P8）。
//!
//! 作为 [`wavecode_app_server`] 的进程内客户端实现全部终端交互：
//! 流式消息渲染、slash 指令补全与执行、审批内联弹窗、Esc 中断与状态栏。
//!
//! **crate 边界（SPEC §3 规则 2）**：tui 只允许依赖 protocol + app-server，
//! 不得依赖 core——保证 TUI 与 Web/Desktop 等远端前端能力等价（全部交互
//! 都走 Submission/Event 协议面）。本 crate Cargo.toml 即事实源，
//! `tests::dependency_matrix_locked` 在测试层锁定。
//!
//! 模块划分：
//! - [`app`]：应用状态机（协议事件 / 键盘输入 → 状态 + 待发 Op，纯函数可测）；
//! - [`markdown`]：markdown → ratatui 行（语义对齐 SPEC §15.5）；
//! - [`ui`]：布局与弹层绘制（纯投影）；
//! - [`text`]：终端净化与截断（镜像 cli render.rs 语义）。

pub mod app;
pub mod markdown;
pub mod text;
pub mod ui;

use std::time::Duration;

use anyhow::Context as _;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CrosstermEvent, EventStream, KeyEventKind, MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use wavecode_app_server::InProcessClient;
use wavecode_protocol::{Op, Submission};

pub use app::{App, TuiContext};

/// TUI 入口：进入交替屏幕并驱动事件循环，返回时终端已恢复原状。
///
/// `client` 由装配侧（cli）以 [`InProcessClient::spawn`] 创建后传入
///（tui 不能依赖 core，故不接触 SessionConfig）；返回前发送
/// `Op::Shutdown` 优雅收尾（client 析构另有 abort 兜底，与 cli 同策略）。
pub async fn run(mut client: InProcessClient, ctx: TuiContext) -> anyhow::Result<()> {
    let mut guard = TerminalGuard::enter()?;
    let mut app = App::new(ctx);
    let mut events = EventStream::new();
    // 100ms tick 驱动等待动画；事件密集时丢弃积压 tick，不补帧。
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        guard.terminal.draw(|f| ui::draw(f, &mut app))?;
        tokio::select! {
            maybe = events.next() => match maybe {
                // Windows 的 crossterm 会同时发 Press/Release，只处理 Press。
                Some(Ok(CrosstermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                }
                Some(Ok(CrosstermEvent::Paste(s))) => app.paste(&s),
                Some(Ok(CrosstermEvent::Mouse(m))) => match m.kind {
                    MouseEventKind::ScrollUp => app.scroll_by(-3),
                    MouseEventKind::ScrollDown => app.scroll_by(3),
                    _ => {}
                },
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            maybe = client.next_event() => match maybe {
                Some(ev) => app.handle_event(&ev),
                None => app.actor_died(),
            },
            _ = ticker.tick() => app.tick(),
        }
        for op in app.take_ops() {
            client.submit(new_submission(op)).await?;
        }
        if app.is_quit() {
            break;
        }
    }

    // 优雅关闭：submit 后即可退出，不阻塞等待（actor Shutdown 路径会
    // 触发 SessionEnd 记忆提取；client 析构的 abort 兜底防任务泄漏）。
    let _ = client.submit(new_submission(Op::Shutdown)).await;
    Ok(())
}

/// 生成一次 Submission（uuid 关联其后续全部事件）。
fn new_submission(op: Op) -> Submission {
    Submission {
        id: uuid::Uuid::new_v4().to_string(),
        op,
    }
}

/// 终端状态守卫：raw mode + 交替屏幕 + 鼠标捕获 + bracketed paste；
/// Drop 恢复（含 panic 路径），不让用户终端烂在半接管状态。
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("enable_raw_mode 失败")?;
        let mut out = std::io::stdout();
        crossterm::execute!(
            out,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .context("进入交替屏幕失败")?;
        let terminal = Terminal::new(CrosstermBackend::new(out))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    //! 关键帧快照测试：选用 ratatui TestBackend 缓冲断言而非 insta——
    //! 快照内容为布局文本（符号级），样式（颜色 / modifier）由 app /
    //! markdown 层单测锁定；避免 insta 快照文件随 ratatui 版本升级
    //! 产生大面积 churn。
    use super::*;
    use ratatui::backend::TestBackend;
    use wavecode_protocol::{Event, EventMsg};

    fn ctx() -> TuiContext {
        TuiContext {
            model_name: "claude-sonnet-4-5".into(),
            cwd: std::path::PathBuf::from("D:/proj/wavecode"),
            permission_mode: wavecode_protocol::PermissionMode::Default,
            memory_index_path: None,
            skill_names: vec!["commit".into()],
            mcp_server_lines: vec![],
        }
    }

    fn ev(msg: EventMsg) -> Event {
        Event {
            id: "s-1".into(),
            msg,
        }
    }

    /// 提取缓冲文本（逐行符号拼接，去行尾空白）：样式不参与断言。
    /// 宽字符（CJK 等）占两格，后续格为占位符——按字符宽度跳格拼接，
    /// 还原真实文本（否则 "命令" 会拼成 "命 令"）。
    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        use unicode_width::UnicodeWidthStr;
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut rows = Vec::new();
        for y in 0..area.height {
            let mut row = String::new();
            let mut x = 0;
            while x < area.width {
                let sym = buf[(x, y)].symbol();
                row.push_str(sym);
                x += UnicodeWidthStr::width(sym).max(1) as u16;
            }
            rows.push(row.trim_end().to_string());
        }
        rows.join("\n")
    }

    fn draw_app(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        buffer_text(&terminal)
    }

    /// 启动布局：欢迎行在消息流顶部；输入框带边框与标题；状态栏含
    /// 模型名 / 权限模式 / tokens 占位 / cwd。
    #[test]
    fn snapshot_startup_layout() {
        let mut app = App::new(ctx());
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("WaveCode TUI"), "欢迎行: {text}");
        assert!(text.contains("∿ 输入"), "输入框标题: {text}");
        assert!(text.contains("claude-sonnet-4-5"), "模型名: {text}");
        assert!(text.contains("default"), "权限模式: {text}");
        assert!(text.contains("tokens —"), "tokens 占位: {text}");
        assert!(text.contains("D:/proj/wavecode"), "cwd: {text}");
        // 三段布局：状态栏在最后一行。
        let last = text.lines().last().unwrap();
        assert!(last.contains("claude-sonnet-4-5"), "状态栏应在底部: {text}");
    }

    /// 消息流 + 状态栏：用户行 / markdown 助手消息 / 工具行符号 /
    /// 失败 ✗ / TokenCount 进状态栏。
    #[test]
    fn snapshot_message_flow_and_status() {
        let mut app = App::new(ctx());
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('你'),
            crossterm::event::KeyModifiers::NONE,
        ));
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        use wavecode_protocol::EventMsg as M;
        app.handle_event(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }));
        app.handle_event(&ev(M::AgentMessageDelta {
            text: "**好的**\n".into(),
        }));
        app.handle_event(&ev(M::AgentMessageComplete {
            text: "**好的**\n".into(),
        }));
        app.handle_event(&ev(M::ToolCallBegin {
            call_id: "c1".into(),
            tool: "read_file".into(),
            input: serde_json::json!({"path": "a.txt"}),
        }));
        app.handle_event(&ev(M::ToolCallEnd {
            call_id: "c1".into(),
            ok: false,
            output: "boom".into(),
        }));
        app.handle_event(&ev(M::TokenCount {
            used: 120,
            window: 200_000,
        }));
        app.handle_event(&ev(M::TurnCompleted {
            stop_reason: wavecode_protocol::StopReason::Completed,
        }));
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("> 你"), "用户行: {text}");
        assert!(text.contains("好的"), "助手消息: {text}");
        assert!(!text.contains("**"), "markdown 记号应被渲染掉: {text}");
        assert!(text.contains("▸ read_file"), "工具行: {text}");
        assert!(text.contains("✗ boom"), "失败行: {text}");
        assert!(text.contains("tokens 120/200000"), "状态栏 tokens: {text}");
    }

    /// 审批弹窗：⚠ 提示行进消息流；弹窗含类型 / detail / y-n-Esc 提示；
    /// n 进入原因录入态后弹窗切换提示。
    #[test]
    fn snapshot_approval_popup() {
        let mut app = App::new(ctx());
        app.handle_event(&ev(EventMsg::ApprovalRequested {
            call_id: "c1".into(),
            kind: wavecode_protocol::ApprovalKind::Exec,
            detail: "shell: rm -rf build/".into(),
        }));
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("⚠ 审批请求"), "提示行/弹窗标题: {text}");
        assert!(text.contains("执行命令"), "类型: {text}");
        assert!(text.contains("shell: rm -rf build/"), "detail: {text}");
        assert!(text.contains("y 放行"), "选择态提示: {text}");
        // n → 原因录入态。
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('n'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("拒绝原因"), "原因录入态: {text}");
        assert!(text.contains("Enter 确认拒绝"), "原因态提示: {text}");
    }

    /// slash 补全弹层：`/c` 过滤出 /compact 与 /commit（skill），
    /// 选中项带 ▸ 前缀。
    #[test]
    fn snapshot_slash_popup() {
        let mut app = App::new(ctx());
        for c in ['/', 'c'] {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("命令"), "弹层标题: {text}");
        assert!(text.contains("▸ /compact"), "选中项: {text}");
        assert!(text.contains("/commit"), "skill 候选: {text}");
        assert!(!text.contains("/memory"), "前缀过滤: {text}");
    }

    /// crate 边界锁定（SPEC §3 规则 2）：tui 的 workspace 内依赖只有
    /// protocol + app-server，不得引入 core 等其他 wavecode-* crate。
    #[test]
    fn dependency_matrix_locked() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            !manifest.contains("wavecode-core"),
            "tui 不得依赖 core（SPEC §3 规则 2）"
        );
        for line in manifest.lines() {
            let Some(name) = line.split('=').next().map(str::trim) else {
                continue;
            };
            if name.starts_with("wavecode-") {
                assert!(
                    matches!(name, "wavecode-protocol" | "wavecode-app-server"),
                    "tui 新增 workspace 内依赖须先改 SPEC §3 矩阵: {name}"
                );
            }
        }
    }
}
