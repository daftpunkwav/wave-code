/*!
 * @file ChildTurnService
 * @description Runs full turns as child tasks behind the task seam.
 *
 * Responsibilities:
 * - Spawn child turns through any TurnDriver with fresh conversations.
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

use std::sync::{
    Arc,
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
        self.runtime.spawn_background(
            ChildSpec {
                kind: Self::profile(request.kind),
                // Moved, not cloned: the factory reads both back off the
                // ticket, so the service holds no second copy.
                input: request.input,
                parent_run_id: request.parent_run_id,
                allowed_tools: request.allowed_tools,
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
        )
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
        })
    }

    fn stop(&self, id: &str) -> bool {
        self.runtime.stop(id)
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
            },
        );
        for _ in 0..1000 {
            if service.query(&id).and_then(|info| info.outcome).is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(service.query(&id).expect("task must be tracked").outcome.is_some());
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
            },
        );
        assert!(allowlist.is_allowed(&open, "shell"));
    }
}
