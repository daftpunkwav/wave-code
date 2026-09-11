/*!
 * @file TaskQueryTools
 * @description Model-invoked inspection of background child tasks.
 *
 * Responsibilities:
 * - Report one task's lifecycle state and terminal outcome.
 * - Stop a running task without touching anything else.
 * - Keep unknown ids as explicit business errors, never panics.
 *
 * This module must not depend on: drivers, actors, or sessions. Tools read
 * the task service only; completion still arrives via notifications.
 */

//! Task query tools: observe and stop what `skill` forked.

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

#[cfg(test)]
mod tests {
    use super::*;
    use action_tasks::{TaskInfo, TaskOutcome};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeTasks {
        tasks: Mutex<HashMap<String, TaskInfo>>,
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
    }

    fn service() -> Arc<FakeTasks> {
        let service = Arc::new(FakeTasks {
            tasks: Mutex::new(HashMap::new()),
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
}
