//! Pure data types and parsing helpers: TuiContext / Item /
//! ApprovalPopup / style and todo input parsing — invariant data outside
//! the App state machine. Theme colors are defined once here and
//! re-exported through the app module for ui.rs; do not redefine them
//! elsewhere.

use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use wavecode_wire::ApprovalKind;

use crate::markdown::render_markdown;

pub(crate) fn accent() -> Style {
    Style::default().fg(Color::LightCyan)
}

pub(crate) fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(crate) fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

pub(crate) fn err() -> Style {
    Style::default().fg(Color::Red)
}

/// Session permission mode (local display copy).
///
/// Wire names stay frozen (`plan`/`guarded`/`auto`) so the actor-side
/// policy parses them back. The TUI cycles locally and submits each
/// step; unknown names are rejected downstream with a warning, never
/// applied silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Read-only exploration; propose via the plan tool or just answer.
    Plan,
    /// Ask only for dangerous operations (command execution, destructive
    /// tools); file edits and other writes flow through.
    Guarded,
    /// Bypass all approval prompts (deny rules still apply).
    Auto,
}

impl PermissionMode {
    /// Canonical wire string of the mode.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Guarded => "guarded",
            Self::Auto => "auto",
        }
    }

    /// Next mode in the `/permissions` cycle order.
    pub fn cycle(&self) -> Self {
        match self {
            Self::Plan => Self::Guarded,
            Self::Guarded => Self::Auto,
            Self::Auto => Self::Plan,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// TUI launch context (the assembly side injects everything the TUI
/// needs: the TUI must not depend on core, so model identity, paths,
/// skill lists, memory text, and the initial mode all arrive here).
pub struct TuiContext {
    /// Model name (status bar).
    pub model_name: String,
    /// Session working directory (status bar).
    pub cwd: PathBuf,
    /// Initial permission mode (`/permissions` cycles from here).
    pub permission_mode: PermissionMode,
    /// Directly invokable skill names (slash completion candidates).
    pub skill_names: Vec<String>,
    /// Pre-rendered MCP server status lines. Empty means unconfigured.
    pub mcp_server_lines: Vec<String>,
    /// Persistent memory index text for local `/memory` rendering.
    /// Empty means memory is unavailable.
    pub memory_index: String,
    /// On-demand status views over plan / goal / snapshot state, served
    /// by the assembly side so slash commands render current state
    /// without knowing where or in what shape it is stored.
    pub status_queries: std::sync::Arc<dyn operations_actor::StatusQueries>,
}

/// One message stream entry (committed, immutable; owns styled rows).
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

/// todo status glyphs (legacy CLI parity: ✓ done, ▸ active, ☐ queued).
pub(super) fn todo_symbol(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "in_progress" => "▸",
        _ => "☐",
    }
}

/// Extract list states (content, status) from a todo_write input for
/// render-time migration diffing.
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

/// Inline approval popup state.
pub struct ApprovalPopup {
    pub call_id: String,
    pub kind: ApprovalKind,
    pub detail: String,
    /// false: y/n selection; true: denial reason entry.
    pub reason_mode: bool,
    pub reason: String,
}

/// Inline interactive-question popup state.
pub struct QuestionPopup {
    pub call_id: String,
    pub question: String,
    /// Numbered answer options (select with 1-4); may be empty.
    pub options: Vec<String>,
    /// Free-text answer buffer (Enter submits it; an empty submit means
    /// the question was dismissed).
    pub input: String,
}
