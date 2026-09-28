//! The two-row footer: mode/model/cwd/tip and transient-hint/context.

use crate::state::{AppState, context_percent, format_tokens};
use crate::theme::{self, Token};
use tui_engine::width;

/// Rotating toolbar tips (10 s cadence), weighted rotation kept simple
/// as a fixed order.
pub const TIPS: [&str; 6] = [
    "ctrl+o expand tool output",
    "ctrl+m toggle mermaid diagrams",
    "ctrl+s steer a running turn",
    "! for shell mode",
    "@ to mention files",
    "up arrow recalls history",
];

/// Tip rotation cadence.
pub const TIP_ROTATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Which transient message owns the left side of row 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransientHint {
    /// The exit confirmation (double Ctrl+C / Ctrl+D).
    ExitConfirm,
    /// Nothing transient: row 2 carries only the context meter.
    None,
}

/// Build footer row 1: mode badge, model, shortened cwd, git branch,
/// rotating tip.
pub fn row1(state: &AppState, tip: Option<&str>, columns: usize) -> String {
    let theme = theme::current();
    let mut line = String::new();
    line.push_str(&mode_badge(&state.permission_mode));
    line.push_str("  ");
    line.push_str(&theme.paint(Token::Text, &state.model_name));
    line.push_str("  ");
    line.push_str(&theme.paint(Token::TextDim, &crate::ui::shorten_cwd(&state.cwd, 3)));
    if let Some(branch) = &state.git_branch {
        line.push_str(&theme.paint(
            Token::TextDim,
            &format!(" {} {branch}", crate::chrome::symbols::BRANCH),
        ));
    }
    if let Some(goal) = &state.goal {
        let minutes = (goal.since.elapsed().as_secs() / 60).max(1);
        line.push_str(&theme.paint(Token::Primary, &format!("  goal ● {minutes}m")));
    }
    if let Some(tip) = tip {
        let used = width::width(&line);
        let tip_text = theme.paint(Token::TextMuted, &format!(" | {tip}"));
        let tip_width = width::width(&tip_text);
        if used + tip_width <= columns {
            line.push_str(&" ".repeat(columns - used - tip_width));
            line.push_str(&tip_text);
        }
    }
    line
}

/// Build footer row 2: transient hint (left) and context meter (right).
pub fn row2(state: &AppState, hint: &TransientHint, columns: usize) -> String {
    let theme = theme::current();
    let mut left = String::new();
    if *hint == TransientHint::ExitConfirm {
        left.push_str(&theme.bold(Token::Warning, "Press ctrl+c again to exit"));
    }
    let mut right = String::new();
    if let (Some(used), Some(window)) = (state.context_used, state.context_window) {
        right = theme.paint(
            Token::Text,
            &format!(
                "context: {}% ({}/{})",
                context_percent(used, window),
                format_tokens(used),
                format_tokens(window)
            ),
        );
    }
    let spacing = columns
        .saturating_sub(width::width(&left) + width::width(&right))
        .max(if right.is_empty() { 0 } else { 1 });
    left.push_str(&" ".repeat(spacing));
    left.push_str(&right);
    // A combined row wider than the terminal overflows to the screen
    // layer's hard truncate, which eats the right edge; trimming the
    // left hint instead keeps the context counter visible.
    if width::width(&left) > columns {
        return width::truncate_to_width(&left, columns);
    }
    left
}

/// The tinted-bar mode badge: a colored tick plus the lowercase mode.
pub fn mode_badge(mode: &str) -> String {
    let theme = theme::current();
    let (label, token) = match mode {
        "plan" => ("plan", Token::Primary),
        "wave" => ("wave", Token::Warning),
        _ => ("auto", Token::Text),
    };
    format!("{}{}", theme.bold(token, "▍"), theme.bold(token, label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state() -> AppState {
        let mut state = AppState::new(
            "test-model".to_string(),
            PathBuf::from("/home/user/work/proj"),
            "guarded".to_string(),
            Vec::new(),
        );
        state.context_used = Some(84_000);
        state.context_window = Some(200_000);
        state
    }

    #[test]
    fn row1_carries_mode_model_cwd_and_tip() {
        theme::set(theme::Theme::dark());
        // No HOME mutation: the cwd assertion below holds under any home
        // (shorten_cwd truncation always yields "~/{tail}"), and mutating
        // the process-global env would race concurrent tests.
        let line = row1(&state(), Some("ctrl+o expand tool output"), 120);
        let plain = width::strip_ansi(&line);
        assert!(plain.contains("▍auto"), "{plain}");
        assert!(plain.contains("test-model"), "{plain}");
        assert!(plain.contains("proj"), "{plain}");
        assert!(plain.contains("ctrl+o expand tool output"), "{plain}");
    }

    #[test]
    fn row1_drops_tip_when_narrow() {
        theme::set(theme::Theme::dark());
        let line = row1(&state(), Some("ctrl+o expand tool output"), 40);
        let plain = width::strip_ansi(&line);
        assert!(!plain.contains("ctrl+o expand"), "tip dropped: {plain}");
    }

    #[test]
    fn row1_shows_git_branch_when_known() {
        theme::set(theme::Theme::dark());
        let mut with_branch = state();
        with_branch.git_branch = Some("feat/console-ui".to_string());
        let line = row1(&with_branch, None, 120);
        let plain = width::strip_ansi(&line);
        assert!(plain.contains("⎇ feat/console-ui"), "{plain}");
        let bare = row1(&state(), None, 120);
        assert!(!width::strip_ansi(&bare).contains("⎇"), "{bare}");
    }

    #[test]
    fn row1_shows_goal_badge_while_active() {
        theme::set(theme::Theme::dark());
        let mut with_goal = state();
        with_goal.goal = Some(crate::state::GoalBadge {
            since: std::time::Instant::now(),
        });
        let line = row1(&with_goal, None, 120);
        let plain = width::strip_ansi(&line);
        assert!(plain.contains("goal ●"), "{plain}");
        assert!(plain.contains("1m"), "{plain}");
        let bare = row1(&state(), None, 120);
        assert!(!width::strip_ansi(&bare).contains("goal ●"), "{bare}");
    }

    #[test]
    fn row2_shows_context_and_exit_hint() {
        theme::set(theme::Theme::dark());
        let line = row2(&state(), &TransientHint::None, 80);
        let plain = width::strip_ansi(&line);
        assert!(plain.ends_with("context: 42% (82.0k/195k)"), "{plain:?}");
        let hinted = row2(&state(), &TransientHint::ExitConfirm, 80);
        let plain = width::strip_ansi(&hinted);
        assert!(plain.contains("Press ctrl+c again to exit"), "{plain}");
    }
}
