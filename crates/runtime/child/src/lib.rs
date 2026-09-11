/*!
 * @file ChildRuntime
 * @description Tracked background child tasks with depth-1 isolation.
 *
 * Responsibilities:
 * - Spawn, query, and stop background child tasks.
 * - Collect completion notifications with a bounded drop-oldest queue.
 * - Guarantee by construction that children cannot spawn grandchildren.
 *
 * This module must not depend on: tools, policy, hooks, memory, skills,
 * models, transport, or any concrete capability implementation. It only
 * depends on infrastructure-base for interruption primitives.
 */

//! Background child runtime with structural depth isolation.
//!
//! Child work is built by a factory that receives a [`ChildTicket`]
//! (identity plus a stop signal) and never a [`ChildRuntime`] reference, so
//! grandchild spawning is unconstructable: there is simply no handle to
//! call. Panics inside child work are caught at the task boundary and
//! recorded as failures, so one faulty child can never take down the parent
//! runtime. Completion reports flow through a data-only [`CompletionSink`],
//! which carries no spawn capability by design.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::FutureExt;
use infrastructure_base::InterruptHandle;
use tokio::sync::Notify;

/// Upper bound of buffered completion notifications.
///
/// Notifications are a convenience re-injection into the parent loop; the
/// authoritative terminal state lives in the task table, so dropping the
/// oldest entry on overflow loses no result.
pub const MAX_NOTIFICATIONS: usize = 64;

/// Capability profile of one child task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    /// Full tool access, excluding any child-spawning tool.
    Standard,
    /// Read-only tool subset.
    ReadOnly,
}

/// Specification for one child task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Capability profile of the child.
    pub kind: ChildKind,
    /// Input text the child works on.
    pub input: String,
    /// Parent run id, used for correlation and idempotency.
    pub parent_run_id: String,
    /// Fork-scoped tool surface; empty keeps the full surface.
    pub allowed_tools: Vec<String>,
}

/// Terminal status of a finished child task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// The child produced its summary normally.
    Completed,
    /// The child failed, including via a caught panic.
    Failed,
    /// A stop was requested before or during execution.
    Stopped,
}

/// Terminal result of one child task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    /// How the task ended.
    pub status: TaskStatus,
    /// Human-readable outcome, re-injected into the parent conversation.
    pub summary: String,
    /// Output tokens consumed by the child, for cost accounting.
    pub output_tokens: u64,
}

impl TaskResult {
    /// Convenience constructor for a completed task.
    pub fn completed(summary: impl Into<String>, output_tokens: u64) -> Self {
        Self {
            status: TaskStatus::Completed,
            summary: summary.into(),
            output_tokens,
        }
    }

    /// Convenience constructor for a failed task.
    pub fn failed(summary: impl Into<String>) -> Self {
        Self {
            status: TaskStatus::Failed,
            summary: summary.into(),
            output_tokens: 0,
        }
    }
}

/// Lifecycle state of one tracked task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Currently executing.
    Running,
    /// Reached a terminal result.
    Finished,
}

/// Point-in-time view of one task for queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    /// Current lifecycle state.
    pub state: TaskState,
    /// Terminal result once finished.
    pub result: Option<TaskResult>,
}

/// Handle handed to child work.
///
/// Deliberately minimal: identity plus a stop signal. Because child work
/// never receives the runtime itself, depth beyond one level cannot be
/// expressed in code.
#[derive(Debug, Clone)]
pub struct ChildTicket {
    /// Identifier of this child task (`child-N`).
    pub task_id: String,
    /// Capability profile of this child.
    pub kind: ChildKind,
    /// Input text the child works on, carried from the spawn spec so
    /// factories stop cloning it around the runtime.
    pub input: String,
    /// Parent run id for correlation, carried from the spawn spec.
    pub parent_run_id: String,
    /// Fork-scoped tool surface, carried from the spawn spec so the
    /// service can restrict the run before the first tool executes.
    pub allowed_tools: Vec<String>,
    /// Stop signal; child work polls `is_triggered` at safe points.
    pub stop: InterruptHandle,
}

/// Data-only completion channel shared with detached drivers.
///
/// This carries queue storage and a wakeup signal but no spawn capability,
/// so handing it to a driver does not weaken depth isolation.
#[derive(Debug, Default)]
struct CompletionSink {
    queue: Mutex<VecDeque<String>>,
    changed: Notify,
}

impl CompletionSink {
    /// Recover the guard after a poison; pushes are single short writes.
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// File one completion, dropping the oldest entry on overflow.
    fn push(&self, task_id: &str) {
        let mut queue = self.lock();
        if queue.len() >= MAX_NOTIFICATIONS {
            queue.pop_front();
        }
        queue.push_back(format!("{task_id} finished"));
        self.changed.notify_one();
    }
}

/// Per-task tracking slot.
#[derive(Debug)]
struct TaskSlot {
    /// Stop requested before the driver started or during the run.
    stop_requested: std::sync::atomic::AtomicBool,
    /// Interrupt handle observed by the running child work.
    stop: InterruptHandle,
    /// Current lifecycle state.
    state: Mutex<TaskState>,
    /// Terminal result once finished.
    result: Mutex<Option<TaskResult>>,
}

impl TaskSlot {
    fn new() -> Self {
        Self {
            stop_requested: std::sync::atomic::AtomicBool::new(false),
            stop: InterruptHandle::new(),
            state: Mutex::new(TaskState::Running),
            result: Mutex::new(None),
        }
    }

    /// Recover guards after a poison; critical sections are single short
    /// writes that leave no half-written invariant behind.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, TaskState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_result(&self) -> std::sync::MutexGuard<'_, Option<TaskResult>> {
        self.result.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record the terminal result; the driver files the notification next.
    fn finish(&self, result: TaskResult) {
        *self.lock_result() = Some(result);
        *self.lock_state() = TaskState::Finished;
    }

    fn stop_was_requested(&self) -> bool {
        self.stop_requested
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Runtime that spawns, tracks, and stops background child tasks.
#[derive(Debug, Default)]
pub struct ChildRuntime {
    /// Task table: id -> tracking slot.
    tasks: Mutex<HashMap<String, Arc<TaskSlot>>>,
    /// Completion channel shared with detached drivers.
    sink: Arc<CompletionSink>,
    /// Monotonic id allocator; ids look like `child-N` starting at 1.
    next_id: AtomicUsize,
}

impl ChildRuntime {
    /// Create an empty runtime.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_tasks(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<TaskSlot>>> {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn alloc_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        format!("child-{n}")
    }

    /// Spawn a background child task and return its id immediately.
    ///
    /// The factory receives only a [`ChildTicket`]; it cannot reach this
    /// runtime, which is what makes depth-1 structural instead of
    /// conventional. A stop requested before the driver starts is honoured
    /// without running any child work.
    pub fn spawn_background<F, MakeWork>(&self, spec: ChildSpec, make_work: MakeWork) -> String
    where
        F: Future<Output = TaskResult> + Send + 'static,
        MakeWork: FnOnce(ChildTicket) -> F + Send + 'static,
    {
        let id = self.alloc_id();
        let slot = Arc::new(TaskSlot::new());
        self.lock_tasks().insert(id.clone(), slot.clone());
        let ticket = ChildTicket {
            task_id: id.clone(),
            kind: spec.kind,
            input: spec.input,
            parent_run_id: spec.parent_run_id,
            allowed_tools: spec.allowed_tools,
            stop: slot.stop.clone(),
        };
        let sink = self.sink.clone();
        let driver_id = id.clone();
        // Detached driver: it owns the slot and a data-only sink, never the
        // runtime, so grandchildren stay unconstructable inside child work.
        tokio::spawn(async move {
            if slot.stop_was_requested() {
                slot.finish(TaskResult {
                    status: TaskStatus::Stopped,
                    summary: "stop requested before start".to_string(),
                    output_tokens: 0,
                });
            } else {
                // Task boundary panic isolation: a panicking child becomes a
                // recorded failure instead of crashing the parent runtime.
                let result = match AssertUnwindSafe(make_work(ticket)).catch_unwind().await {
                    Ok(result) => {
                        if slot.stop_was_requested() {
                            TaskResult {
                                status: TaskStatus::Stopped,
                                ..result
                            }
                        } else {
                            result
                        }
                    }
                    Err(_) => TaskResult::failed("child task panicked"),
                };
                slot.finish(result);
            }
            sink.push(&driver_id);
        });
        id
    }

    /// Query the current view of one task; `None` for unknown ids.
    pub fn query(&self, task_id: &str) -> Option<TaskView> {
        let slot = self.lock_tasks().get(task_id).cloned()?;
        Some(TaskView {
            state: *slot.lock_state(),
            result: slot.lock_result().clone(),
        })
    }

    /// Request a stop; true when a tracked task accepted the signal.
    ///
    /// Sets both the pre-start flag and the live interrupt handle, covering
    /// drivers that have not started yet as well as running work.
    pub fn stop(&self, task_id: &str) -> bool {
        let slot = match self.lock_tasks().get(task_id).cloned() {
            Some(slot) => slot,
            None => return false,
        };
        slot.stop_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        slot.stop.trigger();
        true
    }

    /// Borrow the live stop handle of one task, if tracked.
    pub fn stop_handle(&self, task_id: &str) -> Option<InterruptHandle> {
        self.lock_tasks().get(task_id).map(|slot| slot.stop.clone())
    }

    /// Take all buffered completion notifications (parent loop consumes).
    pub fn drain_notifications(&self) -> Vec<String> {
        std::mem::take(&mut *self.sink.lock()).into_iter().collect()
    }

    /// Wait until any completion notification arrives.
    pub async fn wait_for_update(&self) {
        self.sink.changed.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_until_finished(rt: &ChildRuntime, id: &str) {
        for _ in 0..1000 {
            if matches!(rt.query(id).map(|v| v.state), Some(TaskState::Finished)) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("task {id} did not finish in time");
    }

    async fn wait_for_notification(rt: &ChildRuntime, id: &str) -> Vec<String> {
        for _ in 0..1000 {
            let notes = rt.drain_notifications();
            if notes.iter().any(|n| n.contains(id)) {
                return notes;
            }
            tokio::task::yield_now().await;
        }
        panic!("no notification for {id} in time");
    }

    fn spec(input: &str) -> ChildSpec {
        ChildSpec {
            kind: ChildKind::Standard,
            input: input.to_string(),
            parent_run_id: "run-1".to_string(),
            allowed_tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn spawn_runs_work_and_files_a_notification() {
        let rt = ChildRuntime::new();
        // Ticket-only factory: this closure cannot name ChildRuntime, which
        // is the depth-1 guarantee in miniature.
        let id = rt.spawn_background(spec("hello"), |_ticket| async {
            TaskResult::completed("done", 7)
        });
        assert!(id.starts_with("child-"));
        wait_until_finished(&rt, &id).await;
        let view = rt.query(&id).unwrap();
        assert_eq!(view.state, TaskState::Finished);
        let result = view.result.unwrap();
        assert_eq!(result.status, TaskStatus::Completed);
        assert_eq!(result.output_tokens, 7);
        let notes = wait_for_notification(&rt, &id).await;
        assert!(notes.iter().any(|n| n.contains(&id)));
    }

    #[tokio::test]
    async fn ticket_carries_spec_input_and_parent() {
        let rt = ChildRuntime::new();
        let id = rt.spawn_background(spec("summarize this"), |ticket| async move {
            assert_eq!(ticket.input, "summarize this");
            assert_eq!(ticket.parent_run_id, "run-1");
            assert!(ticket.task_id.starts_with("child-"));
            TaskResult::completed("done", 0)
        });
        wait_until_finished(&rt, &id).await;
        assert_eq!(
            rt.query(&id).unwrap().result.unwrap().status,
            TaskStatus::Completed
        );
    }

    #[tokio::test]
    async fn stop_unknown_id_returns_false_and_stop_sets_the_flag() {
        let rt = ChildRuntime::new();
        assert!(!rt.stop("child-999"));
        let id = rt.spawn_background(spec("x"), |ticket| async move {
            // Cooperative work observes the ticket stop signal.
            if ticket.stop.is_triggered() {
                return TaskResult {
                    status: TaskStatus::Stopped,
                    summary: "observed stop".to_string(),
                    output_tokens: 0,
                };
            }
            TaskResult::completed("ran", 0)
        });
        assert!(rt.stop(&id));
        assert!(rt.stop_handle(&id).unwrap().is_triggered());
        wait_until_finished(&rt, &id).await;
        assert!(rt.query(&id).unwrap().result.is_some());
    }

    #[tokio::test]
    async fn child_panic_becomes_a_recorded_failure() {
        let rt = ChildRuntime::new();
        let id = rt.spawn_background(spec("boom"), |_ticket| async {
            panic!("simulated child fault")
        });
        wait_until_finished(&rt, &id).await;
        let result = rt.query(&id).unwrap().result.unwrap();
        assert_eq!(result.status, TaskStatus::Failed);
        assert_eq!(result.summary, "child task panicked");
    }

    #[tokio::test]
    async fn notification_queue_drops_oldest_on_overflow() {
        let rt = ChildRuntime::new();
        // File completions directly through the sink path to stay
        // deterministic without racing the scheduler.
        for i in 0..(MAX_NOTIFICATIONS + 5) {
            rt.sink.push(&format!("child-{i}"));
        }
        let notes = rt.drain_notifications();
        assert_eq!(notes.len(), MAX_NOTIFICATIONS);
        assert!(notes.iter().any(|n| n.contains("child-68")));
        assert!(!notes.iter().any(|n| n.contains("child-0")));
        assert!(rt.drain_notifications().is_empty());
    }
}
