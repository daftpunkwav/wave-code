//! 协议事件 → 界面状态迁移（自 app/mod.rs 拆分）：事件渲染语义对齐
//! SPEC §15.5 / cli render.rs 的 HumanRenderer。

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use wavecode_protocol::{ApprovalKind, Event, StopReason};

use super::App;
use super::types::{ApprovalPopup, Item, accent, dim, err, parse_todo_input, todo_symbol, warn};
use crate::text::{sanitize_terminal, truncate_chars};

/// 工具调用输入摘要的字符上限（与 cli 一致）。
const TOOL_INPUT_MAX_CHARS: usize = 80;
/// 工具失败输出摘要的字符上限（与 cli 一致）。
const TOOL_OUTPUT_MAX_CHARS: usize = 200;

impl App {
    /// 协议事件 → 状态迁移（渲染语义对齐 cli render.rs 的 HumanRenderer）。
    ///
    /// 仅"产出可见内容"的事件（条目入消息流 / delta 进缓冲）重置
    /// [`App::follow_tail`]：turn 内流式 delta 高频，若 TokenCount 等
    /// 无内容事件也拽回底部，用户翻历史阅读会被立即拉回（不可用）。
    pub fn handle_event(&mut self, ev: &Event) {
        use wavecode_protocol::EventMsg as M;
        let items_before = self.items.len();
        let buf_before = self.msg_buf.len();
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
            // `/memory` 回包:索引内容渲染(空索引与缺文件同态);path=None
            // 表示会话无记忆装配。
            M::MemoryIndex { path, content } => match path {
                Some(p) if content.trim().is_empty() => {
                    self.items.push(Item::plain(
                        format!("（暂无持久记忆；索引文件：{p}）"),
                        dim(),
                    ));
                }
                Some(_) => {
                    let clean = sanitize_terminal(content.trim_end()).into_owned();
                    self.items.push(Item::plain(clean, Style::default()));
                }
                None => self.items.push(Item::plain(
                    "记忆能力不可用（会话未启用记忆装配）".into(),
                    warn(),
                )),
            },
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
        // 有新内容（条目增加 / 缓冲增长）才恢复跟随；清缓冲（TurnStarted）
        // 不算内容。
        if self.items.len() != items_before || self.msg_buf.len() > buf_before {
            self.follow_tail = true;
        }
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
}
