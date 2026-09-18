/*!
 * @file TaskQueryTools
 * @description Model-invoked observation and continuation of background child tasks.
 *
 * Responsibilities:
 * - Report one task's lifecycle state and terminal outcome.
 * - Stop a running task without touching anything else.
 * - Continue a finished task with a follow-up instruction (new depth+1
 *   generation carrying the parent's lineage).
 * - Keep unknown ids as explicit business errors, never panics.
 *
 * This module must not depend on: drivers, actors, or sessions. Tools read
 * the task service only; completion still arrives via notifications.
 */

//! Task tools: observe, stop, and continue what `task` and `skill` forked.

use std::sync::Arc;

use action_tasks::{TaskService, TaskState};
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// Shared lookup helper: unknown ids become business errors.
fn lookup(
    tasks: &Arc<dyn TaskService>,
    id: &str,
) -> std::result::Result<action_tasks::TaskInfo, ToolOutput> {
    match tasks.query(id) {
        Some(info) => Ok(info),
        None => Err(ToolOutput {
            content: format!("unknown task id: {id}"),
            is_error: true,
        }),
    }
}

/// `task_output`: report one child task's state and outcome.
///
/// Running tasks report `still running` so the model polls again later;
/// finished tasks report their summary, failure reason, or stopped state.
pub struct TaskOutputTool {
    tasks: Arc<dyn TaskService>,
}

impl TaskOutputTool {
    /// Wrap the child task service.
    pub fn new(tasks: Arc<dyn TaskService>) -> Self {
        Self { tasks }
    }
}

#[async_trait::async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Check one background child task by id (from the `skill` tool or a \
         previous task id). Reports running state or the terminal outcome: \
         completion summary, failure reason, or stopped. Poll running tasks \
         again after doing other work."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Child task id, e.g. task-1",
                },
            },
            "required": ["id"],
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let id = input
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let info = match lookup(&self.tasks, id) {
            Ok(info) => info,
            Err(output) => return Ok(output),
        };
        let content = match (&info.state, &info.outcome) {
            (TaskState::Running, _) => format!("task {id} is still running"),
            (_, Some(action_tasks::TaskOutcome::Completed { summary })) => {
                format!("task {id} completed: {summary}")
            }
            (_, Some(action_tasks::TaskOutcome::Failed { reason })) => {
                format!("task {id} failed: {reason}")
            }
            (_, Some(action_tasks::TaskOutcome::Stopped)) => {
                format!("task {id} was stopped")
            }
            (_, None) => format!("task {id} finished without an outcome"),
        };
        Ok(ToolOutput {
            content,
            is_error: false,
        })
    }
}

/// `task_stop`: request a stop for one running child task.
///
/// Stopping is cooperative: already-finished tasks report false instead of
/// failing, and unknown ids stay explicit business errors.
pub struct TaskStopTool {
    tasks: Arc<dyn TaskService>,
}

impl TaskStopTool {
    /// Wrap the child task service.
    pub fn new(tasks: Arc<dyn TaskService>) -> Self {
        Self { tasks }
    }
}

#[async_trait::async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "task_stop"
    }

    fn description(&self) -> &str {
        "Stop one background child task by id. Use it when the task is no \
         longer needed or went down the wrong path; its partial result, if \
         any, still arrives as a notification."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Child task id, e.g. task-1",
                },
            },
            "required": ["id"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let id = input
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if self.tasks.stop(id) {
            Ok(ToolOutput {
                content: format!("stop requested for task {id}"),
                is_error: false,
            })
        } else {
            Ok(ToolOutput {
                content: format!("unknown task id: {id}"),
                is_error: true,
            })
        }
    }
}

/// `task_continue`: send a follow-up instruction to a finished child task.
///
/// The seam spawns a new child generation (depth + 1) carrying the
/// parent's profile and lineage; unknown ids stay explicit business
/// errors. Follow-ups nest up to the runtime depth cap: an over-cap
/// child is accepted but finishes failed with a "max child depth"
/// reason, visible via `task_output`.
pub struct TaskContinueTool {
    tasks: Arc<dyn TaskService>,
}

impl TaskContinueTool {
    /// Wrap the child task service.
    pub fn new(tasks: Arc<dyn TaskService>) -> Self {
        Self { tasks }
    }
}

#[async_trait::async_trait]
impl Tool for TaskContinueTool {
    fn name(&self) -> &str {
        "task_continue"
    }

    fn description(&self) -> &str {
        "Send a follow-up instruction to a finished child task: it spawns \
         a new generation carrying the parent's tool surface and lineage \
         (depth + 1), so the follow-up continues the same delegation \
         thread. Works only on finished tasks; running tasks can only be \
         stopped. Follow-ups nest up to the runtime depth cap — an \
         over-cap child is accepted but finishes failed with a 'max \
         child depth' reason (see task_output)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Finished child task id to continue, e.g. task-1",
                },
                "followup": {
                    "type": "string",
                    "description": "Next instruction for the delegation thread (non-empty)",
                },
            },
            "required": ["id", "followup"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let id = input
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let followup = input
            .get("followup")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if followup.is_empty() {
            return Ok(ToolOutput {
                content: "missing parameter 'followup' (non-empty string required)".into(),
                is_error: true,
            });
        }
        match self.tasks.continue_task(id, followup.to_string()) {
            Some(new_id) => {
                // Depth is the new child's nesting level: lineage length
                // minus one (root first). A missing query entry just omits
                // the note instead of failing the continuation.
                let depth_note = match self.tasks.query(&new_id) {
                    Some(info) => format!(" at depth {}", info.lineage.len().saturating_sub(1)),
                    None => String::new(),
                };
                Ok(ToolOutput {
                    content: format!(
                        "continued task {id} as {new_id}{depth_note}; \
                         observe it with task_output"
                    ),
                    is_error: false,
                })
            }
            None => Ok(ToolOutput {
                content: format!("unknown task id: {id}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use action_tasks::{TaskInfo, TaskOutcome};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeTasks {
        tasks: Mutex<HashMap<String, TaskInfo>>,
        /// parent id -> spawned child id, mirrored into `tasks` on continue
        /// so the tool's depth note resolves like the real service.
        continuations: Mutex<HashMap<String, String>>,
    }

    #[async_trait::async_trait]
    impl TaskService for FakeTasks {
        fn spawn(&self, _request: action_tasks::TaskRequest) -> String {
            "task-0".to_string()
        }

        fn query(&self, id: &str) -> Option<TaskInfo> {
            self.tasks.lock().unwrap().get(id).cloned()
        }

        fn stop(&self, id: &str) -> bool {
            self.tasks.lock().unwrap().remove(id).is_some()
        }

        fn continue_task(&self, id: &str, _followup: String) -> Option<String> {
            let child = self.continuations.lock().unwrap().get(id).cloned()?;
            Some(child)
        }
    }

    fn service() -> Arc<FakeTasks> {
        let service = Arc::new(FakeTasks {
            tasks: Mutex::new(HashMap::new()),
            continuations: Mutex::new(HashMap::new()),
        });
        service.tasks.lock().unwrap().insert(
            "task-1".to_string(),
            TaskInfo {
                state: TaskState::Running,
                outcome: None,
                lineage: vec!["task-1".to_string()],
            },
        );
        service.tasks.lock().unwrap().insert(
            "task-2".to_string(),
            TaskInfo {
                state: TaskState::Finished,
                outcome: Some(TaskOutcome::Completed {
                    summary: "all done".to_string(),
                }),
                lineage: vec!["task-2".to_string()],
            },
        );
        // task-2 continues as task-3, one generation deeper.
        service.tasks.lock().unwrap().insert(
            "task-3".to_string(),
            TaskInfo {
                state: TaskState::Running,
                outcome: None,
                lineage: vec!["task-2".to_string(), "task-3".to_string()],
            },
        );
        service
            .continuations
            .lock()
            .unwrap()
            .insert("task-2".to_string(), "task-3".to_string());
        service
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::path::PathBuf::from("/tmp"),
            deny_env: Vec::new(),
        }
    }

    #[tokio::test]
    async fn output_reports_running_finished_and_unknown() {
        let service = service();
        let tool = TaskOutputTool::new(service);
        let running = tool
            .execute(serde_json::json!({"id": "task-1"}), &ctx())
            .await
            .unwrap();
        assert!(!running.is_error);
        assert!(running.content.contains("still running"));
        let done = tool
            .execute(serde_json::json!({"id": "task-2"}), &ctx())
            .await
            .unwrap();
        assert!(done.content.contains("all done"));
        let ghost = tool
            .execute(serde_json::json!({"id": "task-9"}), &ctx())
            .await
            .unwrap();
        assert!(ghost.is_error);
        assert!(ghost.content.contains("unknown task id"));
        assert!(tool.is_read_only());
    }

    #[tokio::test]
    async fn stop_removes_once_then_reports_unknown() {
        let service = service();
        let tool = TaskStopTool::new(service);
        let first = tool
            .execute(serde_json::json!({"id": "task-1"}), &ctx())
            .await
            .unwrap();
        assert!(!first.is_error);
        assert!(first.content.contains("stop requested"));
        let second = tool
            .execute(serde_json::json!({"id": "task-1"}), &ctx())
            .await
            .unwrap();
        assert!(second.is_error);
        assert!(!tool.is_read_only());
    }

    #[tokio::test]
    async fn continue_spawns_generation_and_reports_depth() {
        let service = service();
        let tool = TaskContinueTool::new(service);
        let out = tool
            .execute(
                serde_json::json!({"id": "task-2", "followup": "now audit the diff"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .contains("continued task task-2 as task-3 at depth 1"),
            "{}",
            out.content
        );
        // Unknown ids and blank follow-ups fail openly.
        let ghost = tool
            .execute(serde_json::json!({"id": "task-9", "followup": "x"}), &ctx())
            .await
            .unwrap();
        assert!(ghost.is_error);
        assert!(ghost.content.contains("unknown task id"));
        let blank = tool
            .execute(
                serde_json::json!({"id": "task-2", "followup": "   "}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(blank.is_error);
        assert!(blank.content.contains("'followup'"));
        assert!(!tool.is_read_only());
    }
}
