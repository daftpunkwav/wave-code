/*!
 * @file DurableGoal
 * @description Durable per-session objective with CAS versioning and a round driver.
 *
 * Responsibilities:
 * - Own the Active/Blocked/Paused/Completed transitions with CAS guards.
 * - Enforce the round cap through round_tick().
 * - Persist one goal file per session under `<home>/.wavecode/goals/`.
 * - Host the model-invokable `goal` tool over the machine (`tool` module).
 *
 * The crate root is pure data; the `tool` module adds the single
 * capability edge (wavecode-tools) that renders the machine
 * model-invokable. Nothing here depends on: runtime, action, safety,
 * operations, transport, or any orchestration layer.
 */

//! Durable goal service: one persisted objective per session.
//!
//! The model sets the objective and intermediate sub-goals (the `set`
//! action), mutates them under optimistic concurrency (the `update`
//! action requires the expected version), reads the whole tree (the
//! `status` action), and advances the round driver once per round (the
//! `tick` action). The loop reads this state to decide whether a turn should
//! continue after the model stops (`GoalTracker` in runtime-runner, wired by
//! `operations-bootstrap::goal_adapter`); it never writes it, so the tool
//! stays the single writer. State survives resume because the file path
//! derives from the home directory.

pub mod tool;

use infrastructure_base::now_secs;
use std::path::{Path, PathBuf};

/// Directory name under `<home>/.wavecode` holding goal files.
pub const GOALS_DIR: &str = "goals";

/// Round cap for the driver: the tick that would move past this round
/// blocks the goal instead and reports the cap as a business error.
pub const MAX_ROUND: u32 = 256;

/// Cap on tracked sub-goals: replaces beyond it are rejected as business
/// errors, so a runaway model cannot grow the goal file without bound.
pub const MAX_SUB_GOALS: usize = 64;

/// Progress state of one intermediate sub-goal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubGoalStatus {
    /// The sub-goal is being worked on (the fresh default).
    #[default]
    InProgress,
    /// The sub-goal has been reached.
    Achieved,
}

impl SubGoalStatus {
    /// Wire/display name, locked by tests below.
    pub fn as_str(self) -> &'static str {
        match self {
            SubGoalStatus::InProgress => "in_progress",
            SubGoalStatus::Achieved => "achieved",
        }
    }
}

impl std::fmt::Display for SubGoalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse a wire sub-goal status name; unknown names name the valid set so
/// the model can self-correct.
pub fn parse_sub_goal_status(raw: &str) -> Result<SubGoalStatus, GoalError> {
    match raw {
        "in_progress" => Ok(SubGoalStatus::InProgress),
        "achieved" => Ok(SubGoalStatus::Achieved),
        _ => Err(GoalError::InvalidInput {
            message: format!(
                "unknown sub-goal status {raw:?}: expected one of in_progress, achieved"
            ),
        }),
    }
}

/// One intermediate sub-goal under the main objective.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubGoal {
    /// What "done" means for this intermediate step.
    pub text: String,
    /// Current progress of the sub-goal.
    pub status: SubGoalStatus,
}

/// Lifecycle status of one durable goal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The goal is in progress (also the fresh default).
    #[default]
    Active,
    /// Progress stalled; resume or update may return it to Active.
    Blocked,
    /// Parked by the operator or the model; resume returns it to Active.
    Paused,
    /// Terminal: reached. Only a fresh set leaves this state.
    Completed,
}

impl GoalStatus {
    /// Wire/display name, locked by tests below.
    pub fn as_str(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Paused => "paused",
            GoalStatus::Completed => "completed",
        }
    }

    /// True for the terminal state no transition leaves.
    pub fn is_terminal(self) -> bool {
        matches!(self, GoalStatus::Completed)
    }
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse a wire status name into a GoalStatus; unknown names name the
/// valid set so the model can self-correct.
pub fn parse_status(raw: &str) -> Result<GoalStatus, GoalError> {
    match raw {
        "active" => Ok(GoalStatus::Active),
        "blocked" => Ok(GoalStatus::Blocked),
        "paused" => Ok(GoalStatus::Paused),
        "completed" => Ok(GoalStatus::Completed),
        _ => Err(GoalError::InvalidInput {
            message: format!(
                "unknown goal status {raw:?}: expected one of active, blocked, paused, completed"
            ),
        }),
    }
}

/// Business failures of the goal machine: CAS mismatches name both
/// versions, illegal transitions name the expected state, and the round
/// cap names the cap, so the model can self-correct in each case.
#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    /// Transition attempted from the wrong status.
    #[error("cannot {action}: expected status {expected}, found {actual}")]
    UnexpectedState {
        /// What the caller tried to do.
        action: &'static str,
        /// Status the transition requires.
        expected: &'static str,
        /// Status the goal actually holds.
        actual: &'static str,
    },
    /// Optimistic-concurrency guard failed: the caller worked from a
    /// stale snapshot and must reload before retrying.
    #[error(
        "goal version mismatch: expected version {expected}, found version {actual}; reload with goal status and retry"
    )]
    VersionMismatch {
        /// Version the caller based its edit on.
        expected: u64,
        /// Version the goal actually holds.
        actual: u64,
    },
    /// The round driver hit the cap: the goal is now Blocked.
    #[error("goal round cap reached: round {round} at cap {cap}; goal is now blocked")]
    RoundCapped {
        /// Round the driver held when the cap bit.
        round: u32,
        /// The enforced cap ([`MAX_ROUND`]).
        cap: u32,
    },
    /// Blank objective or unknown status name.
    #[error("invalid goal input: {message}")]
    InvalidInput {
        /// Human-readable reason.
        message: String,
    },
    /// Session id outside `[A-Za-z0-9_-]{1,64}` (it becomes a file name).
    #[error("invalid session id {id:?}: expected 1-64 chars ([A-Za-z0-9_-])")]
    InvalidSessionId {
        /// The rejected id.
        id: String,
    },
    /// Stored goal exists but does not parse.
    #[error("goal file unreadable: {message}")]
    Corrupt {
        /// Human-readable reason.
        message: String,
    },
    /// Filesystem failure while loading or saving.
    #[error("goal IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// One durable goal: objective, status, CAS version, driver round, the
/// last mutation time, and the intermediate sub-goal list.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoalState {
    /// Current objective text (empty until the first set).
    pub objective: String,
    /// Current lifecycle status.
    pub status: GoalStatus,
    /// CAS version, bumped by every mutation (set/update/tick/moves).
    pub version: u64,
    /// Driver round, advanced by `round_tick` up to [`MAX_ROUND`].
    pub round: u32,
    /// Last mutation time as unix seconds.
    pub updated_at: u64,
    /// Intermediate sub-goals under the objective. Absent in files written
    /// before sub-goals existed; `default` keeps those loads compatible.
    #[serde(default)]
    pub sub_goals: Vec<SubGoal>,
}

impl GoalState {
    /// Record a successful mutation: bump the CAS version and stamp time.
    /// The shared clock saturates to zero on a pre-epoch clock, so the
    /// stamp never fails the mutation.
    fn touch(&mut self) {
        self.version = self.version.saturating_add(1);
        self.updated_at = now_secs();
    }

    /// Reject blank objectives shared by set and update.
    fn require_objective(text: &str) -> Result<String, GoalError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(GoalError::InvalidInput {
                message: "objective must not be blank".to_string(),
            });
        }
        Ok(trimmed.to_string())
    }

    /// Enforce the optimistic-concurrency guard before any CAS update.
    fn check_version(&self, expected_version: u64) -> Result<(), GoalError> {
        if self.version != expected_version {
            return Err(GoalError::VersionMismatch {
                expected: expected_version,
                actual: self.version,
            });
        }
        Ok(())
    }

    /// Move to a new status: any non-terminal state reaches any other
    /// state; the terminal Completed state only leaves via `set`.
    /// Same-state moves are idempotent no-ops (no version bump).
    fn move_to(
        &mut self,
        action: &'static str,
        next: GoalStatus,
        expected: &'static str,
    ) -> Result<(), GoalError> {
        if next == self.status {
            return Ok(());
        }
        if self.status.is_terminal() {
            return Err(GoalError::UnexpectedState {
                action,
                expected,
                actual: self.status.as_str(),
            });
        }
        self.status = next;
        self.touch();
        Ok(())
    }

    /// Set (or replace) the objective: always allowed, resets the round
    /// driver to zero, drops the previous sub-goal list, and returns the
    /// goal to Active, so a finished goal restarts cleanly on the next set.
    pub fn set(&mut self, objective: &str) -> Result<(), GoalError> {
        self.objective = Self::require_objective(objective)?;
        self.status = GoalStatus::Active;
        self.round = 0;
        self.sub_goals.clear();
        self.touch();
        Ok(())
    }

    /// Replace the whole sub-goal list under optimistic concurrency: the
    /// expected version must match, blank texts are rejected, and the list
    /// is capped at [`MAX_SUB_GOALS`] so a runaway model cannot grow the
    /// goal file without bound. Every accepted replace bumps the version.
    pub fn replace_sub_goals(
        &mut self,
        expected_version: u64,
        sub_goals: Vec<SubGoal>,
    ) -> Result<(), GoalError> {
        self.check_version(expected_version)?;
        if sub_goals.len() > MAX_SUB_GOALS {
            return Err(GoalError::InvalidInput {
                message: format!(
                    "too many sub-goals ({}): the cap is {MAX_SUB_GOALS}",
                    sub_goals.len()
                ),
            });
        }
        let mut cleaned = Vec::with_capacity(sub_goals.len());
        for SubGoal { text, status } in sub_goals {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err(GoalError::InvalidInput {
                    message: "sub-goal text must not be blank".to_string(),
                });
            }
            cleaned.push(SubGoal {
                text: trimmed.to_string(),
                status,
            });
        }
        self.sub_goals = cleaned;
        self.touch();
        Ok(())
    }

    /// Update under optimistic concurrency: the expected version must
    /// match, then the objective and/or status apply atomically. Status
    /// moves follow the same reachability as the named transitions.
    /// Every accepted update bumps the version, even a no-op one, so the
    /// version stays a monotonic witness of writer activity.
    pub fn update(
        &mut self,
        expected_version: u64,
        objective: Option<&str>,
        status: Option<GoalStatus>,
    ) -> Result<(), GoalError> {
        self.check_version(expected_version)?;
        // A terminal goal is closed for edits of any kind: leaving the
        // state (or rewriting its objective) goes through `set`, which
        // restarts the round driver cleanly. The guard must not depend on
        // the caller also passing `status` — an objective-only update
        // would otherwise rewrite a Completed goal in place.
        if self.status.is_terminal() {
            return Err(GoalError::UnexpectedState {
                action: "update",
                expected: "a non-terminal goal (set a new objective to restart)",
                actual: self.status.as_str(),
            });
        }
        let next_objective = match objective {
            Some(text) => Some(Self::require_objective(text)?),
            None => None,
        };
        if let Some(text) = next_objective {
            self.objective = text;
        }
        if let Some(next) = status {
            self.status = next;
        }
        self.touch();
        Ok(())
    }

    /// Pause progress: Active or Blocked -> Paused.
    pub fn pause(&mut self) -> Result<(), GoalError> {
        self.move_to("pause", GoalStatus::Paused, "active, blocked, or paused")
    }

    /// Resume progress: Paused or Blocked -> Active.
    pub fn resume(&mut self) -> Result<(), GoalError> {
        self.move_to("resume", GoalStatus::Active, "paused, blocked, or active")
    }

    /// Mark the goal blocked: Active or Paused -> Blocked.
    pub fn block(&mut self) -> Result<(), GoalError> {
        self.move_to("block", GoalStatus::Blocked, "active, paused, or blocked")
    }

    /// Mark the goal complete: Active, Paused, or Blocked -> Completed.
    pub fn complete(&mut self) -> Result<(), GoalError> {
        self.move_to(
            "complete",
            GoalStatus::Completed,
            "active, paused, or blocked",
        )
    }

    /// Advance the round driver by one round and bump the version, so the
    /// next update needs the freshly reported version. Ticks on a
    /// Completed goal fail openly; the tick that would move past
    /// [`MAX_ROUND`] blocks the goal and reports the cap instead.
    pub fn round_tick(&mut self) -> Result<(), GoalError> {
        if self.status.is_terminal() {
            return Err(GoalError::UnexpectedState {
                action: "tick",
                expected: "active, blocked, or paused",
                actual: self.status.as_str(),
            });
        }
        if self.round >= MAX_ROUND {
            self.status = GoalStatus::Blocked;
            self.touch();
            return Err(GoalError::RoundCapped {
                round: self.round,
                cap: MAX_ROUND,
            });
        }
        self.round += 1;
        self.touch();
        Ok(())
    }
}

/// Goals root for a home directory: `<home>/.wavecode/goals`.
pub fn goals_root_for_home(home: &Path) -> PathBuf {
    home.join(".wavecode").join(GOALS_DIR)
}

/// Validate a session id: it becomes a single file name, so path
/// separators and traversal sequences are rejected outright.
pub fn validate_session_id(id: &str) -> Result<(), GoalError> {
    // Same cap as the session registry (sessions.rs): one id must be
    // valid everywhere, not legal in one store and rejected in another.
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(GoalError::InvalidSessionId { id: id.to_string() });
    }
    Ok(())
}

/// Goal file for one session: `<goals_root>/<session_id>.json`.
pub fn goal_path_for_session(goals_root: &Path, session_id: &str) -> Result<PathBuf, GoalError> {
    validate_session_id(session_id)?;
    Ok(goals_root.join(format!("{session_id}.json")))
}

/// Load a goal from a file path: a missing file reads as a fresh goal
/// (first run or degraded home), corrupt content is an explicit error.
pub fn load_from_path(path: &Path) -> Result<GoalState, GoalError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GoalState::default()),
        Err(e) => return Err(GoalError::Io(e)),
    };
    serde_json::from_str(&text).map_err(|e| GoalError::Corrupt {
        message: e.to_string(),
    })
}

/// Save a goal atomically: write a sibling temp file, then rename over
/// the target so a crash never leaves a half-written goal behind.
pub fn save_to_path(state: &GoalState, path: &Path) -> Result<(), GoalError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state).map_err(|e| GoalError::Corrupt {
        message: e.to_string(),
    })?;
    infrastructure_base::atomic_write(path, text.as_bytes())?;
    Ok(())
}

/// Load the goal for one session: `<home>/.wavecode/goals/<id>.json`.
/// No home means no persistence (fresh goal, mirroring the memory
/// assembly gate); a missing file also reads as a fresh goal.
pub fn load_for_session(home: Option<&Path>, session_id: &str) -> Result<GoalState, GoalError> {
    let Some(home) = home else {
        return Ok(GoalState::default());
    };
    let path = goal_path_for_session(&goals_root_for_home(home), session_id)?;
    load_from_path(&path)
}

/// Save the goal for one session; a no-op without a home directory.
pub fn save_for_session(
    state: &GoalState,
    home: Option<&Path>,
    session_id: &str,
) -> Result<(), GoalError> {
    let Some(home) = home else {
        return Ok(());
    };
    let path = goal_path_for_session(&goals_root_for_home(home), session_id)?;
    save_to_path(state, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_names_and_terminals_are_locked() {
        assert_eq!(GoalStatus::Active.as_str(), "active");
        assert_eq!(GoalStatus::Blocked.as_str(), "blocked");
        assert_eq!(GoalStatus::Paused.as_str(), "paused");
        assert_eq!(GoalStatus::Completed.as_str(), "completed");
        assert!(!GoalStatus::Active.is_terminal());
        assert!(!GoalStatus::Blocked.is_terminal());
        assert!(!GoalStatus::Paused.is_terminal());
        assert!(GoalStatus::Completed.is_terminal());
        let value = serde_json::to_value(GoalStatus::Paused).unwrap();
        assert_eq!(value, serde_json::json!("paused"));
    }

    #[test]
    fn status_names_parse_and_reject_unknown() {
        assert_eq!(parse_status("active").unwrap(), GoalStatus::Active);
        assert_eq!(parse_status("blocked").unwrap(), GoalStatus::Blocked);
        assert_eq!(parse_status("paused").unwrap(), GoalStatus::Paused);
        assert_eq!(parse_status("completed").unwrap(), GoalStatus::Completed);
        let err = parse_status("done").unwrap_err().to_string();
        assert!(err.contains("active"), "{err}");
        assert!(err.contains("blocked"), "{err}");
    }

    #[test]
    fn set_replaces_and_restarts_the_driver() {
        let mut goal = GoalState::default();
        goal.set("ship the milestone").unwrap();
        assert_eq!(goal.objective, "ship the milestone");
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.version, 1);
        assert_eq!(goal.round, 0);
        goal.round_tick().unwrap();
        goal.pause().unwrap();
        // A fresh set resets the round driver and returns to Active.
        goal.set("next milestone").unwrap();
        assert_eq!(goal.objective, "next milestone");
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.round, 0);
        assert_eq!(goal.version, 4);
        // Completion restarts the same way.
        goal.complete().unwrap();
        goal.set("after completion").unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
    }

    #[test]
    fn cas_mismatch_names_both_versions() {
        let mut goal = GoalState::default();
        goal.set("ship it").unwrap();
        let err = goal
            .update(999, Some("stale edit"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected version 999"), "{err}");
        assert!(err.contains("found version 1"), "{err}");
        // The failed update mutated nothing.
        assert_eq!(goal.objective, "ship it");
        assert_eq!(goal.version, 1);
        // The fresh version applies objective and status atomically.
        goal.update(1, Some("fresh edit"), Some(GoalStatus::Paused))
            .unwrap();
        assert_eq!(goal.objective, "fresh edit");
        assert_eq!(goal.status, GoalStatus::Paused);
        assert_eq!(goal.version, 2);
    }

    #[test]
    fn pause_resume_block_complete_cycle() {
        let mut goal = GoalState::default();
        goal.set("cycle").unwrap();
        goal.pause().unwrap();
        assert_eq!(goal.status, GoalStatus::Paused);
        goal.resume().unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
        goal.block().unwrap();
        assert_eq!(goal.status, GoalStatus::Blocked);
        goal.resume().unwrap();
        goal.complete().unwrap();
        assert_eq!(goal.status, GoalStatus::Completed);
        assert!(goal.status.is_terminal());
    }

    #[test]
    fn illegal_transitions_name_the_expected_state() {
        let mut goal = GoalState::default();
        goal.set("x").unwrap();
        goal.complete().unwrap();
        // The terminal state leaves through no named transition.
        for err in [
            goal.pause().unwrap_err().to_string(),
            goal.resume().unwrap_err().to_string(),
            goal.block().unwrap_err().to_string(),
            goal.round_tick().unwrap_err().to_string(),
            goal.update(goal.version, None, Some(GoalStatus::Active))
                .unwrap_err()
                .to_string(),
        ] {
            assert!(err.contains("completed"), "{err}");
        }
        // Re-completing the terminal state is an idempotent no-op.
        let version = goal.version;
        goal.complete().unwrap();
        assert_eq!(goal.version, version);
        // Same-state moves stay idempotent instead of failing.
        let mut idle = GoalState::default();
        idle.set("y").unwrap();
        let version = idle.version;
        idle.resume().unwrap();
        assert_eq!(idle.version, version);
    }

    /// A terminal goal is closed for edits even when the caller does not
    /// pass a status: an objective-only update must not rewrite a
    /// Completed goal in place (restarting goes through `set`).
    #[test]
    fn terminal_goal_rejects_objective_only_updates() {
        let mut goal = GoalState::default();
        goal.set("done deal").unwrap();
        goal.complete().unwrap();
        let version = goal.version;
        let err = goal.update(version, Some("rewritten"), None).unwrap_err();
        assert!(matches!(err, GoalError::UnexpectedState { .. }), "{err}");
        assert_eq!(goal.objective, "done deal");
        assert_eq!(goal.status, GoalStatus::Completed);
        assert_eq!(goal.version, version, "rejected update must not bump");
        // `set` remains the way out of the terminal state.
        goal.set("round two").unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
    }

    #[test]
    fn round_cap_blocks_with_reason() {
        let mut goal = GoalState::default();
        goal.set("bounded").unwrap();
        for _ in 0..MAX_ROUND {
            goal.round_tick().unwrap();
        }
        assert_eq!(goal.round, MAX_ROUND);
        let version = goal.version;
        let err = goal.round_tick().unwrap_err().to_string();
        assert!(err.contains("cap"), "{err}");
        assert!(err.contains("blocked"), "{err}");
        assert_eq!(goal.status, GoalStatus::Blocked);
        assert_eq!(goal.round, MAX_ROUND);
        assert_eq!(goal.version, version + 1);
        // A blocked goal at the cap stays blocked and keeps reporting.
        assert!(goal.round_tick().is_err());
        assert_eq!(goal.status, GoalStatus::Blocked);
    }

    #[test]
    fn blank_objectives_are_rejected() {
        let mut goal = GoalState::default();
        assert!(goal.set("   ").is_err());
        assert_eq!(goal.version, 0);
        goal.set("real").unwrap();
        assert!(goal.update(goal.version, Some("  "), None).is_err());
        assert_eq!(goal.objective, "real");
    }

    #[test]
    fn sub_goal_status_names_are_locked() {
        assert_eq!(SubGoalStatus::InProgress.as_str(), "in_progress");
        assert_eq!(SubGoalStatus::Achieved.as_str(), "achieved");
        let value = serde_json::to_value(SubGoalStatus::Achieved).unwrap();
        assert_eq!(value, serde_json::json!("achieved"));
        assert_eq!(
            parse_sub_goal_status("in_progress").unwrap(),
            SubGoalStatus::InProgress
        );
        let err = parse_sub_goal_status("done").unwrap_err().to_string();
        assert!(err.contains("in_progress"), "{err}");
        assert!(err.contains("achieved"), "{err}");
    }

    #[test]
    fn sub_goals_replace_under_cas_and_set_clears() {
        let mut goal = GoalState::default();
        goal.set("ship it").unwrap();
        // CAS mismatch is rejected and mutates nothing.
        assert!(
            goal.replace_sub_goals(
                999,
                vec![SubGoal {
                    text: "x".into(),
                    status: SubGoalStatus::InProgress
                }]
            )
            .is_err()
        );
        assert!(goal.sub_goals.is_empty());
        // The fresh version applies the whole list.
        goal.replace_sub_goals(
            1,
            vec![
                SubGoal {
                    text: "fix the bug".into(),
                    status: SubGoalStatus::Achieved,
                },
                SubGoal {
                    text: "add tests".into(),
                    status: SubGoalStatus::InProgress,
                },
            ],
        )
        .unwrap();
        assert_eq!(goal.sub_goals.len(), 2);
        assert_eq!(goal.sub_goals[0].status, SubGoalStatus::Achieved);
        assert_eq!(goal.version, 2);
        // Blank texts are rejected outright.
        assert!(
            goal.replace_sub_goals(
                goal.version,
                vec![SubGoal {
                    text: "   ".into(),
                    status: SubGoalStatus::InProgress
                }]
            )
            .is_err()
        );
        // The cap rejects oversized lists.
        let overflow = vec![
            SubGoal {
                text: "x".into(),
                status: SubGoalStatus::InProgress
            };
            MAX_SUB_GOALS + 1
        ];
        assert!(goal.replace_sub_goals(goal.version, overflow).is_err());
        // A fresh set drops the previous list (the goal restarts cleanly).
        goal.set("next milestone").unwrap();
        assert!(goal.sub_goals.is_empty());
    }

    #[test]
    fn sub_goals_default_keeps_older_files_loadable() {
        // A file written before sub-goals existed lacks the field entirely.
        let legacy =
            r#"{"objective":"old","status":"active","version":3,"round":1,"updated_at":0}"#;
        let state: GoalState = serde_json::from_str(legacy).unwrap();
        assert_eq!(state.objective, "old");
        assert!(state.sub_goals.is_empty());
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
        // Missing file reads as a fresh goal.
        let loaded = load_for_session(Some(home.path()), "s1").unwrap();
        assert_eq!(loaded, GoalState::default());
        // No home degrades to a fresh goal without touching disk.
        assert_eq!(load_for_session(None, "s1").unwrap(), GoalState::default());
        assert!(save_for_session(&GoalState::default(), None, "s1").is_ok());
        // Mutations survive a save/load round-trip.
        let mut goal = GoalState::default();
        goal.set("ship it").unwrap();
        goal.round_tick().unwrap();
        goal.pause().unwrap();
        save_for_session(&goal, Some(home.path()), "s1").unwrap();
        let path = goal_path_for_session(&goals_root_for_home(home.path()), "s1").unwrap();
        assert!(path.exists());
        let back = load_for_session(Some(home.path()), "s1").unwrap();
        assert_eq!(back, goal);
        // Corrupt content is an explicit error, never a silent default.
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(
            load_from_path(&path),
            Err(GoalError::Corrupt { .. })
        ));
    }
}
