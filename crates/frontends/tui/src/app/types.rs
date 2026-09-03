//! 纯数据类型与解析辅助（阶段 6 拆分自 app.rs）：TuiContext / Item /
//! ApprovalPopup / 样式与 todo 输入解析——App 状态机外的不变数据。

use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use wavecode_protocol::{ApprovalKind, PermissionMode};

use crate::markdown::render_markdown;

pub(super) fn accent() -> Style {
    Style::default().fg(Color::LightCyan)
}

pub(super) fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(super) fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(super) fn err() -> Style {
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
    pub(super) fn plain(text: String, style: Style) -> Self {
        let lines = text
            .split('\n')
            .map(|l| Line::from(Span::styled(l.to_string(), style)))
            .collect();
        Self { lines }
    }

    pub(super) fn user(text: &str) -> Self {
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

    pub(super) fn assistant(text: &str) -> Self {
        Self {
            lines: render_markdown(text),
        }
    }
}

/// todo 状态符号（与 cli 一致：✓ 完成、▸ 进行中、☐ 待办）。
pub(super) fn todo_symbol(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "in_progress" => "▸",
        _ => "☐",
    }
}

/// 从 todo_write 输入提取清单状态（content, status），供下次渲染比对。
pub(super) fn parse_todo_input(input: &serde_json::Value) -> Vec<(String, String)> {
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
