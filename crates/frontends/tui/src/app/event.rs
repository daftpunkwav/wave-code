//! Protocol events → UI state transitions: rendering semantics mirror
//! the legacy human renderer.

use operations_wire::{ApprovalKind, Event};
use ratatui::text::{Line, Span};

use super::App;
use super::types::{ApprovalPopup, Item, accent, dim, err, parse_todo_input, todo_symbol, warn};
use crate::text::{sanitize_terminal, truncate_chars};

/// Character budget for tool call input summaries (matches legacy CLI).
const TOOL_INPUT_MAX_CHARS: usize = 80;

impl App {
    /// Protocol event → state transition.
    ///
    /// Only content-producing events (new items / growing buffers) reset
    /// [`App::follow_tail`]: in-turn streaming deltas are frequent, and a
    /// content-free event pulling the user back to the bottom while they
    /// read history would be unusable.
    pub fn handle_event(&mut self, ev: &Event) {
        use operations_wire::EventMsg as M;
        let items_before = self.items.len();
        let buf_before = self.msg_buf.len();
        match &ev.msg {
            M::TurnStarted => {
                self.in_turn = true;
                self.msg_buf.clear();
                self.spinner = 0;
            }
            // Deltas are sanitized against terminal injection first.
            M::AgentMessageDelta { text } => {
                let clean = sanitize_terminal(text);
                self.msg_buf.push_str(&clean);
            }
            M::AgentMessageComplete { .. } => self.flush_message(),
            // Tool rows commit immediately; flush partial messages first
            // to keep chronological order readable.
            M::ToolCallBegin { name, input, .. } => {
                self.flush_message();
                self.tool_begin_item(name, input);
            }
            // Failures echo the call id; outputs stay in the transcript
            // (events stay light by design); successes stay quiet.
            M::ToolCallEnd { call_id, is_error } => {
                if *is_error {
                    self.flush_message();
                    self.push_item(Item::plain(format!("✗ {call_id}"), err()));
                }
            }
            M::TokenCount {
                input_tokens,
                output_tokens,
            } => {
                self.tokens = Some((*input_tokens, *output_tokens));
            }
            M::CompactStarted { .. } => {
                self.flush_message();
                self.items
                    .push(Item::plain("⟳ 正在压缩上下文…".into(), dim()));
            }
            M::CompactCompleted { summary_tokens } => {
                self.flush_message();
                self.push_item(Item::plain(
                    format!("✓ 上下文已压缩（摘要 {summary_tokens} tokens）"),
                    dim(),
                ));
            }
            // Approval request: yellow notice line + inline popup
            // (decisions are handled on keys).
            M::ApprovalRequested {
                call_id,
                kind,
                detail,
            } => {
                self.flush_message();
                let kind_label = match kind {
                    ApprovalKind::Exec => "执行命令",
                    ApprovalKind::Write => "写入文件",
                };
                let detail = sanitize_terminal(detail).into_owned();
                self.push_item(Item::plain(
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
            M::Warning { message } => {
                self.flush_message();
                let msg = sanitize_terminal(message).into_owned();
                self.push_item(Item::plain(msg, warn()));
            }
            M::Error { message, .. } => {
                self.flush_message();
                let msg = sanitize_terminal(message).into_owned();
                self.push_item(Item::plain(msg, err()));
            }
            M::TurnCompleted { interrupted } => {
                self.flush_message(); // leftover buffer on interrupt paths
                if *interrupted {
                    self.push_item(Item::plain("（已中断）".into(), warn()));
                }
                self.in_turn = false;
            }
        }
        // Only new content (more items / grown buffer) resumes following;
        // clearing the buffer (TurnStarted) does not count as content.
        if self.items.len() != items_before || self.msg_buf.len() > buf_before {
            self.follow_tail = true;
        }
    }

    /// Render the buffered message (markdown) and clear; empty buffers
    /// are a no-op.
    fn flush_message(&mut self) {
        if self.msg_buf.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.msg_buf);
        self.push_item(Item::assistant(&text));
    }

    /// Tool call start row: todo_write shows list state migration,
    /// child_spawn shows the child input summary, others show
    /// `▸ {tool}` + input summary (legacy CLI semantics).
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
            self.push_item(Item { lines });
        } else if tool == "child_spawn" {
            let kind = input
                .get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or("standard");
            let prompt = input.get("input").and_then(|d| d.as_str()).unwrap_or("");
            let summary = truncate_chars(&sanitize_terminal(prompt), TOOL_INPUT_MAX_CHARS);
            self.push_item(Item {
                lines: vec![Line::from(vec![
                    Span::styled("▸ child_spawn", accent()),
                    Span::styled(format!(" ({kind})"), dim()),
                    Span::raw(format!(" {summary}")),
                ])],
            });
        } else {
            let summary =
                truncate_chars(&sanitize_terminal(&input.to_string()), TOOL_INPUT_MAX_CHARS);
            self.push_item(Item {
                lines: vec![Line::from(vec![
                    Span::styled(format!("▸ {tool}"), accent()),
                    Span::styled(format!(" {summary}"), dim()),
                ])],
            });
        }
    }
}
