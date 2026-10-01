/*!
 * @file ChildTurnService
 * @description Runs full turns as child tasks behind the task seam.
 *
 * Responsibilities:
 * - Spawn child turns through any TurnDriver with fresh conversations.
 * - Pass depth/parent through so the runtime enforces the depth cap.
 * - Continue finished children with depth + 1 follow-ups and lineage.
 * - Scope stop signals per child via run-scoped interrupt handles, so
 *   stopping one child never disturbs the parent turn or its siblings.
 * - Narrow every child's tool surface: read-only profiles get the
 *   registry's read-only subset, and no child may ever invoke a
 *   child-spawning tool (the depth cap stays meaningful on a shared
 *   registry).
 * - Map runtime outcomes onto the capability-neutral task vocabulary.
 *
 * This module must not depend on: concrete drivers, tools, or models.
 * Stop propagation is best-effort polling by design (see below).
 */

//! Child turns: full RunLoop-equivalent work as background tasks.
//!
//! Stop bridging polls the ticket flag every few milliseconds and flips
//! the child's own run-scoped interrupt (registered with the driver loop
//! under the child's run id). Polling (not events) because the ticket
//! handle is a bare atomic flag; the watcher exits with the turn via a
//! drop guard, so no task leaks past completion — including through a
//! panicking turn. Mid-turn stops therefore land within milliseconds,
//! while pre-start stops skip work entirely via the runtime fast path.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use action_tasks::{TaskInfo, TaskKind, TaskOutcome, TaskRequest, TaskService, TaskState};
use infrastructure_base::InterruptHandle;
use runtime_child::{ChildKind, ChildRuntime, ChildSpec};
use runtime_runner::{RunAllowlist, RunContext, RunInterrupts, StopReason, TurnDriver, TurnInput};
use state_store::{Conversation, HistoryEntry, Role};

/// How often the stop watcher polls the ticket flag.
const STOP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Namespace prefix for child run keys.
///
/// The run-scoped registries ([`RunInterrupts`] and the tool allowlist)
/// are keyed by `RunContext.run_id`, which for parent turns is the
/// caller-controlled submission id. Registering children under their raw
/// `child-N` ids would put both namespaces in one map: a frontend
/// submission id that happens to equal a live child id would be treated
/// as a child turn (keeping the session gates uncleared and inheriting
/// the child's allowlist). Prefixing makes the collision unconstructable.
fn child_run_key(task_id: &str) -> String {
    format!("child/{task_id}")
}

/// Registry-derived tool-surface policy applied to every child spawn.
///
/// Built by the composition root from the fully assembled registry and
/// attached to the service before the actor starts; spawns before that
/// (tests, early callers) fall back to the legacy behavior — an explicit
/// `allowed_tools` restricts, an empty one keeps the full surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildSurface {
    /// Every tool the session registry offers: the base surface for
    /// children spawned without an explicit `allowed_tools`. Native
    /// (non-registry) tools are not part of this snapshot, so children
    /// observe the parent's tasks via the registry `task_output` /
    /// `task_stop` tools rather than the native `child_*` ones.
    pub all: HashSet<String>,
    /// The read-only subset; `ReadOnly` children are narrowed to it.
    pub read_only: HashSet<String>,
    /// Child-spawning surfaces a child may never invoke. Children share
    /// the session registry, so without this subtraction a child could
    /// re-fork `task`/`skill`/workflow spawns and the runtime depth cap
    /// would never see the nested generations.
    pub forbidden: HashSet<String>,
}

/// Drop guard releasing everything one spawned child borrowed for its
/// lifetime: the allowlist entry, the run-scoped interrupt registration,
/// and the stop-watcher exit flag.
///
/// Held across the turn await, so an unwinding turn (panic caught at the
/// child runtime boundary) still tears down instead of leaking a
/// restriction entry or leaving the watcher spinning forever.
struct SpawnTeardown {
    allowlist: RunAllowlist,
    run_interrupts: RunInterrupts,
    run_id: String,
    done: Arc<AtomicBool>,
}

impl Drop for SpawnTeardown {
    fn drop(&mut self) {
        self.allowlist.release(&self.run_id);
        self.run_interrupts.release(&self.run_id);
        self.done.store(true, Ordering::SeqCst);
    }
}

/// Poll the parent ticket's stop flag into the child's own run-scoped
/// interrupt every [`STOP_POLL_INTERVAL`]. The teardown guard flips
/// `done` on every exit path, so no watcher outlives its child.
fn spawn_stop_watcher(
    stop: InterruptHandle,
    child_interrupt: InterruptHandle,
    done: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        while !done.load(Ordering::SeqCst) {
            if stop.is_triggered() {
                child_interrupt.trigger();
                break;
            }
            tokio::time::sleep(STOP_POLL_INTERVAL).await;
        }
    });
}

/// Drive one child turn on a fresh conversation, handing back the
/// transcript and the raw stop reason for the caller to map.
async fn drive_child_turn(
    driver: &Arc<dyn TurnDriver>,
    ctx: &RunContext,
    input: &str,
    system: &str,
) -> (Conversation, StopReason) {
    let mut conv = Conversation::new();
    let outcome = driver
        .drive_turn(
            ctx,
            &mut conv,
            TurnInput {
                text: input,
                images: &ctx.images,
            },
            system,
            &|_| {},
        )
        .await;
    (conv, outcome)
}

/// The summary is the child's final answer, not whatever entry landed
/// last: a round-ceiling or stop-hook exit leaves tool results (user
/// role) at the tail, and a failure should carry the reason, not a
/// transcript tail.
fn summarize_child_run(snapshot: &[HistoryEntry], outcome: &StopReason) -> String {
    match outcome {
        StopReason::Error(reason) => reason.clone(),
        // A ceiling exit whose last round had no final text
        // would render as an empty summary downstream; give
        // it an honest placeholder instead.
        _ => snapshot
            .iter()
            .rev()
            .find(|entry| entry.role == Role::Assistant)
            .map(|entry| entry.text())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "(no final answer text)".to_string()),
    }
}

/// Append the child's transcript to its own journal under the parent
/// session. Best-effort: a subagent's log must never fail the run that
/// spawned it.
fn record_child_journal(
    journal: &ChildJournal,
    task_id: &str,
    input: &str,
    snapshot: &[HistoryEntry],
    outcome: &StopReason,
) {
    let pairs: Vec<(bool, String)> = snapshot
        .iter()
        .map(|entry| (entry.role == Role::Assistant, entry.text()))
        .collect();
    let noop = |text: &str| text.to_string();
    let redact = journal.redact.as_deref().unwrap_or(&noop);
    let _ = state_persistence::sessions::record_child_turn(
        &journal.home,
        &journal.parent,
        task_id,
        input,
        &pairs,
        &format!("{outcome:?}"),
        redact,
    );
}

/// Map the raw stop reason onto the capability-neutral task vocabulary.
fn child_task_status(outcome: &StopReason) -> runtime_child::TaskStatus {
    match outcome {
        // Both ceilings end the child without a fault: the
        // repeat breaker hands back a text summary of the
        // blocker, exactly like the round ceiling.
        StopReason::Completed | StopReason::MaxToolRounds | StopReason::RepeatBreaker => {
            runtime_child::TaskStatus::Completed
        }
        StopReason::Interrupted => runtime_child::TaskStatus::Stopped,
        StopReason::Error(_) => runtime_child::TaskStatus::Failed,
    }
}

/// TaskService over child turns driven by any TurnDriver.
///
/// Concurrency contract: the driver is shared across children. Per-turn
/// state that used to live on drivers (interrupt flags, approval slots)
/// is now scoped per child through run-scoped handles and the per-run
/// tool allowlist, so sharing one loop no longer couples a child's stop
/// signal to the session-wide flag. Drivers with other per-turn mutable
/// state still need one service per driver for parallel children.
pub struct TurnChildService {
    driver: Arc<dyn TurnDriver>,
    runtime: Arc<ChildRuntime>,
    /// System prompt for child turns; set by assembly once the fully
    /// assembled prompt exists (it needs the complete tool catalog).
    system: OnceLock<String>,
    /// Tool-surface policy; set by assembly once the registry is complete.
    surface: OnceLock<ChildSurface>,
    allowlist: RunAllowlist,
    run_interrupts: RunInterrupts,
    /// Spawned requests by id, so `continue_task` can rebuild the parent
    /// profile (kind, run correlation, tool scope) for a follow-up child.
    history: Mutex<HashMap<String, TaskRequest>>,
    /// Where each child task appends its own turn journal
    /// (`sessions/children/<parent>/<child>.jsonl`). `None` when the session
    /// has no home or id; writing is best-effort and never fails a run.
    journal: Option<ChildJournal>,
}

/// Home plus parent session id, the pair naming a child's journal,
/// plus the credential mask every persisted text rides (assembly
/// injects it; `None` keeps bytes verbatim, as tests do).
#[derive(Clone)]
struct ChildJournal {
    home: PathBuf,
    parent: String,
    redact: Option<std::sync::Arc<crate::session::SecretRedactor>>,
}

impl TurnChildService {
    /// Wire a driver, a runtime, the loop's per-run tool allowlist, and
    /// the loop's run-scoped interrupt registry.
    pub fn new(
        driver: Arc<dyn TurnDriver>,
        runtime: Arc<ChildRuntime>,
        allowlist: RunAllowlist,
        run_interrupts: RunInterrupts,
    ) -> Self {
        Self {
            driver,
            runtime,
            system: OnceLock::new(),
            surface: OnceLock::new(),
            allowlist,
            run_interrupts,
            history: Mutex::new(HashMap::new()),
            journal: None,
        }
    }

    /// Record every child task's turns to its own journal under the parent
    /// session's directory, so a long session can explain what a subagent
    /// actually did instead of keeping only its returned summary.
    pub fn with_child_journal(mut self, home: PathBuf, parent: String) -> Self {
        self.journal = Some(ChildJournal {
            home,
            parent,
            redact: None,
        });
        self
    }

    /// Mask credentials out of the child journals' persisted text; the
    /// closure comes from the composition root's secret store.
    pub fn with_journal_redaction(
        mut self,
        redact: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) -> Self {
        match &mut self.journal {
            Some(journal) => journal.redact = Some(redact),
            None => self.journal = None, // nothing to journal: no gate either
        }
        self
    }

    /// Attach the child system prompt (assembly-time; first set wins).
    pub fn set_system(&self, system: String) {
        let _ = self.system.set(system);
    }

    /// Attach the tool-surface policy (assembly-time; first set wins).
    pub fn set_surface(&self, surface: ChildSurface) {
        let _ = self.surface.set(surface);
    }

    /// Concrete tool surface for one spawn: the requested list narrowed
    /// by the surface policy, or the policy's full-minus-forbidden set
    /// for open spawns. `None` keeps the legacy unrestricted surface
    /// (no policy attached and no explicit list).
    fn effective_surface(&self, request: &TaskRequest) -> Option<HashSet<String>> {
        let Some(surface) = self.surface.get() else {
            return if request.allowed_tools.is_empty() {
                None
            } else {
                Some(request.allowed_tools.iter().cloned().collect())
            };
        };
        let mut base: HashSet<String> = if request.allowed_tools.is_empty() {
            surface.all.clone()
        } else {
            request.allowed_tools.iter().cloned().collect()
        };
        for name in &surface.forbidden {
            base.remove(name);
        }
        if request.kind == TaskKind::ReadOnly {
            base.retain(|name| surface.read_only.contains(name));
        }
        Some(base)
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
        let system = self.system.get().cloned().unwrap_or_default();
        let allowlist = self.allowlist.clone();
        let run_interrupts = self.run_interrupts.clone();
        // The computed surface rides the future as Option: Some (even
        // empty) restricts, None means the legacy unrestricted surface.
        let effective = self.effective_surface(&request);
        let done = Arc::new(AtomicBool::new(false));
        let journal_sink = self.journal.clone();
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
                // the ticket while history keeps the parent profile. The
                // effective surface (sorted for deterministic specs) is
                // what the child actually runs with.
                input: request.input.clone(),
                parent_run_id: request.parent_run_id.clone(),
                allowed_tools: match &effective {
                    Some(set) => {
                        let mut names: Vec<String> = set.iter().cloned().collect();
                        names.sort();
                        names
                    }
                    None => Vec::new(),
                },
                depth: request.depth,
                parent: request.parent.clone(),
            },
            move |ticket| async move {
                let run_id = child_run_key(&ticket.task_id);
                // Stop bridge: poll the ticket flag into this child's own
                // run-scoped interrupt. The loop consumes the registration
                // at turn start, so the bridge never touches the
                // session-wide flag other children and the parent share.
                let child_interrupt = InterruptHandle::new();
                run_interrupts.register(&run_id, child_interrupt.clone());
                spawn_stop_watcher(ticket.stop.clone(), child_interrupt, done.clone());
                let ctx = RunContext {
                    run_id: run_id.clone(),
                    submission_id: ticket.task_id.clone(),
                    input: ticket.input.clone(),
                    images: Vec::new(),
                };
                // Restrict before the first sample so denied tools are
                // hidden from the catalog and refused at execution; the
                // teardown guard releases on every exit path (including
                // unwinds) so finished runs leave no entries.
                if let Some(set) = effective {
                    allowlist.restrict(&run_id, set);
                }
                let teardown = SpawnTeardown {
                    allowlist: allowlist.clone(),
                    run_interrupts,
                    run_id,
                    done,
                };
                let (conv, outcome) =
                    drive_child_turn(&driver, &ctx, &ticket.input, &system).await;
                drop(teardown);
                let snapshot = conv.snapshot();
                let summary = summarize_child_run(&snapshot, &outcome);
                if let Some(journal) = &journal_sink {
                    record_child_journal(
                        journal,
                        &ticket.task_id,
                        &ticket.input,
                        &snapshot,
                        &outcome,
                    );
                }
                runtime_child::TaskResult {
                    status: child_task_status(&outcome),
                    summary,
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

    /// Spawn a depth + 1 follow-up of a finished task.
    ///
    /// Rebuilds the parent request (kind, run correlation, tool scope)
    /// with the follow-up input, `depth + 1`, and `parent` set to `id`,
    /// so the lineage chain extends by one. Unknown ids and tasks still
    /// running return `None` (a follow-up is defined against the parent's
    /// recorded outcome; a running task can only be stopped). A follow-up
    /// past the runtime cap still returns an id, but the runtime refuses
    /// it with an explicit Failed outcome visible in query, matching
    /// direct over-depth spawns.
    fn continue_task(&self, id: &str, followup_input: String) -> Option<String> {
        let parent = self
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()?;
        // Only finished tasks continue: the tool contract promises a
        // follow-up against a recorded outcome, so a still-running task
        // must not silently fork a sibling generation.
        if self.query(id)?.state != TaskState::Finished {
            return None;
        }
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
    use runtime_runner::{HookPoint, TurnDriver};
    use state_store::CompactTrigger;
    use wavecode_wire::Event;

    struct EchoDriver;

    #[async_trait::async_trait]
    impl TurnDriver for EchoDriver {
        async fn drive_turn(
            &self,
            _ctx: &RunContext,
            conv: &mut Conversation,
            input: TurnInput<'_>,
            _system: &str,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> StopReason {
            conv.push(Role::User, input.text);
            conv.push(Role::Assistant, format!("child saw {}", input.text));
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

    /// Driver that lingers in the turn so a spawned task is observably
    /// Running (stopped via the cooperative stop flag).
    struct LingeringDriver;

    #[async_trait::async_trait]
    impl TurnDriver for LingeringDriver {
        async fn drive_turn(
            &self,
            _ctx: &RunContext,
            _conv: &mut Conversation,
            _input: TurnInput<'_>,
            _system: &str,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> StopReason {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
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

    /// Driver exposing a session-wide interrupt handle, so tests can
    /// prove a child stop no longer flips it.
    struct WatchDriver {
        interrupt: InterruptHandle,
    }

    #[async_trait::async_trait]
    impl TurnDriver for WatchDriver {
        async fn drive_turn(
            &self,
            _ctx: &RunContext,
            conv: &mut Conversation,
            _input: TurnInput<'_>,
            _system: &str,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> StopReason {
            // Linger until the session-wide flag flips, bounded so a
            // regression cannot hang the test for the full 30 seconds.
            for _ in 0..200 {
                if self.interrupt.is_triggered() {
                    conv.push(Role::Assistant, "interrupted".to_string());
                    return StopReason::Interrupted;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            conv.push(Role::Assistant, "finished".to_string());
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

    fn service_with(driver: Arc<dyn TurnDriver>, allowlist: &RunAllowlist) -> TurnChildService {
        TurnChildService::new(
            driver,
            Arc::new(ChildRuntime::new()),
            allowlist.clone(),
            RunInterrupts::default(),
        )
    }

    /// Surface policy with a three-tool catalog for scope tests.
    fn test_surface() -> ChildSurface {
        ChildSurface {
            all: ["read_file", "shell", "task"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            read_only: ["read_file"].iter().map(|s| s.to_string()).collect(),
            forbidden: ["task", "skill"].iter().map(|s| s.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn child_turns_complete_through_the_task_seam() {
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(EchoDriver), &allowlist);
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
        let service = service_with(Arc::new(EchoDriver), &allowlist);
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

    #[tokio::test]
    async fn read_only_children_are_narrowed_to_the_read_only_subset() {
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(LingeringDriver), &allowlist);
        service.set_surface(test_surface());
        let id = TaskService::spawn(
            &service,
            TaskRequest {
                kind: TaskKind::ReadOnly,
                input: "explore".to_string(),
                parent_run_id: "run-1".to_string(),
                allowed_tools: Vec::new(),
                depth: 0,
                parent: None,
            },
        );
        // While the child runs, its surface is the read-only subset.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(service.query(&id).unwrap().state, TaskState::Running);
        assert!(allowlist.is_allowed(&child_run_key(&id), "read_file"));
        assert!(
            !allowlist.is_allowed(&child_run_key(&id), "shell"),
            "write tools are gone"
        );
        assert!(
            !allowlist.is_allowed(&child_run_key(&id), "task"),
            "spawn tools are gone"
        );
        assert!(service.stop(&id));
    }

    #[tokio::test]
    async fn explicit_children_allowlists_lose_the_spawn_tools() {
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(LingeringDriver), &allowlist);
        service.set_surface(test_surface());
        let id = TaskService::spawn(
            &service,
            TaskRequest {
                kind: TaskKind::Standard,
                input: "work".to_string(),
                parent_run_id: "run-1".to_string(),
                allowed_tools: vec!["task".to_string(), "shell".to_string()],
                depth: 0,
                parent: None,
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(service.query(&id).unwrap().state, TaskState::Running);
        // The explicit list survives minus the forbidden surfaces, so a
        // child can never re-fork regardless of what it requests.
        assert!(allowlist.is_allowed(&child_run_key(&id), "shell"));
        assert!(!allowlist.is_allowed(&child_run_key(&id), "task"));
        assert!(service.stop(&id));
    }

    #[tokio::test]
    async fn child_stops_never_flip_the_driver_wide_interrupt() {
        let allowlist = RunAllowlist::default();
        let interrupt = InterruptHandle::new();
        let service = service_with(
            Arc::new(WatchDriver {
                interrupt: interrupt.clone(),
            }),
            &allowlist,
        );
        let id = TaskService::spawn(&service, top_level("long work"));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(service.stop(&id));
        // WatchDriver ignores scoped handles (only the real RunLoop reads
        // them), so it runs out its bounded linger; the ticket flag still
        // forces the terminal state to Stopped.
        let info = wait_for_outcome(&service, &id).await;
        assert_eq!(info.state, TaskState::Finished);
        // The scoped stop bridged into the child's own handle: the
        // driver-wide flag the parent turn and siblings share is
        // untouched.
        assert!(
            !interrupt.is_triggered(),
            "a child stop must not interrupt the session-wide flag"
        );
        assert!(matches!(info.outcome, Some(TaskOutcome::Stopped)));
    }

    #[tokio::test]
    async fn failing_children_report_the_error_reason_as_summary() {
        struct ErrorDriver;
        #[async_trait::async_trait]
        impl TurnDriver for ErrorDriver {
            async fn drive_turn(
                &self,
                _ctx: &RunContext,
                _conv: &mut Conversation,
                _input: TurnInput<'_>,
                _system: &str,
                _on_event: &(dyn Fn(Event) + Send + Sync),
            ) -> StopReason {
                StopReason::Error("provider exploded".to_string())
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
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(ErrorDriver), &allowlist);
        let id = TaskService::spawn(&service, top_level("doomed"));
        let info = wait_for_outcome(&service, &id).await;
        assert!(matches!(
            info.outcome,
            Some(TaskOutcome::Failed { ref reason }) if reason.contains("provider exploded")
        ));
    }

    #[tokio::test]
    async fn summary_prefers_the_final_assistant_entry_over_the_tail() {
        struct AssistantThenToolDriver;
        #[async_trait::async_trait]
        impl TurnDriver for AssistantThenToolDriver {
            async fn drive_turn(
                &self,
                _ctx: &RunContext,
                conv: &mut Conversation,
                _input: TurnInput<'_>,
                _system: &str,
                _on_event: &(dyn Fn(Event) + Send + Sync),
            ) -> StopReason {
                // The shape a round-ceiling / stop-hook exit leaves: the
                // assistant answered, then a tool result (user role) is
                // the literal tail of the conversation.
                conv.push(Role::Assistant, "final answer".to_string());
                conv.push(Role::User, "[c1] ok: raw tool output".to_string());
                StopReason::MaxToolRounds
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
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(AssistantThenToolDriver), &allowlist);
        let id = TaskService::spawn(&service, top_level("work"));
        let info = wait_for_outcome(&service, &id).await;
        assert!(matches!(
            info.outcome,
            Some(TaskOutcome::Completed { ref summary }) if summary == "final answer"
        ));
    }

    #[tokio::test]
    async fn ceiling_exit_without_final_text_gets_a_placeholder() {
        struct SilentCeilingDriver;
        #[async_trait::async_trait]
        impl TurnDriver for SilentCeilingDriver {
            async fn drive_turn(
                &self,
                _ctx: &RunContext,
                _conv: &mut Conversation,
                _input: TurnInput<'_>,
                _system: &str,
                _on_event: &(dyn Fn(Event) + Send + Sync),
            ) -> StopReason {
                StopReason::MaxToolRounds
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
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(SilentCeilingDriver), &allowlist);
        let id = TaskService::spawn(&service, top_level("silent work"));
        let info = wait_for_outcome(&service, &id).await;
        assert!(matches!(
            info.outcome,
            Some(TaskOutcome::Completed { ref summary }) if summary == "(no final answer text)"
        ));
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
            // A real sleep, so a lingering child's bounded wait (seconds)
            // fits the poll budget.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("task {id} produced no outcome in time");
    }

    #[tokio::test]
    async fn continue_task_extends_the_lineage_chain() {
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(EchoDriver), &allowlist);
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
    async fn continue_task_refuses_still_running_tasks() {
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(LingeringDriver), &allowlist);
        let id = TaskService::spawn(&service, top_level("long work"));
        // The task stays Running for the whole 30s driver sleep, so the
        // query sees Running deterministically; continuation must refuse
        // instead of forking a sibling generation.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(service.query(&id).unwrap().state, TaskState::Running);
        assert_eq!(service.continue_task(&id, "x".to_string()), None);
        // Cleanup: stop the parked child so the test ends promptly.
        assert!(service.stop(&id));
    }

    #[tokio::test]
    async fn over_depth_spawns_fail_open_through_the_task_seam() {
        let allowlist = RunAllowlist::default();
        let service = service_with(Arc::new(EchoDriver), &allowlist);
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
