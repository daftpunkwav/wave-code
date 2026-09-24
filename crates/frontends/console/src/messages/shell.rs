//! The local shell-command transcript component (`!` REPL commands).
//!
//! Live: a pulsing saw bullet, the `$ command` header, and a scrolling
//! tail of output. Finished: the exit outcome colors the bullet, output
//! collapses to a preview with an elision row, and Ctrl+O expansion is
//! driven through the shared flag like every other expandable block.

use std::time::Instant;

use crate::messages::ExpandedFlag;
use crate::theme::{self, Token};
use tui_engine::component::Component;
use tui_engine::loader::SAW_FRAMES;
use tui_engine::width;

/// Output tail rows shown live.
pub const LIVE_TAIL_LINES: usize = 5;
/// Output preview rows shown when finished and collapsed.
pub const PREVIEW_LINES: usize = 10;
/// Rolling output buffer cap (oldest rows drop and are counted).
const BUFFER_CAP: usize = 400;
/// Frame interval of the running saw pulse in milliseconds.
const RUN_FRAME_INTERVAL: u128 = 130;

/// One captured output row: `(stderr, text)`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellRow {
    stderr: bool,
    text: String,
}

/// The transcript card for one shell command.
pub struct ShellCard {
    command: String,
    rows: Vec<ShellRow>,
    dropped: usize,
    /// `None` while running; `Some(code)` after exit, with `None` code
    /// meaning killed / reaped without an exit code.
    exit: Option<Option<i32>>,
    expanded_flag: ExpandedFlag,
    /// When the command started; the running-pulse phase reference.
    started: Instant,
}

impl ShellCard {
    /// A card for a command that just started.
    pub fn running(command: &str, expanded_flag: ExpandedFlag) -> Self {
        Self {
            command: command.to_string(),
            rows: Vec::new(),
            dropped: 0,
            exit: None,
            expanded_flag,
            started: Instant::now(),
        }
    }

    /// Append one output row. Wire-sourced text is sanitized here: this
    /// is the chokepoint before shell output reaches the terminal.
    pub fn push_output(&mut self, line: &str, stderr: bool) {
        let line = tui_engine::sanitize::sanitize_terminal(line);
        if self.rows.len() >= BUFFER_CAP {
            self.rows.remove(0);
            self.dropped += 1;
        }
        self.rows.push(ShellRow {
            stderr,
            text: line.into_owned(),
        });
    }

    /// Record the exit outcome.
    pub fn finish(&mut self, code: Option<i32>) {
        self.exit = Some(code);
    }

    /// True until [`Self::finish`] runs.
    pub fn is_running(&self) -> bool {
        self.exit.is_none()
    }
}

impl Component for ShellCard {
    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let expanded = self.expanded_flag.get();
        let running = self.is_running();
        let failed = matches!(self.exit, Some(code) if code.is_some_and(|c| c != 0));
        let body_width = columns.saturating_sub(4);

        let mut out = Vec::new();
        let bullet = if running {
            let step = (self.started.elapsed().as_millis() / RUN_FRAME_INTERVAL) as usize;
            let frame = SAW_FRAMES[step % SAW_FRAMES.len()];
            theme.paint(Token::TextDim, &format!("{frame} "))
        } else if failed {
            theme.paint(
                Token::Error,
                &format!("{} ", crate::chrome::symbols::FAILED),
            )
        } else {
            theme.paint(
                Token::Success,
                &format!("{} ", crate::chrome::symbols::DONE),
            )
        };
        out.push(format!(
            "{bullet}{}",
            theme.paint(Token::ShellMode, &format!("$ {}", self.command))
        ));

        let budget = if running {
            LIVE_TAIL_LINES
        } else if expanded {
            self.rows.len()
        } else {
            PREVIEW_LINES
        };
        let start = self.rows.len().saturating_sub(budget);
        if self.dropped > 0 || start > 0 {
            let hidden = self.dropped + start;
            out.push(format!(
                "  {}",
                theme.paint(Token::TextDim, &format!("… (+{hidden} lines)"))
            ));
        }
        for row in &self.rows[start..] {
            let token = if failed && row.stderr {
                Token::Error
            } else {
                Token::TextDim
            };
            for (index, line) in width::wrap_line(&row.text, body_width)
                .into_iter()
                .enumerate()
            {
                if index == 0 {
                    out.push(format!("  {}", theme.paint(token, &line)));
                } else {
                    out.push(format!("    {}", theme.paint(token, &line)));
                }
            }
        }
        if running {
            out.push(format!(
                "  {}",
                theme.paint(Token::TextDim, "(esc to cancel)")
            ));
        } else {
            match self.exit {
                Some(Some(code)) if code != 0 => {
                    out.push(format!(
                        "  {}",
                        theme.paint(Token::Error, &format!("exit {code}"))
                    ));
                }
                Some(None) => {
                    out.push(format!("  {}", theme.paint(Token::Error, "terminated")));
                }
                _ => {}
            }
        }
        out.push(String::new());
        out
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
    fn running_card_shows_tail_and_cancel_hint() {
        theme::set(theme::Theme::synthwave());
        let mut card = ShellCard::running("echo hi", ExpandedFlag::new());
        for i in 0..8 {
            card.push_output(&format!("line {i}"), false);
        }
        let lines = card.render(60);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(plain[0].contains("$ echo hi"), "{plain:?}");
        assert!(
            plain[1].contains("… (+3 lines)"),
            "elision first: {plain:?}"
        );
        assert!(plain[2].contains("line 3"), "tail starts at 3: {plain:?}");
        assert!(
            plain[plain.len() - 3].contains("line 7"),
            "last tail: {plain:?}"
        );
        assert!(plain.last().unwrap().is_empty(), "spacer: {plain:?}");
        assert!(
            plain.iter().any(|l| l.contains("(esc to cancel)")),
            "{plain:?}"
        );
        assert!(
            !plain.iter().any(|l| l.contains("line 0")),
            "no head: {plain:?}"
        );
    }

    #[test]
    fn finished_card_collapses_with_exit_code() {
        theme::set(theme::Theme::synthwave());
        let mut card = ShellCard::running("false", ExpandedFlag::new());
        for i in 0..30 {
            card.push_output(&format!("out {i}"), false);
        }
        card.finish(Some(2));
        let lines = card.render(60);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(plain[0].contains("✗"), "error bullet: {plain:?}");
        assert!(
            plain.iter().any(|l| l.contains("… (+20 lines)")),
            "elision row: {plain:?}"
        );
        assert!(plain.iter().any(|l| l.contains("exit 2")), "{plain:?}");
        assert!(!plain.iter().any(|l| l.contains("esc to cancel")));
    }

    #[test]
    fn success_keeps_success_bullet_and_no_exit_row() {
        theme::set(theme::Theme::synthwave());
        let mut card = ShellCard::running("true", ExpandedFlag::new());
        card.push_output("done", false);
        card.finish(Some(0));
        let lines = card.render(60);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(plain[0].starts_with("● "), "{plain:?}");
        assert!(plain.iter().any(|l| l.contains("done")));
        assert!(!plain.iter().any(|l| l.contains("exit")));
    }

    #[test]
    fn expansion_shows_whole_buffer() {
        theme::set(theme::Theme::synthwave());
        let flag = ExpandedFlag::new();
        let mut card = ShellCard::running("boom", flag.clone());
        for i in 0..30 {
            card.push_output(&format!("out {i}"), false);
        }
        card.finish(Some(1));
        let collapsed: Vec<String> = card.render(60).iter().map(|l| strip_ansi(l)).collect();
        assert!(!collapsed.iter().any(|l| l.contains("out 0")));
        flag.toggle();
        let expanded_lines: Vec<String> = card.render(60).iter().map(|l| strip_ansi(l)).collect();
        assert!(expanded_lines.iter().any(|l| l.contains("out 0")));
    }

    #[test]
    fn output_is_sanitized() {
        theme::set(theme::Theme::synthwave());
        let mut card = ShellCard::running("evil", ExpandedFlag::new());
        card.push_output("a\x1b[2Jb", false);
        let lines = card.render(60);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(plain.iter().any(|l| l.contains("ab")), "{plain:?}");
        assert!(
            !lines.iter().any(|l| l.contains("\x1b[2J")),
            "injected CSI must be stripped: {lines:?}"
        );
    }
}
