/*!
 * @file TaskService
 * @description Child task lifecycle behind a capability-neutral seam.
 *
 * Responsibilities:
 * - Name task requests, handles, states, and outcomes.
 * - Carry depth and parent fields for nested child accounting.
 * - Define the spawn/query/stop seam drivers implement, plus an optional
 *   follow-up continuation hook.
 * - Ship fakes for tests: immediate completion and scripted lifecycles.
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
    /// Fork-scoped tool surface from skill `allowed-tools`; empty keeps
    /// the full surface.
    pub allowed_tools: Vec<String>,
    /// Nesting depth: 0 for top-level tasks, parent depth + 1 for
    /// follow-ups. Execution layers refuse anything past their cap.
    pub depth: u8,
    /// Parent task id for lineage; None for top-level tasks.
    pub parent: Option<String>,
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
    /// Lineage chain from the oldest ancestor down to this task itself
    /// (last element is the queried id when known).
    pub lineage: Vec<String>,
}

/// Walk parent links root-first, ending with the queried id itself.
/// Cycle-safe: a repeated id ends the walk instead of looping.
fn lineage_of(spawned: &[(String, TaskRequest)], id: &str) -> Vec<String> {
    let mut chain = vec![id.to_string()];
    let mut current = id.to_string();
    while let Some((_, request)) = spawned.iter().find(|(known, _)| *known == current) {
        match request.parent.clone() {
            Some(parent_id) if !chain.contains(&parent_id) => {
                chain.push(parent_id.clone());
                current = parent_id;
            }
            _ => break,
        }
    }
    chain.reverse();
    chain
}

/// Task lifecycle seam.
pub trait TaskService: Send + Sync {
    /// Spawn a task, returning its handle id immediately.
    fn spawn(&self, request: TaskRequest) -> String;

    /// Query one task; `None` for unknown ids.
    fn query(&self, id: &str) -> Option<TaskInfo>;

    /// Request a stop; false for unknown ids.
    fn stop(&self, id: &str) -> bool;

    /// Spawn a follow-up child of `id` working on `followup_input`.
    ///
    /// Provided default returns `None` (no continuation support), so
    /// existing implementors keep compiling untouched. Services backed by
    /// a depth-accounting runtime override this to spawn a depth + 1
    /// child with `parent` set to `id`, extending the lineage chain.
    /// Unknown ids return `None`.
    fn continue_task(&self, _id: &str, _followup_input: String) -> Option<String> {
        None
    }
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
        let spawned = self.spawned();
        spawned
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, request)| TaskInfo {
                state: TaskState::Finished,
                outcome: Some(TaskOutcome::Completed {
                    summary: request.input.clone(),
                }),
                lineage: lineage_of(&spawned, id),
            })
    }

    fn stop(&self, id: &str) -> bool {
        self.spawned().iter().any(|(known, _)| known == id)
    }
}

/// Scripted fake: tasks start Running and advance only when the test says.
///
/// The immediate fake above can never represent in-flight work, so no test
/// can exercise Running-state handling or stop-before-finish flows. This
/// fake fills that hole: `spawn` returns a Running task, and the test
/// drives it to an outcome with `complete`, `fail`, or `stop`.
#[derive(Debug, Default)]
pub struct ScriptedTaskService {
    tasks: std::sync::Mutex<std::collections::HashMap<String, TaskInfo>>,
    requests: std::sync::Mutex<std::collections::HashMap<String, TaskRequest>>,
    next: std::sync::Mutex<u64>,
}

impl ScriptedTaskService {
    /// Create an empty scripted service.
    pub fn new() -> Self {
        Self::default()
    }

    /// Finish one Running task with a summary; false for unknown ids.
    pub fn complete(&self, id: &str, summary: impl Into<String>) -> bool {
        self.finish(
            id,
            TaskOutcome::Completed {
                summary: summary.into(),
            },
        )
    }

    /// Fail one Running task with a reason; false for unknown ids.
    pub fn fail(&self, id: &str, reason: impl Into<String>) -> bool {
        self.finish(
            id,
            TaskOutcome::Failed {
                reason: reason.into(),
            },
        )
    }

    fn finish(&self, id: &str, outcome: TaskOutcome) -> bool {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        match tasks.get_mut(id) {
            Some(info) if info.state == TaskState::Running => {
                info.state = TaskState::Finished;
                info.outcome = Some(outcome);
                true
            }
            _ => false,
        }
    }
}

impl TaskService for ScriptedTaskService {
    fn spawn(&self, request: TaskRequest) -> String {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        *next += 1;
        let id = format!("task-{next}");
        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id.clone(),
            TaskInfo {
                state: TaskState::Running,
                outcome: None,
                lineage: Vec::new(),
            },
        );
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), request);
        id
    }

    fn query(&self, id: &str) -> Option<TaskInfo> {
        let info = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()?;
        let requests = self.requests.lock().unwrap_or_else(|e| e.into_inner());
        let lineage: Vec<(String, TaskRequest)> = requests
            .iter()
            .map(|(known, request)| (known.clone(), request.clone()))
            .collect();
        Some(TaskInfo {
            lineage: lineage_of(&lineage, id),
            ..info
        })
    }

    fn stop(&self, id: &str) -> bool {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        match tasks.get_mut(id) {
            Some(info) if info.state == TaskState::Running => {
                info.state = TaskState::Finished;
                info.outcome = Some(TaskOutcome::Stopped);
                true
            }
            _ => false,
        }
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
            allowed_tools: Vec::new(),
            depth: 0,
            parent: None,
        }
    }

    fn child_request(parent: &str, depth: u8) -> TaskRequest {
        TaskRequest {
            kind: TaskKind::ReadOnly,
            input: "follow up".to_string(),
            parent_run_id: "run-1".to_string(),
            allowed_tools: Vec::new(),
            depth,
            parent: Some(parent.to_string()),
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

    #[test]
    fn continue_task_defaults_to_none_for_a_stub_service() {
        struct Stub;
        impl TaskService for Stub {
            fn spawn(&self, _request: TaskRequest) -> String {
                "task-1".to_string()
            }
            fn query(&self, _id: &str) -> Option<TaskInfo> {
                None
            }
            fn stop(&self, _id: &str) -> bool {
                false
            }
        }
        let service = Stub;
        assert_eq!(service.continue_task("task-1", "more".to_string()), None);
        // Fakes that do not override the hook inherit the same default.
        assert_eq!(
            FakeTaskService::new().continue_task("task-1", "more".to_string()),
            None
        );
        assert_eq!(
            ScriptedTaskService::new().continue_task("task-1", "more".to_string()),
            None
        );
    }

    #[test]
    fn fakes_expose_lineage_chains() {
        let service = FakeTaskService::new();
        let root = service.spawn(request());
        assert_eq!(service.query(&root).unwrap().lineage, vec![root.clone()]);
        let middle = service.spawn(child_request(&root, 1));
        assert_eq!(
            service.query(&middle).unwrap().lineage,
            vec![root.clone(), middle.clone()]
        );
        let scripted = ScriptedTaskService::new();
        let s_root = scripted.spawn(request());
        let s_leaf = scripted.spawn(child_request(&s_root, 1));
        assert_eq!(
            scripted.query(&s_leaf).unwrap().lineage,
            vec![s_root.clone(), s_leaf.clone()]
        );
    }

    #[test]
    fn scripted_drives_full_lifecycles() {
        let service = ScriptedTaskService::new();
        let running = service.spawn(request());
        let info = service.query(&running).unwrap();
        assert_eq!(info.state, TaskState::Running);
        assert_eq!(info.outcome, None);
        // Unknown ids advance nothing.
        assert!(!service.complete("task-999", "x"));
        assert!(!service.fail("task-999", "x"));
        // Complete, fail, and stop each terminate exactly once.
        assert!(service.complete(&running, "done"));
        assert!(!service.complete(&running, "again"));
        let second = service.spawn(request());
        assert!(service.fail(&second, "boom"));
        assert!(matches!(
            service.query(&second).unwrap().outcome,
            Some(TaskOutcome::Failed { .. })
        ));
        let third = service.spawn(request());
        assert!(service.stop(&third));
        assert!(!service.stop(&third));
        assert_eq!(
            service.query(&third).unwrap().outcome,
            Some(TaskOutcome::Stopped)
        );
    }
}
