/*!
 * @file StatusQueries
 * @description Composition-root status views for frontend slash commands.
 *
 * Responsibilities:
 * - Implement `operations_actor::StatusQueries` over the same stores the
 *   plan / goal / snapshot tools use, so displays always reflect current
 *   state.
 * - Own the storage layout knowledge frontends must never copy: paths,
 *   file shapes, and label rules stay here.
 * - Render bounded, human-facing text (body heads, not full documents).
 *
 * This module must not be depended on by: frontends receive only the
 * trait object; the concrete type never crosses the crate boundary.
 */

//! Status queries backing `/plan` `/goal` `/snapshots` `/rewind`.
//!
//! This is the single home of what the frontends previously duplicated
//! by reading `<home>/.wavecode/...` files by hand. Rendering here is the
//! bounded display view (body heads); the model-facing full-fidelity
//! renderers are [`state_plan::tool::render_status`] and
//! [`state_goal::tool::render_status`].

use std::path::Path;
use std::sync::Arc;

use operations_actor::StatusQueries;
use state_checkpoint::SnapshotStore;
use state_goal::GoalState;
use state_plan::PlanState;

use state_goal::tool::DEFAULT_GOAL_SESSION_ID;
use state_plan::tool::DEFAULT_PLAN_SESSION_ID;

/// Body head cap for plan / goal text in status displays; the full text
/// stays available through the model-facing status tools.
const HEAD_CHARS: usize = 500;

/// The composition root's status views: the only place that knows where
/// plan / goal / snapshot state lives and in what shape.
pub struct SessionStatus {
    home: Option<std::path::PathBuf>,
    snapshots: SnapshotStore,
}

impl SessionStatus {
    /// Build over the session's home and the snapshot root already used
    /// by the registered `snapshot` / `restore` tools (single root, never
    /// a frontend-derived re-resolution).
    pub fn new(home: Option<&Path>, snapshot_root: std::path::PathBuf) -> Self {
        Self {
            home: home.map(Path::to_path_buf),
            snapshots: SnapshotStore::new(snapshot_root),
        }
    }

    /// Shared as the frontend-facing trait object.
    pub fn shared(self) -> Arc<dyn StatusQueries> {
        Arc::new(self)
    }

    /// Render a plan state as bounded display text (`None` = no proposal).
    fn plan_view(&self) -> Option<String> {
        match state_plan::load_for_session(self.home.as_deref(), DEFAULT_PLAN_SESSION_ID) {
            Ok(state) => plan_display(&state),
            Err(e) => Some(format!(
                "(plan state unreadable: {e}; delete <home>/.wavecode/plans/{DEFAULT_PLAN_SESSION_ID}.json to start over)"
            )),
        }
    }

    /// Render a goal state as bounded display text (`None` = no goal).
    fn goal_view(&self) -> Option<String> {
        match state_goal::load_for_session(self.home.as_deref(), DEFAULT_GOAL_SESSION_ID) {
            Ok(state) => goal_display(&state),
            Err(e) => Some(format!(
                "(goal state unreadable: {e}; delete <home>/.wavecode/goals/{DEFAULT_GOAL_SESSION_ID}.json to start over)"
            )),
        }
    }
}

impl StatusQueries for SessionStatus {
    fn plan_status(&self) -> Option<String> {
        self.plan_view()
    }

    fn goal_status(&self) -> Option<String> {
        self.goal_view()
    }

    fn snapshot_labels(&self) -> Vec<String> {
        self.snapshots.list_labels()
    }

    fn snapshot_summary(&self, label: &str) -> Option<String> {
        match self.snapshots.load_info(label) {
            Ok(info) => Some(info.display()),
            Err(_) => None,
        }
    }
}

/// Bounded display of one plan state; `None` means nothing proposed yet.
fn plan_display(state: &PlanState) -> Option<String> {
    if state.plan_text.trim().is_empty() {
        return None;
    }
    let mut out = format!(
        "plan status: {} (updated round {})",
        state.status, state.updated_round
    );
    let body = state.plan_text.trim();
    let head: String = body.chars().take(HEAD_CHARS).collect();
    out.push_str(&format!("\nplan:\n{head}"));
    if body.chars().count() > HEAD_CHARS {
        out.push_str("\n(truncated; ask the agent for the full plan)");
    }
    Some(out)
}

/// Bounded display of one goal state; `None` means no goal yet.
fn goal_display(state: &GoalState) -> Option<String> {
    if state.objective.trim().is_empty() {
        return None;
    }
    let mut out = format!(
        "goal status: {} (version {}, round {})",
        state.status, state.version, state.round
    );
    let body = state.objective.trim();
    let head: String = body.chars().take(HEAD_CHARS).collect();
    out.push_str(&format!("\ngoal:\n{head}"));
    if body.chars().count() > HEAD_CHARS {
        out.push_str("\n(truncated; ask the agent for the full goal)");
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_states_report_none() {
        assert_eq!(plan_display(&PlanState::default()), None);
        assert_eq!(goal_display(&GoalState::default()), None);
    }

    #[test]
    fn plan_display_caps_long_text() {
        let state = PlanState {
            status: state_plan::PlanStatus::Proposed,
            plan_text: "x".repeat(HEAD_CHARS + 10),
            updated_round: 1,
            ..Default::default()
        };
        let text = plan_display(&state).unwrap();
        assert!(text.starts_with("plan status: proposed (updated round 1)"));
        assert!(text.contains("(truncated"));
        assert!(!text.contains("xxxxxxxxxx]"));
    }

    #[test]
    fn goal_display_shows_status_version_and_round() {
        let state = GoalState {
            objective: "ship it".to_string(),
            version: 2,
            round: 3,
            ..Default::default()
        };
        let text = goal_display(&state).unwrap();
        assert!(text.starts_with("goal status: "));
        assert!(text.contains("version 2, round 3"));
        assert!(text.contains("goal:\nship it"));
    }

    #[test]
    fn snapshot_summary_is_none_for_unknown_labels() {
        let dir = tempfile::tempdir().unwrap();
        let status = SessionStatus::new(None, dir.path().to_path_buf());
        assert!(status.snapshot_labels().is_empty());
        assert_eq!(status.snapshot_summary("nope"), None);
        // Malicious labels never panic; they just do not resolve.
        assert_eq!(status.snapshot_summary("../escape"), None);
    }
}
