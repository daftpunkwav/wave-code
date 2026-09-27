/*!
 * @file GoalTools
 * @description Single model-invokable tool over the durable goal service.
 *
 * Responsibilities:
 * - Route the goal machine's four actions: set / update / status / tick.
 * - Share one goal state handle across all actions.
 * - Persist every mutation to the home-derived goal file (resume-safe).
 * - Report business failures as model-readable errors, never panics.
 *
 * This module must not depend on: drivers, actors, or sessions. Assembly
 * owns the store handle; the tool only mutates through it.
 */

//! Durable goal tool: a thin [`Tool`] adapter over this crate's goal
//! machine; the transition rules and file layout live in the crate root,
//! this module only maps tool input/output and persistence.
//!
//! Actions:
//! - `set` — replace the objective (and optionally seed sub-goals);
//!   resets the round driver and clears the previous sub-goal list.
//! - `update` — CAS-guarded mutation of the objective, status, and/or
//!   the whole sub-goal list (full rewrite, not a patch).
//! - `status` — read the goal tree back (version, round, progress).
//! - `tick` — advance the round driver once; tools-only by design, no
//!   loop hook calls it automatically yet, so the model ticks once per
//!   round and carries the reported version into the next `update`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{GoalState, SubGoal};
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// Session id used when assembly has no finer identity: the path stays
/// home-derived, so resume in the same home reopens the same goal file.
pub const DEFAULT_GOAL_SESSION_ID: &str = "default";

/// Load the goal for one session, degrading to a fresh goal with a
/// warning instead of failing assembly on corrupt content.
///
/// Returns the state plus an optional startup warning for assembly.
pub fn load_for_session(home: Option<&Path>, session_id: &str) -> (GoalState, Option<String>) {
    match crate::load_for_session(home, session_id) {
        Ok(state) => (state, None),
        Err(e) => (
            GoalState::default(),
            Some(format!("goal state unreadable, starting empty: {e}")),
        ),
    }
}

/// Shared goal handle: the in-memory machine plus its resume file.
/// Mutations persist outside the lock via blocking IO so tool
/// execution stays truly async.
#[derive(Debug)]
pub struct GoalStore {
    state: Mutex<GoalState>,
    file: Option<PathBuf>,
}

impl GoalStore {
    /// Wrap a loaded state with its home-derived resume path. No home
    /// means memory-only (same gate as the memory assembly path).
    pub fn new(state: GoalState, home: Option<&Path>, session_id: &str) -> Self {
        let file = home
            .map(crate::goals_root_for_home)
            .and_then(|root| crate::goal_path_for_session(&root, session_id).ok());
        Self {
            state: Mutex::new(state),
            file,
        }
    }

    /// Lock helper matching the tools-crate poison convention: a panic
    /// while holding this short critical section leaves no half-broken
    /// invariant behind, so take the guard back and keep going.
    fn lock(&self) -> std::sync::MutexGuard<'_, GoalState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Current state, for readers that must not hold the lock across an
    /// await (the loop's continuation check is one; see `goal_adapter`).
    pub fn snapshot(&self) -> GoalState {
        self.lock().clone()
    }

    /// Persist a snapshot taken under the lock (runs on the blocking
    /// pool; IO failures feed back to the model as business errors).
    async fn persist(
        &self,
        snapshot: GoalState,
        file: Option<PathBuf>,
    ) -> std::result::Result<(), String> {
        let Some(path) = file else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || crate::save_to_path(&snapshot, &path))
            .await
            .map_err(|e| format!("goal persist task failed: {e}"))?
            .map_err(|e| e.to_string())
    }
}

/// Build a business-failure output (reason is fed back to the model).
fn err_output(reason: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: reason.into(),
        is_error: true,
    }
}

/// Human rendering of the current goal tree for tool and slash display.
pub fn render_status(state: &GoalState) -> String {
    let mut out = format!(
        "goal status: {} (version {}, round {})",
        state.status, state.version, state.round
    );
    if state.objective.trim().is_empty() {
        out.push_str("\n(no objective yet)");
    } else {
        out.push_str(&format!("\nobjective:\n{}", state.objective));
    }
    if state.sub_goals.is_empty() {
        return out;
    }
    let achieved = state
        .sub_goals
        .iter()
        .filter(|s| s.status == crate::SubGoalStatus::Achieved)
        .count();
    out.push_str(&format!(
        "\nsub-goals ({achieved}/{} achieved):",
        state.sub_goals.len()
    ));
    for sub in &state.sub_goals {
        out.push_str(&format!("\n- [{}] {}", sub.status, sub.text));
    }
    out
}

/// Parse the optional `sub_goals` input array: each item needs a
/// non-empty `text` and an optional `status` (`in_progress`, the
/// default, or `achieved`); unknown names are business errors. The list
/// cap mirrors [`crate::MAX_SUB_GOALS`] here so an oversized list
/// fails in parsing — before the composite set/update transition touches
/// any state — instead of failing halfway through one.
fn parse_sub_goals(input: &serde_json::Value) -> std::result::Result<Option<Vec<SubGoal>>, String> {
    let Some(raw) = input.get("sub_goals") else {
        return Ok(None);
    };
    let serde_json::Value::Array(items) = raw else {
        return Err("invalid parameter 'sub_goals' (array of {text, status?} required)".into());
    };
    if items.len() > crate::MAX_SUB_GOALS {
        return Err(format!(
            "too many sub-goals ({}): the cap is {}",
            items.len(),
            crate::MAX_SUB_GOALS
        ));
    }
    let mut sub_goals = Vec::with_capacity(items.len());
    for item in items {
        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            return Err("invalid sub-goal: 'text' must be a non-empty string".into());
        }
        let status = match item.get("status").and_then(|v| v.as_str()) {
            None => crate::SubGoalStatus::default(),
            Some(raw) => crate::parse_sub_goal_status(raw).map_err(|e| e.to_string())?,
        };
        sub_goals.push(SubGoal { text, status });
    }
    Ok(Some(sub_goals))
}

/// `goal`: the durable per-session objective and its sub-goal tree.
///
/// One state machine behind four actions. Mutations persist outside the
/// lock; the driver is tools-only (no automatic loop hook yet), so the
/// model ticks once per round and carries the reported version into the
/// next `update`.
#[derive(Debug, Clone)]
pub struct GoalTool {
    store: Arc<GoalStore>,
}

impl GoalTool {
    /// Share the session goal handle.
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for GoalTool {
    fn name(&self) -> &str {
        "goal"
    }

    fn kind(&self) -> wavecode_protocol::ToolKind {
        wavecode_protocol::ToolKind::SessionState
    }

    fn description(&self) -> &str {
        "Durable per-session goal: a main objective plus intermediate \
         sub-goals that survives restarts. Actions: 'set' replaces the \
         objective (clearing the round driver and old sub-goals; seed new \
         sub-goals via sub_goals), 'update' mutates objective and/or \
         status and/or rewrites the whole sub-goal list under optimistic \
         concurrency (expected_version must equal the version the last \
         result reported; stale versions fail naming both versions — \
         reload with status and retry), 'status' reads the tree without \
         mutating, 'tick' advances the round driver once per round (at \
         the cap of 256 the goal blocks and the cap reports as an error). \
         Status moves: active, blocked, paused, completed; the terminal \
         completed state only leaves via set. Sub-goal statuses: \
         in_progress, achieved."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["set", "update", "status", "tick"],
                    "description": "Which goal operation to run",
                },
                "objective": {
                    "type": "string",
                    "description": "set (required): the objective text; update (optional): replacement text; blank is rejected",
                },
                "status": {
                    "type": "string",
                    "description": "update (optional): target status — active, blocked, paused, or completed",
                },
                "expected_version": {
                    "type": "integer",
                    "description": "update (required): current goal version from the last result; stale versions are rejected",
                },
                "sub_goals": {
                    "type": "array",
                    "description": "set (optional) seeds the list; update (optional) rewrites the whole list. Items: {text, status?} with status in_progress (default) or achieved",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {
                                "type": "string",
                                "description": "What done means for this sub-goal (non-empty)"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["in_progress", "achieved"]
                            }
                        },
                        "required": ["text"]
                    }
                }
            },
            "required": ["action"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "set" => self.action_set(&input).await,
            "update" => self.action_update(&input).await,
            "status" => Ok(ToolOutput {
                content: render_status(&self.store.lock()),
                is_error: false,
            }),
            "tick" => self.action_tick().await,
            other => Ok(err_output(format!(
                "unknown action {other:?}: expected one of set, update, status, tick"
            ))),
        }
    }
}

impl GoalTool {
    /// `set`: replace the objective and optionally seed sub-goals in the
    /// same transition (single version bump per applied change).
    async fn action_set(&self, input: &serde_json::Value) -> Result<ToolOutput> {
        let objective = input
            .get("objective")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sub_goals = match parse_sub_goals(input) {
            Ok(sub_goals) => sub_goals,
            Err(reason) => return Ok(err_output(reason)),
        };
        Ok(apply_transition(&self.store, "set", move |goal| {
            goal.set(&objective)?;
            if let Some(sub_goals) = sub_goals {
                goal.replace_sub_goals(goal.version, sub_goals)?;
            }
            Ok(())
        })
        .await)
    }

    /// `update`: CAS-guarded mutation of objective, status, and/or the
    /// whole sub-goal list. A missing version can never match a real
    /// one, so it fails as a version mismatch naming both versions
    /// instead of a schema error.
    async fn action_update(&self, input: &serde_json::Value) -> Result<ToolOutput> {
        let expected_version = input
            .get("expected_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX);
        let objective = input
            .get("objective")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let status = match input.get("status").and_then(|v| v.as_str()) {
            None => None,
            Some(raw) => match crate::parse_status(raw) {
                Ok(status) => Some(status),
                Err(e) => return Ok(err_output(e.to_string())),
            },
        };
        let sub_goals = match parse_sub_goals(input) {
            Ok(sub_goals) => sub_goals,
            Err(reason) => return Ok(err_output(reason)),
        };
        if objective.is_none() && status.is_none() && sub_goals.is_none() {
            return Ok(err_output(
                "update needs something to change: objective, status, expected_version, \
                 and at least one of objective/status/sub_goals is required",
            ));
        }
        Ok(apply_transition(&self.store, "update", move |goal| {
            goal.update(expected_version, objective.as_deref(), status)?;
            // The main update bumped the version; the sub-goal rewrite
            // CAS-checks against the freshly reported one, so both apply
            // atomically inside this one transition.
            if let Some(sub_goals) = sub_goals {
                goal.replace_sub_goals(goal.version, sub_goals)?;
            }
            Ok(())
        })
        .await)
    }

    /// `tick`: advance the round driver. The cap transition mutates (it
    /// blocks the goal) while reporting an error, so unlike the other
    /// actions it must persist the snapshot on the cap path too.
    async fn action_tick(&self) -> Result<ToolOutput> {
        let (snapshot, file, rendered, capped) = {
            let mut guard = self.store.lock();
            match guard.round_tick() {
                Ok(()) => (
                    guard.clone(),
                    self.store.file.clone(),
                    render_status(&guard),
                    false,
                ),
                Err(crate::GoalError::RoundCapped { .. }) => (
                    guard.clone(),
                    self.store.file.clone(),
                    render_status(&guard),
                    true,
                ),
                Err(e) => return Ok(err_output(e.to_string())),
            }
        };
        match self.store.persist(snapshot, file).await {
            Ok(()) if !capped => Ok(ToolOutput {
                content: rendered,
                is_error: false,
            }),
            Ok(()) => Ok(err_output(format!(
                "goal round cap reached: round {} at cap {}; goal is now blocked\n{rendered}",
                crate::MAX_ROUND,
                crate::MAX_ROUND,
            ))),
            Err(reason) => Ok(err_output(format!(
                "goal tick applied but persistence failed: {reason}"
            ))),
        }
    }
}

/// Apply a mutating transition under the lock, then persist the
/// snapshot outside the lock. Returns the rendered status or a
/// business-failure output.
async fn apply_transition(
    store: &GoalStore,
    action: &'static str,
    transition: impl FnOnce(&mut GoalState) -> std::result::Result<(), crate::GoalError>,
) -> ToolOutput {
    let (snapshot, file, rendered) = {
        let mut guard = store.lock();
        if let Err(e) = transition(&mut guard) {
            return err_output(e.to_string());
        }
        let rendered = render_status(&guard);
        (guard.clone(), store.file.clone(), rendered)
    };
    match store.persist(snapshot, file).await {
        Ok(()) => ToolOutput {
            content: rendered,
            is_error: false,
        },
        Err(reason) => err_output(format!(
            "goal {action} applied but persistence failed: {reason}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SubGoalStatus;

    fn ctx(dir: &tempfile::TempDir) -> ToolCtx {
        ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        }
    }

    fn store_in(home: &Path) -> Arc<GoalStore> {
        let (state, warning) = load_for_session(Some(home), "test");
        assert!(warning.is_none());
        Arc::new(GoalStore::new(state, Some(home), "test"))
    }

    fn sub_goal(text: &str, status: SubGoalStatus) -> serde_json::Value {
        serde_json::json!({"text": text, "status": status.as_str()})
    }

    #[tokio::test]
    async fn goal_actions_cycle_persists() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        let goal = GoalTool::new(store.clone());
        assert!(!goal.is_read_only());
        assert!(!goal.is_destructive());
        // Set seeds the objective and sub-goals in one call.
        let out = goal
            .execute(
                serde_json::json!({
                    "action": "set",
                    "objective": "ship the milestone",
                    "sub_goals": [sub_goal("fix the bug", SubGoalStatus::Achieved),
                                  sub_goal("add tests", SubGoalStatus::InProgress)]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("active"), "{}", out.content);
        // set + sub-goal seed = two version bumps.
        assert!(out.content.contains("version 2"), "{}", out.content);
        assert!(
            out.content.contains("sub-goals (1/2 achieved)"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("[achieved] fix the bug"),
            "{}",
            out.content
        );
        // Tick advances the driver and hands back a fresh version.
        let out = goal
            .execute(serde_json::json!({"action": "tick"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("round 1"), "{}", out.content);
        assert!(out.content.contains("version 3"), "{}", out.content);
        // Update under the fresh version pauses the goal and rewrites the
        // sub-goal list in the same transition.
        let out = goal
            .execute(
                serde_json::json!({
                    "action": "update",
                    "expected_version": 3,
                    "status": "paused",
                    "sub_goals": [sub_goal("add tests", SubGoalStatus::Achieved)]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("paused"), "{}", out.content);
        assert!(
            out.content.contains("sub-goals (1/1 achieved)"),
            "{}",
            out.content
        );
        // Read-only status action never mutates and reports the same state.
        let out = goal
            .execute(serde_json::json!({"action": "status"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("paused"), "{}", out.content);
        assert!(
            out.content.contains("ship the milestone"),
            "{}",
            out.content
        );
        // Resume in the same home reopens the persisted goal.
        let resumed = GoalTool::new(store_in(home.path()));
        let out = resumed
            .execute(serde_json::json!({"action": "status"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("paused"), "{}", out.content);
        // set(1) + seed(2) + tick(3) + update(4) + sub-goal rewrite(5).
        assert!(out.content.contains("version 5"), "{}", out.content);
    }

    #[tokio::test]
    async fn cas_mismatch_names_both_versions() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        let goal = GoalTool::new(store);
        goal.execute(
            serde_json::json!({"action": "set", "objective": "v1"}),
            &ctx,
        )
        .await
        .unwrap();
        let out = goal
            .execute(
                serde_json::json!({"action": "update", "expected_version": 999, "objective": "stale"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("expected version 999"),
            "{}",
            out.content
        );
        assert!(out.content.contains("found version 1"), "{}", out.content);
        // Unknown status names fail openly without mutating.
        let out = goal
            .execute(
                serde_json::json!({"action": "update", "expected_version": 1, "status": "done"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("active"), "{}", out.content);
        let out = goal
            .execute(serde_json::json!({"action": "status"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("version 1"), "{}", out.content);
    }

    #[tokio::test]
    async fn update_without_changes_is_a_business_error() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        let goal = GoalTool::new(store);
        goal.execute(
            serde_json::json!({"action": "set", "objective": "v1"}),
            &ctx,
        )
        .await
        .unwrap();
        let out = goal
            .execute(
                serde_json::json!({"action": "update", "expected_version": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("needs something to change"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn blank_sub_goal_text_and_unknown_status_fail_openly() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        let goal = GoalTool::new(store);
        goal.execute(
            serde_json::json!({"action": "set", "objective": "v1"}),
            &ctx,
        )
        .await
        .unwrap();
        let out = goal
            .execute(
                serde_json::json!({
                    "action": "update",
                    "expected_version": 1,
                    "sub_goals": [{"text": "   "}]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("non-empty"), "{}", out.content);
        let out = goal
            .execute(
                serde_json::json!({
                    "action": "set",
                    "objective": "v2",
                    "sub_goals": [{"text": "x", "status": "done"}]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("in_progress"), "{}", out.content);
    }

    #[tokio::test]
    async fn cap_blocks_and_persists_through_the_tool() {
        let home = tempfile::tempdir().unwrap();
        let capped = GoalState {
            objective: "bounded".to_string(),
            status: crate::GoalStatus::Active,
            version: 7,
            round: crate::MAX_ROUND,
            updated_at: 0,
            sub_goals: Vec::new(),
        };
        let store = Arc::new(GoalStore::new(capped, Some(home.path()), "test"));
        let ctx = ctx(&home);
        let goal = GoalTool::new(store);
        let out = goal
            .execute(serde_json::json!({"action": "tick"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("cap"), "{}", out.content);
        assert!(out.content.contains("blocked"), "{}", out.content);
        // The blocked state persisted: resume reopens it.
        let resumed = GoalTool::new(store_in(home.path()));
        let out = resumed
            .execute(serde_json::json!({"action": "status"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("blocked"), "{}", out.content);
    }

    #[tokio::test]
    async fn illegal_transitions_fail_openly() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        let goal = GoalTool::new(store);
        goal.execute(
            serde_json::json!({"action": "set", "objective": "finish me"}),
            &ctx,
        )
        .await
        .unwrap();
        // Complete under the fresh version, then every move off the
        // terminal state fails naming it.
        let out = goal
            .execute(
                serde_json::json!({"action": "update", "expected_version": 1, "status": "completed"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let out = goal
            .execute(
                serde_json::json!({"action": "update", "expected_version": 2, "status": "active"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("completed"), "{}", out.content);
        // Ticking a completed goal fails without persisting a new version.
        let out = goal
            .execute(serde_json::json!({"action": "tick"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("completed"), "{}", out.content);
        // Blank objectives are rejected without mutating.
        let out = goal
            .execute(
                serde_json::json!({"action": "set", "objective": "  "}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        let out = goal
            .execute(serde_json::json!({"action": "status"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("completed"), "{}", out.content);
    }

    #[test]
    fn corrupt_goal_degrades_with_a_warning() {
        let home = tempfile::tempdir().unwrap();
        let root = crate::goals_root_for_home(home.path());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("bad.json"), "{not json").unwrap();
        let (state, warning) = load_for_session(Some(home.path()), "bad");
        assert_eq!(state, GoalState::default());
        assert!(warning.unwrap().contains("unreadable"));
    }

    /// The policy layer keys on the declared class (architecture rule 4):
    /// the goal tool must keep claiming the coordination-state class, or
    /// its session-state exemption silently changes meaning.
    #[test]
    fn goal_claims_the_session_state_class() {
        let home = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(GoalStore::new(
            GoalState::default(),
            Some(home.path()),
            "kind-lock",
        ));
        assert_eq!(
            Tool::kind(&GoalTool::new(store)),
            wavecode_protocol::ToolKind::SessionState
        );
    }
}
