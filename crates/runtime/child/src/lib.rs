/*!
 * @file ChildRuntime
 * @description Tracked background child tasks with depth accounting.
 *
 * Responsibilities:
 * - Spawn, query, and stop background child tasks.
 * - Enforce a max child depth cap with explicit failed outcomes.
 * - Expose lineage chains on query for continuable children.
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
//! autonomous grandchild spawning from inside a child (e.g. a child model
//! invoking a fork tool) is unconstructable: there is simply no handle to
//! call. Nested depth still exists through the parent-side service layer,
//! which spawns follow-up children with `depth + 1` and `parent` set; the
//! runtime accounts that depth and refuses anything past [`MAX_CHILD_DEPTH`]
//! with an explicit failed outcome instead of panicking. Panics inside child
//! work are caught at the task boundary and recorded as failures, so one
//! faulty child can never take down the parent runtime. Completion reports
//! flow through a data-only [`CompletionSink`], which carries no spawn
//! capability by design.

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

/// Finished tasks kept queryable per runtime; older ones are reaped
/// at spawn time so a long-lived process cannot grow the table forever.
pub const MAX_FINISHED_TASKS: usize = 64;

/// Maximum accepted child depth.
///
/// Depth 0 is a top-level child spawned by the parent loop; each follow-up
/// generation adds one. Spawns past this cap are refused with an explicit
/// failed result (see [`ChildRuntime::spawn_background`]), never a panic.
pub const MAX_CHILD_DEPTH: u8 = 3;

/// Capability profile of one child task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    /// Full tool access minus the child-spawning surfaces; the actual
    /// narrowing is applied by the spawning service (bootstrap), which
    /// owns the registry-derived surface policy.
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
    /// Nesting depth: 0 for top-level children, parent depth + 1 for
    /// follow-ups. Past [`MAX_CHILD_DEPTH`] the spawn is refused.
    pub depth: u8,
    /// Parent task id for lineage; None for top-level children.
    pub parent: Option<String>,
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
    /// Lineage chain from the oldest ancestor down to this task itself
    /// (last element is always the queried id); best-effort when a parent
    /// id is unknown or untracked.
    pub lineage: Vec<String>,
}

/// Handle handed to child work.
///
/// Deliberately minimal: identity plus a stop signal. Because child work
/// never receives the runtime itself, autonomous grandchild spawning (a
/// child invoking a fork from inside) cannot be expressed in code; deeper
/// generations are only created parent-side via follow-up spawns that bump
/// `depth` and set `parent`.
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
    /// Nesting depth carried from the spawn spec, for observability.
    pub depth: u8,
    /// Parent task id carried from the spawn spec; None at depth 0.
    pub parent: Option<String>,
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
        self.push_text(&format!("{task_id} finished"));
    }

    /// File one preformatted notice, dropping the oldest entry on overflow.
    fn push_text(&self, text: &str) {
        let mut queue = self.lock();
        if queue.len() >= MAX_NOTIFICATIONS {
            queue.pop_front();
        }
        queue.push_back(text.to_string());
        self.changed.notify_one();
    }
}

/// Per-task tracking slot.
#[derive(Debug)]
struct TaskSlot {
    /// Nesting depth recorded at spawn, authoritative for cap checks.
    depth: u8,
    /// Parent task id recorded at spawn, for lineage walks.
    parent: Option<String>,
    /// Stop requested before the driver started or during the run.
    stop_requested: std::sync::atomic::AtomicBool,
    /// Interrupt handle observed by the running child work.
    stop: InterruptHandle,
    /// Lifecycle state plus terminal result under one lock, so a
    /// query can never observe `Finished` with a missing result (or
    /// a result ahead of its state) mid-transition.
    inner: Mutex<SlotInner>,
}

/// The state/result pair a [`TaskView`] snapshot reads atomically.
#[derive(Debug)]
struct SlotInner {
    state: TaskState,
    result: Option<TaskResult>,
}

impl TaskSlot {
    fn new(depth: u8, parent: Option<String>) -> Self {
        Self {
            depth,
            parent,
            stop_requested: std::sync::atomic::AtomicBool::new(false),
            stop: InterruptHandle::new(),
            inner: Mutex::new(SlotInner {
                state: TaskState::Running,
                result: None,
            }),
        }
    }

    /// Recover guards after a poison; critical sections are single short
    /// writes that leave no half-written invariant behind.
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, SlotInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record the terminal result; the driver files the notification next.
    fn finish(&self, result: TaskResult) {
        let mut inner = self.lock_inner();
        inner.result = Some(result);
        inner.state = TaskState::Finished;
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

    /// Keep the table bounded: finished tasks beyond the newest
    /// [`MAX_FINISHED_TASKS`] are reaped oldest-first at spawn time,
    /// mirroring the bounded notification queue. Running tasks are never
    /// reaped, and a reaped id simply queries as unknown afterwards.
    fn reap_finished_tasks(&self) {
        let mut tasks = self.lock_tasks();
        let finished: Vec<(usize, String)> = tasks
            .iter()
            .filter(|(_, slot)| matches!(slot.lock_inner().state, TaskState::Finished))
            .filter_map(|(id, _)| {
                id.strip_prefix("child-")
                    .and_then(|n| n.parse::<usize>().ok())
                    .map(|n| (n, id.clone()))
            })
            .collect();
        // Leave one slot for the spawn that triggered this reap.
        let bound = MAX_FINISHED_TASKS.saturating_sub(1);
        if finished.len() <= bound {
            return;
        }
        let mut finished = finished;
        finished.sort();
        for (_, id) in finished.iter().take(finished.len() - bound) {
            tasks.remove(id);
        }
    }

    fn alloc_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        format!("child-{n}")
    }

    /// Spawn a background child task and return its id immediately.
    ///
    /// The factory receives only a [`ChildTicket`]; it cannot reach this
    /// runtime, which is what makes depth isolation structural instead of
    /// conventional. A stop requested before the driver starts is honoured
    /// without running any child work.
    ///
    /// Depth cap: a spec with `depth > MAX_CHILD_DEPTH` is refused without
    /// running any child work. The returned id is still tracked and its
    /// query view is immediately `Finished` with a `Failed` result naming
    /// the cap, so callers observe the refusal through the normal query
    /// path instead of a panic or an unknown id.
    pub fn spawn_background<F, MakeWork>(&self, spec: ChildSpec, make_work: MakeWork) -> String
    where
        F: Future<Output = TaskResult> + Send + 'static,
        MakeWork: FnOnce(ChildTicket) -> F + Send + 'static,
    {
        self.reap_finished_tasks();
        let id = self.alloc_id();
        if spec.depth > MAX_CHILD_DEPTH {
            let slot = Arc::new(TaskSlot::new(spec.depth, spec.parent));
            slot.finish(TaskResult::failed(format!(
                "max child depth {} exceeded",
                MAX_CHILD_DEPTH
            )));
            self.lock_tasks().insert(id.clone(), slot);
            // Keep the completion contract: a refused spawn still files a
            // notification carrying the child id, like any terminal task.
            self.sink.push(&id);
            // The factory is never built, so over-depth work cannot run.
            let _ = make_work;
            return id;
        }
        let slot = Arc::new(TaskSlot::new(spec.depth, spec.parent.clone()));
        self.lock_tasks().insert(id.clone(), slot.clone());
        let ticket = ChildTicket {
            task_id: id.clone(),
            kind: spec.kind,
            input: spec.input,
            parent_run_id: spec.parent_run_id,
            allowed_tools: spec.allowed_tools,
            depth: spec.depth,
            parent: spec.parent,
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
    ///
    /// The lineage chain runs from the oldest tracked ancestor down to the
    /// queried task itself; a parent id that is unknown or untracked ends
    /// the walk, so the chain is best-effort rather than exact.
    pub fn query(&self, task_id: &str) -> Option<TaskView> {
        let slot = self.lock_tasks().get(task_id).cloned()?;
        let (state, result) = {
            let inner = slot.lock_inner();
            (inner.state, inner.result.clone())
        };
        Some(TaskView {
            state,
            result,
            lineage: self.lineage_of(task_id),
        })
    }

    /// Recorded nesting depth of one task; `None` for unknown ids.
    pub fn depth_of(&self, task_id: &str) -> Option<u8> {
        self.lock_tasks().get(task_id).map(|slot| slot.depth)
    }

    /// Walk parent links root-first, ending with the queried id itself.
    /// Cycle-safe: a repeated id ends the walk instead of looping.
    fn lineage_of(&self, task_id: &str) -> Vec<String> {
        let tasks = self.lock_tasks();
        let mut chain = vec![task_id.to_string()];
        let mut current = task_id.to_string();
        while let Some(slot) = tasks.get(&current) {
            let parent = slot.parent.clone();
            match parent {
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

    /// File an external completion notice on the shared channel.
    ///
    /// Background jobs finish outside any child task, but the parent loop
    /// drains one queue: pushing here reuses that exact sink instead of a
    /// second channel. Drops the oldest entry on overflow, like task pushes.
    pub fn notify_completion(&self, message: String) {
        self.sink.push_text(&message);
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
            depth: 0,
            parent: None,
        }
    }

    fn child_spec(input: &str, depth: u8, parent: Option<&str>) -> ChildSpec {
        ChildSpec {
            kind: ChildKind::Standard,
            input: input.to_string(),
            parent_run_id: "run-1".to_string(),
            allowed_tools: Vec::new(),
            depth,
            parent: parent.map(|id| id.to_string()),
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

    #[test]
    fn max_child_depth_cap_is_three() {
        assert_eq!(MAX_CHILD_DEPTH, 3);
    }

    #[tokio::test]
    async fn spawn_past_max_depth_refuses_with_a_failed_outcome() {
        let rt = ChildRuntime::new();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_flag = ran.clone();
        // Depth past the cap must not run any child work and must not
        // panic: the refusal surfaces as an immediate Failed result.
        let id = rt.spawn_background(
            child_spec("too deep", MAX_CHILD_DEPTH + 1, Some("child-1")),
            move |_ticket| {
                ran_flag.store(true, Ordering::SeqCst);
                async { TaskResult::completed("must never run", 0) }
            },
        );
        wait_until_finished(&rt, &id).await;
        assert!(!ran.load(Ordering::SeqCst));
        let view = rt.query(&id).unwrap();
        assert_eq!(view.state, TaskState::Finished);
        let result = view.result.unwrap();
        assert_eq!(result.status, TaskStatus::Failed);
        assert!(result.summary.contains("max child depth"));
        assert!(result.summary.contains(&MAX_CHILD_DEPTH.to_string()));
        assert_eq!(rt.depth_of(&id), Some(MAX_CHILD_DEPTH + 1));
        // The refused spawn still files a notification with the child id.
        let notes = wait_for_notification(&rt, &id).await;
        assert!(notes.iter().any(|n| n.contains(&id)));
    }

    #[tokio::test]
    async fn spawn_at_max_depth_still_runs() {
        let rt = ChildRuntime::new();
        let id = rt.spawn_background(
            child_spec("edge", MAX_CHILD_DEPTH, None),
            |ticket| async move {
                assert_eq!(ticket.depth, MAX_CHILD_DEPTH);
                TaskResult::completed("done", 0)
            },
        );
        wait_until_finished(&rt, &id).await;
        assert_eq!(
            rt.query(&id).unwrap().result.unwrap().status,
            TaskStatus::Completed
        );
    }

    #[tokio::test]
    async fn query_exposes_lineage_from_ancestor_to_self() {
        let rt = ChildRuntime::new();
        let root = rt.spawn_background(spec("root"), |_ticket| async {
            TaskResult::completed("root done", 0)
        });
        wait_until_finished(&rt, &root).await;
        let root_id = root.clone();
        let middle =
            rt.spawn_background(child_spec("middle", 1, Some(&root)), |ticket| async move {
                assert_eq!(ticket.depth, 1);
                assert_eq!(ticket.parent, Some(root_id.clone()));
                TaskResult::completed("middle done", 0)
            });
        wait_until_finished(&rt, &middle).await;
        let leaf = rt.spawn_background(child_spec("leaf", 2, Some(&middle)), |_ticket| async {
            TaskResult::completed("leaf done", 0)
        });
        wait_until_finished(&rt, &leaf).await;
        assert_eq!(rt.query(&root).unwrap().lineage, vec![root.clone()]);
        assert_eq!(
            rt.query(&middle).unwrap().lineage,
            vec![root.clone(), middle.clone()]
        );
        assert_eq!(
            rt.query(&leaf).unwrap().lineage,
            vec![root.clone(), middle.clone(), leaf.clone()]
        );
        assert_eq!(rt.depth_of(&leaf), Some(2));
        assert!(rt.depth_of("child-999").is_none());
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
    /// Finished tasks beyond the newest bound are reaped oldest-first at
    /// spawn time; running tasks are never touched.
    #[tokio::test]
    async fn finished_tasks_are_reaped_oldest_first() {
        let rt = ChildRuntime::new();
        let total = MAX_FINISHED_TASKS + 16;
        for i in 0..total {
            let id = rt.spawn_background(spec(&format!("task {i}")), |_| async {
                TaskResult::completed("done", 1)
            });
            wait_until_finished(&rt, &id).await;
        }
        let map = rt.lock_tasks();
        assert_eq!(
            map.len(),
            MAX_FINISHED_TASKS,
            "the finished table stays at the bound"
        );
        // The reaped ids are the oldest: child-1 is gone, child-{total} remains.
        assert!(!map.contains_key("child-1"));
        assert!(map.contains_key(&format!("child-{total}")));
    }
}
