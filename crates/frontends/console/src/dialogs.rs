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
    /// A rewind point picked in the undo picker (double-Esc).
    RewindTurns {
        /// How many whole turns to drop.
        turns: u32,
    },
    /// The dialog closed without producing an answer (settings).
    Dismissed,
    /// A theme picked in the theme selector.
    ThemeSelected {
        /// Theme name to apply (`auto` / `dark` / `deepwave` / `light`
        /// or a custom theme name).
        name: String,
    },
    /// A reasoning-effort level picked in the effort selector.
    EffortSelected {
        /// Chosen level (`None` = off).
        level: Option<String>,
    },
    /// Free text submitted by the bare-command prompt.
    Prompt {
        /// Which command asked for the text.
        purpose: PromptPurpose,
        /// The submitted (trimmed) text.
        value: String,
    },
    /// A model spec built by the `/model add` form, ready to insert
    /// into the catalog.
    ModelForm {
        /// Catalog alias the model is saved under.
        alias: String,
        /// The assembled spec (every form field resolved).
        spec: wavecode_config::ModelSpec,
    },
}

/// Which bare-command prompt an [`Answer::Prompt`] routes back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptPurpose {
    /// `/title` — rename the session.
    SessionTitle,
    /// `/editor` — set the external editor command.
    EditorCommand,
    /// `/export` — the markdown export path.
    ExportPath,
    /// `/compact` — an optional steering instruction.
    CompactInstruction,
    /// `/btw` — the side question.
    BtwQuestion,
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
    /// The rewind picker (double-Esc; feeds the `/undo` path).
    Undo(UndoPickerDialog),
    /// The theme selector (`/theme`).
    Theme(ThemePickerDialog),
    /// The reasoning-effort selector (`/effort`).
    Effort(EffortPickerDialog),
    /// The bare-command free-text prompt (`/title`, `/editor`,
    /// `/export`, `/compact`, `/btw`).
    Prompt(PromptDialog),
    /// The `/model add` spec form.
    ModelForm(ModelFormDialog),
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
            Self::Undo(dialog) => dialog.title.clone(),
            Self::Theme(dialog) => dialog.title.clone(),
            Self::Effort(dialog) => dialog.title.clone(),
            Self::Prompt(dialog) => dialog.title.clone(),
            Self::ModelForm(dialog) => dialog.title.clone(),
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
            Self::Undo(dialog) => dialog.handle_key(event),
            Self::Theme(dialog) => dialog.handle_key(event),
            Self::Effort(dialog) => dialog.handle_key(event),
            Self::Prompt(dialog) => dialog.handle_key(event),
            Self::ModelForm(dialog) => dialog.handle_key(event),
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
            Self::Undo(dialog) => dialog.render(width),
            Self::Theme(dialog) => dialog.render(width),
            Self::Effort(dialog) => dialog.render(width),
            Self::Prompt(dialog) => dialog.render(width),
            Self::ModelForm(dialog) => dialog.render(width),
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
            // Every printable character — digits included — enters the
            // free-text answer: a numeric shortcut here would submit
            // the named option mid-word ("2 hours" picking #2). The
            // numbers on the options are visual; selection rides
            // ↑/↓ + Enter (the approval dialog, with no free text,
            // keeps its digit shortcuts).
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
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
            "↑/↓ select · type for free text · ↵ answers · esc dismisses",
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
        let answer = {
            let mut dialog = QuestionDialog::new(
                "q1".to_string(),
                "Pick one",
                vec!["alpha".to_string(), "beta".to_string()],
            );
            dialog.handle_key(KeyEvent::plain(Key::Down));
            dialog.handle_key(KeyEvent::plain(Key::Enter))
        };
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

    /// Free-text typing accepts digits too: a numeric shortcut would
    /// submit the named option mid-word ("2 hours" picking #2).
    #[test]
    fn question_free_text_accepts_digits() {
        theme::set(theme::Theme::dark());
        let mut dialog = QuestionDialog::new(
            "q3".to_string(),
            "How long?",
            vec!["one hour".to_string(), "one day".to_string()],
        );
        for c in "2 hours".chars() {
            if let Some(answer) = dialog.handle_key(KeyEvent::plain(Key::Char(c))) {
                panic!("digit '{c}' submitted mid-word: {answer:?}");
            }
        }
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        assert!(matches!(
            answer,
            Some(Answer::Question { answer, .. }) if answer == "2 hours"
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

    #[test]
    fn write_approval_colors_diff_lines() {
        theme::set(theme::Theme::dark());
        let detail = "write: src/lib.rs\n+fn added() {}\n-fn gone() {}";
        let mut dialog = ApprovalDialog::new("c5".to_string(), ApprovalKind::Write, detail);
        let lines = dialog.render(70);
        let added = lines
            .iter()
            .find(|l| strip_ansi(l).contains("+fn added() {}"))
            .expect("added row present");
        let removed = lines
            .iter()
            .find(|l| strip_ansi(l).contains("-fn gone() {}"))
            .expect("removed row present");
        // Diff rows carry their token color instead of the plain text
        // style the `$` command rows use; each row must carry the SGR
        // foreground of its own diff token (added vs removed).
        let theme = theme::current();
        let fg = |token: Token| {
            let color = theme.color(token);
            format!("38;2;{};{};{}", color.r, color.g, color.b)
        };
        let fgs = |line: &str| {
            line.split("\x1b[")
                .skip(1)
                .map(|seq| seq.split('m').next().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|")
        };
        assert!(
            fgs(added).contains(&fg(Token::DiffAdded)),
            "added row must paint DiffAdded: {:?} vs {}",
            fgs(added),
            fg(Token::DiffAdded)
        );
        assert!(
            fgs(removed).contains(&fg(Token::DiffRemoved)),
            "removed row must paint DiffRemoved: {:?} vs {}",
            fgs(removed),
            fg(Token::DiffRemoved)
        );
        // The head line keeps the `$` command shape.
        let joined: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("$ write: src/lib.rs"), "{joined}");
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

    /// The `/model add` form resolves every field into a full spec:
    /// identity, limits, thinking variants (first = default), and the
    /// modality lists.
    #[test]
    fn model_form_resolves_all_fields() {
        theme::set(theme::Theme::dark());
        let mut dialog = ModelFormDialog::new();
        let inputs = [
            "mini",                     // alias
            "",                         // api (keep the default anthropic-messages)
            "minimax",                  // provider
            "https://api.minimax.chat", // base url
            "MiniMax-M3",               // model
            "MINIMAX_API_KEY",          // api key env
            "195000",                   // context tokens
            "8192",                     // max output
            "low, high",                // thinking variants
            ",image",                   // input modalities (appends to the "text" seed)
            "",                         // output modalities (keeps the "text" seed)
        ];
        for input in inputs {
            for c in input.chars() {
                dialog.handle_key(KeyEvent::plain(Key::Char(c)));
            }
            if dialog.active < dialog.fields.len() - 1 {
                dialog.handle_key(KeyEvent::plain(Key::Enter));
            }
        }
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        let Some(Answer::ModelForm { alias, spec }) = answer else {
            panic!("form did not resolve: {answer:?}");
        };
        assert_eq!(alias, "mini");
        assert_eq!(spec.provider, "minimax");
        assert_eq!(spec.base_url, "https://api.minimax.chat");
        assert_eq!(spec.model, "MiniMax-M3");
        assert_eq!(spec.kind, wavecode_config::ApiKind::AnthropicMessages);
        assert_eq!(spec.api_key_env.as_deref(), Some("MINIMAX_API_KEY"));
        assert_eq!(spec.context_window, Some(195000));
        assert_eq!(spec.max_output, Some(8192));
        assert!(spec.reasoning.enabled);
        assert_eq!(spec.reasoning.variants, vec!["low", "high"]);
        assert_eq!(spec.reasoning.default.as_deref(), Some("low"));
        assert_eq!(spec.modalities.input, vec!["text", "image"]);
        assert_eq!(spec.modalities.output, vec!["text"]);
    }

    /// Blank required fields block the save with the form open and the
    /// error pointing at the field; counts must be numeric. Enter on a
    /// middle field only advances — saving happens on the last field.
    #[test]
    fn model_form_validates_before_resolving() {
        theme::set(theme::Theme::dark());
        let mut dialog = ModelFormDialog::new();
        // Alias filled, walk to the last field and save: provider is
        // required.
        dialog.handle_key(KeyEvent::plain(Key::Char('x')));
        while dialog.active < dialog.fields.len() - 1 {
            dialog.handle_key(KeyEvent::plain(Key::Down));
        }
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        assert!(answer.is_none(), "incomplete form must not resolve");
        assert_eq!(dialog.error.as_deref(), Some("provider is required"));
        // Count fields filter non-digits at the key: "12x" stores "12"
        // and the form then saves fine (counts stay optional).
        let mut dialog = ModelFormDialog::new();
        let values = ["a", "", "p", "https://h", "m", "", "12x"];
        for value in values {
            for c in value.chars() {
                dialog.handle_key(KeyEvent::plain(Key::Char(c)));
            }
            dialog.handle_key(KeyEvent::plain(Key::Enter));
        }
        assert_eq!(dialog.fields[6].value, "12", "non-digits filtered");
        while dialog.active < dialog.fields.len() - 1 {
            dialog.handle_key(KeyEvent::plain(Key::Down));
        }
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        let Some(Answer::ModelForm { alias, spec }) = answer else {
            panic!("valid form must resolve: {answer:?}");
        };
        assert_eq!(alias, "a");
        assert_eq!(spec.context_window, Some(12));
        assert_eq!(spec.max_output, None);
    }

    /// ←/→ cycle the API dialect field through the three spellings.
    #[test]
    fn model_form_cycles_api_kind() {
        theme::set(theme::Theme::dark());
        let mut dialog = ModelFormDialog::new();
        dialog.handle_key(KeyEvent::plain(Key::Down)); // to `api`
        dialog.handle_key(KeyEvent::plain(Key::Right));
        assert_eq!(dialog.fields[1].value, "openai-chat");
        dialog.handle_key(KeyEvent::plain(Key::Right));
        assert_eq!(dialog.fields[1].value, "openai-responses");
        dialog.handle_key(KeyEvent::plain(Key::Right));
        assert_eq!(dialog.fields[1].value, "anthropic-messages");
        dialog.handle_key(KeyEvent::plain(Key::Left));
        assert_eq!(dialog.fields[1].value, "openai-responses");
    }

    /// The list window slides with the selection: a selection past the
    /// fixed page still renders with its marker, and the collapse line
    /// tracks what is left below the window.
    #[test]
    fn model_picker_window_slides_with_selection() {
        theme::set(theme::Theme::dark());
        let entries: Vec<ModelEntryView> = (0..12)
            .map(|i| ModelEntryView {
                label: format!("model-{i:02}"),
                provider: "p".to_string(),
                model: format!("model-{i:02}"),
                effort: None,
            })
            .collect();
        let mut dialog = ModelPickerDialog::new(entries, "absent".to_string(), None, Vec::new());
        for _ in 0..9 {
            dialog.handle_key(KeyEvent::plain(Key::Down));
        }
        assert_eq!(dialog.selected, 9);
        let joined: String = dialog
            .render(80)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("❯ model-09"), "selection visible: {joined}");
        assert!(joined.contains("▼ 2 more"), "collapse tracks: {joined}");
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
        panel.handle_key(KeyEvent::plain(Key::PageDown));
        let rendered = panel.render(80);
        let joined: String = rendered
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("40/40 lines"), "{joined}");
        // At the last page the panel is pinned: further scrolling neither
        // shrinks the frame nor changes the visible window.
        for key in [Key::Down, Key::PageDown, Key::Down, Key::Down] {
            panel.handle_key(KeyEvent::plain(key));
        }
        assert_eq!(
            panel
                .render(80)
                .iter()
                .map(|l| strip_ansi(l))
                .collect::<Vec<_>>(),
            rendered.iter().map(|l| strip_ansi(l)).collect::<Vec<_>>(),
            "render must stay stable at the bottom"
        );
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

    fn user_entry(text: &str) -> crate::state::DialogueEntry {
        crate::state::DialogueEntry {
            from_user: true,
            text: text.to_string(),
        }
    }

    fn assistant_entry(text: &str) -> crate::state::DialogueEntry {
        crate::state::DialogueEntry {
            from_user: false,
            text: text.to_string(),
        }
    }

    #[test]
    fn undo_picker_lists_newest_first_with_stable_distances() {
        let history = vec![
            user_entry("one"),
            assistant_entry("a1"),
            user_entry("two"),
            assistant_entry("a2"),
            // A blank first line hides the label but the turn still
            // counts: the kernel rewinds it like any other user turn.
            user_entry("  \nbody"),
            assistant_entry("a3"),
            user_entry("four"),
        ];
        let picker = UndoPickerDialog::new(&history);
        let labels: Vec<&str> = picker.rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, vec!["four", "two", "one"]);
        let turns: Vec<u32> = picker.rows.iter().map(|row| row.turns).collect();
        // "two" sits two user turns back even though the blank turn in
        // between has no row of its own ("one" sits three back).
        assert_eq!(turns, vec![1, 3, 4]);
        assert!(!picker.is_empty());
    }

    #[test]
    fn undo_picker_caps_rows_and_reports_empty() {
        let long: Vec<crate::state::DialogueEntry> =
            (0..12).map(|i| user_entry(&format!("turn {i}"))).collect();
        let picker = UndoPickerDialog::new(&long);
        assert_eq!(picker.rows.len(), MAX_UNDO_ROWS);
        assert_eq!(picker.rows[0].turns, 1);
        assert_eq!(picker.rows[MAX_UNDO_ROWS - 1].turns, MAX_UNDO_ROWS as u32);

        let empty = vec![user_entry("   "), assistant_entry("only noise")];
        assert!(UndoPickerDialog::new(&empty).is_empty());
        assert!(UndoPickerDialog::new(&[]).is_empty());
    }

    #[test]
    fn theme_picker_marks_current_and_applies_builtins() {
        theme::set(theme::Theme::dark());
        let mut dialog = ThemePickerDialog::new("deepwave", vec![("sunset".to_string(), None)]);
        assert_eq!(dialog.selected, 2, "the live theme preselects");
        let lines = dialog.render(70);
        let joined: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Select a theme"), "{joined}");
        assert!(joined.contains("← current"), "{joined}");
        assert!(joined.contains("the teal ocean identity"), "{joined}");
        // Custom rows describe themselves once highlighted.
        dialog.handle_key(KeyEvent::plain(Key::Down));
        dialog.handle_key(KeyEvent::plain(Key::Down));
        let lines = dialog.render(70);
        let joined: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("custom theme"), "{joined}");
        // Digit quick-select resolves the matching builtin.
        match dialog.handle_key(KeyEvent::plain(Key::Char('4'))) {
            Some(Answer::ThemeSelected { name }) => assert_eq!(name, "light"),
            other => panic!("expected a theme selection: {other:?}"),
        }
        // Arrows move with wrap-around; Enter resolves the cursor row.
        let mut dialog = ThemePickerDialog::new("auto", Vec::new());
        dialog.handle_key(KeyEvent::plain(Key::Up));
        let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
        match answer {
            Some(Answer::ThemeSelected { name }) => assert_eq!(name, "light"),
            other => panic!("expected a theme selection: {other:?}"),
        }
    }

    #[test]
    fn effort_picker_seeds_off_and_resolves_levels() {
        theme::set(theme::Theme::dark());
        let mut dialog =
            EffortPickerDialog::new(Some("high"), &["low".to_string(), "high".to_string()]);
        assert_eq!(dialog.levels, vec!["off", "low", "high"], "off seeds first");
        assert_eq!(dialog.selected, 2, "the live level preselects");
        match dialog.handle_key(KeyEvent::plain(Key::Enter)) {
            Some(Answer::EffortSelected { level: Some(level) }) => assert_eq!(level, "high"),
            other => panic!("expected an effort selection: {other:?}"),
        }
        // Without provider levels the picker collapses to off (None).
        let mut dialog = EffortPickerDialog::new(None, &[]);
        assert_eq!(dialog.levels, vec!["off"]);
        match dialog.handle_key(KeyEvent::plain(Key::Enter)) {
            Some(Answer::EffortSelected { level: None }) => {}
            other => panic!("expected off: {other:?}"),
        }
    }

    #[test]
    fn prompt_dialog_edits_prefilled_text_and_resolves() {
        theme::set(theme::Theme::dark());
        let mut dialog = PromptDialog::new(
            "Session title",
            PromptPurpose::SessionTitle,
            "old name",
            "hint",
        );
        // Render shows the prefilled value under the framed title.
        let lines = dialog.render(70);
        let joined: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("old name"), "{joined}");
        // Backspace edits, typed characters append, Enter submits.
        dialog.handle_key(KeyEvent::plain(Key::Backspace));
        for c in "re".chars() {
            dialog.handle_key(KeyEvent::plain(Key::Char(c)));
        }
        match dialog.handle_key(KeyEvent::plain(Key::Enter)) {
            Some(Answer::Prompt { purpose, value }) => {
                assert_eq!(purpose, PromptPurpose::SessionTitle);
                assert_eq!(value, "old namre");
            }
            other => panic!("expected a prompt answer: {other:?}"),
        }
        // Esc dismisses.
        assert_eq!(
            dialog.handle_key(KeyEvent::plain(Key::Esc)),
            Some(Answer::Dismissed)
        );
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
        use crate::settings::{EditDisplay, ToolDisplay};
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
                label: "wave denylist",
                value: format!("{} entries (edit settings file)", view.wave_denylist.len()),
            },
            SettingsRow {
                label: "thinking starts expanded",
                value: Self::bool_value(view.thinking_expanded),
            },
            SettingsRow {
                label: "stream the assistant draft",
                value: Self::bool_value(view.show_streaming_draft),
            },
            SettingsRow {
                label: "context meter in footer",
                value: Self::bool_value(view.show_context_footer),
            },
            SettingsRow {
                label: "rotate footer tips",
                value: Self::bool_value(view.rotate_tips),
            },
            SettingsRow {
                label: "confirm before exit",
                value: Self::bool_value(view.confirm_exit),
            },
            SettingsRow {
                label: "input history limit",
                value: if view.history_limit == 0 {
                    "default (100)".to_string()
                } else {
                    format!("{}", view.history_limit)
                },
            },
        ]
    }

    /// The on/off cell for a boolean row.
    fn bool_value(value: bool) -> String {
        if value { "on" } else { "off" }.to_string()
    }

    /// Cycle the selected row's value one step (direction: +1 / -1).
    fn cycle(&self, direction: isize) {
        use crate::settings::{EditDisplay, ToolDisplay};
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
            Some("thinking starts expanded") => {
                view.thinking_expanded = !view.thinking_expanded;
            }
            Some("stream the assistant draft") => {
                view.show_streaming_draft = !view.show_streaming_draft;
            }
            Some("context meter in footer") => {
                view.show_context_footer = !view.show_context_footer;
            }
            Some("rotate footer tips") => {
                view.rotate_tips = !view.rotate_tips;
            }
            Some("confirm before exit") => {
                view.confirm_exit = !view.confirm_exit;
            }
            Some("input history limit") => {
                // Three stops: default (100) → 500 → 1000 → default. A
                // cycle keeps the row keyboard-only, like every other
                // setting here.
                view.history_limit = match (view.history_limit, direction) {
                    (0, 1) | (500, -1) => 500,
                    (500, 1) | (1000, -1) => 1000,
                    _ => 0,
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
                self.selected = if self.selected == 0 {
                    count.saturating_sub(1)
                } else {
                    self.selected - 1
                };
                None
            }
            (Key::Down, _) => {
                self.selected = (self.selected + 1) % count.max(1);
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
            // Painted like every other selection glyph (footer's mode
            // badge does the same): unpainted, it drops off the theme
            // contract and goes unreadable on a recolored light paper.
            let marker = theme.paint(
                Token::Primary,
                if index == self.selected { "▍" } else { " " },
            );
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
        // The window slides with the selection: a fixed top-anchored
        // window leaves the highlighted row invisible once it moves
        // past the page.
        let visible = self.filtered.len().min(PICKER_PAGE);
        let start = if self.selected >= visible {
            (self.selected + 1 - visible).min(self.filtered.len() - visible)
        } else {
            0
        };
        for offset in 0..visible {
            let index = start + offset;
            let entry_index = self.filtered[index];
            let entry = &self.entries[entry_index];
            let marker = if index == self.selected { "❯ " } else { "  " };
            let mut line = format!("{marker}{}", entry.label);
            if self.is_current(index) {
                line.push_str(&theme.paint(Token::Success, "  ← current"));
            }
            if index == self.selected {
                body.push(theme.bold(Token::TextStrong, &line));
            } else {
                body.push(theme.paint(Token::Text, &line));
            }
        }
        if self.filtered.is_empty() {
            body.push(theme.paint(Token::TextDim, "No matches"));
        } else if start + visible < self.filtered.len() {
            body.push(theme.paint(
                Token::TextDim,
                &format!("  ▼ {} more", self.filtered.len() - start - visible),
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

    /// Deepest scroll offset: the window pins to the last `HELP_WINDOW`
    /// rows, so scrolling stops once the footer reads `N/N`.
    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(HELP_WINDOW)
    }

    fn page_down(&mut self) {
        self.scroll = (self.scroll + HELP_WINDOW).min(self.max_scroll());
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
                self.scroll = (self.scroll + 1).min(self.max_scroll());
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
        // The window slides with the selection (see the model picker):
        // a fixed top-anchored page strands the highlighted row below
        // it, visually unreachable.
        let start = if self.selected >= visible {
            (self.selected + 1 - visible).min(self.filtered.len() - visible)
        } else {
            0
        };
        for offset in 0..visible {
            let index = start + offset;
            let entry = &self.rows[self.filtered[index]];
            let marker = if index == self.selected { "❯ " } else { "  " };
            let head = format!(
                "{marker}{}  {}  {} turns",
                width::truncate_to_width(&entry.title, columns.saturating_sub(24)),
                entry.age,
                entry.turns
            );
            if index == self.selected {
                body.push(theme.bold(Token::TextStrong, &head));
                body.push(theme.paint(Token::TextDim, &format!("    {}", entry.id)));
            } else {
                body.push(theme.paint(Token::Text, &head));
            }
        }
        if self.filtered.is_empty() {
            body.push(theme.paint(Token::TextDim, "No matching sessions"));
        } else if start + visible < self.filtered.len() {
            body.push(theme.paint(
                Token::TextDim,
                &format!("  ▼ {} more", self.filtered.len() - start - visible),
            ));
        }
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// Rewind points offered in the picker.
pub const MAX_UNDO_ROWS: usize = 8;

/// One rewind point: how many turns to drop and the user message that
/// started the turn.
pub struct UndoRow {
    /// Whole turns dropped when this point is picked.
    pub turns: u32,
    /// First line of the user message that started the turn.
    pub label: String,
}

/// The rewind picker (double-Esc): the most recent user turns, newest
/// first. Picking a row drops that turn and everything after it.
pub struct UndoPickerDialog {
    title: String,
    rows: Vec<UndoRow>,
    selected: usize,
}

impl UndoPickerDialog {
    /// Build over the untrimmed dialogue; rows are newest-first and
    /// capped at [`MAX_UNDO_ROWS`]. Turn counts span every user turn —
    /// matching the kernel, which rewinds all of them — while only the
    /// label decides whether a row is shown, so a turn whose first line
    /// is blank never shifts the older rows' distances.
    pub fn new(history: &[crate::state::DialogueEntry]) -> Self {
        let rows: Vec<UndoRow> = history
            .iter()
            .filter(|entry| entry.from_user)
            .rev()
            .enumerate()
            .filter_map(|(back, entry)| {
                let label = entry.text.lines().next()?.trim();
                (!label.is_empty()).then(|| UndoRow {
                    turns: back as u32 + 1,
                    label: label.to_string(),
                })
            })
            .take(MAX_UNDO_ROWS)
            .collect();
        Self {
            title: "Rewind".to_string(),
            rows,
            selected: 0,
        }
    }

    /// True when there is nothing to offer.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up if !self.rows.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.rows.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down if !self.rows.is_empty() => {
                self.selected = (self.selected + 1) % self.rows.len();
                None
            }
            Key::Enter => {
                let row = self.rows.get(self.selected)?;
                Some(Answer::RewindTurns { turns: row.turns })
            }
            _ => None,
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut body = Vec::new();
        body.push(theme.paint(
            Token::TextDim,
            "rewind to before this turn · ↑/↓ navigate · ↵ rewind · Esc cancel",
        ));
        body.push(String::new());
        for (row, point) in self.rows.iter().enumerate() {
            let marker = if row == self.selected { "❯ " } else { "  " };
            let turns = match point.turns {
                1 => "1 turn".to_string(),
                n => format!("{n} turns"),
            };
            let head = format!(
                "{marker}{}  ({})",
                width::truncate_to_width(&point.label, columns.saturating_sub(16)),
                turns
            );
            if row == self.selected {
                body.push(theme.bold(Token::TextStrong, &head));
            } else {
                body.push(theme.paint(Token::Text, &head));
            }
        }
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// One selectable row of the theme picker: the theme name plus a
/// one-line description.
struct ThemeRow {
    name: String,
    description: String,
}

/// The theme selector (`/theme` with no args): the built-in themes
/// first, then the user themes from `~/.wavecode/themes/`, the live
/// theme marked. Enter applies, Esc cancels.
pub struct ThemePickerDialog {
    title: String,
    rows: Vec<ThemeRow>,
    /// Name of the live theme (drives the `current` marker).
    current: String,
    selected: usize,
}

impl ThemePickerDialog {
    /// Build the picker over the built-ins plus `custom` names paired
    /// with their file descriptions; the live theme is marked when its
    /// name matches a row.
    pub fn new(current: &str, custom: Vec<(String, Option<String>)>) -> Self {
        let builtin_row = |name: &str, fallback: &str| ThemeRow {
            name: name.to_string(),
            description: crate::theme::builtin::description(name)
                .unwrap_or(fallback)
                .to_string(),
        };
        let mut rows = vec![
            ThemeRow {
                name: "auto".to_string(),
                description: "follow the terminal background".to_string(),
            },
            builtin_row("dark", "the default dark identity"),
            builtin_row("deepwave", "the teal ocean identity"),
            builtin_row("light", "the light identity"),
        ];
        rows.extend(custom.into_iter().map(|(name, description)| ThemeRow {
            name,
            description: description.unwrap_or_else(|| "custom theme".to_string()),
        }));
        let selected = rows.iter().position(|row| row.name == current).unwrap_or(0);
        Self {
            title: "Select a theme".to_string(),
            rows,
            current: current.to_string(),
            selected,
        }
    }

    fn resolve(&self) -> Answer {
        Answer::ThemeSelected {
            name: self.rows[self.selected].name.clone(),
        }
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up => {
                self.selected = if self.selected == 0 {
                    self.rows.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.rows.len();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                if let Ok(digit) = c.to_string().parse::<usize>()
                    && digit >= 1
                    && digit <= self.rows.len()
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
        for (index, row) in self.rows.iter().enumerate() {
            let number = index + 1;
            let marker = if row.name == self.current {
                theme.paint(Token::Success, "  ← current")
            } else {
                String::new()
            };
            if index == self.selected {
                body.push(format!(
                    "{} {}{marker}",
                    theme.bold(Token::Accent, &format!("▶ {number}.")),
                    theme.bold(Token::TextStrong, &row.name)
                ));
                body.push(theme.paint(
                    Token::TextDim,
                    &format!(
                        "     {}",
                        width::truncate_to_width(&row.description, columns.saturating_sub(10))
                    ),
                ));
            } else {
                body.push(format!(
                    "  {} {}{marker}",
                    theme.paint(Token::TextDim, &format!("{number}.")),
                    theme.paint(Token::Text, &row.name)
                ));
            }
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "↑/↓ select · 1-9 choose · ↵ apply · esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// The reasoning-effort selector (`/effort` with no args): the
/// provider's thinking levels (or the standard ramp), the live level
/// marked. Enter applies, Esc cancels.
pub struct EffortPickerDialog {
    title: String,
    levels: Vec<String>,
    /// The live level (`off` when unset; drives the marker).
    current: String,
    selected: usize,
}

impl EffortPickerDialog {
    /// Build the picker over `levels`, seeding `off` at the front when
    /// the provider list omits it (off is always available).
    pub fn new(current: Option<&str>, levels: &[String]) -> Self {
        let mut levels = levels.to_vec();
        if !levels.iter().any(|level| level == "off") {
            levels.insert(0, "off".to_string());
        }
        let current = current.unwrap_or("off").to_lowercase();
        let selected = levels
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        Self {
            title: "Select a reasoning effort".to_string(),
            levels,
            current,
            selected,
        }
    }

    fn resolve(&self) -> Answer {
        let level = &self.levels[self.selected];
        Answer::EffortSelected {
            level: (level != "off").then(|| level.clone()),
        }
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Up => {
                self.selected = if self.selected == 0 {
                    self.levels.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.levels.len();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                if let Ok(digit) = c.to_string().parse::<usize>()
                    && digit >= 1
                    && digit <= self.levels.len()
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
        for (index, level) in self.levels.iter().enumerate() {
            let number = index + 1;
            let marker = if *level == self.current {
                theme.paint(Token::Success, "  ← current")
            } else {
                String::new()
            };
            if index == self.selected {
                body.push(format!(
                    "{} {}{marker}",
                    theme.bold(Token::Accent, &format!("▶ {number}.")),
                    theme.bold(Token::TextStrong, level)
                ));
            } else {
                body.push(format!(
                    "  {} {}{marker}",
                    theme.paint(Token::TextDim, &format!("{number}.")),
                    theme.paint(Token::Text, level)
                ));
            }
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "↑/↓ select · 1-9 choose · ↵ apply · esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// The bare-command free-text prompt: one prefilled editable line.
/// Enter submits (trimmed; empty allowed where the command treats it
/// as "use the default"), Esc cancels.
pub struct PromptDialog {
    title: String,
    /// Where the answer routes back to.
    purpose: PromptPurpose,
    /// The editable line, prefilled by the caller.
    value: String,
    hint: &'static str,
}

impl PromptDialog {
    /// Build the prompt over `initial` text.
    pub fn new(title: &str, purpose: PromptPurpose, initial: &str, hint: &'static str) -> Self {
        Self {
            title: title.to_string(),
            purpose,
            value: initial.to_string(),
            hint,
        }
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        match event.key {
            Key::Esc => Some(Answer::Dismissed),
            Key::Backspace => {
                self.value.pop();
                None
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                self.value.push(c);
                None
            }
            Key::Enter => Some(Answer::Prompt {
                purpose: self.purpose.clone(),
                value: self.value.trim().to_string(),
            }),
            _ => None,
        }
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let body = vec![
            theme.paint(
                Token::Text,
                &width::truncate_to_width(&self.value, columns.saturating_sub(6)),
            ),
            String::new(),
            theme.paint(Token::TextDim, self.hint),
        ];
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}

/// One editable row of the model spec form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormRow {
    /// Free text (required for the identity fields).
    Text,
    /// The API dialect, cycled with ←/→ over its spellings.
    ApiKind,
    /// Unsigned token count; blank means unset.
    Count,
    /// Comma-separated tokens (thinking variants, modalities).
    List,
}

struct FormField {
    label: &'static str,
    kind: FormRow,
    value: String,
}

impl FormField {
    fn new(label: &'static str, kind: FormRow, value: &str) -> Self {
        Self {
            label,
            kind,
            value: value.to_string(),
        }
    }
}

/// The `/model add` spec form: one field per catalog property, filled
/// top to bottom. Enter advances (and saves on the last field), ←/→
/// cycles the API dialect, Esc cancels. Required identity fields must
/// be non-blank and count fields must parse before the form resolves.
impl Default for ModelFormDialog {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ModelFormDialog {
    title: String,
    fields: Vec<FormField>,
    active: usize,
    error: Option<String>,
}

impl ModelFormDialog {
    /// A blank form in fill order (thinking/modalities preseeded with
    /// the catalog defaults).
    pub fn new() -> Self {
        Self {
            title: "Add a model".to_string(),
            fields: vec![
                FormField::new("alias", FormRow::Text, ""),
                FormField::new("api", FormRow::ApiKind, "anthropic-messages"),
                FormField::new("provider", FormRow::Text, ""),
                FormField::new("base url", FormRow::Text, ""),
                FormField::new("model", FormRow::Text, ""),
                FormField::new("api key env", FormRow::Text, ""),
                FormField::new("context tokens", FormRow::Count, ""),
                FormField::new("max output", FormRow::Count, ""),
                FormField::new("thinking", FormRow::List, ""),
                FormField::new("input modalities", FormRow::List, "text"),
                FormField::new("output modalities", FormRow::List, "text"),
            ],
            active: 0,
            error: None,
        }
    }

    fn api_kinds() -> [&'static str; 3] {
        ["anthropic-messages", "openai-chat", "openai-responses"]
    }

    /// Compose the spec from the form values; `Err` names the problem
    /// and points at the offending field.
    fn resolve(&self) -> Result<(String, wavecode_config::ModelSpec), (usize, String)> {
        let value = |index: usize| self.fields[index].value.trim();
        for index in [0, 2, 3, 4] {
            if value(index).is_empty() {
                return Err((index, format!("{} is required", self.fields[index].label)));
            }
        }
        let context_window = match value(6).parse::<u64>() {
            Ok(parsed) => Some(parsed),
            Err(_) if value(6).is_empty() => None,
            Err(_) => return Err((6, "context tokens must be a number".to_string())),
        };
        let max_output = match value(7).parse::<u32>() {
            Ok(parsed) => Some(parsed),
            Err(_) if value(7).is_empty() => None,
            Err(_) => return Err((7, "max output must be a number".to_string())),
        };
        let kind = match value(1) {
            "openai-chat" => wavecode_config::ApiKind::OpenaiChat,
            "openai-responses" => wavecode_config::ApiKind::OpenaiResponses,
            _ => wavecode_config::ApiKind::AnthropicMessages,
        };
        // Thinking: a variant list enables the control, the first
        // variant is the default effort.
        let variants: Vec<String> = value(8)
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect();
        let reasoning = wavecode_config::ReasoningSpec {
            enabled: !variants.is_empty(),
            default: variants.first().cloned(),
            variants,
        };
        let split_list = |raw: &str| -> Vec<String> {
            raw.split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect()
        };
        let spec = wavecode_config::ModelSpec {
            provider: value(2).to_string(),
            model: value(4).to_string(),
            kind,
            base_url: value(3).to_string(),
            api_key_env: (!value(5).is_empty()).then(|| value(5).to_string()),
            api_key: None,
            context_window,
            max_output,
            reasoning,
            modalities: wavecode_config::ModalitiesSpec {
                input: split_list(value(9)),
                output: split_list(value(10)),
            },
        };
        Ok((value(0).to_string(), spec))
    }

    fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        let last = self.fields.len() - 1;
        match event.key {
            Key::Esc => return Some(Answer::Dismissed),
            Key::Up => {
                self.active = self.active.saturating_sub(1);
            }
            Key::Down => {
                self.active = (self.active + 1).min(last);
            }
            Key::Left | Key::Right if self.fields[self.active].kind == FormRow::ApiKind => {
                let kinds = Self::api_kinds();
                let current = self.fields[self.active].value.clone();
                let position = kinds.iter().position(|kind| *kind == current).unwrap_or(0);
                let step = if event.key == Key::Right {
                    1
                } else {
                    kinds.len() - 1
                };
                self.fields[self.active].value = kinds[(position + step) % kinds.len()].to_string();
            }
            Key::Backspace => {
                self.fields[self.active].value.pop();
                self.error = None;
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                let field = &mut self.fields[self.active];
                // The dialect cycles, it does not take typed text; count
                // fields only take digits.
                let editable = field.kind != FormRow::ApiKind;
                if editable && (field.kind != FormRow::Count || c.is_ascii_digit()) {
                    field.value.push(c);
                }
                self.error = None;
            }
            Key::Enter => {
                if self.active < last {
                    self.active += 1;
                } else {
                    match self.resolve() {
                        Ok((alias, spec)) => return Some(Answer::ModelForm { alias, spec }),
                        Err((index, message)) => {
                            self.active = index;
                            self.error = Some(message);
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let label_width = self
            .fields
            .iter()
            .map(|field| field.label.len())
            .max()
            .unwrap_or(0);
        let mut body = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            let label = format!("{:>width$}:", field.label, width = label_width);
            let value = if index == self.active {
                let mut text = field.value.clone();
                text.push('▏');
                theme.bold(Token::TextStrong, &text)
            } else if field.value.is_empty() {
                theme.paint(Token::TextMuted, "(unset)")
            } else {
                theme.paint(Token::Text, &field.value)
            };
            let label = if index == self.active {
                theme.bold(Token::Accent, &label)
            } else {
                theme.paint(Token::TextDim, &label)
            };
            body.push(format!("{label} {value}"));
        }
        if let Some(error) = &self.error {
            body.push(theme.paint(Token::Warning, error));
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextDim,
            "↑/↓ field · ←/→ api · ↵ next / save · esc cancel",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
