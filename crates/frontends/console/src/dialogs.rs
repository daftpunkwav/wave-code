//! Modal dialogs: tool approvals and structured questions.
//!
//! While a dialog is open it owns all key input. Approvals offer
//! numbered choices with quick-select digits, wrap-around navigation,
//! and Esc = deny. Questions mirror the layout for option answers;
//! free-text answers route through the embedded one-line input.

use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};
use tui_engine::width::{self};
use wavecode_wire::{ApprovalKind, WireDecision};

/// Body blocks show at most this many lines.
pub const MAX_BODY_LINES: usize = 10;

/// The user's answer to a dialog.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// An approval decision for a parked call.
    Approval {
        /// Tool call id.
        call_id: String,
        /// The decision.
        decision: WireDecision,
    },
    /// An answer to a parked question.
    Question {
        /// Tool call id.
        call_id: String,
        /// Chosen option or free text (empty = dismissed).
        answer: String,
    },
}

/// Which dialog is showing.
pub enum Dialog {
    /// A tool approval.
    Approval(ApprovalDialog),
    /// A structured question.
    Question(QuestionDialog),
}

impl Dialog {
    /// Title line for the panel.
    pub fn title(&self) -> String {
        match self {
            Self::Approval(dialog) => dialog.title.clone(),
            Self::Question(dialog) => dialog.title.clone(),
        }
    }

    /// Handle one key; `Some(Answer)` when the dialog resolved.
    pub fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match self {
            Self::Approval(dialog) => dialog.handle_key(event),
            Self::Question(dialog) => dialog.handle_key(event),
        }
    }

    /// Render the dialog box lines at `width`.
    pub fn render(&mut self, width: usize) -> Vec<String> {
        match self {
            Self::Approval(dialog) => dialog.render(width),
            Self::Question(dialog) => dialog.render(width),
        }
    }
}

/// A tool approval panel.
pub struct ApprovalDialog {
    call_id: String,
    title: String,
    /// Pre-rendered display lines (command, diff, file content…).
    body: Vec<String>,
    /// (label, decision) choices.
    choices: Vec<(String, WireDecision)>,
    selected: usize,
}

impl ApprovalDialog {
    /// Build the approval for one parked call. The detail string is the
    /// wire-supplied display payload (already sanitized by the caller).
    pub fn new(call_id: String, kind: ApprovalKind, detail: &str) -> Self {
        let title = match kind {
            ApprovalKind::Exec => "Run this command?".to_string(),
            ApprovalKind::Write => "Write this file?".to_string(),
        };
        let mut body: Vec<String> = Vec::new();
        for line in detail.lines().take(MAX_BODY_LINES) {
            body.push(format!("$ {line}"));
        }
        if detail.lines().count() > MAX_BODY_LINES {
            body.push(format!(
                "… ({} more lines)",
                detail.lines().count() - MAX_BODY_LINES
            ));
        }
        Self {
            call_id,
            title,
            body,
            choices: vec![
                ("Yes, allow once".to_string(), WireDecision::AllowOnce),
                (
                    "No, and tell the agent what to do differently".to_string(),
                    WireDecision::Deny {
                        reason: "the user declined this action".to_string(),
                    },
                ),
            ],
            selected: 0,
        }
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Up => {
                if self.selected == 0 {
                    self.selected = self.choices.len() - 1;
                } else {
                    self.selected -= 1;
                }
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.choices.len();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                if let Ok(digit) = c.to_string().parse::<usize>()
                    && digit >= 1
                    && digit <= self.choices.len()
                {
                    self.selected = digit - 1;
                    return self.resolve();
                }
                None
            }
            Key::Enter => self.resolve(),
            Key::Esc => Some(self.deny()),
            _ => None,
        }
    }

    fn resolve(&self) -> Option<Answer> {
        let (_, decision) = self.choices[self.selected].clone();
        Some(Answer::Approval {
            call_id: self.call_id.clone(),
            decision,
        })
    }

    fn deny(&self) -> Answer {
        Answer::Approval {
            call_id: self.call_id.clone(),
            decision: WireDecision::Deny {
                reason: "dismissed".to_string(),
            },
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut content = Vec::new();
        content.push(format!(
            "{} {}",
            theme.paint(Token::BorderFocus, "▶"),
            theme.bold(Token::BorderFocus, &self.title)
        ));
        for line in &self.body {
            let dollar = theme.paint(Token::Accent, "$");
            content.push(format!(
                "  {} {}",
                dollar,
                theme.paint(Token::Text, line.trim_start_matches("$ "))
            ));
        }
        content.push(String::new());
        for (index, (label, _)) in self.choices.iter().enumerate() {
            let number = index + 1;
            if index == self.selected {
                content.push(format!(
                    "{} {}",
                    theme.bold(Token::Accent, &format!("▶ {number}.")),
                    theme.bold(Token::TextStrong, label)
                ));
            } else {
                content.push(format!(
                    "  {} {}",
                    theme.paint(Token::TextDim, &format!("{number}.")),
                    theme.paint(Token::Text, label)
                ));
            }
        }
        content.push(theme.paint(
            Token::TextDim,
            "↑/↓ select · 1/2 choose · ↵ confirm · esc denies",
        ));
        border::frame(content, columns, theme.style(Token::BorderFocus), None)
    }
}

/// A structured question with numbered options; free text supported.
pub struct QuestionDialog {
    call_id: String,
    title: String,
    options: Vec<String>,
    selected: usize,
    /// Free-text buffer (activated by typing when options are empty or
    /// via the `Other` choice).
    pub free_text: String,
}

impl QuestionDialog {
    /// Build the question dialog (wire supplies question + options).
    pub fn new(call_id: String, question: &str, options: Vec<String>) -> Self {
        Self {
            call_id,
            title: question.to_string(),
            options,
            selected: 0,
            free_text: String::new(),
        }
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Up if !self.options.is_empty() => {
                if self.selected == 0 {
                    self.selected = self.options.len() - 1;
                } else {
                    self.selected -= 1;
                }
                None
            }
            Key::Down if !self.options.is_empty() => {
                self.selected = (self.selected + 1) % self.options.len();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                if let Ok(digit) = c.to_string().parse::<usize>()
                    && digit >= 1
                    && digit <= self.options.len()
                {
                    self.selected = digit - 1;
                    return Some(self.answer_selected());
                }
                // Free-text entry: any other printable character.
                self.free_text.push(c);
                None
            }
            Key::Backspace => {
                self.free_text.pop();
                None
            }
            Key::Enter => {
                if !self.free_text.trim().is_empty() {
                    return Some(Answer::Question {
                        call_id: self.call_id.clone(),
                        answer: self.free_text.trim().to_string(),
                    });
                }
                if !self.options.is_empty() {
                    return Some(self.answer_selected());
                }
                Some(Answer::Question {
                    call_id: self.call_id.clone(),
                    answer: String::new(),
                })
            }
            Key::Esc => Some(Answer::Question {
                call_id: self.call_id.clone(),
                answer: String::new(),
            }),
            _ => None,
        }
    }

    fn answer_selected(&self) -> Answer {
        Answer::Question {
            call_id: self.call_id.clone(),
            answer: self.options.get(self.selected).cloned().unwrap_or_default(),
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut content = Vec::new();
        content.push(format!(
            "{} {}",
            theme.paint(Token::BorderFocus, "▶"),
            theme.bold(
                Token::BorderFocus,
                &width::truncate_to_width(&self.title, columns.saturating_sub(6))
            )
        ));
        for (index, option) in self.options.iter().enumerate() {
            let number = index + 1;
            if index == self.selected {
                content.push(format!(
                    "{} {}",
                    theme.bold(Token::Accent, &format!("▶ {number}.")),
                    theme.bold(Token::TextStrong, option)
                ));
            } else {
                content.push(format!(
                    "  {} {}",
                    theme.paint(Token::TextDim, &format!("{number}.")),
                    theme.paint(Token::Text, option)
                ));
            }
        }
        let typed = format!("Other: {}▏", self.free_text);
        content.push(theme.paint(Token::TextDim, &typed));
        content.push(theme.paint(
            Token::TextDim,
            "type for free text · ↵ answers · esc dismisses",
        ));
        border::frame(content, columns, theme.style(Token::BorderFocus), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use tui_engine::width::strip_ansi;

    #[test]
    fn approval_quick_select_and_enter() {
        theme::set(theme::Theme::dark());
        let mut dialog = ApprovalDialog::new("c1".to_string(), ApprovalKind::Exec, "npm test");
        let answer = dialog.handle_key(KeyEvent::plain(Key::Char('1')));
        match answer {
            Some(Answer::Approval { call_id, decision }) => {
                assert_eq!(call_id, "c1");
                assert_eq!(decision, WireDecision::AllowOnce);
            }
            other => panic!("expected approval: {other:?}"),
        }
    }

    #[test]
    fn approval_escape_denies() {
        theme::set(theme::Theme::dark());
        let mut dialog = ApprovalDialog::new("c2".to_string(), ApprovalKind::Write, "write x");
        match dialog.handle_key(KeyEvent::plain(Key::Esc)) {
            Some(Answer::Approval {
                decision: WireDecision::Deny { .. },
                ..
            }) => {}
            other => panic!("expected deny: {other:?}"),
        }
    }

    #[test]
    fn approval_selection_moves_and_enters() {
        theme::set(theme::Theme::dark());
        let mut dialog = ApprovalDialog::new("c3".to_string(), ApprovalKind::Exec, "ls");
        dialog.handle_key(KeyEvent::plain(Key::Down));
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        assert!(matches!(
            answer,
            Some(Answer::Approval {
                decision: WireDecision::Deny { .. },
                ..
            })
        ));
    }

    #[test]
    fn question_options_and_free_text() {
        theme::set(theme::Theme::dark());
        let mut dialog = QuestionDialog::new(
            "q1".to_string(),
            "Pick one",
            vec!["alpha".to_string(), "beta".to_string()],
        );
        let answer = dialog.handle_key(KeyEvent::plain(Key::Char('2')));
        assert!(matches!(
            answer,
            Some(Answer::Question { answer, .. }) if answer == "beta"
        ));

        let mut dialog = QuestionDialog::new("q2".to_string(), "Why?", Vec::new());
        for c in "because".chars() {
            dialog.handle_key(KeyEvent::plain(Key::Char(c)));
        }
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        assert!(matches!(
            answer,
            Some(Answer::Question { answer, .. }) if answer == "because"
        ));
    }

    #[test]
    fn approval_renders_focus_frame() {
        theme::set(theme::Theme::dark());
        let mut dialog = ApprovalDialog::new("c4".to_string(), ApprovalKind::Exec, "echo hi");
        let lines = dialog.render(70);
        let joined: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Run this command?"), "{joined}");
        assert!(joined.contains("$ echo hi"), "{joined}");
        assert!(joined.contains("Yes, allow once"), "{joined}");
        assert!(strip_ansi(&lines[0]).starts_with('╭'));
    }
}
