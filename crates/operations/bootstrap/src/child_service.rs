/*!
 * @file ChildTurnService
 * @description Runs full turns as child tasks behind the task seam.
 *
 * Responsibilities:
 * - Spawn child turns through any TurnDriver with fresh conversations.
 * - Pass depth/parent through so the runtime enforces the depth cap.
 * - Continue finished children with depth + 1 follow-ups and lineage.
 * - Bridge ticket stop signals into turn interrupts with a watcher.
 * - Map runtime outcomes onto the capability-neutral task vocabulary.
 *
 * This module must not depend on: concrete drivers, tools, or models.
 * Stop propagation is best-effort polling by design (see below).
 */

//! Child turns: full RunLoop-equivalent work as background tasks.
//!
//! Stop bridging polls the ticket flag every few milliseconds and flips
//! the turn interrupt. Polling (not events) because the ticket handle is
//! a bare atomic flag; the watcher exits with the turn, so no task leaks
//! past completion. Mid-turn stops therefore land within milliseconds,
//! while pre-start stops skip work entirely via the runtime fast path.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use action_tasks::{TaskInfo, TaskKind, TaskOutcome, TaskRequest, TaskService, TaskState};
use runtime_child::{ChildKind, ChildRuntime, ChildSpec};
use runtime_runner::{RunAllowlist, RunContext, StopReason, TurnDriver};
use state_store::Conversation;

/// How often the stop watcher polls the ticket flag.
const STOP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// TaskService over child turns driven by any TurnDriver.
///
/// Concurrency contract: the driver is shared across children, so drivers
/// with per-turn mutable state (interrupt flags, approval slots) need one
/// service per driver for parallel children. Stateless or externally
/// synchronized drivers may share freely.
pub struct TurnChildService {
    driver: Arc<dyn TurnDriver>,
    runtime: Arc<ChildRuntime>,
    system: String,
    allowlist: RunAllowlist,
    /// Spawned requests by id, so `continue_task` can rebuild the parent
    /// profile (kind, run correlation, tool scope) for a follow-up child.
    history: Mutex<HashMap<String, TaskRequest>>,
}

impl TurnChildService {
    /// Wire a driver, a runtime, the system prompt, and the loop's
    /// per-run tool allowlist for fork-scoped `allowed-tools`.
    pub fn new(
        driver: Arc<dyn TurnDriver>,
        runtime: Arc<ChildRuntime>,
        system: String,
        allowlist: RunAllowlist,
    ) -> Self {
        Self {
            driver,
            runtime,
            system,
            allowlist,
            history: Mutex::new(HashMap::new()),
        }
    }

    /// Map requested kinds onto child capability profiles.
    fn profile(kind: TaskKind) -> ChildKind {
        match kind {
            TaskKind::Standard => ChildKind::Standard,
            TaskKind::ReadOnly => ChildKind::ReadOnly,
        }
    }
}

impl TaskService for TurnChildService {
    fn spawn(&self, request: TaskRequest) -> String {
        let driver = self.driver.clone();
        let system = self.system.clone();
        let allowlist = self.allowlist.clone();
        let turn_interrupt = driver.interrupt_handle();
        let done = Arc::new(AtomicBool::new(false));
        let watcher_done = done.clone();
        // Depth/parent flow straight into the runtime spec: the runtime
        // owns cap enforcement, refusing depth past its max with an
        // explicit Failed outcome (no panic, visible through query), so
        // this spawn signature stays non-breaking by design. The request
        // is cloned (not moved) so the history map can rebuild the parent
        // profile for `continue_task` follow-ups.
        let allocated = self.runtime.spawn_background(
            ChildSpec {
                kind: Self::profile(request.kind),
                // Cloned so the factory still reads everything back off
                // the ticket while history keeps the parent profile.
                input: request.input.clone(),
                parent_run_id: request.parent_run_id.clone(),
                allowed_tools: request.allowed_tools.clone(),
                depth: request.depth,
                parent: request.parent.clone(),
            },
            move |ticket| async move {
                // Stop bridge: poll the ticket flag into the driver's turn
                // interrupt. Drivers without an exposed handle (None) keep
                // pre-start stops only; the watcher still exits cleanly.
                if let Some(handle) = turn_interrupt {
                    let stop = ticket.stop.clone();
                    tokio::spawn(async move {
                        while !watcher_done.load(Ordering::SeqCst) {
                            if stop.is_triggered() {
                                handle.trigger();
                                break;
                            }
                            tokio::time::sleep(STOP_POLL_INTERVAL).await;
                        }
                    });
                } else {
                    watcher_done.store(true, Ordering::SeqCst);
                }
                let ctx = RunContext {
                    run_id: ticket.task_id.clone(),
                    submission_id: ticket.task_id.clone(),
                    input: ticket.input.clone(),
                };
                // Restrict before the first sample so denied tools are
                // hidden from the catalog and refused at execution;
                // release on teardown so finished runs leave no entries.
                if !ticket.allowed_tools.is_empty() {
                    allowlist.restrict(
                        &ticket.task_id,
                        ticket.allowed_tools.iter().cloned().collect(),
                    );
                }
                let mut conv = Conversation::new();
                let outcome = driver
                    .drive_turn(&ctx, &mut conv, &ticket.input, &system, &|_| {})
                    .await;
                allowlist.release(&ticket.task_id);
                done.store(true, Ordering::SeqCst);
                runtime_child::TaskResult {
                    status: match &outcome {
                        StopReason::Completed | StopReason::MaxToolRounds => {
                            runtime_child::TaskStatus::Completed
                        }
                        StopReason::Interrupted => runtime_child::TaskStatus::Stopped,
                        StopReason::Error(_) => runtime_child::TaskStatus::Failed,
                    },
                    summary: conv
                        .snapshot()
                        .last()
                        .map(|entry| entry.text.clone())
                        .unwrap_or_default(),
                    output_tokens: conv.usage_carry().output_tokens,
                }
            },
        );
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(allocated.clone(), request);
        allocated
    }

    fn query(&self, id: &str) -> Option<TaskInfo> {
        let view = self.runtime.query(id)?;
        let outcome = view.result.map(|result| match result.status {
            runtime_child::TaskStatus::Completed => TaskOutcome::Completed {
                summary: result.summary,
            },
            runtime_child::TaskStatus::Failed => TaskOutcome::Failed {
                reason: result.summary,
            },
            runtime_child::TaskStatus::Stopped => TaskOutcome::Stopped,
        });
        Some(TaskInfo {
            state: match view.state {
                runtime_child::TaskState::Running => TaskState::Running,
                runtime_child::TaskState::Finished => TaskState::Finished,
            },
            outcome,
            lineage: view.lineage,
        })
    }

    fn stop(&self, id: &str) -> bool {
        self.runtime.stop(id)
    }

    /// Spawn a depth + 1 follow-up of a tracked task.
    ///
    /// Rebuilds the parent request (kind, run correlation, tool scope)
    /// with the follow-up input, `depth + 1`, and `parent` set to `id`,
    /// so the lineage chain extends by one. Unknown ids return `None`.
    /// A follow-up past the runtime cap still returns an id, but the
    /// runtime refuses it with an explicit Failed outcome visible in
    /// query, matching direct over-depth spawns.
    fn continue_task(&self, id: &str, followup_input: String) -> Option<String> {
        let parent = self
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()?;
        Some(self.spawn(TaskRequest {
            kind: parent.kind,
            input: followup_input,
            parent_run_id: parent.parent_run_id,
            allowed_tools: parent.allowed_tools,
            depth: parent.depth + 1,
            parent: Some(id.to_string()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use operations_wire::Event;
    use runtime_runner::{HookPoint, TurnDriver};
    use state_store::{CompactTrigger, Role};

    struct EchoDriver;

    #[async_trait::async_trait]
    impl TurnDriver for EchoDriver {
        async fn drive_turn(
            &self,
            _ctx: &RunContext,
            conv: &mut Conversation,
            input: &str,
            _system: &str,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> StopReason {
            conv.push(Role::User, input);
            conv.push(Role::Assistant, format!("child saw {input}"));
            StopReason::Completed
        }

        async fn drive_compact(
            &self,
            _conv: &mut Conversation,
            _trigger: CompactTrigger,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> Result<(), String> {
            Ok(())
        }

        async fn drive_hook(
            &self,
            _point: HookPoint,
            _payload: &str,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn child_turns_complete_through_the_task_seam() {
        let allowlist = RunAllowlist::default();
        let service = TurnChildService::new(
            Arc::new(EchoDriver),
            Arc::new(ChildRuntime::new()),
            "sys".to_string(),
            allowlist.clone(),
        );
        let id = TaskService::spawn(
            &service,
            TaskRequest {
                kind: TaskKind::ReadOnly,
                input: "summarize this".to_string(),
                parent_run_id: "run-1".to_string(),
                allowed_tools: Vec::new(),
                depth: 0,
                parent: None,
            },
        );
        for _ in 0..1000 {
            if matches!(
                service.query(&id).and_then(|info| info.outcome),
                Some(TaskOutcome::Completed { .. })
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
        let info = service.query(&id).expect("task must be tracked");
        assert!(matches!(
            info.outcome,
            Some(TaskOutcome::Completed { ref summary }) if summary.contains("child saw")
        ));
        assert!(!TaskService::stop(&service, "child-999"));
    }

    #[tokio::test]
    async fn restricted_children_release_their_allowlist_entry() {
        let allowlist = RunAllowlist::default();
        let service = TurnChildService::new(
            Arc::new(EchoDriver),
            Arc::new(ChildRuntime::new()),
            "sys".to_string(),
            allowlist.clone(),
        );
        let id = TaskService::spawn(
            &service,
            TaskRequest {
                kind: TaskKind::Standard,
                input: "locked down".to_string(),
                parent_run_id: "run-1".to_string(),
                allowed_tools: vec!["read_file".to_string()],
                depth: 0,
                parent: None,
            },
        );
        for _ in 0..1000 {
            if service.query(&id).and_then(|info| info.outcome).is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            service
                .query(&id)
                .expect("task must be tracked")
                .outcome
                .is_some()
        );
        // Teardown releases the restriction: no entry outlives the run.
        assert!(allowlist.is_allowed(&id, "read_file"));
        assert!(allowlist.is_allowed(&id, "shell"));
        // Unrestricted spawns never register in the first place.
        let open = TaskService::spawn(
            &service,
            TaskRequest {
                kind: TaskKind::Standard,
                input: "open".to_string(),
                parent_run_id: "run-1".to_string(),
                allowed_tools: Vec::new(),
                depth: 0,
                parent: None,
            },
        );
        assert!(allowlist.is_allowed(&open, "shell"));
    }

    fn top_level(input: &str) -> TaskRequest {
        TaskRequest {
            kind: TaskKind::ReadOnly,
            input: input.to_string(),
            parent_run_id: "run-1".to_string(),
            allowed_tools: Vec::new(),
            depth: 0,
            parent: None,
        }
    }

    async fn wait_for_outcome(service: &TurnChildService, id: &str) -> TaskInfo {
        for _ in 0..1000 {
            if let Some(info) = service.query(id)
                && info.outcome.is_some()
            {
                return info;
            }
            tokio::task::yield_now().await;
        }
        panic!("task {id} produced no outcome in time");
    }

    #[tokio::test]
    async fn continue_task_extends_the_lineage_chain() {
        let service = TurnChildService::new(
            Arc::new(EchoDriver),
            Arc::new(ChildRuntime::new()),
            "sys".to_string(),
            RunAllowlist::default(),
        );
        let root = TaskService::spawn(&service, top_level("root work"));
        wait_for_outcome(&service, &root).await;
        // Unknown ids continue nothing.
        assert_eq!(service.continue_task("child-999", "x".to_string()), None);
        let middle = service
            .continue_task(&root, "follow up".to_string())
            .expect("known task continues");
        assert_ne!(middle, root);
        let middle_info = wait_for_outcome(&service, &middle).await;
        assert!(matches!(
            middle_info.outcome,
            Some(TaskOutcome::Completed { .. })
        ));
        assert_eq!(middle_info.lineage, vec![root.clone(), middle.clone()]);
        let leaf = service
            .continue_task(&middle, "follow up again".to_string())
            .expect("middle continues");
        let leaf_info = wait_for_outcome(&service, &leaf).await;
        assert_eq!(
            leaf_info.lineage,
            vec![root.clone(), middle.clone(), leaf.clone()]
        );
        // Root lineage stays a single-element chain.
        assert_eq!(service.query(&root).unwrap().lineage, vec![root.clone()]);
    }

    #[tokio::test]
    async fn over_depth_spawns_fail_open_through_the_task_seam() {
        let service = TurnChildService::new(
            Arc::new(EchoDriver),
            Arc::new(ChildRuntime::new()),
            "sys".to_string(),
            RunAllowlist::default(),
        );
        // Past the runtime cap: tracked, immediately Finished, Failed
        // naming the cap, no panic, no driver work.
        let id = TaskService::spawn(
            &service,
            TaskRequest {
                depth: runtime_child::MAX_CHILD_DEPTH + 1,
                ..top_level("too deep")
            },
        );
        let info = wait_for_outcome(&service, &id).await;
        assert_eq!(info.state, TaskState::Finished);
        assert!(matches!(
            info.outcome,
            Some(TaskOutcome::Failed { ref reason }) if reason.contains("max child depth")
        ));
    }
}
