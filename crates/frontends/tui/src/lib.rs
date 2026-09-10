//! wavecode-tui — ratatui terminal UI over the new harness stack.
//!
//! Implements all terminal interaction as an in-process client of the
//! session actor: streaming message rendering, slash completion and
//! commands, inline approval popups, Esc interrupts, and a status bar.
//!
//! **Crate boundary**: the TUI may only depend on operations-wire and
//! operations-actor (in-process transport), never on capabilities or
//! the composition root — every interaction crosses the Submission/Event
//! surface, keeping remote frontends equivalent. This Cargo.toml is the
//! source of truth; `tests::dependency_matrix_locked` locks it in tests.
//!
//! Modules:
//! - [`app`]: application state machine (protocol events / keyboard
//!   input → state + outbound Ops, side-effect free and testable);
//! - [`markdown`]: markdown → ratatui rows;
//! - [`ui`]: layout and popup drawing (pure projection);
//! - [`text`]: terminal sanitizing and truncation (shared with the
//!   harness CLI through the allowed frontends dependency edge).

// Export surface discipline: the CLI uses only [`run`] + [`TuiContext`]
// for interaction (app state machine / markdown / drawing internals are
// not exported). The single exception is [`text`]: terminal sanitizing
// is security-sensitive logic shared with the harness CLI through the
// allowed frontends edge instead of being synced by hand.
mod app;
mod markdown;
pub mod text;
mod ui;

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
use operations_actor::ActorClient;
use operations_wire::{Op, Submission};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::App;
pub use app::{PermissionMode, TuiContext};

/// TUI entry: enter the alternate screen and drive the event loop,
/// restoring the terminal on return.
///
/// `client` arrives built from the assembly side (the TUI never touches
/// configuration); a graceful `Op::Shutdown` goes out before returning
/// (client drop aborts as backup, same policy as the harness CLI).
pub async fn run(mut client: ActorClient, ctx: TuiContext) -> anyhow::Result<()> {
    let mut guard = TerminalGuard::enter()?;
    let mut app = App::new(ctx);
    let mut events = EventStream::new();
    // 100ms tick drives the spinner; dense event bursts drop queued
    // ticks instead of catching up frame by frame.
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        guard.terminal.draw(|f| ui::draw(f, &mut app))?;
        tokio::select! {
            maybe = events.next() => match maybe {
                // Windows crossterm emits Press+Release together; Press only.
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
            client
                .submit(new_submission(op))
                .await
                .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
        }
        if app.is_quit() {
            break;
        }
    }

    // Graceful shutdown: exit right after submitting, never blocking
    // (the actor Shutdown path triggers SessionEnd memory extraction;
    // client-drop abort guards against task leaks).
    let _ = client.submit(new_submission(Op::Shutdown)).await;
    Ok(())
}

/// Build one Submission (uuid correlates all its later events).
fn new_submission(op: Op) -> Submission {
    Submission {
        id: uuid::Uuid::new_v4().to_string(),
        op,
    }
}

/// Terminal state guard: raw mode + alternate screen + mouse capture +
/// bracketed paste; Drop restores everything (panic paths included) so
/// the user terminal never stays half-taken.
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
    //! Keyframe snapshot tests: ratatui TestBackend buffer assertions
    //! instead of insta — snapshots hold layout text (glyph level) while
    //! styles (colors / modifiers) lock in app / markdown unit tests, so
    //! ratatui upgrades never churn snapshot files.
    use super::*;
    use operations_wire::{Event, EventMsg};
    use ratatui::backend::TestBackend;

    fn ctx() -> TuiContext {
        TuiContext {
            model_name: "claude-sonnet-4-5".into(),
            cwd: std::path::PathBuf::from("D:/proj/wavecode"),
            permission_mode: PermissionMode::Default,
            skill_names: vec!["commit".into()],
            mcp_server_lines: vec![],
            memory_index: String::new(),
        }
    }

    fn ev(msg: EventMsg) -> Event {
        Event {
            id: "s-1".into(),
            msg,
        }
    }

    /// Extract buffer text (symbols joined per row, trailing space
    /// trimmed): styles stay out of the assertions. Wide characters
    /// (CJK etc.) occupy two cells with the second as placeholder —
    /// advance by character width to restore the true text (otherwise
    /// multi-cell glyphs would split apart).
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

    /// Startup layout: welcome row atop the stream; bordered input box
    /// with title; status bar with model / permission / tokens slot / cwd.
    #[test]
    fn snapshot_startup_layout() {
        let mut app = App::new(ctx());
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("WaveCode TUI"), "welcome row: {text}");
        assert!(text.contains("∿ 输入"), "input title: {text}");
        assert!(text.contains("claude-sonnet-4-5"), "model name: {text}");
        assert!(text.contains("default"), "permission mode: {text}");
        assert!(text.contains("tokens —"), "tokens slot: {text}");
        assert!(text.contains("D:/proj/wavecode"), "cwd: {text}");
        // Three-part layout: status bar on the last row.
        let last = text.lines().last().unwrap();
        assert!(
            last.contains("claude-sonnet-4-5"),
            "status bar at bottom: {text}"
        );
    }

    /// Message flow + status bar: user row / markdown assistant message /
    /// tool row symbols / failure ✗ / TokenCount in the status bar.
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
        use operations_wire::EventMsg as M;
        app.handle_event(&ev(M::TurnStarted));
        app.handle_event(&ev(M::AgentMessageDelta {
            text: "**好的**\n".into(),
        }));
        app.handle_event(&ev(M::AgentMessageComplete {
            text: "**好的**\n".into(),
        }));
        app.handle_event(&ev(M::ToolCallBegin {
            call_id: "c1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "a.txt"}),
        }));
        app.handle_event(&ev(M::ToolCallEnd {
            call_id: "c1".into(),
            is_error: true,
        }));
        app.handle_event(&ev(M::TokenCount {
            input_tokens: 120,
            output_tokens: 200_000,
        }));
        app.handle_event(&ev(M::TurnCompleted { interrupted: false }));
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("> 你"), "user row: {text}");
        assert!(text.contains("好的"), "assistant message: {text}");
        assert!(!text.contains("**"), "markdown markers render away: {text}");
        assert!(text.contains("▸ read_file"), "tool row: {text}");
        assert!(text.contains("✗ c1"), "failure row: {text}");
        assert!(
            text.contains("tokens 120/200000"),
            "status bar tokens: {text}"
        );
    }

    /// Approval popup: ⚠ notice row enters the stream; the popup shows
    /// kind / detail / y-n-Esc hints; pressing n switches to reason mode.
    #[test]
    fn snapshot_approval_popup() {
        let mut app = App::new(ctx());
        app.handle_event(&ev(EventMsg::ApprovalRequested {
            call_id: "c1".into(),
            kind: operations_wire::ApprovalKind::Exec,
            detail: "shell: rm -rf build/".into(),
        }));
        let text = draw_app(&mut app, 80, 24);
        assert!(
            text.contains("⚠ 审批请求"),
            "notice row/popup title: {text}"
        );
        assert!(text.contains("执行命令"), "kind: {text}");
        assert!(text.contains("shell: rm -rf build/"), "detail: {text}");
        assert!(text.contains("y 放行"), "selection hints: {text}");
        // n → reason mode.
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('n'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("拒绝原因"), "reason mode: {text}");
        assert!(text.contains("Enter 确认拒绝"), "reason hints: {text}");
    }

    /// Slash completion popup: `/c` filters to /compact and /commit
    /// (skill), the selected item carries the ▸ prefix.
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
        assert!(text.contains("命令"), "popup title: {text}");
        assert!(text.contains("▸ /compact"), "selected item: {text}");
        assert!(text.contains("/commit"), "skill candidate: {text}");
        assert!(!text.contains("/memory"), "prefix filter: {text}");
    }

    /// Crate boundary: the TUI workspace dependencies are exactly
    /// operations-wire and operations-actor. Nothing else internal.
    #[test]
    fn dependency_matrix_locked() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            !manifest.contains("wavecode-core"),
            "tui must not depend on core"
        );
        for line in manifest.lines() {
            let Some(name) = line.split('=').next().map(str::trim) else {
                continue;
            };
            if name.starts_with("operations-") {
                assert!(
                    matches!(name, "operations-wire" | "operations-actor"),
                    "tui new internal deps need a matrix update: {name}"
                );
            }
            assert!(
                !name.starts_with("wavecode-"),
                "tui must not depend on legacy crates: {name}"
            );
        }
    }
}
