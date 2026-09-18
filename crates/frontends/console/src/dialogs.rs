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
    /// A model picked in the model selector.
    ModelSelected {
        /// Display label of the entry.
        label: String,
        /// Provider id the entry samples through.
        provider: String,
        /// Wire model name.
        model: String,
        /// Chosen reasoning-effort level (`None` = off/unsupported).
        effort: Option<String>,
        /// True when the choice applies to this session only (Alt+S).
        session_only: bool,
    },
    /// A permission mode picked in the permission selector.
    PermissionSelected {
        /// Mode wire name (`plan` / `auto` / `wave`).
        mode: String,
    },
    /// A session picked in the session picker (resume).
    ResumeSession {
        /// Session id to resume.
        id: String,
    },
    /// The dialog closed without producing an answer (settings).
    Dismissed,
}

/// Which dialog is showing.
pub enum Dialog {
    /// A tool approval.
    Approval(ApprovalDialog),
    /// A structured question.
    Question(QuestionDialog),
    /// The interactive settings panel.
    Settings(SettingsDialog),
    /// The model selector (`/model`).
    Model(ModelPickerDialog),
    /// The permission-mode selector (`/permissions`).
    Permissions(PermissionPickerDialog),
    /// The scrollable help panel (`/help`).
    Help(HelpPanel),
    /// The session picker (`/sessions`).
    Sessions(SessionPickerDialog),
}

impl Dialog {
    /// Title line for the panel.
    pub fn title(&self) -> String {
        match self {
            Self::Approval(dialog) => dialog.title.clone(),
            Self::Question(dialog) => dialog.title.clone(),
            Self::Settings(dialog) => dialog.title(),
            Self::Model(dialog) => dialog.title.clone(),
            Self::Permissions(dialog) => dialog.title.clone(),
            Self::Help(dialog) => dialog.title.clone(),
            Self::Sessions(dialog) => dialog.title.clone(),
        }
    }

    /// Handle one key; `Some(Answer)` when the dialog resolved.
    pub fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match self {
            Self::Approval(dialog) => dialog.handle_key(event),
            Self::Question(dialog) => dialog.handle_key(event),
            Self::Settings(dialog) => dialog.handle_key(event),
            Self::Model(dialog) => dialog.handle_key(event),
            Self::Permissions(dialog) => dialog.handle_key(event),
            Self::Help(dialog) => dialog.handle_key(event),
            Self::Sessions(dialog) => dialog.handle_key(event),
        }
    }

    /// Render the dialog box lines at `width`.
    pub fn render(&mut self, width: usize) -> Vec<String> {
        match self {
            Self::Approval(dialog) => dialog.render(width),
            Self::Question(dialog) => dialog.render(width),
            Self::Settings(dialog) => dialog.render(width),
            Self::Model(dialog) => dialog.render(width),
            Self::Permissions(dialog) => dialog.render(width),
            Self::Help(dialog) => dialog.render(width),
            Self::Sessions(dialog) => dialog.render(width),
        }
    }

    /// The dismissal answer (Esc equivalent): deny for approvals,
    /// empty answer for questions.
    pub fn dismiss(&self) -> Answer {
        match self {
            Self::Approval(dialog) => dialog.deny(),
            Self::Question(dialog) => Answer::Question {
                call_id: dialog.call_id.clone(),
                answer: String::new(),
            },
            _ => Answer::Dismissed,
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
            "↑/↓ select · 1/2/3 choose · ↵ confirm · esc denies",
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
    use tui_engine::keys::Mods;
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
        // One Down lands on the middle "always allow" choice.
        dialog.handle_key(KeyEvent::plain(Key::Down));
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        assert!(matches!(
            answer,
            Some(Answer::Approval {
                decision: WireDecision::AllowAlways,
                ..
            })
        ));
        // Two Downs land on deny.
        let mut dialog = ApprovalDialog::new("c3b".to_string(), ApprovalKind::Exec, "ls");
        dialog.handle_key(KeyEvent::plain(Key::Down));
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
    fn approval_quick_select_always() {
        theme::set(theme::Theme::dark());
        let mut dialog = ApprovalDialog::new("c5".to_string(), ApprovalKind::Exec, "npm test");
        let answer = dialog.handle_key(KeyEvent::plain(Key::Char('2')));
        match answer {
            Some(Answer::Approval {
                decision: WireDecision::AllowAlways,
                ..
            }) => {}
            other => panic!("expected allow-always: {other:?}"),
        }
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

    fn picker_entries() -> Vec<ModelEntryView> {
        vec![
            ModelEntryView {
                label: "deepseek-chat".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
                effort: None,
            },
            ModelEntryView {
                label: "deepseek-reasoner".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-reasoner".to_string(),
                effort: Some("high".to_string()),
            },
            ModelEntryView {
                label: "MiniMax-M3".to_string(),
                provider: "minimax".to_string(),
                model: "MiniMax-M3".to_string(),
                effort: None,
            },
        ]
    }

    #[test]
    fn model_picker_search_filters_and_enter_resolves() {
        theme::set(theme::Theme::dark());
        let mut dialog = ModelPickerDialog::new(
            picker_entries(),
            "deepseek-chat".to_string(),
            None,
            vec!["off".to_string(), "low".to_string(), "high".to_string()],
        );
        // Type to filter down to the reasoner entry.
        for c in "deepseek-re".chars() {
            dialog.handle_key(KeyEvent::plain(Key::Char(c)));
        }
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        match answer {
            Some(Answer::ModelSelected { label, model, .. }) => {
                assert_eq!(label, "deepseek-reasoner");
                assert_eq!(model, "deepseek-reasoner");
            }
            other => panic!("expected model selection: {other:?}"),
        }
    }

    #[test]
    fn fuzzy_filter_ranks_lower_scores_first() {
        // `fuzzy::score` ranks lower scores higher: the adjacent match
        // ("ma") must surface before the gappy one ("ama").
        let entries = vec![
            ModelEntryView {
                label: "ama".to_string(),
                provider: "p".to_string(),
                model: "ama".to_string(),
                effort: None,
            },
            ModelEntryView {
                label: "ma".to_string(),
                provider: "p".to_string(),
                model: "ma".to_string(),
                effort: None,
            },
        ];
        assert_eq!(filter_indices(&entries, "ma"), vec![1, 0]);
    }

    #[test]
    fn model_picker_alt_s_marks_session_only() {
        theme::set(theme::Theme::dark());
        let mut dialog =
            ModelPickerDialog::new(picker_entries(), "none".to_string(), None, Vec::new());
        // No thinking levels: effort stays None.
        let answer = dialog.handle_key(KeyEvent::new(
            Key::Char('s'),
            Mods {
                ctrl: false,
                alt: true,
                shift: false,
            },
        ));
        assert!(matches!(
            answer,
            Some(Answer::ModelSelected {
                session_only: true,
                effort: None,
                ..
            })
        ));
    }

    #[test]
    fn model_picker_thinking_row_switches_with_arrows() {
        theme::set(theme::Theme::dark());
        let mut dialog = ModelPickerDialog::new(
            picker_entries(),
            "deepseek-chat".to_string(),
            Some("low".to_string()),
            vec!["off".to_string(), "low".to_string(), "high".to_string()],
        );
        // The highlighted entry (deepseek-chat) has no effort default;
        // the draft seeds from the live effort ("low"). Right → "high".
        dialog.handle_key(KeyEvent::plain(Key::Right));
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        assert!(matches!(
            answer,
            Some(Answer::ModelSelected {
                effort: Some(level),
                ..
            }) if level == "high"
        ));
        // Left twice cycles around: high → low → off (as None).
        dialog.handle_key(KeyEvent::plain(Key::Left));
        dialog.handle_key(KeyEvent::plain(Key::Left));
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        assert!(matches!(
            answer,
            Some(Answer::ModelSelected { effort: None, .. })
        ));
    }

    #[test]
    fn model_picker_tab_cycles_providers() {
        theme::set(theme::Theme::dark());
        let mut dialog =
            ModelPickerDialog::new(picker_entries(), "none".to_string(), None, Vec::new());
        // Tab once moves to the "deepseek" tab; only its entries show.
        dialog.handle_key(KeyEvent::plain(Key::Tab));
        let lines = dialog.render(80);
        let joined: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[deepseek]"), "{joined}");
        assert!(
            !joined.contains("MiniMax-M3"),
            "other provider hidden: {joined}"
        );
    }

    #[test]
    fn permission_picker_digits_apply() {
        theme::set(theme::Theme::dark());
        let mut dialog = PermissionPickerDialog::new("auto");
        let answer = dialog.handle_key(KeyEvent::plain(Key::Char('3')));
        assert!(matches!(
            answer,
            Some(Answer::PermissionSelected { mode }) if mode == "wave"
        ));
    }

    #[test]
    fn help_panel_scrolls_and_closes() {
        theme::set(theme::Theme::dark());
        let lines = (0..40).map(|i| format!("line {i}")).collect();
        let mut panel = HelpPanel::new(lines);
        for _ in 0..3 {
            panel.handle_key(KeyEvent::plain(Key::PageDown));
        }
        let rendered = panel.render(80);
        let joined: String = rendered
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("40/40 lines"), "{joined}");
        assert!(
            panel.handle_key(KeyEvent::plain(Key::Char('q'))).is_some(),
            "q closes the panel"
        );
    }

    #[test]
    fn session_picker_searches_and_resumes() {
        theme::set(theme::Theme::dark());
        let rows = vec![
            SessionRow {
                id: "id-refactor".to_string(),
                title: "refactor the runner".to_string(),
                cwd: "/tmp/a".to_string(),
                age: "5m ago".to_string(),
                turns: 3,
            },
            SessionRow {
                id: "id-docs".to_string(),
                title: "write docs".to_string(),
                cwd: "/tmp/b".to_string(),
                age: "1h ago".to_string(),
                turns: 1,
            },
        ];
        let mut dialog = SessionPickerDialog::new(rows, "/tmp/a");
        // cwd scope hides the /tmp/b session until Ctrl+A toggles it.
        dialog.handle_key(KeyEvent::new(
            Key::Char('a'),
            Mods {
                ctrl: true,
                alt: false,
                shift: false,
            },
        ));
        for c in "docs".chars() {
            dialog.handle_key(KeyEvent::plain(Key::Char(c)));
        }
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        assert!(matches!(
            answer,
            Some(Answer::ResumeSession { id }) if id == "id-docs"
        ));
    }
}

/// The interactive settings panel: rows of (setting, value), Up/Down to
/// move, Left/Right or Enter to cycle the value. Changes apply
/// immediately and persist to the settings file.
pub struct SettingsDialog {
    settings: crate::settings::SharedSettings,
    title: String,
    selected: usize,
}

/// One settings row as displayed: label plus the current value text.
struct SettingsRow {
    label: &'static str,
    value: String,
}

impl SettingsDialog {
    /// A panel bound to the shared settings handle.
    pub fn new(settings: crate::settings::SharedSettings) -> Self {
        Self {
            settings,
            title: "Settings (left/right to change, esc to close)".to_string(),
            selected: 0,
        }
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    /// Snapshot the rows in display order.
    fn rows(&self) -> Vec<SettingsRow> {
        use crate::settings::{DiffStyle, EditDisplay, ToolDisplay};
        let view = self.settings.get();
        vec![
            SettingsRow {
                label: "render user input as markdown",
                value: if view.render_user_markdown {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
            },
            SettingsRow {
                label: "tool call display",
                value: match view.tool_display {
                    ToolDisplay::Names => "names",
                    ToolDisplay::Summary => "summary",
                    ToolDisplay::Full => "full",
                }
                .to_string(),
            },
            SettingsRow {
                label: "edit tool rendering",
                value: match view.edit_display {
                    EditDisplay::Tool => "tool only",
                    EditDisplay::Diff => "diff",
                }
                .to_string(),
            },
            SettingsRow {
                label: "diff layout",
                value: match view.diff_style {
                    DiffStyle::Unified => "unified",
                    DiffStyle::Split => "split",
                }
                .to_string(),
            },
            SettingsRow {
                label: "wave denylist",
                value: format!("{} entries (edit settings file)", view.wave_denylist.len()),
            },
        ]
    }

    /// Cycle the selected row's value one step (direction: +1 / -1).
    fn cycle(&self, direction: isize) {
        use crate::settings::{DiffStyle, EditDisplay, ToolDisplay};
        let rows = self.rows();
        let label = rows.get(self.selected).map(|r| r.label);
        self.settings.update(|view| match label {
            Some("render user input as markdown") => {
                view.render_user_markdown = !view.render_user_markdown;
            }
            Some("tool call display") => {
                view.tool_display = match (view.tool_display, direction) {
                    (ToolDisplay::Names, 1) | (ToolDisplay::Summary, -1) => ToolDisplay::Summary,
                    (ToolDisplay::Summary, 1) | (ToolDisplay::Full, -1) => ToolDisplay::Full,
                    _ => ToolDisplay::Names,
                };
            }
            Some("edit tool rendering") => {
                view.edit_display = match view.edit_display {
                    EditDisplay::Tool => EditDisplay::Diff,
                    EditDisplay::Diff => EditDisplay::Tool,
                };
            }
            Some("diff layout") => {
                view.diff_style = match view.diff_style {
                    DiffStyle::Unified => DiffStyle::Split,
                    DiffStyle::Split => DiffStyle::Unified,
                };
            }
            _ => {}
        });
    }

    /// Handle one key; `Some(Answer::Dismissed)` when the panel closes.
    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        let count = self.rows().len();
        match (event.key, event.mods) {
            (Key::Esc, _) => Some(Answer::Dismissed),
            (Key::Up, _) => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            (Key::Down, _) => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
                None
            }
            (Key::Left, _) => {
                self.cycle(-1);
                None
            }
            (Key::Right, _) | (Key::Enter, _) => {
                self.cycle(1);
                None
            }
            _ => None,
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let rows = self.rows();
        let mut body = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let marker = if index == self.selected { "▍" } else { " " };
            let label = theme.paint(Token::Text, row.label);
            let value = if index == self.selected {
                theme.bold(Token::Primary, &row.value)
            } else {
                theme.paint(Token::TextDim, &row.value)
            };
            body.push(format!("{marker} {label}  {value}"));
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextMuted,
            "the wave denylist lives in ~/.wavecode/console-settings.json",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// One selectable model entry shown in the model picker.
///
/// Pure display data; the harness derives it from config (`[models]`
/// aliases plus the configured default model) so this crate never
/// depends on the config layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntryView {
    /// Display label (alias or wire model name).
    pub label: String,
    /// Provider id the entry samples through.
    pub provider: String,
    /// Wire model name sent on `SetModel`.
    pub model: String,
    /// Reasoning-effort default for this entry, when known.
    pub effort: Option<String>,
}

/// Fuzzy-scored, order-stable filter over the model entries.
fn filter_indices(entries: &[ModelEntryView], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut scored: Vec<(i64, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let haystack = format!("{} {}", entry.label, entry.provider);
            tui_engine::fuzzy::score(query, &haystack).map(|score| (score, index))
        })
        .collect();
    // `fuzzy::score` ranks lower scores higher; ties keep entry order.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, index)| index).collect()
}

/// The model selector: provider tabs, type-to-search, and a thinking
/// level row. Enter saves the choice as the default; Alt+S applies it
/// to this session only; Esc cancels.
pub struct ModelPickerDialog {
    title: String,
    entries: Vec<ModelEntryView>,
    /// Label of the live model (drives the `current` marker).
    current_label: String,
    /// Thinking levels offered for the current provider kind; empty
    /// hides the thinking row entirely (e.g. budget-driven Anthropic).
    thinking_levels: Vec<String>,
    /// Tab list: `All` first, then providers in entry order.
    tabs: Vec<String>,
    active_tab: usize,
    query: String,
    /// Filtered indices into `entries` under tab + query.
    filtered: Vec<usize>,
    selected: usize,
    /// Thinking draft for the highlighted entry (one of the levels).
    thinking_draft: String,
    /// Live effort for the current model (marks the current segment).
    current_effort: Option<String>,
}

/// Visible list rows before the `N more` collapse.
const PICKER_PAGE: usize = 8;

impl ModelPickerDialog {
    /// Build the picker over `entries`; `thinking_levels` empty hides
    /// the thinking row.
    pub fn new(
        entries: Vec<ModelEntryView>,
        current_label: String,
        current_effort: Option<String>,
        thinking_levels: Vec<String>,
    ) -> Self {
        let tabs = Self::tabs_of(&entries);
        let mut picker = Self {
            title: "Select a model".to_string(),
            entries,
            current_label,
            current_effort,
            thinking_levels,
            tabs,
            active_tab: 0,
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
            thinking_draft: String::new(),
        };
        picker.refilter();
        picker
    }

    /// `All` plus the unique provider ids in first-seen order.
    fn tabs_of(entries: &[ModelEntryView]) -> Vec<String> {
        let mut tabs = vec!["All".to_string()];
        for entry in entries {
            if !tabs.iter().any(|tab| tab == &entry.provider) {
                tabs.push(entry.provider.clone());
            }
        }
        tabs
    }

    /// Recompute the filtered list; keeps the cursor in range and
    /// re-seeds the thinking draft from the highlighted entry.
    fn refilter(&mut self) {
        self.filtered = filter_indices(&self.entries, &self.query)
            .into_iter()
            .filter(|index| {
                self.active_tab == 0 || self.entries[*index].provider == self.tabs[self.active_tab]
            })
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.thinking_draft = self
            .highlighted()
            .and_then(|entry| entry.effort.clone())
            .or_else(|| self.current_effort.clone())
            .or_else(|| self.thinking_levels.first().cloned())
            .unwrap_or_else(|| "off".to_string());
    }

    fn highlighted(&self) -> Option<&ModelEntryView> {
        self.filtered
            .get(self.selected)
            .map(|index| &self.entries[*index])
    }

    /// True when the highlighted entry is the live model.
    fn is_current(&self, index: usize) -> bool {
        self.filtered
            .get(index)
            .map(|entry_index| self.entries[*entry_index].label == self.current_label)
            .unwrap_or(false)
    }

    fn resolve(&self, session_only: bool) -> Option<Answer> {
        let entry = self.highlighted()?;
        let effort = if self.thinking_levels.is_empty() {
            None
        } else {
            let draft = self.thinking_draft.as_str();
            if draft.eq_ignore_ascii_case("off") {
                None
            } else {
                Some(draft.to_string())
            }
        };
        Some(Answer::ModelSelected {
            label: entry.label.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            effort,
            session_only,
        })
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up if !self.filtered.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.filtered.len() - 1
                } else {
                    self.selected - 1
                };
                self.refilter();
                None
            }
            Key::Down if !self.filtered.is_empty() => {
                self.selected = (self.selected + 1) % self.filtered.len();
                self.refilter();
                None
            }
            Key::Tab => {
                // BackTab arrives as Tab+shift; both cycle the tabs.
                let count = self.tabs.len();
                let step = if event.mods.shift { count - 1 } else { 1 };
                self.active_tab = (self.active_tab + step) % count;
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Left | Key::Right if !self.thinking_levels.is_empty() => {
                let position = self
                    .thinking_levels
                    .iter()
                    .position(|level| level.eq_ignore_ascii_case(&self.thinking_draft))
                    .unwrap_or(0);
                let count = self.thinking_levels.len();
                let step = if event.key == Key::Right {
                    1
                } else {
                    count - 1
                };
                self.thinking_draft = self.thinking_levels[(position + step) % count].clone();
                None
            }
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Char(c) if event.mods.alt && c == 's' => self.resolve(true),
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                self.query.push(c);
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Enter => self.resolve(false),
            _ => None,
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        // Tab strip: [All] provider-a provider-b …
        let mut strip = String::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            let label = if index == self.active_tab {
                format!("[{tab}]")
            } else {
                tab.clone()
            };
            if index == self.active_tab {
                strip.push_str(&theme.bold(Token::Primary, &label));
            } else {
                strip.push_str(&theme.paint(Token::TextDim, &label));
            }
            strip.push_str("  ");
        }
        body.push(strip.trim_end().to_string());
        body.push(String::new());
        // List window with the current marker and `N more` collapse.
        let visible = self.filtered.len().min(PICKER_PAGE);
        for row in 0..visible {
            let entry_index = self.filtered[row];
            let entry = &self.entries[entry_index];
            let marker = if row == self.selected { "❯ " } else { "  " };
            let mut line = format!("{marker}{}", entry.label);
            if self.is_current(row) {
                line.push_str(&theme.paint(Token::Success, "  ← current"));
            }
            if row == self.selected {
                body.push(theme.bold(Token::TextStrong, &line));
            } else {
                body.push(theme.paint(Token::Text, &line));
            }
        }
        if self.filtered.is_empty() {
            body.push(theme.paint(Token::TextDim, "No matches"));
        } else if self.filtered.len() > visible && self.query.trim().is_empty() {
            body.push(theme.paint(
                Token::TextDim,
                &format!("  ▼ {} more", self.filtered.len() - visible),
            ));
        }
        if !self.query.trim().is_empty() {
            body.push(theme.paint(
                Token::TextDim,
                &format!(
                    "Search: {} · {}/{}",
                    self.query,
                    self.filtered.len(),
                    self.entries.len()
                ),
            ));
        }
        // Thinking row (hidden when the provider offers no levels).
        if !self.thinking_levels.is_empty() {
            body.push(String::new());
            body.push(theme.paint(Token::Text, "Thinking  (←/→ to switch)"));
            let mut row = String::from("   ");
            for level in &self.thinking_levels {
                let active = level.eq_ignore_ascii_case(&self.thinking_draft);
                let label = if active {
                    format!("[ {level} ]")
                } else {
                    format!("{level}  ")
                };
                if active {
                    row.push_str(&theme.bold(Token::Primary, &label));
                } else {
                    row.push_str(&theme.paint(Token::TextDim, &label));
                }
            }
            body.push(row);
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "Tab provider · type to search · ↑/↓ navigate · ↵ save · Alt+S session-only · Esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// The permission-mode selector: three modes with descriptions, Enter
/// applies, Esc cancels.
pub struct PermissionPickerDialog {
    title: String,
    modes: Vec<(&'static str, &'static str)>,
    selected: usize,
}

impl PermissionPickerDialog {
    /// Build the selector over plan/auto/wave.
    pub fn new(current: &str) -> Self {
        let modes = vec![
            (
                "plan",
                "Plan Mode — read-only exploration; changes wait for approval",
            ),
            (
                "auto",
                "Auto Mode — asks before exec and destructive actions",
            ),
            (
                "wave",
                "Wave Mode — fully automatic; the denylist still applies",
            ),
        ];
        let selected = modes
            .iter()
            .position(|(name, _)| *name == current)
            .unwrap_or(1);
        Self {
            title: "Select a permission mode".to_string(),
            modes,
            selected,
        }
    }

    fn resolve(&self) -> Answer {
        Answer::PermissionSelected {
            mode: self.modes[self.selected].0.to_string(),
        }
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up => {
                self.selected = if self.selected == 0 {
                    self.modes.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.modes.len();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                if let Ok(digit) = c.to_string().parse::<usize>()
                    && digit >= 1
                    && digit <= self.modes.len()
                {
                    self.selected = digit - 1;
                    return Some(self.resolve());
                }
                None
            }
            Key::Enter => Some(self.resolve()),
            _ => None,
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        for (index, (name, description)) in self.modes.iter().enumerate() {
            let number = index + 1;
            if index == self.selected {
                body.push(format!(
                    "{} {}",
                    theme.bold(Token::Accent, &format!("▶ {number}.")),
                    theme.bold(Token::TextStrong, name)
                ));
                body.push(theme.paint(Token::TextDim, &format!("     {description}")));
            } else {
                body.push(format!(
                    "  {} {}",
                    theme.paint(Token::TextDim, &format!("{number}.")),
                    theme.paint(Token::Text, name)
                ));
            }
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "↑/↓ select · 1/2/3 choose · ↵ apply · esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// The scrollable help panel: keybindings plus every command with its
/// description. Esc / q / Enter close it.
pub struct HelpPanel {
    title: String,
    lines: Vec<String>,
    scroll: usize,
}

/// Help content rows shown at once.
const HELP_WINDOW: usize = 20;

impl HelpPanel {
    /// Build the panel from pre-wrapped plain lines.
    pub fn new(lines: Vec<String>) -> Self {
        Self {
            title: "Help".to_string(),
            lines,
            scroll: 0,
        }
    }

    fn page_down(&mut self) {
        self.scroll = (self.scroll + HELP_WINDOW).min(self.lines.len().saturating_sub(1));
    }

    fn page_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(HELP_WINDOW);
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc | Key::Enter | Key::Char('q') => Some(Answer::Dismissed),
            Key::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            Key::Down => {
                self.scroll = (self.scroll + 1).min(self.lines.len().saturating_sub(1));
                None
            }
            Key::PageUp => {
                self.page_up();
                None
            }
            Key::PageDown => {
                self.page_down();
                None
            }
            _ => None,
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        let end = (self.scroll + HELP_WINDOW).min(self.lines.len());
        for line in &self.lines[self.scroll..end] {
            body.push(theme.paint(Token::Text, line));
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            &format!(
                "↑/↓ scroll · PgUp/PgDn page · esc closes  ({}/{} lines)",
                end,
                self.lines.len()
            ),
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// One row of the session picker: meta plus the rendered last-prompt
/// preview head.
pub struct SessionRow {
    /// Session id (resume target).
    pub id: String,
    /// Display title (or the id when untitled).
    pub title: String,
    /// Working directory of the session.
    pub cwd: String,
    /// Relative timestamp label (e.g. `5m ago`).
    pub age: String,
    /// Recorded turn count.
    pub turns: u32,
}

/// The session picker: fuzzy search over titles, cwd/all scoping,
/// Enter resumes.
pub struct SessionPickerDialog {
    title: String,
    rows: Vec<SessionRow>,
    /// True scopes the list to the current cwd, false lists all.
    cwd_scoped: bool,
    current_cwd: String,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
}

impl SessionPickerDialog {
    /// Build the picker over `rows`; sessions from other directories
    /// stay reachable via the Ctrl+A scope toggle.
    pub fn new(rows: Vec<SessionRow>, current_cwd: &str) -> Self {
        let mut picker = Self {
            title: "Sessions".to_string(),
            rows,
            cwd_scoped: true,
            current_cwd: current_cwd.to_string(),
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker
    }

    /// Ctrl+A toggles cwd ↔ all scoping.
    fn toggle_scope(&mut self) {
        self.cwd_scoped = !self.cwd_scoped;
        self.selected = 0;
        self.refilter();
    }

    fn refilter(&mut self) {
        self.filtered = (0..self.rows.len())
            .filter(|index| {
                let row = &self.rows[*index];
                if self.cwd_scoped && row.cwd != self.current_cwd {
                    return false;
                }
                let query = self.query.trim();
                query.is_empty()
                    || tui_engine::fuzzy::score(query, &row.title).is_some()
                    || row.title.to_lowercase().contains(&query.to_lowercase())
            })
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up if !self.filtered.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.filtered.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down if !self.filtered.is_empty() => {
                self.selected = (self.selected + 1) % self.filtered.len();
                None
            }
            Key::Char('a') if event.mods.ctrl => {
                self.toggle_scope();
                None
            }
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                self.query.push(c);
                self.selected = 0;
                self.refilter();
                None
            }
            Key::Enter => {
                let index = self.filtered.get(self.selected)?;
                Some(Answer::ResumeSession {
                    id: self.rows[*index].id.clone(),
                })
            }
            _ => None,
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        let scope = if self.cwd_scoped {
            "Sessions (this directory)"
        } else {
            "All sessions"
        };
        body.push(theme.bold(Token::Primary, scope));
        body.push(theme.paint(
            Token::TextDim,
            "type to search · ↑/↓ navigate · Ctrl+A toggle directory scope · ↵ resume · Esc cancel",
        ));
        body.push(String::new());
        let visible = self.filtered.len().min(PICKER_PAGE);
        for row in 0..visible {
            let entry = &self.rows[self.filtered[row]];
            let marker = if row == self.selected { "❯ " } else { "  " };
            let head = format!(
                "{marker}{}  {}  {} turns",
                width::truncate_to_width(&entry.title, columns.saturating_sub(24)),
                entry.age,
                entry.turns
            );
            if row == self.selected {
                body.push(theme.bold(Token::TextStrong, &head));
                body.push(theme.paint(Token::TextDim, &format!("    {}", entry.id)));
            } else {
                body.push(theme.paint(Token::Text, &head));
            }
        }
        if self.filtered.is_empty() {
            body.push(theme.paint(Token::TextDim, "No sessions yet"));
        }
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
