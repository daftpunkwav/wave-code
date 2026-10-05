//! Modal dialogs: tool approvals and structured questions.
//!
//! While a dialog is open it owns all key input. Approvals offer
//! numbered choices with quick-select digits, wrap-around navigation,
//! and Esc = deny. Questions mirror the layout for option answers;
//! free-text answers route through the embedded one-line input.

use tui_engine::keys::KeyEvent;
use wavecode_wire::WireDecision;

// Test-only imports: re-exported to the tests submodule through its
// `use super::*` glob.
#[cfg(test)]
use crate::theme::Token;
#[cfg(test)]
use tui_engine::keys::Key;
#[cfg(test)]
use wavecode_wire::ApprovalKind;

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
    /// Model specs built by the `/provider` wizard, ready to insert
    /// into the catalog (one wizard pass can stage several models on
    /// one provider).
    ModelForm {
        /// Each (alias, spec) in entry order.
        entries: Vec<(String, wavecode_config::ModelSpec)>,
    },
    /// A provider picked from the opening list (`None` = add a new
    /// one); the caller opens the wizard seeded from it.
    ProviderPicked {
        /// The provider name, or `None` for a brand-new provider.
        name: Option<String>,
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
    /// The `/provider` opening view: existing providers plus a
    /// new-provider row.
    ProviderPick(Box<ProviderPickerDialog>),
    /// The step-at-a-time model spec wizard.
    ModelForm(Box<ModelWizardDialog>),
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
            Self::ProviderPick(dialog) => dialog.title.clone(),
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
            Self::ProviderPick(dialog) => dialog.handle_key(event),
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
            Self::ProviderPick(dialog) => dialog.render(width),
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

mod approval;
mod effort_picker;
mod help;
mod model_picker;
mod model_wizard;
mod permission_picker;
mod prompt;
mod provider_picker;
mod question;
mod session_picker;
mod settings;
mod theme_picker;
mod undo_picker;

// Re-exports keep every historical `crate::dialogs::*` path
// resolving; the implementations live in the submodules above.
pub use approval::ApprovalDialog;
pub use effort_picker::EffortPickerDialog;
pub use help::HelpPanel;
pub use model_picker::{ModelEntryView, ModelPickerDialog};
pub use model_wizard::{API_KINDS, ModelWizardDialog, THINKING_PRESETS};
pub use permission_picker::PermissionPickerDialog;
pub use prompt::PromptDialog;
pub use provider_picker::ProviderPickerDialog;
pub use question::QuestionDialog;
pub use session_picker::{SessionPickerDialog, SessionRow};
pub use settings::SettingsDialog;
pub use theme_picker::ThemePickerDialog;
pub use undo_picker::{MAX_UNDO_ROWS, UndoPickerDialog, UndoRow};

// Test-only items, re-exported so the tests submodule
// `use super::*;` keeps resolving them unqualified.
#[cfg(test)]
pub(crate) use model_picker::filter_indices;
#[cfg(test)]
pub(crate) use model_wizard::WizardStep;

#[cfg(test)]
mod tests;
