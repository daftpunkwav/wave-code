/*!
 * @file ReviewedPlanTools
 * @description Model-invokable tools for the reviewed plan mode.
 *
 * Responsibilities:
 * - Share one plan state handle across propose/approve/feedback/status.
 * - Persist every mutation to the home-derived plan file (resume-safe).
 * - Report business failures as model-readable errors, never panics.
 *
 * This module must not depend on: drivers, actors, or sessions. Assembly
 * owns the store handle; the tools only mutate through it.
 */

//! Reviewed plan tools: thin [`Tool`] adapters over the `state-plan`
//! machine; the transition rules and file layout live in `state-plan`,
//! this module only maps tool input/output and persistence.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use state_plan::PlanState;
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// Session id used when assembly has no finer identity: the path stays
/// home-derived, so resume in the same home reopens the same plan file.
pub const DEFAULT_PLAN_SESSION_ID: &str = "default";

/// Load the plan for one session, degrading to an empty Draft with a
/// warning instead of failing assembly on corrupt content.
///
/// Returns the state plus an optional startup warning for assembly.
pub fn load_for_session(
    home: Option<&Path>,
    session_id: &str,
) -> (PlanState, Option<String>) {
    match state_plan::load_for_session(home, session_id) {
        Ok(state) => (state, None),
        Err(e) => (
            PlanState::default(),
            Some(format!("plan state unreadable, starting empty: {e}")),
        ),
    }
}

/// Shared plan handle: the in-memory machine plus its resume file.
/// Mutations persist outside the lock via blocking IO so tool
/// execution stays truly async.
#[derive(Debug)]
pub struct PlanStore {
    state: Mutex<PlanState>,
    file: Option<PathBuf>,
}

impl PlanStore {
    /// Wrap a loaded state with its home-derived resume path. No home
    /// means memory-only (same gate as the memory assembly path).
    pub fn new(state: PlanState, home: Option<&Path>, session_id: &str) -> Self {
        let file = home
            .map(state_plan::plans_root_for_home)
            .and_then(|root| state_plan::plan_path_for_session(&root, session_id).ok());
        Self {
            state: Mutex::new(state),
            file,
        }
    }

    /// Lock helper matching the tools-crate poison convention: a panic
    /// while holding this short critical section leaves no half-broken
    /// invariant behind, so take the guard back and keep going.
    fn lock(&self) -> std::sync::MutexGuard<'_, PlanState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Persist a snapshot taken under the lock (runs on the blocking
    /// pool; IO failures feed back to the model as business errors).
    async fn persist(&self, snapshot: PlanState, file: Option<PathBuf>) -> std::result::Result<(), String> {
        let Some(path) = file else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || state_plan::save_to_path(&snapshot, &path))
            .await
            .map_err(|e| format!("plan persist task failed: {e}"))?
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

/// Human rendering of the current plan for tool and slash display.
pub fn render_status(state: &PlanState) -> String {
    let mut out = format!(
        "plan status: {} (updated round {})",
        state.status, state.updated_round
    );
    if state.plan_text.trim().is_empty() {
        out.push_str("\n(no plan text yet)");
    } else {
        out.push_str(&format!("\nplan:\n{}", state.plan_text));
    }
    if !state.history.is_empty() {
        let trail = state
            .history
            .iter()
            .map(|(round, status)| format!("{status}@{round}"))
            .collect::<Vec<_>>()
            .join(" -> ");
        out.push_str(&format!("\nhistory: {trail}"));
    }
    out
}

/// Apply a mutating transition under the lock, then persist the
/// snapshot outside the lock. Returns the rendered status or a
/// business-failure output.
async fn apply_transition(
    store: &PlanStore,
    action: &'static str,
    transition: impl FnOnce(&mut PlanState) -> std::result::Result<(), state_plan::PlanError>,
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
        Err(reason) => err_output(format!("plan {action} applied but persistence failed: {reason}")),
    }
}

/// `plan_propose`: present a plan for review (Draft -> Proposed).
///
/// Mutates plan state, not the repo: not read-only, but not destructive
/// either, so it never parks on an approval gate (no approval paradox).
#[derive(Debug, Clone)]
pub struct PlanProposeTool {
    store: Arc<PlanStore>,
}

impl PlanProposeTool {
    /// Share the session plan handle.
    pub fn new(store: Arc<PlanStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for PlanProposeTool {
    fn name(&self) -> &str {
        "plan_propose"
    }

    fn description(&self) -> &str {
        "Propose a reviewed plan for the user to approve: explore first, \
         then present the plan text here. The plan waits in Proposed until \
         the user approves (plan_approve) or sends it back with feedback \
         (plan_feedback). Only one proposal is active at a time."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The plan text to present for review; blank text is rejected",
                },
            },
            "required": ["text"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let text = text.to_string();
        Ok(apply_transition(&self.store, "propose", move |plan| plan.propose(&text)).await)
    }
}

/// `plan_approve`: approve the pending proposal (Proposed -> Approved).
#[derive(Debug, Clone)]
pub struct PlanApproveTool {
    store: Arc<PlanStore>,
}

impl PlanApproveTool {
    /// Share the session plan handle.
    pub fn new(store: Arc<PlanStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for PlanApproveTool {
    fn name(&self) -> &str {
        "plan_approve"
    }

    fn description(&self) -> &str {
        "Approve the currently proposed plan (Proposed -> Approved). Use \
         after the user confirms the proposal; execution may start only \
         once the plan is approved."
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
        Ok(apply_transition(&self.store, "approve", |plan| plan.approve()).await)
    }
}

/// `plan_feedback`: send the proposal back with notes (Proposed -> Draft).
#[derive(Debug, Clone)]
pub struct PlanFeedbackTool {
    store: Arc<PlanStore>,
}

impl PlanFeedbackTool {
    /// Share the session plan handle.
    pub fn new(store: Arc<PlanStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for PlanFeedbackTool {
    fn name(&self) -> &str {
        "plan_feedback"
    }

    fn description(&self) -> &str {
        "Send the proposed plan back for revision with feedback notes \
         (Proposed -> Draft). The feedback appends to the plan text so \
         the next proposal keeps the review context."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Feedback notes for the revision; blank text is rejected",
                },
            },
            "required": ["text"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let text = text.to_string();
        Ok(apply_transition(&self.store, "feedback", move |plan| plan.feedback(&text)).await)
    }
}

/// `plan_status`: show the current plan without mutating it.
#[derive(Debug, Clone)]
pub struct PlanStatusTool {
    store: Arc<PlanStore>,
}

impl PlanStatusTool {
    /// Share the session plan handle.
    pub fn new(store: Arc<PlanStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for PlanStatusTool {
    fn name(&self) -> &str {
        "plan_status"
    }

    fn description(&self) -> &str {
        "Show the current reviewed-plan status and text. Read-only: use \
         it to check where the plan stands before proposing or executing."
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &tempfile::TempDir) -> ToolCtx {
        ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        }
    }

    fn store_in(home: &Path) -> Arc<PlanStore> {
        let (state, warning) = load_for_session(Some(home), "test");
        assert!(warning.is_none());
        Arc::new(PlanStore::new(state, Some(home), "test"))
    }

    #[tokio::test]
    async fn propose_approve_status_cycle_persists() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        let propose = PlanProposeTool::new(store.clone());
        assert!(!propose.is_read_only());
        assert!(!propose.is_destructive());
        let out = propose
            .execute(serde_json::json!({"text": "migrate the store"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("proposed"));
        let approve = PlanApproveTool::new(store.clone());
        let out = approve.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("approved"));
        // Read-only status never mutates and reports the same state.
        let status = PlanStatusTool::new(store.clone());
        assert!(status.is_read_only());
        let out = status.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("approved"));
        assert!(out.content.contains("migrate the store"));
        // Resume in the same home reopens the persisted plan.
        let resumed = store_in(home.path());
        let out = PlanStatusTool::new(resumed)
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("approved"));
    }

    #[tokio::test]
    async fn feedback_returns_to_draft_and_illegal_moves_fail_openly() {
        let home = tempfile::tempdir().unwrap();
        let store = store_in(home.path());
        let ctx = ctx(&home);
        let approve = PlanApproveTool::new(store.clone());
        // Approving a Draft names the expected state.
        let out = approve.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("proposed"), "{}", out.content);
        PlanProposeTool::new(store.clone())
            .execute(serde_json::json!({"text": "v1"}), &ctx)
            .await
            .unwrap();
        let feedback = PlanFeedbackTool::new(store.clone());
        assert!(!feedback.is_read_only());
        let out = feedback
            .execute(serde_json::json!({"text": "add rollback"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("draft"));
        assert!(out.content.contains("add rollback"));
        // Blank proposals are rejected without mutating.
        let out = PlanProposeTool::new(store.clone())
            .execute(serde_json::json!({"text": "  "}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        let out = PlanStatusTool::new(store)
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("draft"));
    }

    #[test]
    fn corrupt_plan_degrades_with_a_warning() {
        let home = tempfile::tempdir().unwrap();
        let root = state_plan::plans_root_for_home(home.path());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("bad.json"), "{not json").unwrap();
        let (state, warning) = load_for_session(Some(home.path()), "bad");
        assert_eq!(state, PlanState::default());
        assert!(warning.unwrap().contains("unreadable"));
    }
}
