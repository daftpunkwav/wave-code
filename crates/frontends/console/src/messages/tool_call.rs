//! Tool call transcript cards: state dot, verb, tool name, key
//! argument, result chip, and a collapsible output body.
//!
//! Cards carry no borders (indented rows, matching the reference);
//! the shared Ctrl+O flag expands the result body. Per-tool argument
//! summaries come from [`args_summary`].

use crate::diff;
use crate::messages::ExpandedFlag;
use crate::theme::{self, Token};
use tui_engine::component::Component;
use tui_engine::sanitize::sanitize_terminal;
use tui_engine::width;

/// Argument values truncate to this length (head/tail aware).
pub const MAX_ARG_LENGTH: usize = 60;
/// Collapsed result shows at most this many lines.
pub const OUTCOME_MAX_LINES: usize = 3;
/// Expanded result renders at most this many wrapped lines.
pub const MAX_EXPANDED_LINES: usize = 200;

/// Lifecycle of one tool call card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    /// Call dispatched, result pending.
    Running,
    /// Completed successfully.
    Done,
    /// Completed with an error result.
    Failed,
}

/// Extract a human summary of the key argument for a tool input.
/// Names follow the real registry (`read`/`edit`/`ls`/`web_fetch`/...,
/// see `capabilities/tools`); unknown tools fall through to a generic
/// key scan.
pub fn args_summary(name: &str, input: &serde_json::Value) -> String {
    let pick = |keys: &[&str]| -> Option<String> {
        for key in keys {
            if let Some(value) = input.get(*key).and_then(|v| v.as_str()) {
                return Some(value.to_string());
            }
        }
        None
    };
    let raw = match name {
        "read" | "write" | "edit" | "ls" | "view" | "present" => pick(&["path"]),
        "shell" | "pty_shell" => pick(&["command", "cmd"]),
        "grep" => pick(&["pattern"]).map(|p| format!("“{p}”")),
        "glob" => pick(&["pattern"]),
        "web_fetch" => pick(&["url"]),
        "web_search" => pick(&["query"]),
        "python" | "node" => pick(&["code"]),
        _ => pick(&[
            "path",
            "command",
            "url",
            "query",
            "code",
            "name",
            "description",
        ]),
    }
    .unwrap_or_default();
    truncate_arg(sanitize_terminal(&raw).as_ref(), MAX_ARG_LENGTH)
}

/// Smart path shortening: deep paths keep their tail.
fn truncate_arg(value: &str, max: usize) -> String {
    let value = value.replace('\\', "/");
    if width::width(&value) <= max {
        return value;
    }
    let segments: Vec<&str> = value.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() > 2 {
        let tail = segments[segments.len() - 2..].join("/");
        if width::width(&tail) + 2 <= max {
            return format!("…/{tail}");
        }
    }
    let budget = max.saturating_sub(1);
    let mut out: String = value.chars().take(budget).collect();
    out.push('…');
    out
}

/// Old/new text pairs worth a clustered diff preview.
const DIFF_TOOLS: [&str; 1] = ["edit"];

/// The verb for a card header. Names follow the real registry; MCP
/// tools (`mcp__server__tool`) and anything unknown degrade to
/// Using/Used.
pub fn verb(name: &str, state: ToolState) -> String {
    let present = match name {
        "read" | "view" => "Reading",
        "write" => "Writing",
        "edit" => "Editing",
        "ls" => "Listing",
        "grep" | "glob" | "web_search" => "Searching",
        "shell" | "pty_shell" | "python" | "node" => "Running",
        "web_fetch" => "Fetching",
        "present" => "Presenting",
        _ => "Using",
    };
    let past = match name {
        "read" | "view" => "Read",
        "write" => "Wrote",
        "edit" => "Edited",
        "ls" => "Listed",
        "grep" | "glob" | "web_search" => "Searched",
        "shell" | "pty_shell" | "python" | "node" => "Ran",
        "web_fetch" => "Fetched",
        "present" => "Presented",
        _ => "Used",
    };
    match state {
        ToolState::Running => present.to_string(),
        _ => past.to_string(),
    }
}

/// A tool call card.
pub struct ToolCall {
    name: String,
    args: String,
    state: ToolState,
    /// Result preview (head of the output, truncation flag).
    output: Option<(String, bool)>,
    /// Old/new pair for edit-style tools (clustered diff preview).
    edit: Option<(String, String, Option<String>)>,
    expanded: ExpandedFlag,
    lines: Option<(usize, ToolState, bool, Vec<String>)>,
}

impl ToolCall {
    /// A running card for a call.
    pub fn running(name: &str, input: &serde_json::Value, expanded: ExpandedFlag) -> Self {
        let name = sanitize_terminal(name).into_owned();
        let args = args_summary(&name, input);
        let edit = DIFF_TOOLS.contains(&name.as_str()).then(|| {
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .map(|s| sanitize_terminal(s).into_owned())
                .unwrap_or_default();
            let new = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .map(|s| sanitize_terminal(s).into_owned())
                .unwrap_or_default();
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| sanitize_terminal(s).into_owned());
            (old, new, path)
        });
        Self {
            name,
            args,
            state: ToolState::Running,
            output: None,
            edit,
            expanded,
            lines: None,
        }
    }

    /// Record the end state and the bounded output preview.
    pub fn finish(&mut self, is_error: bool, output: Option<(String, bool)>) {
        self.state = if is_error {
            ToolState::Failed
        } else {
            ToolState::Done
        };
        self.output = output;
        self.lines = None;
    }

    /// True when the collapsed body hides content (footer hint).
    pub fn has_hidden(&self) -> bool {
        self.output
            .as_ref()
            .map(|(text, _)| {
                !self.expanded.get() && width::wrap_line(text, 80).len() > OUTCOME_MAX_LINES
            })
            .unwrap_or(false)
    }

    fn header(&self) -> String {
        let theme = theme::current();
        let (dot, dot_token) = match self.state {
            ToolState::Running => ("●", Token::Text),
            ToolState::Done => ("●", Token::Success),
            ToolState::Failed => ("✗", Token::Error),
        };
        let verb = verb(&self.name, self.state);
        let mut line = format!(
            "{} {} {}",
            theme.paint(dot_token, dot),
            theme.bold(Token::Primary, &verb),
            theme.bold(Token::Primary, &self.name)
        );
        if !self.args.is_empty() {
            line.push_str(&theme.paint(Token::TextDim, &format!(" ({})", self.args)));
        }
        line
    }

    fn body(&self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        // Edit-style calls lead with the clustered diff preview.
        if let Some((old, new, path)) = &self.edit {
            let incomplete = self.state == ToolState::Running;
            let budget = if self.expanded.get() { 200 } else { 10 };
            let mut rows = diff::render(old, new, path.as_deref(), incomplete, budget);
            if rows.is_empty() {
                // Oversized edit: summarize instead of computing a diff.
                rows.push(theme.paint(Token::TextDim, "  (edit too large for an inline preview)"));
            }
            // A failed edit's reason lives in the output preview; the
            // diff alone would hide why the apply did not land.
            if self.state == ToolState::Failed
                && let Some((text, _)) = &self.output
            {
                let reason = text.lines().next().unwrap_or("edit failed");
                rows.push(theme.paint(Token::Error, &format!("  ✗ {reason}")));
            }
            return rows;
        }
        let Some((text, truncated)) = &self.output else {
            return Vec::new();
        };
        if text.contains("<system-reminder>") {
            return Vec::new(); // metadata envelope: never rendered
        }
        let expanded = self.expanded.get();
        let budget = columns.saturating_sub(4);
        let mut rows: Vec<String> = Vec::new();
        for line in text.lines() {
            for wrapped in width::wrap_line(line, budget) {
                rows.push(format!("  {}", theme.paint(Token::TextDim, &wrapped)));
                if !expanded && rows.len() > OUTCOME_MAX_LINES {
                    break;
                }
                if expanded && rows.len() >= MAX_EXPANDED_LINES {
                    break;
                }
            }
            if rows.len()
                >= (if expanded {
                    MAX_EXPANDED_LINES
                } else {
                    OUTCOME_MAX_LINES + 1
                })
            {
                break;
            }
        }
        if expanded || rows.len() <= OUTCOME_MAX_LINES {
            if *truncated && expanded {
                rows.push(theme.paint(Token::TextDim, "  … (output truncated)"));
            }
            return rows;
        }
        let kept: Vec<String> = rows[..OUTCOME_MAX_LINES].to_vec();
        let mut out = kept;
        out.push(theme.paint(
            Token::TextDim,
            &format!(
                "  … ({} more lines, ctrl+o to expand)",
                text.lines().count().saturating_sub(OUTCOME_MAX_LINES)
            ),
        ));
        out
    }
}

impl Component for ToolCall {
    fn render(&mut self, columns: usize) -> Vec<String> {
        let expanded = self.expanded.get();
        if let Some((cached_columns, cached_state, cached_expanded, lines)) = &self.lines
            && *cached_columns == columns
            && *cached_state == self.state
            && *cached_expanded == expanded
        {
            return lines.clone();
        }
        let mut lines = vec![self.header()];
        lines.extend(self.body(columns));
        lines.push(String::new());
        self.lines = Some((columns, self.state, expanded, lines.clone()));
        lines
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use tui_engine::width::strip_ansi;

    #[test]
    fn args_summary_picks_key_fields() {
        let input = serde_json::json!({"path": "/a/b/c.rs"});
        assert_eq!(args_summary("read", &input), "/a/b/c.rs");
        let input = serde_json::json!({"command": "npm test"});
        assert_eq!(args_summary("shell", &input), "npm test");
        let input = serde_json::json!({"pattern": "TODO"});
        assert_eq!(args_summary("grep", &input), "“TODO”");
    }

    #[test]
    fn deep_paths_collapse_to_tail() {
        let long = "/very/deep/tree/with/many/segments/and/a/really/long/filename.rs";
        let input = serde_json::json!({"path": long});
        let summary = args_summary("read", &input);
        assert!(summary.starts_with("…/"), "{summary}");
        assert!(width::width(&summary) <= MAX_ARG_LENGTH);
    }

    #[test]
    fn verb_switches_with_state() {
        assert_eq!(verb("shell", ToolState::Running), "Running");
        assert_eq!(verb("shell", ToolState::Done), "Ran");
        assert_eq!(verb("read", ToolState::Done), "Read");
        assert_eq!(verb("edit", ToolState::Done), "Edited");
        assert_eq!(verb("edit", ToolState::Running), "Editing");
        assert_eq!(verb("unknown_tool", ToolState::Done), "Used");
        assert_eq!(verb("grep", ToolState::Done), "Searched");
    }

    #[test]
    fn card_states_and_output_body() {
        theme::set(theme::Theme::dark());
        let flag = ExpandedFlag::new();
        let mut card =
            ToolCall::running("shell", &serde_json::json!({"command": "ls"}), flag.clone());
        let lines = card.render(80);
        assert!(
            strip_ansi(&lines[0]).starts_with("● Running shell (ls)"),
            "{lines:?}"
        );

        card.finish(
            false,
            Some(("out1\nout2\nout3\nout4\nout5".to_string(), false)),
        );
        let lines = card.render(80);
        assert!(strip_ansi(&lines[0]).starts_with("● Ran shell"));
        let plain = strip_ansi(&lines[1]);
        assert!(plain.contains("out1"), "{plain}");
        assert!(
            strip_ansi(&lines[4]).contains("more lines"),
            "collapsed hint after 3 rows: {lines:?}"
        );

        flag.toggle();
        let lines = card.render(80);
        let joined: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("out5"), "expanded shows all: {joined}");
    }

    #[test]
    fn edit_cards_render_clustered_diff() {
        theme::set(theme::Theme::dark());
        let input = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "a\nb\nc",
            "new_string": "a\nX\nc",
        });
        let flag = ExpandedFlag::new();
        let mut card = ToolCall::running("edit", &input, flag.clone());
        card.finish(false, Some(("applied".to_string(), false)));
        let lines = card.render(80);
        let plain: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("+1 -1 src/lib.rs"), "diff header: {plain}");
        assert!(plain.contains("+ X"), "added row: {plain}");
        assert!(plain.contains("- b"), "removed row: {plain}");
    }

    #[test]
    fn oversized_edit_degrades_to_summary() {
        theme::set(theme::Theme::dark());
        let big = "x\n".repeat(diff::MAX_DIFF_LINES + 1);
        let input = serde_json::json!({
            "path": "big.rs",
            "old_string": big,
            "new_string": big,
        });
        let mut card = ToolCall::running("edit", &input, ExpandedFlag::new());
        card.finish(false, Some(("applied".to_string(), false)));
        let lines = card.render(80);
        let plain: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("too large for an inline preview"), "{plain}");
        assert!(!plain.contains("+0"), "no empty diff header: {plain}");
    }

    #[test]
    fn failed_edit_shows_reason_under_diff() {
        theme::set(theme::Theme::dark());
        let input = serde_json::json!({
            "path": "a.rs",
            "old_string": "a\nb",
            "new_string": "a\nc",
        });
        let mut card = ToolCall::running("edit", &input, ExpandedFlag::new());
        card.finish(true, Some(("old_string not found".to_string(), false)));
        let lines = card.render(80);
        let plain: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("- b"), "diff still shows: {plain}");
        assert!(plain.contains("old_string not found"), "reason: {plain}");
    }

    #[test]
    fn arg_summary_strips_escapes() {
        theme::set(theme::Theme::dark());
        let input = serde_json::json!({"path": "a\x1b[31mevil.rs"});
        let summary = args_summary("read", &input);
        assert!(!summary.contains('\x1b'), "{summary:?}");
    }

    #[test]
    fn failed_cards_show_error_dot() {
        theme::set(theme::Theme::dark());
        let mut card = ToolCall::running(
            "shell",
            &serde_json::json!({"command": "nope"}),
            ExpandedFlag::new(),
        );
        card.finish(true, Some(("boom".to_string(), false)));
        let lines = card.render(80);
        assert!(strip_ansi(&lines[0]).starts_with("✗"), "{lines:?}");
    }

    #[test]
    fn system_reminder_output_suppressed() {
        theme::set(theme::Theme::dark());
        let mut card = ToolCall::running(
            "read",
            &serde_json::json!({"path": "x"}),
            ExpandedFlag::new(),
        );
        card.finish(
            false,
            Some((
                "<system-reminder>secret</system-reminder>".to_string(),
                false,
            )),
        );
        let lines = card.render(80);
        assert_eq!(lines.len(), 2, "header + spacer only: {lines:?}");
    }
}
