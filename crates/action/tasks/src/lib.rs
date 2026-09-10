/*!
 * @file TaskService
 * @description Child task lifecycle behind a capability-neutral seam.
 *
 * Responsibilities:
 * - Name task requests, handles, states, and outcomes.
 * - Define the spawn/query/stop seam drivers implement.
 * - Ship an immediate fake for tests and dry runs.
 *
 * This module must not depend on: runtime, state, or any execution
 * layer. Layer direction forbids action crates from naming runtime
 * types; composition roots map between the two vocabularies.
 */

//! Task service seam: lifecycle without execution knowledge.

/// Requested task profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// Full tool access except task-spawning tools.
    Standard,
    /// Read-only tool subset.
    ReadOnly,
}

/// One task request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRequest {
    /// Requested profile.
    pub kind: TaskKind,
    /// Input text the task works on.
    pub input: String,
    /// Parent run id for correlation.
    pub parent_run_id: String,
}

/// Lifecycle state of one tracked task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Currently executing.
    Running,
    /// Reached a terminal outcome.
    Finished,
}

/// Terminal outcome of one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    /// Produced its summary normally.
    Completed {
        /// Human-readable outcome.
        summary: String,
    },
    /// Failed, including via caught panics.
    Failed {
        /// Human-readable cause.
        reason: String,
    },
    /// Stopped before or during execution.
    Stopped,
}

/// Point-in-time task view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    /// Current lifecycle state.
    pub state: TaskState,
    /// Terminal outcome once finished.
    pub outcome: Option<TaskOutcome>,
}

/// Task lifecycle seam.
pub trait TaskService: Send + Sync {
    /// Spawn a task, returning its handle id immediately.
    fn spawn(&self, request: TaskRequest) -> String;

    /// Query one task; `None` for unknown ids.
    fn query(&self, id: &str) -> Option<TaskInfo>;

    /// Request a stop; false for unknown ids.
    fn stop(&self, id: &str) -> bool;
}

/// Immediate fake: tasks complete on spawn with an echo summary.
#[derive(Debug, Default)]
pub struct FakeTaskService {
    spawned: std::sync::Mutex<Vec<(String, TaskRequest)>>,
}

impl FakeTaskService {
    /// Create an empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests seen so far, in spawn order.
    pub fn spawned(&self) -> Vec<(String, TaskRequest)> {
        self.spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl TaskService for FakeTaskService {
    fn spawn(&self, request: TaskRequest) -> String {
        let id = format!("task-{}", self.spawned().len() + 1);
        self.spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id.clone(), request));
        id
    }

    fn query(&self, id: &str) -> Option<TaskInfo> {
        self.spawned()
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, request)| TaskInfo {
                state: TaskState::Finished,
                outcome: Some(TaskOutcome::Completed {
                    summary: request.input.clone(),
                }),
            })
    }

    fn stop(&self, id: &str) -> bool {
        self.spawned().iter().any(|(known, _)| known == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> TaskRequest {
        TaskRequest {
            kind: TaskKind::ReadOnly,
            input: "summarize".to_string(),
            parent_run_id: "run-1".to_string(),
        }
    }

    #[test]
    fn fake_completes_immediately_with_echo() {
        let service = FakeTaskService::new();
        let id = service.spawn(request());
        assert!(id.starts_with("task-"));
        let info = service.query(&id).unwrap();
        assert_eq!(info.state, TaskState::Finished);
        assert!(matches!(info.outcome, Some(TaskOutcome::Completed { .. })));
        assert!(service.stop(&id));
        assert!(!service.stop("task-999"));
        assert!(service.query("task-999").is_none());
    }
}
