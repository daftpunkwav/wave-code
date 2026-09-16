/*!
 * @file WorkflowTools
 * @description Model-invoked workflow runs, Ralph loops, durable schedules.
 *
 * Responsibilities:
 * - Run validated DAG specs through the child task service.
 * - Drive Ralph loops with an immutable objective until the done marker.
 * - Add, list, and remove durable cron entries on the shared scheduler.
 *
 * This module must not depend on: drivers, actors, or sessions. Tools read
 * the task service and the shared scheduler only; completion still arrives
 * via notifications.
 */

//! Workflow and schedule tools: DAG runs, Ralph loops, cron entries.
//!
//! `workflow_run` takes a spec JSON object and returns one JSON object of
//! step summaries keyed by step id. `ralph_run` respawns a fresh child per
//! round with the same objective until the child reports `RALPH_DONE`.
//! The `schedule` tool manages durable cron entries persisted under
//! `<home>/.wavecode/schedule.json`.

use std::sync::{Arc, Mutex, MutexGuard};

use action_tasks::TaskService;
use action_workflow::{DEFAULT_RALPH_ROUNDS, MAX_RALPH_ROUNDS};
use runtime_scheduler::Scheduler;
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// Lock helper matching the tools-crate poison convention: a panic while
/// holding this short critical section leaves no half-broken invariant
/// behind, so take the guard back and keep going.
fn lock_scheduler(store: &Arc<Mutex<Scheduler>>) -> MutexGuard<'_, Scheduler> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// Required string field helper: missing or blank becomes a business error.
fn required_str<'a>(
    input: &'a serde_json::Value,
    field: &str,
) -> std::result::Result<&'a str, ToolOutput> {
    match input.get(field).and_then(|value| value.as_str()) {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ToolOutput {
            content: format!("missing required field: {field}"),
            is_error: true,
        }),
    }
}

/// `workflow_run`: run a DAG spec, return summaries keyed by step id.
///
/// The spec is a JSON object `{"steps": [{"id", "kind": "task"|"fanout",
/// "input", "depends_on": [...], "max_parallel"}]}`. Unknown dependencies
/// and cycles are business errors; a failing step fails the run with its
/// id named. No partial retry in v1.
pub struct WorkflowRunTool {
    tasks: Arc<dyn TaskService>,
}

impl WorkflowRunTool {
    /// Wrap the child task service.
    pub fn new(tasks: Arc<dyn TaskService>) -> Self {
        Self { tasks }
    }
}

#[async_trait::async_trait]
impl Tool for WorkflowRunTool {
    fn name(&self) -> &str {
        "workflow_run"
    }

    fn description(&self) -> &str {
        "Run a workflow DAG through child tasks and return one JSON object \
         of step summaries keyed by step id. Spec JSON: {\"steps\": \
         [{\"id\", \"kind\": \"task\"|\"fanout\", \"input\", \"depends_on\": \
         [ids], \"max_parallel\"}]}. Fan-out steps take an input array and \
         run one child per item. A failing step fails the run with its id \
         named; there is no partial retry."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "spec_json": {
                    "type": "string",
                    "description": "Workflow spec as a JSON object string",
                },
            },
            "required": ["spec_json"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let spec_json = match required_str(&input, "spec_json") {
            Ok(text) => text,
            Err(output) => return Ok(output),
        };
        let spec: action_workflow::WorkflowSpec = match serde_json::from_str(spec_json) {
            Ok(spec) => spec,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("invalid spec_json: {e}"),
                    is_error: true,
                });
            }
        };
        match action_workflow::run_workflow(&spec, self.tasks.as_ref()).await {
            Ok(merged) => Ok(ToolOutput {
                content: serde_json::to_string_pretty(&merged).unwrap_or_else(|e| e.to_string()),
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                content: e.to_string(),
                is_error: true,
            }),
        }
    }
}

/// `ralph_run`: loop a fresh child per round on one immutable objective.
///
/// Each round receives the same objective plus the prior summary, stopping
/// at the first summary with a `RALPH_DONE` line or when `max_rounds`
/// (default 5, hard cap 10) exhausts. Returns the rounds log as JSON.
pub struct RalphRunTool {
    tasks: Arc<dyn TaskService>,
}

impl RalphRunTool {
    /// Wrap the child task service.
    pub fn new(tasks: Arc<dyn TaskService>) -> Self {
        Self { tasks }
    }
}

#[async_trait::async_trait]
impl Tool for RalphRunTool {
    fn name(&self) -> &str {
        "ralph_run"
    }

    fn description(&self) -> &str {
        "Run a Ralph loop: spawn a fresh child per round with the same \
         objective plus the prior round summary, until the child reports a \
         RALPH_DONE line or max_rounds exhausts (default 5, cap 10). \
         Returns the rounds log as JSON."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "Immutable objective every round works on",
                },
                "max_rounds": {
                    "type": "integer",
                    "description": "Round budget, 1-10 (default 5)",
                },
            },
            "required": ["objective"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let objective = match required_str(&input, "objective") {
            Ok(objective) => objective.to_string(),
            Err(output) => return Ok(output),
        };
        let max_rounds = match input.get("max_rounds") {
            None | Some(serde_json::Value::Null) => DEFAULT_RALPH_ROUNDS,
            Some(value) => match value.as_u64() {
                Some(rounds) => rounds as u32,
                None => {
                    return Ok(ToolOutput {
                        content: "invalid parameter 'max_rounds' (integer 1-10 required)"
                            .to_string(),
                        is_error: true,
                    });
                }
            },
        };
        if max_rounds == 0 || max_rounds > MAX_RALPH_ROUNDS {
            return Ok(ToolOutput {
                content: format!(
                    "invalid parameter 'max_rounds' (integer 1-{MAX_RALPH_ROUNDS} required)"
                ),
                is_error: true,
            });
        }
        match action_workflow::ralph_run(&objective, max_rounds, self.tasks.as_ref()).await {
            Ok(report) => Ok(ToolOutput {
                content: serde_json::to_string_pretty(&report).unwrap_or_else(|e| e.to_string()),
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                content: e.to_string(),
                is_error: true,
            }),
        }
    }
}

/// `schedule`: the durable cron schedule in one tool.
///
/// One persisted entry list behind three actions. The whole tool is
/// marked destructive because `remove` deletes persisted state, so the
/// approval policy gates every action by name.
pub struct ScheduleTool {
    scheduler: Arc<Mutex<Scheduler>>,
}

impl ScheduleTool {
    /// Wrap the shared durable scheduler.
    pub fn new(scheduler: Arc<Mutex<Scheduler>>) -> Self {
        Self { scheduler }
    }
}

#[async_trait::async_trait]
impl Tool for ScheduleTool {
    fn name(&self) -> &str {
        "schedule"
    }

    fn description(&self) -> &str {
        "Durable cron schedule. Actions: 'add' persists one cron entry \
         (five fields: minute hour day month weekday) with its task \
         input — the entry survives restarts and returns a sched-N id; \
         invalid cron expressions are rejected. 'list' reports persisted \
         entries as JSON (running jobs are never persisted). 'remove' \
         deletes one persisted entry by id (e.g. sched-1); unknown ids \
         are explicit errors."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove"],
                    "description": "Which schedule operation to run"
                },
                "cron": {
                    "type": "string",
                    "description": "add (required): five-field cron expression, e.g. '0 9 * * *'",
                },
                "input": {
                    "type": "string",
                    "description": "add (required): task input the host fires on schedule",
                },
                "id": {
                    "type": "string",
                    "description": "remove (required): schedule entry id, e.g. sched-1",
                },
            },
            "required": ["action"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        // `remove` deletes persisted state; the attribute is tool-level,
        // so the policy gates the whole family by name.
        true
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "add" => {
                let cron = match required_str(&input, "cron") {
                    Ok(cron) => cron.to_string(),
                    Err(output) => return Ok(output),
                };
                let task_input = match required_str(&input, "input") {
                    Ok(task_input) => task_input.to_string(),
                    Err(output) => return Ok(output),
                };
                match lock_scheduler(&self.scheduler).add(&cron, &task_input) {
                    Ok(entry) => Ok(ToolOutput {
                        content: format!("scheduled {} ({})", entry.id, entry.cron),
                        is_error: false,
                    }),
                    Err(e) => Ok(ToolOutput {
                        content: e.to_string(),
                        is_error: true,
                    }),
                }
            }
            "list" => {
                let entries = lock_scheduler(&self.scheduler).entries().to_vec();
                Ok(ToolOutput {
                    content: serde_json::to_string_pretty(&entries)
                        .unwrap_or_else(|e| e.to_string()),
                    is_error: false,
                })
            }
            "remove" => {
                let id = match required_str(&input, "id") {
                    Ok(id) => id.trim().to_string(),
                    Err(output) => return Ok(output),
                };
                match lock_scheduler(&self.scheduler).remove(&id) {
                    Ok(true) => Ok(ToolOutput {
                        content: format!("removed schedule {id}"),
                        is_error: false,
                    }),
                    Ok(false) => Ok(ToolOutput {
                        content: format!("unknown schedule id: {id}"),
                        is_error: true,
                    }),
                    Err(e) => Ok(ToolOutput {
                        content: e.to_string(),
                        is_error: true,
                    }),
                }
            }
            other => Ok(ToolOutput {
                content: format!(
                    "unknown action {other:?}: expected one of add, list, remove"
                ),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::path::PathBuf::from("/tmp"),
            deny_env: Vec::new(),
        }
    }

    fn tasks() -> Arc<dyn TaskService> {
        Arc::new(action_tasks::FakeTaskService::new())
    }

    fn scheduler_in(home: &std::path::Path) -> Arc<Mutex<Scheduler>> {
        let (scheduler, _) = Scheduler::load_or_default(home);
        Arc::new(Mutex::new(scheduler))
    }

    #[tokio::test]
    async fn workflow_run_rejects_invalid_spec_json() {
        let tool = WorkflowRunTool::new(tasks());
        let output = tool
            .execute(serde_json::json!({"spec_json": "{nope"}), &ctx())
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("invalid spec_json"));
    }

    #[tokio::test]
    async fn workflow_run_merges_fanout_summaries() {
        let tool = WorkflowRunTool::new(tasks());
        let output = tool
            .execute(
                serde_json::json!({"spec_json": r#"{"steps": [
                    {"id": "a", "kind": "task", "input": "alpha"},
                    {"id": "b", "kind": "fanout", "input": ["x", "y"],
                     "depends_on": ["a"]}
                ]}"#}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!output.is_error, "run failed: {}", output.content);
        let merged: serde_json::Value = serde_json::from_str(&output.content).expect("json");
        assert_eq!(merged["a"], serde_json::json!("alpha"));
        assert_eq!(merged["b"], serde_json::json!(["x", "y"]));
        assert!(!tool.is_read_only());
    }

    #[tokio::test]
    async fn workflow_run_reports_dag_errors() {
        let tool = WorkflowRunTool::new(tasks());
        let output = tool
            .execute(
                serde_json::json!({"spec_json": r#"{"steps": [
                    {"id": "a", "input": "x", "depends_on": ["ghost"]}
                ]}"#}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("unknown step"));
    }

    #[tokio::test]
    async fn ralph_run_caps_rounds_and_reports_progress() {
        let tool = RalphRunTool::new(tasks());
        let capped = tool
            .execute(
                serde_json::json!({"objective": "work", "max_rounds": 99}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(capped.is_error);
        assert!(capped.content.contains("max_rounds"));
        let report = tool
            .execute(
                serde_json::json!({"objective": "work", "max_rounds": 2}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!report.is_error, "ralph failed: {}", report.content);
        let parsed: serde_json::Value = serde_json::from_str(&report.content).expect("json");
        assert_eq!(parsed["rounds"].as_array().expect("array").len(), 2);
        assert_eq!(parsed["completed"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn schedule_actions_round_trip_in_tempdir() {
        let home = tempfile::tempdir().unwrap();
        let scheduler = scheduler_in(home.path());
        let schedule = ScheduleTool::new(scheduler.clone());
        let added = schedule
            .execute(
                serde_json::json!({"action": "add", "cron": "0 9 * * *", "input": "sync"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!added.is_error, "add failed: {}", added.content);
        assert!(added.content.contains("sched-1"));

        let bad = schedule
            .execute(
                serde_json::json!({"action": "add", "cron": "nope", "input": "x"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(bad.is_error);

        // list is folded into the destructive-marked tool, so the
        // read-only attribute no longer applies family-wide.
        assert!(schedule.is_destructive());
        let listed = schedule
            .execute(serde_json::json!({"action": "list"}), &ctx())
            .await
            .unwrap();
        let entries: serde_json::Value = serde_json::from_str(&listed.content).expect("json");
        assert_eq!(entries.as_array().expect("array").len(), 1);

        let ghost = schedule
            .execute(
                serde_json::json!({"action": "remove", "id": "sched-9"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(ghost.is_error);
        let removed = schedule
            .execute(
                serde_json::json!({"action": "remove", "id": "sched-1"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!removed.is_error);
        let unknown = schedule
            .execute(serde_json::json!({"action": "explode"}), &ctx())
            .await
            .unwrap();
        assert!(unknown.is_error);

        // Removal persisted: a fresh load in the same home stays empty.
        let (reloaded, _) = Scheduler::load_or_default(home.path());
        assert!(reloaded.entries().is_empty());
    }
}
