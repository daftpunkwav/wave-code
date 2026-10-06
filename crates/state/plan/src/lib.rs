/*!
 * @file ReviewedPlan
 * @description Reviewed plan mode state machine with home-scoped persistence.
 *
 * Responsibilities:
 * - Own the Draft -> Proposed -> Approved -> Executing -> Done/Abandoned transitions.
 * - Reject out-of-order transitions as explicit business errors.
 * - Persist one plan file per session under `<home>/.wavecode/plans/`.
 * - Host the model-invokable `plan` tool over the machine (`tool` module).
 *
 * The crate root is pure data; the `tool` module adds the single
 * capability edge (wavecode-tools) that renders the machine
 * model-invokable. Nothing here depends on: runtime, action, safety,
 * operations, transport, or any orchestration layer.
 */

//! Reviewed plan mode: explore -> present -> approve -> execute.
//!
//! The model proposes a plan (`Proposed`), the user approves it or sends
//! it back with feedback (`Draft`), approved plans run (`Executing`) to a
//! terminal state (`Done` or `Abandoned`). State survives resume because
//! the file path derives from the home directory.

pub mod tool;

use std::path::{Path, PathBuf};

/// Directory name under `<home>/.wavecode` holding plan files.
pub const PLANS_DIR: &str = "plans";

/// Lifecycle status of one reviewed plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// No proposal yet, or feedback sent the proposal back for revision.
    #[default]
    Draft,
    /// A plan text awaits user review.
    Proposed,
    /// The user approved the proposal; execution may start.
    Approved,
    /// The approved plan is running.
    Executing,
    /// The plan ran to completion (terminal).
    Done,
    /// The plan was dropped (terminal).
    Abandoned,
}

impl PlanStatus {
    /// Wire/display name, locked by tests below.
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Draft => "draft",
            PlanStatus::Proposed => "proposed",
            PlanStatus::Approved => "approved",
            PlanStatus::Executing => "executing",
            PlanStatus::Done => "done",
            PlanStatus::Abandoned => "abandoned",
        }
    }

    /// True for the terminal states no transition leaves.
    pub fn is_terminal(self) -> bool {
        matches!(self, PlanStatus::Done | PlanStatus::Abandoned)
    }
}

impl std::fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Business failures of the plan machine: illegal transitions name the
/// expected state so the model can self-correct.
#[derive(thiserror::Error)]
pub enum PlanError {
    /// Transition attempted from the wrong status.
    #[error("cannot {action}: expected status {expected}, found {actual}")]
    UnexpectedState {
        /// What the caller tried to do.
        action: &'static str,
        /// Status the transition requires.
        expected: &'static str,
        /// Status the plan actually holds.
        actual: &'static str,
    },
    /// Blank plan or feedback text.
    #[error("invalid plan input: {message}")]
    InvalidInput {
        /// Human-readable reason.
        message: String,
    },
    /// Session id outside `[A-Za-z0-9_-]{1,128}` (it becomes a file name).
    /// The rejected id is not echoed back: error text ends up on terminals
    /// and logs, and the id is unvalidated input at this point.
    #[error("invalid session id (expected 1-128 ASCII chars [A-Za-z0-9_-])")]
    InvalidSessionId {
        /// The rejected id, kept for programmatic matching only.
        id: String,
    },
    /// Stored plan exists but does not parse.
    #[error("plan file unreadable: {message}")]
    Corrupt {
        /// Human-readable reason.
        message: String,
    },
    /// Filesystem failure while loading or saving.
    #[error("plan IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Debug keeps the variant name for diagnostics while routing the fields
/// through Display, which never echoes unvalidated input.
impl std::fmt::Debug for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let variant = match self {
            Self::UnexpectedState { .. } => "UnexpectedState",
            Self::InvalidInput { .. } => "InvalidInput",
            Self::InvalidSessionId { .. } => "InvalidSessionId",
            Self::Corrupt { .. } => "Corrupt",
            Self::Io(_) => "Io",
        };
        write!(f, "{variant}: {self}")
    }
}

/// One reviewed plan: status, current text, and the transition history.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanState {
    /// Current lifecycle status.
    pub status: PlanStatus,
    /// Current plan text (empty until first proposed).
    pub plan_text: String,
    /// Monotonic update counter, bumped by every transition.
    pub updated_round: u64,
    /// Transition history as (round, status) pairs in order.
    pub history: Vec<(u64, PlanStatus)>,
}

impl PlanState {
    /// Record a successful transition: bump the round and append history.
    fn record(&mut self, status: PlanStatus) {
        self.status = status;
        self.updated_round += 1;
        self.history.push((self.updated_round, status));
    }

    /// Reject blank text shared by propose and feedback.
    fn require_text(text: &str, what: &'static str) -> Result<String, PlanError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(PlanError::InvalidInput {
                message: format!("{what} must not be blank"),
            });
        }
        Ok(trimmed.to_string())
    }

    /// Propose a plan: Draft -> Proposed.
    pub fn propose(&mut self, text: &str) -> Result<(), PlanError> {
        if self.status != PlanStatus::Draft {
            return Err(PlanError::UnexpectedState {
                action: "propose",
                expected: PlanStatus::Draft.as_str(),
                actual: self.status.as_str(),
            });
        }
        self.plan_text = Self::require_text(text, "plan text")?;
        self.record(PlanStatus::Proposed);
        Ok(())
    }

    /// Approve the proposal: Proposed -> Approved.
    pub fn approve(&mut self) -> Result<(), PlanError> {
        if self.status != PlanStatus::Proposed {
            return Err(PlanError::UnexpectedState {
                action: "approve",
                expected: PlanStatus::Proposed.as_str(),
                actual: self.status.as_str(),
            });
        }
        self.record(PlanStatus::Approved);
        Ok(())
    }

    /// Send the proposal back with feedback: Proposed -> Draft, appending
    /// the feedback so the next proposal keeps the review context.
    pub fn feedback(&mut self, text: &str) -> Result<(), PlanError> {
        if self.status != PlanStatus::Proposed {
            return Err(PlanError::UnexpectedState {
                action: "feedback",
                expected: PlanStatus::Proposed.as_str(),
                actual: self.status.as_str(),
            });
        }
        let note = Self::require_text(text, "feedback text")?;
        if self.plan_text.trim().is_empty() {
            self.plan_text = note;
        } else {
            self.plan_text = format!("{}\n\nFeedback:\n{note}", self.plan_text);
        }
        self.record(PlanStatus::Draft);
        Ok(())
    }

    /// Start executing the approved plan: Approved -> Executing.
    pub fn begin(&mut self) -> Result<(), PlanError> {
        if self.status != PlanStatus::Approved {
            return Err(PlanError::UnexpectedState {
                action: "begin",
                expected: PlanStatus::Approved.as_str(),
                actual: self.status.as_str(),
            });
        }
        self.record(PlanStatus::Executing);
        Ok(())
    }

    /// Finish the running plan: Executing -> Done (terminal).
    pub fn complete(&mut self) -> Result<(), PlanError> {
        if self.status != PlanStatus::Executing {
            return Err(PlanError::UnexpectedState {
                action: "complete",
                expected: PlanStatus::Executing.as_str(),
                actual: self.status.as_str(),
            });
        }
        self.record(PlanStatus::Done);
        Ok(())
    }

    /// Drop the plan: Proposed, Approved, or Executing -> Abandoned.
    /// Abandon stays open before execution so a rejected direction never
    /// blocks the session on a stale proposal.
    pub fn abandon(&mut self) -> Result<(), PlanError> {
        match self.status {
            PlanStatus::Proposed | PlanStatus::Approved | PlanStatus::Executing => {
                self.record(PlanStatus::Abandoned);
                Ok(())
            }
            _ => Err(PlanError::UnexpectedState {
                action: "abandon",
                expected: "proposed, approved, or executing",
                actual: self.status.as_str(),
            }),
        }
    }
}

/// Plans root for a home directory: `<home>/.wavecode/plans`.
pub fn plans_root_for_home(home: &Path) -> PathBuf {
    home.join(".wavecode").join(PLANS_DIR)
}

/// Validate a session id: it becomes a single file name, so path
/// separators and traversal sequences are rejected outright.
pub fn validate_session_id(id: &str) -> Result<(), PlanError> {
    // Same cap as the session registry (sessions.rs): one id must be
    // valid everywhere, not legal in one store and rejected in another.
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(PlanError::InvalidSessionId { id: id.to_string() });
    }
    Ok(())
}

/// Plan file for one session: `<plans_root>/<session_id>.json`.
pub fn plan_path_for_session(plans_root: &Path, session_id: &str) -> Result<PathBuf, PlanError> {
    validate_session_id(session_id)?;
    Ok(plans_root.join(format!("{session_id}.json")))
}

/// Load a plan from a file path: a missing file reads as a fresh Draft
/// (first run or degraded home), corrupt content is an explicit error.
pub fn load_from_path(path: &Path) -> Result<PlanState, PlanError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PlanState::default()),
        Err(e) => return Err(PlanError::Io(e)),
    };
    serde_json::from_str(&text).map_err(|e| PlanError::Corrupt {
        message: e.to_string(),
    })
}

/// Save a plan atomically: write a sibling temp file, then rename over
/// the target so a crash never leaves a half-written plan behind. The
/// file lands owner-only: plan state mirrors the session conversation
/// and lives under `<home>/.wavecode`.
pub fn save_to_path(state: &PlanState, path: &Path) -> Result<(), PlanError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state).map_err(|e| PlanError::Corrupt {
        message: e.to_string(),
    })?;
    infrastructure_base::atomic_write_private(path, text.as_bytes())?;
    Ok(())
}

/// Load the plan for one session: `<home>/.wavecode/plans/<id>.json`.
/// No home means no persistence (fresh Draft, mirroring the memory
/// assembly gate); a missing file also reads as a fresh Draft.
pub fn load_for_session(home: Option<&Path>, session_id: &str) -> Result<PlanState, PlanError> {
    let Some(home) = home else {
        return Ok(PlanState::default());
    };
    let path = plan_path_for_session(&plans_root_for_home(home), session_id)?;
    load_from_path(&path)
}

/// Save the plan for one session; a no-op without a home directory.
pub fn save_for_session(
    state: &PlanState,
    home: Option<&Path>,
    session_id: &str,
) -> Result<(), PlanError> {
    let Some(home) = home else {
        return Ok(());
    };
    let path = plan_path_for_session(&plans_root_for_home(home), session_id)?;
    save_to_path(state, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_names_and_terminals_are_locked() {
        assert_eq!(PlanStatus::Draft.as_str(), "draft");
        assert_eq!(PlanStatus::Proposed.as_str(), "proposed");
        assert_eq!(PlanStatus::Approved.as_str(), "approved");
        assert_eq!(PlanStatus::Executing.as_str(), "executing");
        assert_eq!(PlanStatus::Done.as_str(), "done");
        assert_eq!(PlanStatus::Abandoned.as_str(), "abandoned");
        assert!(!PlanStatus::Draft.is_terminal());
        assert!(!PlanStatus::Executing.is_terminal());
        assert!(PlanStatus::Done.is_terminal());
        assert!(PlanStatus::Abandoned.is_terminal());
        let value = serde_json::to_value(PlanStatus::Proposed).unwrap();
        assert_eq!(value, serde_json::json!("proposed"));
    }

    #[test]
    fn happy_path_runs_the_full_lifecycle() {
        let mut plan = PlanState::default();
        plan.propose("migrate the store").unwrap();
        assert_eq!(plan.status, PlanStatus::Proposed);
        assert_eq!(plan.plan_text, "migrate the store");
        plan.approve().unwrap();
        plan.begin().unwrap();
        assert_eq!(plan.status, PlanStatus::Executing);
        plan.complete().unwrap();
        assert_eq!(plan.status, PlanStatus::Done);
        assert_eq!(plan.updated_round, 4);
        assert_eq!(
            plan.history,
            vec![
                (1, PlanStatus::Proposed),
                (2, PlanStatus::Approved),
                (3, PlanStatus::Executing),
                (4, PlanStatus::Done),
            ]
        );
    }

    #[test]
    fn feedback_returns_to_draft_and_appends() {
        let mut plan = PlanState::default();
        plan.propose("v1").unwrap();
        plan.feedback("add rollback").unwrap();
        assert_eq!(plan.status, PlanStatus::Draft);
        assert!(plan.plan_text.contains("v1"));
        assert!(plan.plan_text.contains("add rollback"));
        // A revised proposal keeps the review context on disk.
        plan.propose("v2").unwrap();
        assert!(plan.plan_text.contains("v2"));
    }

    /// Saved plan files land owner-only: plan text mirrors the session
    /// conversation and lives under `<home>/.wavecode`.
    #[test]
    #[cfg(unix)]
    fn saved_plan_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut plan = PlanState::default();
        plan.propose("migrate the store").unwrap();
        let path = dir.path().join("plan.json");
        save_to_path(&plan, &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "plan file stays owner-only: {mode:o}");
    }

    #[test]
    fn abandon_accepts_pre_terminal_states() {
        for setup in [
            |p: &mut PlanState| p.propose("x").unwrap(),
            |p: &mut PlanState| {
                p.propose("x").unwrap();
                p.approve().unwrap();
            },
            |p: &mut PlanState| {
                p.propose("x").unwrap();
                p.approve().unwrap();
                p.begin().unwrap();
            },
        ] {
            let mut plan = PlanState::default();
            setup(&mut plan);
            plan.abandon().unwrap();
            assert_eq!(plan.status, PlanStatus::Abandoned);
        }
    }

    #[test]
    fn illegal_transitions_name_the_expected_state() {
        let mut plan = PlanState::default();
        // approve from Draft: expects Proposed.
        let err = plan.approve().unwrap_err().to_string();
        assert!(err.contains("approve"), "{err}");
        assert!(err.contains("proposed"), "{err}");
        assert!(err.contains("draft"), "{err}");
        // begin from Draft: expects Approved.
        let err = plan.begin().unwrap_err().to_string();
        assert!(err.contains("approved"), "{err}");
        // complete from Draft: expects Executing.
        let err = plan.complete().unwrap_err().to_string();
        assert!(err.contains("executing"), "{err}");
        // feedback from Draft: expects Proposed.
        let err = plan.feedback("x").unwrap_err().to_string();
        assert!(err.contains("proposed"), "{err}");
        // propose twice without feedback: expects Draft.
        plan.propose("v1").unwrap();
        let err = plan.propose("v2").unwrap_err().to_string();
        assert!(err.contains("draft"), "{err}");
        // approve then approve again: expects Proposed.
        plan.approve().unwrap();
        let err = plan.approve().unwrap_err().to_string();
        assert!(err.contains("proposed"), "{err}");
        // abandon from Draft and from Done both fail openly.
        let mut fresh = PlanState::default();
        assert!(fresh.abandon().is_err());
        plan.begin().unwrap();
        plan.complete().unwrap();
        assert!(plan.status.is_terminal());
        let err = plan.complete().unwrap_err().to_string();
        assert!(err.contains("executing"), "{err}");
        assert!(plan.abandon().is_err());
        assert!(plan.propose("late").is_err());
    }

    #[test]
    fn blank_text_is_rejected() {
        let mut plan = PlanState::default();
        assert!(plan.propose("   ").is_err());
        assert_eq!(plan.status, PlanStatus::Draft);
        plan.propose("v1").unwrap();
        assert!(plan.feedback("  ").is_err());
        assert_eq!(plan.status, PlanStatus::Proposed);
    }

    #[test]
    fn session_ids_reject_path_traversal() {
        assert!(validate_session_id("default").is_ok());
        assert!(validate_session_id("thread-1_2").is_ok());
        // The 128-char cap matches the session registry (sessions.rs).
        assert!(validate_session_id(&"x".repeat(128)).is_ok());
        for bad in ["", "../evil", "a/b", "a.json", "x".repeat(129).as_str()] {
            assert!(validate_session_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn persistence_round_trips_in_tempdir() {
        let home = tempfile::tempdir().unwrap();
        // Missing file reads as a fresh Draft.
        let loaded = load_for_session(Some(home.path()), "s1").unwrap();
        assert_eq!(loaded, PlanState::default());
        // No home degrades to a fresh Draft without touching disk.
        assert_eq!(load_for_session(None, "s1").unwrap(), PlanState::default());
        assert!(save_for_session(&PlanState::default(), None, "s1").is_ok());
        // Full lifecycle survives a save/load round-trip.
        let mut plan = PlanState::default();
        plan.propose("ship it").unwrap();
        plan.approve().unwrap();
        plan.begin().unwrap();
        save_for_session(&plan, Some(home.path()), "s1").unwrap();
        let path = plan_path_for_session(&plans_root_for_home(home.path()), "s1").unwrap();
        assert!(path.exists());
        let back = load_for_session(Some(home.path()), "s1").unwrap();
        assert_eq!(back, plan);
        // Corrupt content is an explicit error, never a silent Draft.
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(
            load_from_path(&path),
            Err(PlanError::Corrupt { .. })
        ));
    }
}
