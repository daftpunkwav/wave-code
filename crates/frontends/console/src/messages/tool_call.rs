//! Tool call transcript cards: state dot, verb, tool name, key
//! argument, result chip, and a collapsible output body.
//!
//! Cards carry no borders (indented rows, matching the reference);
//! the shared Ctrl+O flag expands the result body. Per-tool argument
//! summaries come from [`args_summary`].

use std::sync::Arc;
use std::time::Instant;

use crate::diff;
use crate::messages::ExpandedFlag;
use crate::settings::{EditDisplay, ToolDisplay};
use crate::theme::{self, Token};
use tui_engine::component::{Component, Segment};
use tui_engine::sanitize::sanitize_terminal;
use tui_engine::width;

/// Argument values truncate to this length (head/tail aware).
pub const MAX_ARG_LENGTH: usize = 60;
/// Collapsed result shows at most this many lines.
pub const OUTCOME_MAX_LINES: usize = 3;
/// Expanded result renders at most this many wrapped lines.
pub const MAX_EXPANDED_LINES: usize = 200;
/// Expanded input parameters render at most this many lines.
const INPUT_MAX_LINES: usize = 30;

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
/// Names follow the real registry (`read`/`edit`/`web_fetch`/...,
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
        "read" | "write" | "edit" | "view" | "present" => pick(&["path"]),
        "shell" => pick(&["command", "cmd"]),
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
        "grep" | "glob" | "web_search" => "Searching",
        "shell" | "python" | "node" => "Running",
        "web_fetch" => "Fetching",
        "present" => "Presenting",
        _ => "Using",
    };
    let past = match name {
        "read" | "view" => "Read",
        "write" => "Wrote",
        "edit" => "Edited",
        "grep" | "glob" | "web_search" => "Searched",
        "shell" | "python" | "node" => "Ran",
        "web_fetch" => "Fetched",
        "present" => "Presented",
        _ => "Used",
    };
    match state {
        ToolState::Running => present.to_string(),
        _ => past.to_string(),
    }
}

/// The running-tool dot pulses a saw amplitude in one cell: climbs,
/// drops back, climbs again — machine work in a single glyph.
const RUN_FRAMES: [&str; 4] = ["▁", "▃", "▅", "▇"];
/// Frame interval of the running-tool animation in milliseconds.
const RUN_FRAME_INTERVAL: u128 = 130;

/// The saw-ramp frame for `elapsed`.
fn run_frame(elapsed: std::time::Duration) -> &'static str {
    let step = (elapsed.as_millis() / RUN_FRAME_INTERVAL) as usize;
    RUN_FRAMES[step % RUN_FRAMES.len()]
}

/// A tool call card.
pub struct ToolCall {
    name: String,
    args: String,
    /// The full sanitized input, kept for the expanded/Full views.
    input: Option<serde_json::Value>,
    state: ToolState,
    /// Result preview (head of the output, truncation flag).
    output: Option<(String, bool)>,
    /// Old/new pair for edit-style tools (clustered diff preview).
    edit: Option<(String, String, Option<String>)>,
    expanded: ExpandedFlag,
    settings: crate::settings::SharedSettings,
    started: Instant,
    lines: Option<(usize, ToolState, bool, usize, Segment)>,
}

/// Per-tool output styling for one (possibly wrapped) line. Grep match
/// lines light up their path and line number; anything else — and any
/// wrap segment too broken to parse — stays dim body text.
fn style_output_line(tool: &str, line: &str) -> String {
    let theme = theme::current();
    let body = theme.style(Token::TextDim);
    if tool.eq_ignore_ascii_case("grep")
        && let Some((path, rest)) = line.split_once(':')
        && let Some((number, content)) = rest.split_once(':')
        && !path.is_empty()
        && !path.contains(char::is_whitespace)
        && number.chars().all(|c| c.is_ascii_digit())
    {
        return format!(
            "  {}{}{}{}",
            theme.style(Token::Text).paint(path),
            body.paint(":"),
            theme.style(Token::Accent).paint(number),
            body.paint(&format!(":{content}")),
        );
    }
    format!("  {}", body.paint(line))
}

impl ToolCall {
    /// A running card for a call.
    pub fn running(
        name: &str,
        input: &serde_json::Value,
        expanded: ExpandedFlag,
        settings: crate::settings::SharedSettings,
    ) -> Self {
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
        // Keep a sanitized copy of the full input for the expanded view.
        let input = sanitize_terminal(&input.to_string()).into_owned();
        let input = serde_json::from_str(&input).ok();
        Self {
            name,
            args,
            input,
            state: ToolState::Running,
            output: None,
            edit,
            expanded,
            settings,
            started: Instant::now(),
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
        // Saw-ramp window while the machine works; result dots on finish.
        let (dot, dot_token) = match self.state {
            ToolState::Running => (run_frame(self.started.elapsed()).to_string(), Token::Text),
            ToolState::Done => (crate::chrome::symbols::DONE.to_string(), Token::Success),
            ToolState::Failed => (crate::chrome::symbols::FAILED.to_string(), Token::Error),
        };
        let verb = verb(&self.name, self.state);
        let mut line = format!(
            "{} {} {}",
            theme.paint(dot_token, &dot),
            theme.bold(Token::TextStrong, &verb),
            theme.bold(Token::TextStrong, &self.name)
        );
        // Names verbosity stops right after the tool name.
        if !self.args.is_empty() && self.settings.get().tool_display != ToolDisplay::Names {
            line.push_str(&theme.paint(Token::TextDim, &format!(" ({})", self.args)));
        }
        line
    }

    /// Pretty-printed full input, truncated to [`INPUT_MAX_LINES`].
    fn input_lines(&self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let Some(input) = &self.input else {
            return Vec::new();
        };
        let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
        let budget = columns.saturating_sub(4);
        let mut rows = Vec::new();
        rows.push(theme.paint(Token::TextMuted, "  params"));
        let mut shown = 0usize;
        for line in pretty.lines() {
            if shown >= INPUT_MAX_LINES {
                let rest = pretty.lines().count() - shown;
                rows.push(theme.paint(Token::TextMuted, &format!("  … ({rest} more lines)")));
                break;
            }
            for (index, wrapped) in width::wrap_line(line, budget).into_iter().enumerate() {
                if shown >= INPUT_MAX_LINES {
                    rows.push(theme.paint(Token::TextMuted, "  … (truncated)"));
                    return rows;
                }
                let _ = index;
                rows.push(theme.paint(Token::TextMuted, &format!("  {wrapped}")));
                shown += 1;
            }
        }
        rows
    }

    fn body(&self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let view = self.settings.get();
        let show_details = self.expanded.get() || view.tool_display == ToolDisplay::Full;
        // Edit-style calls lead with the clustered diff preview.
        if let Some((old, new, path)) = &self.edit {
            if view.edit_display == EditDisplay::Tool && !show_details {
                return Vec::new();
            }
            let incomplete = self.state == ToolState::Running;
            let budget = if show_details { 200 } else { 10 };
            let mut rows = diff::render(old, new, path.as_deref(), incomplete, budget, columns);
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
                rows.push(theme.paint(
                    Token::Error,
                    &format!("  {} {reason}", crate::chrome::symbols::FAILED),
                ));
            }
            return rows;
        }
        let mut rows = Vec::new();
        // The full call parameters, shown expanded or at Full verbosity.
        if show_details {
            rows.extend(self.input_lines(columns));
        }
        let Some((text, truncated)) = &self.output else {
            return rows;
        };
        if text.contains("<system-reminder>") {
            return Vec::new(); // metadata envelope: never rendered
        }
        let budget = columns.saturating_sub(4);
        let mut out_rows: Vec<String> = Vec::new();
        for line in text.lines() {
            for wrapped in width::wrap_line(line, budget) {
                out_rows.push(style_output_line(&self.name, &wrapped));
                if !show_details && out_rows.len() > OUTCOME_MAX_LINES {
                    break;
                }
                if show_details && out_rows.len() >= MAX_EXPANDED_LINES {
                    break;
                }
            }
            if out_rows.len()
                >= (if show_details {
                    MAX_EXPANDED_LINES
                } else {
                    OUTCOME_MAX_LINES + 1
                })
            {
                break;
            }
        }
        if show_details || out_rows.len() <= OUTCOME_MAX_LINES {
            if *truncated && show_details {
                out_rows.push(theme.paint(Token::TextDim, "  … (output truncated)"));
            }
            rows.extend(out_rows);
            return rows;
        }
        let kept: Vec<String> = out_rows[..OUTCOME_MAX_LINES].to_vec();
        rows.extend(kept);
        rows.push(theme.paint(
            Token::TextDim,
            &format!(
                "  … ({} more lines, ctrl+o to expand)",
                text.lines().count().saturating_sub(OUTCOME_MAX_LINES)
            ),
        ));
        rows
    }
}

impl Component for ToolCall {
    fn render(&mut self, columns: usize) -> Segment {
        let expanded = self.expanded.get();
        // The running animation is part of the cache key so live ticks
        // repaint; finished cards pin frame 0 forever.
        let frame = if self.state == ToolState::Running {
            (self.started.elapsed().as_millis() / RUN_FRAME_INTERVAL) as usize
        } else {
            0
        };
        if let Some((cached_columns, cached_state, cached_expanded, cached_frame, lines)) =
            &self.lines
            && *cached_columns == columns
            && *cached_state == self.state
            && *cached_expanded == expanded
            && *cached_frame == frame
        {
            return Arc::clone(lines);
        }
        let mut lines = vec![self.header()];
        lines.extend(self.body(columns));
        lines.push(String::new());
        let lines = Arc::new(lines);
        self.lines = Some((columns, self.state, expanded, frame, Arc::clone(&lines)));
        lines
    }

    fn invalidate(&mut self) {
        self.lines = None;
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
    fn grep_lines_light_up_path_and_number() {
        theme::set(theme::Theme::synthwave());
        let styled = style_output_line("grep", "src/lib.rs:42:pub fn main() {}");
        let plain = strip_ansi(&styled);
        assert_eq!(plain, "  src/lib.rs:42:pub fn main() {}");
        // The path rides a different SGR than the dim body.
        assert!(styled.contains("src/lib.rs\x1b[0m"), "{styled}");
        assert!(styled.contains("42"), "{styled}");
        // Non-grep tools stay dim even for identical shapes.
        let other = style_output_line("read", "src/lib.rs:42:pub fn main() {}");
        assert_eq!(strip_ansi(&other), "  src/lib.rs:42:pub fn main() {}");
        assert_eq!(
            other,
            format!(
                "  {}",
                theme::current()
                    .style(Token::TextDim)
                    .paint("src/lib.rs:42:pub fn main() {}")
            )
        );
    }

    #[test]
    fn malformed_grep_lines_stay_dim() {
        theme::set(theme::Theme::synthwave());
        for line in [
            "no colons here",
            ":42:x",
            "path:notanumber:x",
            "path with space.rs:1:x",
        ] {
            let styled = style_output_line("grep", line);
            assert_eq!(
                styled,
                format!("  {}", theme::current().style(Token::TextDim).paint(line)),
                "{line}"
            );
        }
    }

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
        theme::set(theme::Theme::synthwave());
        let flag = ExpandedFlag::new();
        let mut card = ToolCall::running(
            "shell",
            &serde_json::json!({"command": "ls"}),
            flag.clone(),
            crate::settings::SharedSettings::new(crate::settings::UiSettings::default()),
        );
        let lines = card.render(80);
        assert!(
            strip_ansi(&lines[0]).starts_with("▁ Running shell (ls)"),
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
        theme::set(theme::Theme::synthwave());
        let input = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "a\nb\nc",
            "new_string": "a\nX\nc",
        });
        let flag = ExpandedFlag::new();
        let mut card = ToolCall::running(
            "edit",
            &input,
            flag.clone(),
            crate::settings::SharedSettings::new(crate::settings::UiSettings::default()),
        );
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
        theme::set(theme::Theme::synthwave());
        let big = "x\n".repeat(diff::MAX_DIFF_LINES + 1);
        let input = serde_json::json!({
            "path": "big.rs",
            "old_string": big,
            "new_string": big,
        });
        let mut card = ToolCall::running(
            "edit",
            &input,
            ExpandedFlag::new(),
            crate::settings::SharedSettings::new(crate::settings::UiSettings::default()),
        );
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
        theme::set(theme::Theme::synthwave());
        let input = serde_json::json!({
            "path": "a.rs",
            "old_string": "a\nb",
            "new_string": "a\nc",
        });
        let mut card = ToolCall::running(
            "edit",
            &input,
            ExpandedFlag::new(),
            crate::settings::SharedSettings::new(crate::settings::UiSettings::default()),
        );
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
        theme::set(theme::Theme::synthwave());
        let input = serde_json::json!({"path": "a\x1b[31mevil.rs"});
        let summary = args_summary("read", &input);
        assert!(!summary.contains('\x1b'), "{summary:?}");
    }

    #[test]
    fn failed_cards_show_error_dot() {
        theme::set(theme::Theme::synthwave());
        let mut card = ToolCall::running(
            "shell",
            &serde_json::json!({"command": "nope"}),
            ExpandedFlag::new(),
            crate::settings::SharedSettings::new(crate::settings::UiSettings::default()),
        );
        card.finish(true, Some(("boom".to_string(), false)));
        let lines = card.render(80);
        assert!(strip_ansi(&lines[0]).starts_with("✗"), "{lines:?}");
    }

    #[test]
    fn system_reminder_output_suppressed() {
        theme::set(theme::Theme::synthwave());
        let mut card = ToolCall::running(
            "read",
            &serde_json::json!({"path": "x"}),
            ExpandedFlag::new(),
            crate::settings::SharedSettings::new(crate::settings::UiSettings::default()),
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
