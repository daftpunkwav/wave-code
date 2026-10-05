//! Tool approval dialog: numbered choices with quick-select digits,
//! wrap-around navigation, and Esc = deny.

use super::{Answer, MAX_BODY_LINES};
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};
use wavecode_wire::{ApprovalKind, WireDecision};

/// A tool approval panel.
pub struct ApprovalDialog {
    call_id: String,
    pub(super) title: String,
    /// Pre-rendered display lines (command, diff, file content…).
    body: Vec<String>,
    /// (label, decision) choices.
    choices: Vec<(String, WireDecision)>,
    selected: usize,
}

impl ApprovalDialog {
    /// Build the approval for one parked call. The detail string is the
    /// wire-supplied display payload (already sanitized by the caller);
    /// file-write payloads carry `-`/`+` diff lines for the renderer.
    pub fn new(call_id: String, kind: ApprovalKind, detail: &str) -> Self {
        let title = match kind {
            ApprovalKind::Exec => "Run this command?".to_string(),
            ApprovalKind::Write => "Write this file?".to_string(),
        };
        let mut body: Vec<String> = Vec::new();
        for line in detail.lines().take(MAX_BODY_LINES) {
            body.push(line.to_string());
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
                    "Yes, always allow this exact call for the session".to_string(),
                    WireDecision::AllowAlways,
                ),
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

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
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

    pub(super) fn deny(&self) -> Answer {
        Answer::Approval {
            call_id: self.call_id.clone(),
            decision: WireDecision::Deny {
                reason: "dismissed".to_string(),
            },
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut content = Vec::new();
        content.push(format!(
            "{} {}",
            theme.paint(Token::BorderFocus, "▶"),
            theme.bold(Token::BorderFocus, &self.title)
        ));
        let dollar = theme.paint(Token::Accent, "$");
        for line in &self.body {
            // Diff rows (file-write payloads) keep their sign prefix and
            // take the diff colors; other lines render as `$` commands.
            if let Some(added) = line.strip_prefix('+') {
                content.push(format!(
                    "  {}",
                    theme.style(Token::DiffAdded).paint(&format!("+{added}"))
                ));
            } else if let Some(removed) = line.strip_prefix('-') {
                content.push(format!(
                    "  {}",
                    theme
                        .style(Token::DiffRemoved)
                        .paint(&format!("-{removed}"))
                ));
            } else {
                content.push(format!(
                    "  {} {}",
                    dollar,
                    theme.paint(Token::Text, line.trim_start_matches("$ "))
                ));
            }
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
            "↑/↓ select · 1/2/3 choose · ↵ confirm · esc denies",
        ));
        border::frame(content, columns, theme.style(Token::BorderFocus), None)
    }
}
