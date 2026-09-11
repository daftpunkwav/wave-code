/*!
 * @file DurableGoalTools
 * @description Model-invokable tools for the durable goal service.
 *
 * Responsibilities:
 * - Share one goal state handle across set/update/status/tick.
 * - Persist every mutation to the home-derived goal file (resume-safe).
 * - Report business failures as model-readable errors, never panics.
 *
 * This module must not depend on: drivers, actors, or sessions. Assembly
 * owns the store handle; the tools only mutate through it.
 */

//! Durable goal tools: thin [`Tool`] adapters over the `state-goal`
//! machine; the transition rules and file layout live in `state-goal`,
//! this module only maps tool input/output and persistence.
//!
//! The round driver is tools-only: no loop hook calls `round_tick`
//! automatically yet, so the model must call `goal_tick` once per round
//! and carry the reported version into the next `goal_update`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use state_goal::GoalState;
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// Session id used when assembly has no finer identity: the path stays
/// home-derived, so resume in the same home reopens the same goal file.
pub const DEFAULT_GOAL_SESSION_ID: &str = "default";

/// Load the goal for one session, degrading to a fresh goal with a
/// warning instead of failing assembly on corrupt content.
///
/// Returns the state plus an optional startup warning for assembly.
pub fn load_for_session(home: Option<&Path>, session_id: &str) -> (GoalState, Option<String>) {
    match state_goal::load_for_session(home, session_id) {
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
            .map(state_goal::goals_root_for_home)
            .and_then(|root| state_goal::goal_path_for_session(&root, session_id).ok());
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
        tokio::task::spawn_blocking(move || state_goal::save_to_path(&snapshot, &path))
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

/// Human rendering of the current goal for tool and slash display.
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
    out
}

/// Apply a mutating transition under the lock, then persist the
/// snapshot outside the lock. Returns the rendered status or a
/// business-failure output.
async fn apply_transition(
    store: &GoalStore,
    action: &'static str,
    transition: impl FnOnce(&mut GoalState) -> std::result::Result<(), state_goal::GoalError>,
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

/// `goal_set`: set (or replace) the durable per-session objective.
///
/// Resets the round driver to zero and returns the goal to Active, so a
/// finished goal restarts cleanly. The driver is tools-only: call
/// `goal_tick` once per round and carry the reported version into the
/// next `goal_update`.
#[derive(Debug, Clone)]
pub struct GoalSetTool {
    store: Arc<GoalStore>,
}

impl GoalSetTool {
    /// Share the session goal handle.
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for GoalSetTool {
    fn name(&self) -> &str {
        "goal_set"
    }

    fn description(&self) -> &str {
        "Set the durable per-session objective, replacing any previous \
         goal and resetting the round driver to zero. The driver is \
         tools-only (no automatic loop hook yet): call goal_tick once per \
         round and use the reported version for the next goal_update."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The objective text to persist; blank text is rejected",
                },
            },
            "required": ["objective"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let objective = input
            .get("objective")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let objective = objective.to_string();
        Ok(apply_transition(&self.store, "set", move |goal| goal.set(&objective)).await)
    }
}

/// `goal_update`: mutate the durable goal under optimistic concurrency.
///
/// The expected version must match the version `goal_status` reported;
/// a stale version fails naming both versions so the caller reloads and
/// retries. Objective and status apply atomically when both are given.
#[derive(Debug, Clone)]
pub struct GoalUpdateTool {
    store: Arc<GoalStore>,
}

impl GoalUpdateTool {
    /// Share the session goal handle.
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for GoalUpdateTool {
    fn name(&self) -> &str {
        "goal_update"
    }

    fn description(&self) -> &str {
        "Update the durable goal under optimistic concurrency: \
         expected_version must equal the version goal_status reported \
         (stale versions fail naming both versions; reload and retry). \
         Optionally replaces the objective and/or moves the status \
         (active, blocked, paused, completed); the terminal completed \
         state only leaves via goal_set."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "expected_version": {
                    "type": "integer",
                    "description": "Current goal version from goal_status; stale versions are rejected",
                },
                "objective": {
                    "type": "string",
                    "description": "Replacement objective text (optional; blank is rejected)",
                },
                "status": {
                    "type": "string",
                    "description": "Target status (optional): active, blocked, paused, or completed",
                },
            },
            "required": ["expected_version"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        // A missing version can never match a real one, so it fails as a
        // version mismatch naming both versions instead of a schema error.
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
            Some(raw) => match state_goal::parse_status(raw) {
                Ok(status) => Some(status),
                Err(e) => return Ok(err_output(e.to_string())),
            },
        };
        Ok(apply_transition(&self.store, "update", move |goal| {
            goal.update(expected_version, objective.as_deref(), status)
        })
        .await)
    }
}

/// `goal_status`: show the current goal without mutating it.
#[derive(Debug, Clone)]
pub struct GoalStatusTool {
    store: Arc<GoalStore>,
}

impl GoalStatusTool {
    /// Share the session goal handle.
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for GoalStatusTool {
    fn name(&self) -> &str {
        "goal_status"
    }

    fn description(&self) -> &str {
        "Show the durable goal objective, status, CAS version, and round. \
         Read-only: use it to reload the version before goal_update and \
         to check where the goal stands before mutating it."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        Ok(ToolOutput {
            content: render_status(&self.store.lock()),
            is_error: false,
        })
    }
}

/// `goal_tick`: advance the durable-goal round driver by one round.
///
/// Call once per model round: the loop has no automatic hook yet, so the
/// driver only advances through this tool. Returns the new version for
/// the next `goal_update`; at the round cap (256) the goal transitions
/// to blocked and the cap reports as a business error (the blocked
/// state still persists).
#[derive(Debug, Clone)]
pub struct GoalTickTool {
    store: Arc<GoalStore>,
}

impl GoalTickTool {
    /// Share the session goal handle.
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for GoalTickTool {
    fn name(&self) -> &str {
        "goal_tick"
    }

    fn description(&self) -> &str {
        "Advance the durable-goal round driver by one round. Call once per \
         model round (the loop does not call it automatically yet). The \
         returned status carries the new version for the next goal_update; \
         at the round cap (256) the goal blocks and the cap reports as a \
         business error."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let _ = input;
        // The cap transition mutates (it blocks the goal) while reporting
        // an error, so unlike apply_transition it must persist the
        // snapshot on the cap path too.
        let (snapshot, file, rendered, capped) = {
            let mut guard = self.store.lock();
            match guard.round_tick() {
                Ok(()) => (
                    guard.clone(),
                    self.store.file.clone(),
                    render_status(&guard),
                    false,
                ),
                Err(state_goal::GoalError::RoundCapped { .. }) => (
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
                state_goal::MAX_ROUND,
                state_goal::MAX_ROUND,
            ))),
            Err(reason) => Ok(err_output(format!(
                "goal tick applied but persistence failed: {reason}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn set_update_status_tick_cycle_persists() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        let set = GoalSetTool::new(store.clone());
        assert!(!set.is_read_only());
        assert!(!set.is_destructive());
        let out = set
            .execute(serde_json::json!({"objective": "ship the milestone"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("active"), "{}", out.content);
        assert!(out.content.contains("version 1"), "{}", out.content);
        // Tick advances the driver and hands back a fresh version.
        let tick = GoalTickTool::new(store.clone());
        assert!(!tick.is_read_only());
        let out = tick.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("round 1"), "{}", out.content);
        assert!(out.content.contains("version 2"), "{}", out.content);
        // Update under the fresh version pauses the goal.
        let update = GoalUpdateTool::new(store.clone());
        let out = update
            .execute(
                serde_json::json!({"expected_version": 2, "status": "paused"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("paused"), "{}", out.content);
        // Read-only status never mutates and reports the same state.
        let status = GoalStatusTool::new(store.clone());
        assert!(status.is_read_only());
        let out = status.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("paused"), "{}", out.content);
        assert!(out.content.contains("ship the milestone"), "{}", out.content);
        // Resume in the same home reopens the persisted goal.
        let resumed = store_in(home.path());
        let out = GoalStatusTool::new(resumed)
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("paused"), "{}", out.content);
        assert!(out.content.contains("version 3"), "{}", out.content);
    }

    #[tokio::test]
    async fn cas_mismatch_names_both_versions() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        GoalSetTool::new(store.clone())
            .execute(serde_json::json!({"objective": "v1"}), &ctx)
            .await
            .unwrap();
        let out = GoalUpdateTool::new(store.clone())
            .execute(
                serde_json::json!({"expected_version": 999, "objective": "stale"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("expected version 999"), "{}", out.content);
        assert!(out.content.contains("found version 1"), "{}", out.content);
        // Unknown status names fail openly without mutating.
        let out = GoalUpdateTool::new(store.clone())
            .execute(
                serde_json::json!({"expected_version": 1, "status": "done"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("active"), "{}", out.content);
        let out = GoalStatusTool::new(store)
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("version 1"), "{}", out.content);
    }

    #[tokio::test]
    async fn cap_blocks_and_persists_through_the_tool() {
        let home = tempfile::tempdir().unwrap();
        let capped = GoalState {
            objective: "bounded".to_string(),
            status: state_goal::GoalStatus::Active,
            version: 7,
            round: state_goal::MAX_ROUND,
            updated_at: 0,
        };
        let store = Arc::new(GoalStore::new(capped, Some(home.path()), "test"));
        let ctx = ctx(&home);
        let out = GoalTickTool::new(store.clone())
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("cap"), "{}", out.content);
        assert!(out.content.contains("blocked"), "{}", out.content);
        // The blocked state persisted: resume reopens it.
        let resumed = store_in(home.path());
        let out = GoalStatusTool::new(resumed)
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("blocked"), "{}", out.content);
    }

    #[tokio::test]
    async fn illegal_transitions_fail_openly() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        GoalSetTool::new(store.clone())
            .execute(serde_json::json!({"objective": "finish me"}), &ctx)
            .await
            .unwrap();
        // Complete under the fresh version, then every move off the
        // terminal state fails naming it.
        let out = GoalUpdateTool::new(store.clone())
            .execute(
                serde_json::json!({"expected_version": 1, "status": "completed"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let out = GoalUpdateTool::new(store.clone())
            .execute(
                serde_json::json!({"expected_version": 2, "status": "active"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("completed"), "{}", out.content);
        // Ticking a completed goal fails without persisting a new version.
        let out = GoalTickTool::new(store.clone())
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("completed"), "{}", out.content);
        // Blank objectives are rejected without mutating.
        let out = GoalSetTool::new(store.clone())
            .execute(serde_json::json!({"objective": "  "}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        let out = GoalStatusTool::new(store)
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("completed"), "{}", out.content);
    }

    #[test]
    fn corrupt_goal_degrades_with_a_warning() {
        let home = tempfile::tempdir().unwrap();
        let root = state_goal::goals_root_for_home(home.path());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("bad.json"), "{not json").unwrap();
        let (state, warning) = load_for_session(Some(home.path()), "bad");
        assert_eq!(state, GoalState::default());
        assert!(warning.unwrap().contains("unreadable"));
    }
}
