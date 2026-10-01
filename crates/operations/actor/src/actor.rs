/*!
 * @file SessionActor
 * @description Serial turn driver with queueing and control routing.
 *
 * Responsibilities:
 * - Serialize UserInput turns while routing control ops immediately.
 * - Bound the pending queue with explicit, recoverable rejections.
 * - Inject child completion notices and forward approval decisions, warning on late ones.
 *
 * This module must not depend on: concrete tools, policy, hooks, models,
 * memory, skills, frontends, or any capability implementation.
 */

//! Session actor implementation.
//!
//! Queueing exists because compaction rewrites history and a new input
//! starts a new turn; running either mid-turn would corrupt the active
//! snapshot. Control operations (interrupt, approval, shutdown) never
//! queue and are never blocked by a full queue.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;

use infrastructure_base::{
    CONTROL_CHANNEL_CAP, EVENT_CHANNEL_CAP, InterruptHandle, PENDING_QUEUE_CAP, SHUTDOWN_DRAIN,
};
use runtime_child::ChildRuntime;
use runtime_runner::{HookPoint, RunContext, StopReason, TurnDriver, TurnInput};
use safety_gate::{ApprovalDecision, ApprovalGate, QuestionGate};
use state_checkpoint::CheckpointPolicy;
use state_store::{CompactTrigger, Conversation, Role, Usage};
use tokio::sync::mpsc;
use wavecode_wire::{Event, EventMsg, Op, Submission, WireDecision};

use crate::client::ActorClient;
use crate::durable::{
    DurabilityConfig, TurnDurability, checkpoint_turn, list_resume_labels, render_snapshot,
    turn_label,
};

/// Synthetic correlation id for session lifecycle warnings, which belong
/// to no submission. Reserved across the wire (see `wavecode_wire::Event`):
/// frontends match it by value, so changing the spelling is a breaking
/// protocol change.
const LIFECYCLE_ID: &str = "session-lifecycle";

/// Serial session driver, generic over the turn-driving seam.
pub struct SessionActor<D> {
    driver: D,
    conv: Conversation,
    children: Arc<ChildRuntime>,
    approvals: Arc<ApprovalGate>,
    questions: Arc<QuestionGate>,
    interrupt: InterruptHandle,
    system: String,
    submit_rx: mpsc::Receiver<Submission>,
    event_tx: mpsc::Sender<Event>,
    pending: VecDeque<Submission>,
    durability: Option<TurnDurability>,
}

impl<D> SessionActor<D>
where
    D: TurnDriver + Send + 'static,
{
    /// Spawn the actor task and return the client handle.
    ///
    /// Must be called inside a tokio runtime. The caller supplies every
    /// seam implementation; this crate only routes and drives.
    pub fn spawn(
        driver: D,
        conv: Conversation,
        children: Arc<ChildRuntime>,
        approvals: Arc<ApprovalGate>,
        questions: Arc<QuestionGate>,
        interrupt: InterruptHandle,
        system: String,
    ) -> ActorClient {
        Self::spawn_inner(
            driver, conv, children, approvals, questions, interrupt, system, None,
        )
    }

    /// Spawn with durable turn checkpoints.
    ///
    /// Before each turn-driving path acts, the actor saves the pre-turn
    /// snapshot to `store` plus an fsynced `<root>/turn-N.json` copy, gated
    /// by `policy`. On startup the actor names the newest durable label so
    /// the frontend can offer resume-from-checkpoint.
    // The parameters mirror the spawned Actor's own fields one-to-one;
    // grouping them into a config struct would only rename the list.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_durability(
        driver: D,
        conv: Conversation,
        children: Arc<ChildRuntime>,
        approvals: Arc<ApprovalGate>,
        questions: Arc<QuestionGate>,
        interrupt: InterruptHandle,
        system: String,
        durability: DurabilityConfig,
    ) -> ActorClient {
        Self::spawn_inner(
            driver,
            conv,
            children,
            approvals,
            questions,
            interrupt,
            system,
            Some(TurnDurability::from(durability)),
        )
    }

    // Same parameter bag as `spawn_with_durability` above.
    #[allow(clippy::too_many_arguments)]
    fn spawn_inner(
        driver: D,
        conv: Conversation,
        children: Arc<ChildRuntime>,
        approvals: Arc<ApprovalGate>,
        questions: Arc<QuestionGate>,
        interrupt: InterruptHandle,
        system: String,
        durability: Option<TurnDurability>,
    ) -> ActorClient {
        let (submit_tx, submit_rx) = mpsc::channel(CONTROL_CHANNEL_CAP);
        // Bounded event channel: a stalled frontend must not turn a
        // heavy streaming turn into unbounded process memory. The
        // forwarding sinks cannot await backpressure (they are
        // synchronous driver callbacks), so a full buffer drops events
        // — see [`try_send_event`] for the drop policy.
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let client_interrupt = interrupt.clone();
        // The actor does not own the driver loop; it only holds the driver
        // handle. Mid-turn steering rides the loop's shared inbox handle
        // captured here (see ActorClient::steer), never the submit channel,
        // so no wire Op is involved.
        let inbox = driver.inbox_handle();
        let handle = tokio::spawn(
            Self {
                driver,
                conv,
                children,
                approvals,
                questions,
                interrupt,
                system,
                submit_rx,
                event_tx,
                pending: VecDeque::new(),
                durability,
            }
            .run(),
        );
        ActorClient::new(submit_tx, event_rx, client_interrupt, inbox, handle)
    }

    /// Actor main loop: serve queued submissions first, then the channel.
    async fn run(self) {
        // Destructure once so the turn future and the routing arm borrow
        // disjoint locals instead of fighting over `self`.
        let Self {
            driver,
            mut conv,
            children,
            approvals,
            questions,
            interrupt,
            system,
            mut submit_rx,
            event_tx,
            mut pending,
            mut durability,
        } = self;

        run_lifecycle_hook(&driver, &event_tx, HookPoint::SessionStart).await;

        // Offer resume-from-checkpoint: when durable labels exist, name
        // the newest so the frontend can restore it.
        if let Some(dur) = durability.as_ref() {
            let labels = list_resume_labels(&dur.root);
            if let Some(newest) = labels.last() {
                try_send_event(
                    &event_tx,
                    Event {
                        id: LIFECYCLE_ID.to_string(),
                        msg: EventMsg::Warning {
                            message: format!(
                                "resume available from checkpoint {newest} ({} saved)",
                                labels.len()
                            ),
                        },
                    },
                );
            }
        }
        let mut turn_seq: u64 = 0;

        loop {
            let sub = match pending.pop_front() {
                Some(sub) => Some(sub),
                None => submit_rx.recv().await,
            };
            let Some(sub) = sub else {
                // All clients dropped: end the session best-effort.
                end_session_lifecycle(&driver, &event_tx, &conv).await;
                return;
            };
            match sub.op {
                Op::UserInput { text, images } => {
                    // Child completions re-enter as user messages before
                    // the turn snapshot is taken.
                    for note in children.drain_notifications() {
                        conv.push(Role::User, note);
                    }
                    // Persist-then-act: the pre-turn snapshot is durable
                    // before the driver broadcasts or executes anything.
                    checkpoint_pre_turn(
                        &mut durability,
                        |policy| policy.checkpoint_before_model_request,
                        &mut turn_seq,
                        &conv,
                        &event_tx,
                        &sub.id,
                    )
                    .await;
                    let ctx = RunContext {
                        run_id: sub.id.clone(),
                        submission_id: sub.id.clone(),
                        input: text.clone(),
                        images,
                    };
                    let sink = forwarding_sink(&event_tx);
                    let op = async {
                        let _ = driver
                            .drive_turn(
                                &ctx,
                                &mut conv,
                                TurnInput {
                                    text: &ctx.input,
                                    images: &ctx.images,
                                },
                                &system,
                                &sink,
                            )
                            .await;
                    };
                    if supervised(
                        op,
                        &mut submit_rx,
                        &mut pending,
                        &event_tx,
                        &interrupt,
                        &approvals,
                        &questions,
                    )
                    .await
                    {
                        end_session_lifecycle(&driver, &event_tx, &conv).await;
                        return;
                    }
                }
                // Idle interrupt with no turn running: nothing to stop.
                Op::Interrupt => {}
                Op::ExecApproval { call_id, decision } => {
                    if !approvals.decide(&call_id, map_decision(decision)) {
                        // No parked waiter: the turn ended or never asked.
                        warn_late_decision(&event_tx, &sub.id, &call_id);
                    }
                }
                Op::QuestionAnswer { call_id, answer } => {
                    if !questions.answer(&call_id, answer) {
                        // No parked waiter: the turn ended or never asked.
                        warn_late_decision(&event_tx, &sub.id, &call_id);
                    }
                }
                Op::Compact { instruction } => {
                    let id = sub.id.clone();
                    // Compaction rewrites shared history (a state side
                    // effect), so it honors the side-effect flag.
                    checkpoint_pre_turn(
                        &mut durability,
                        |policy| policy.before_tool_side_effect,
                        &mut turn_seq,
                        &conv,
                        &event_tx,
                        &sub.id,
                    )
                    .await;
                    let tx = event_tx.clone();
                    let sink = move |event: Event| {
                        try_send_event(
                            &tx,
                            Event {
                                id: id.clone(),
                                msg: event.msg,
                            },
                        );
                    };
                    let op = async {
                        if let Err(cause) = driver
                            .drive_compact(&mut conv, CompactTrigger::Manual { instruction }, &sink)
                            .await
                        {
                            sink(Event {
                                id: sub.id.clone(),
                                msg: EventMsg::Error {
                                    message: cause,
                                    recoverable: true,
                                    code: Some("compact.failed".to_string()),
                                },
                            });
                        }
                    };
                    if supervised(
                        op,
                        &mut submit_rx,
                        &mut pending,
                        &event_tx,
                        &interrupt,
                        &approvals,
                        &questions,
                    )
                    .await
                    {
                        end_session_lifecycle(&driver, &event_tx, &conv).await;
                        return;
                    }
                }
                Op::Rewind { turns } => {
                    // Rewind rewrites shared history, so it is idle-only
                    // by contract: the frontend gates it, and running it
                    // against an active snapshot would race the turn.
                    let removed = rewind_conversation(&mut conv, turns);
                    if removed == 0 {
                        try_send_event(
                            &event_tx,
                            Event {
                                id: sub.id.clone(),
                                msg: EventMsg::Warning {
                                    message: "nothing to rewind".to_string(),
                                },
                            },
                        );
                    } else {
                        // Stale usage must not outlive the dropped turns;
                        // the next sample settles real numbers again.
                        conv.settle(Usage::default());
                        try_send_event(
                            &event_tx,
                            Event {
                                id: sub.id.clone(),
                                msg: EventMsg::HistoryRewound { turns: removed },
                            },
                        );
                    }
                }
                Op::Shutdown => {
                    interrupt.trigger();
                    end_session_lifecycle(&driver, &event_tx, &conv).await;
                    return;
                }
                Op::SetPermissionMode { mode } => {
                    // Live mode switch through the driver seam; unknown
                    // names warn so typos never silently stick.
                    if !driver.set_permission_mode(&mode) {
                        try_send_event(
                            &event_tx,
                            Event {
                                id: sub.id.clone(),
                                msg: EventMsg::Warning {
                                    message: format!("unknown permission mode: {mode:?}"),
                                },
                            },
                        );
                    }
                }
                Op::SetModel { name } => {
                    // Live model switch through the driver seam; a reject
                    // (unknown/fixed gateway) warns so the switch never
                    // silently fails.
                    if !driver.set_model(&name) {
                        try_send_event(
                            &event_tx,
                            Event {
                                id: sub.id.clone(),
                                msg: EventMsg::Warning {
                                    message: format!("model switch rejected: {name:?}"),
                                },
                            },
                        );
                    }
                }
                Op::SetThinking { effort } => {
                    // Live reasoning-effort switch; gateways without a
                    // mutable effort warn instead of silently ignoring.
                    if !driver.set_thinking(&effort) {
                        try_send_event(
                            &event_tx,
                            Event {
                                id: sub.id.clone(),
                                msg: EventMsg::Warning {
                                    message: format!("thinking switch rejected: {effort:?}"),
                                },
                            },
                        );
                    }
                }
            }
        }
    }
}

/// Drop the last `turns` user turns from the conversation: each user
/// prompt and everything after it, up to the next user prompt. Entries
/// before the first user prompt (compaction summaries) are never
/// dropped. Tool results, continuation prompts, and system reminders
/// are user-role entries but not turn boundaries — cutting on them
/// leaves an assistant `tool_use` with no result. Returns the number
/// of turns actually removed.
fn rewind_conversation(conv: &mut Conversation, turns: u32) -> u32 {
    let entries = conv.snapshot();
    let user_positions: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| is_user_turn(entry))
        .map(|(index, _)| index)
        .collect();
    let drop = (turns as usize).min(user_positions.len());
    if drop == 0 {
        return 0;
    }
    let cutoff = user_positions[user_positions.len() - drop];
    conv.replace(entries[..cutoff].to_vec());
    drop as u32
}

/// A user-authored turn, as opposed to a harness entry stored under the
/// user role so providers will accept it.
fn is_user_turn(entry: &state_store::HistoryEntry) -> bool {
    if entry.role != Role::User {
        return false;
    }
    if entry
        .blocks
        .iter()
        .any(|block| matches!(block, state_store::Block::ToolResult { .. }))
    {
        return false;
    }
    let prose = entry.prose();
    if prose.contains(wavecode_wire::SYSTEM_REMINDER_OPEN)
        || prose == runtime_runner::CONTINUATION_PROMPT
    {
        return false;
    }
    entry.blocks.iter().any(|block| {
        matches!(
            block,
            state_store::Block::Text(_) | state_store::Block::Image { .. }
        )
    })
}

/// Run one session lifecycle hook point, tagging events with the
/// synthetic lifecycle id.
async fn run_lifecycle_hook<D: TurnDriver>(
    driver: &D,
    event_tx: &mpsc::Sender<Event>,
    point: HookPoint,
) {
    let tx = event_tx.clone();
    driver
        .drive_hook(point, "", &|event| {
            try_send_event(
                &tx,
                Event {
                    id: LIFECYCLE_ID.to_string(),
                    msg: event.msg,
                },
            );
        })
        .await;
}

/// Best-effort session teardown work (memory extraction) on the final
/// transcript, one `role: text` line per entry. Runs after the SessionEnd
/// hooks at every teardown exit; failures are impossible by contract (the
/// default is a no-op and overrides must never fail shutdown).
async fn end_session<D: TurnDriver>(driver: &D, conv: &Conversation) {
    let transcript: Vec<String> = conv
        .snapshot()
        .iter()
        .map(|entry| {
            let role = match entry.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            format!("{role}: {}", entry.text())
        })
        .collect();
    driver.end_session(&transcript).await;
}

/// Persist the pre-turn durable checkpoint for one turn-driving op.
///
/// Shared by the two turn-carrying operations (`UserInput`, `Compact`):
/// bump `turn_seq`, render the pre-turn snapshot, and save the
/// checkpoint when `checkpoint_on` asks. The flag is resolved by the
/// caller because each operation maps to a different policy hook point
/// (`checkpoint_before_model_request` vs `before_tool_side_effect`).
/// Warnings carry the submission's own id; a durable failure is loud
/// but never aborts the turn (see [`checkpoint_turn`]).
///
/// The fsynced write runs on the blocking pool: `durable_save` performs
/// write + fsync + rename, and an NTFS fsync can take tens of milliseconds,
/// which would otherwise hold a runtime worker once per turn. Ordering is
/// unchanged — the await completes before the caller acts — and a panicking
/// checkpoint still unwinds via `resume_unwind`, matching the inline call.
async fn checkpoint_pre_turn(
    durability: &mut Option<TurnDurability>,
    enabled: impl Fn(&CheckpointPolicy) -> bool,
    turn_seq: &mut u64,
    conv: &Conversation,
    event_tx: &mpsc::Sender<Event>,
    id: &str,
) {
    // The policy decides per hook point; the flag resolves once here so
    // every call site stays a single expression.
    let checkpoint_on = durability.as_ref().is_some_and(|dur| enabled(&dur.policy));
    // Disabled policy exits first: rendering the snapshot and deriving the
    // label are a full O(history) walk that would otherwise be computed and
    // dropped on every turn when no checkpoint can be written (the plain
    // `spawn` path has no durability at all).
    if !checkpoint_on {
        return;
    }
    *turn_seq += 1;
    let label = turn_label(*turn_seq);
    let snapshot = render_snapshot(conv);
    let warn_tx = event_tx.clone();
    let warn_id = id.to_string();
    // Take the store out so the blocking closure owns it ('static); the
    // actor task is the only accessor, so nothing observes the transient
    // `None` across the await.
    let mut taken = durability.take();
    match tokio::task::spawn_blocking(move || {
        checkpoint_turn(&mut taken, checkpoint_on, &label, &snapshot, &|message| {
            try_send_event(
                &warn_tx,
                Event {
                    id: warn_id.clone(),
                    msg: EventMsg::Warning { message },
                },
            );
        });
        taken
    })
    .await
    {
        Ok(returned) => *durability = returned,
        Err(join_error) => std::panic::resume_unwind(join_error.into_panic()),
    }
}

/// Session teardown shared by every actor exit path (clients dropped,
/// turn exit, explicit shutdown): SessionEnd hooks first, then the
/// best-effort memory-extraction pass. One sequence so no exit path can
/// run a half teardown.
async fn end_session_lifecycle<D: TurnDriver>(
    driver: &D,
    event_tx: &mpsc::Sender<Event>,
    conv: &Conversation,
) {
    run_lifecycle_hook(driver, event_tx, HookPoint::SessionEnd).await;
    end_session(driver, conv).await;
}

/// Forwarding sink over the bounded event channel.
///
/// Never blocks the driver: a full buffer drops (see [`try_send_event`])
/// and send fails outright only when the frontend is gone, which is
/// exactly when dropping is correct.
fn forwarding_sink(event_tx: &mpsc::Sender<Event>) -> impl Fn(Event) + '_ {
    move |event: Event| {
        try_send_event(event_tx, event);
    }
}

/// Forward one event into the bounded event channel without awaiting.
///
/// The driver-facing sinks are synchronous `Fn` callbacks, so they cannot
/// apply await-based backpressure; `try_send` guarantees the actor task
/// never parks on a slow consumer and the interrupt path stays reachable.
/// When a stalled frontend has filled [`EVENT_CHANNEL_CAP`] buffered
/// events, the event is dropped with a log line: stream deltas self-heal
/// (the completion event carries the full text), and reaching a full
/// buffer means the consumer has been wedged for a while, where the
/// interrupt path and process exit remain the recovery routes. A closed
/// receiver keeps the established silent-drop convention (frontend gone).
fn try_send_event(event_tx: &mpsc::Sender<Event>, event: Event) {
    match event_tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(event)) => {
            tracing::warn!(
                id = %event.id,
                "event channel full; dropping event for a stalled consumer"
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

/// Drive one operation while routing inbound submissions.
///
/// Queueable operations wait their turn; control operations take effect
/// immediately. Returns true when the actor must exit: on shutdown, or
/// when all clients dropped (implicit shutdown). A 2-second drain lets
/// the operation settle before exit.
async fn supervised<F>(
    op: F,
    submit_rx: &mut mpsc::Receiver<Submission>,
    pending: &mut VecDeque<Submission>,
    event_tx: &mpsc::Sender<Event>,
    interrupt: &InterruptHandle,
    approvals: &Arc<ApprovalGate>,
    questions: &Arc<QuestionGate>,
) -> bool
where
    F: Future<Output = ()>,
{
    tokio::pin!(op);
    loop {
        tokio::select! {
            () = &mut op => return false,
            maybe = submit_rx.recv() => {
                match maybe {
                    Some(extra) => {
                        if route_extra(extra, pending, event_tx, interrupt, approvals, questions) {
                            return true;
                        }
                    }
                    None => {
                        interrupt.trigger();
                        let _ = tokio::time::timeout(SHUTDOWN_DRAIN, &mut op).await;
                        return true;
                    }
                }
            }
        }
    }
}

/// Route one out-of-turn submission; true means the actor must exit.
fn route_extra(
    sub: Submission,
    pending: &mut VecDeque<Submission>,
    event_tx: &mpsc::Sender<Event>,
    interrupt: &InterruptHandle,
    approvals: &Arc<ApprovalGate>,
    questions: &Arc<QuestionGate>,
) -> bool {
    match sub.op {
        Op::UserInput { .. } | Op::Compact { .. } => queue_or_reject(pending, sub, event_tx),
        Op::Rewind { .. } => {
            // Never race the active snapshot: rewind waits for idle.
            tracing::warn!(id = %sub.id, "rewind rejected mid-turn");
            try_send_event(
                event_tx,
                Event {
                    id: sub.id,
                    msg: EventMsg::Warning {
                        message: "rewind rejected: a turn is running".to_string(),
                    },
                },
            );
        }
        Op::Interrupt => interrupt.trigger(),
        Op::SetPermissionMode { .. } | Op::SetModel { .. } | Op::SetThinking { .. } => {
            // Mode/model/thinking switches apply at the next turn
            // boundary; queue one marker so ordering with inputs is
            // preserved. Control-class immediacy is unnecessary: policy
            // and the gateway read the value per sample.
            queue_or_reject(pending, sub, event_tx);
        }
        Op::ExecApproval { call_id, decision } => {
            if !approvals.decide(&call_id, map_decision(decision)) {
                warn_late_decision(event_tx, &sub.id, &call_id);
            }
        }
        Op::QuestionAnswer { call_id, answer } => {
            if !questions.answer(&call_id, answer) {
                warn_late_decision(event_tx, &sub.id, &call_id);
            }
        }
        Op::Shutdown => {
            interrupt.trigger();
            return true;
        }
    }
    false
}

/// Queue a submission or reject it explicitly with its own id.
///
/// Rejection beats backpressure: a parked sender would also park the
/// interrupt queued behind it, leaving the user no way to stop the turn.
fn queue_or_reject(
    pending: &mut VecDeque<Submission>,
    sub: Submission,
    event_tx: &mpsc::Sender<Event>,
) {
    if pending.len() >= PENDING_QUEUE_CAP {
        tracing::warn!(id = %sub.id, "submission queue is full; rejecting");
        try_send_event(
            event_tx,
            Event {
                id: sub.id,
                msg: EventMsg::Error {
                    message: format!(
                        "submission queue is full ({PENDING_QUEUE_CAP} pending); retry later"
                    ),
                    recoverable: true,
                    code: Some("queue.full".to_string()),
                },
            },
        );
    } else {
        pending.push_back(sub);
    }
}

/// Warn the frontend about a late approval or question answer with no
/// parked waiter.
///
/// Decisions are one-shot slots, so dropping is safe; staying silent is
/// not: without this, a stale or mistyped call id fails invisibly.
fn warn_late_decision(event_tx: &mpsc::Sender<Event>, id: &str, call_id: &str) {
    tracing::warn!(%call_id, "late approval with no parked waiter; dropped");
    try_send_event(
        event_tx,
        Event {
            id: id.to_string(),
            msg: EventMsg::Warning {
                message: format!("late approval for {call_id}: no parked waiter; dropped"),
            },
        },
    );
}

/// Map wire approval decisions onto gate decisions one to one.
fn map_decision(decision: WireDecision) -> ApprovalDecision {
    match decision {
        WireDecision::AllowOnce => ApprovalDecision::AllowOnce,
        WireDecision::AllowAlways => ApprovalDecision::AllowAlways,
        WireDecision::Deny { reason } => ApprovalDecision::Deny { reason },
    }
}

/// Silence unused-variant analysis for outcome documentation.
///
/// [`StopReason`] outcomes are observed by drivers through events; the
/// actor intentionally ignores the value after a clean turn.
#[allow(dead_code)]
fn _outcome_docs(outcome: StopReason) -> &'static str {
    match outcome {
        StopReason::Completed => "turn ended normally",
        StopReason::Interrupted => "turn stopped at a safe point",
        StopReason::MaxToolRounds => "round ceiling reached",
        StopReason::RepeatBreaker => "turn stopped after repeated identical tool calls",
        StopReason::Error(_) => "turn failed after settle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime_runner::{InboxHandle, SteerTarget};
    use state_checkpoint::{CheckpointPolicy, CheckpointStore};
    use state_store::CompactTrigger;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    struct FakeDriver {
        inputs: Mutex<Vec<String>>,
        hold: bool,
        release: Arc<Notify>,
        ended: Mutex<Vec<Vec<String>>>,
        inbox: InboxHandle,
    }

    #[async_trait::async_trait]
    impl TurnDriver for FakeDriver {
        async fn drive_turn(
            &self,
            ctx: &RunContext,
            _conv: &mut Conversation,
            input: TurnInput<'_>,
            _system: &str,
            on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> StopReason {
            self.inputs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(input.text.to_string());
            on_event(Event {
                id: ctx.submission_id.clone(),
                msg: EventMsg::TurnStarted {
                    model: "m".to_string(),
                },
            });
            if self.hold {
                self.release.notified().await;
            }
            on_event(Event {
                id: ctx.submission_id.clone(),
                msg: EventMsg::TurnCompleted { interrupted: false },
            });
            StopReason::Completed
        }

        async fn drive_compact(
            &self,
            _conv: &mut Conversation,
            _trigger: CompactTrigger,
            on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> Result<(), String> {
            on_event(Event {
                id: String::new(),
                msg: EventMsg::CompactStarted {
                    trigger: "manual".to_string(),
                },
            });
            on_event(Event {
                id: String::new(),
                msg: EventMsg::CompactCompleted { summary_tokens: 0 },
            });
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

        async fn end_session(&self, transcript: &[String]) {
            self.ended
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(transcript.to_vec());
        }

        fn inbox_handle(&self) -> Option<InboxHandle> {
            Some(self.inbox.clone())
        }
    }

    fn spawn_actor(hold: bool) -> (ActorClient, Arc<Notify>) {
        let release = Arc::new(Notify::new());
        let driver = FakeDriver {
            inputs: Mutex::new(Vec::new()),
            hold,
            release: release.clone(),
            ended: Mutex::new(Vec::new()),
            inbox: InboxHandle::new(),
        };
        let client = SessionActor::spawn(
            driver,
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        );
        (client, release)
    }

    /// A conversation with `n` user turns, each one user+assistant.
    fn seeded_conversation(turns: usize) -> Conversation {
        let mut conv = Conversation::new();
        for i in 0..turns {
            conv.push(Role::User, format!("question {i}"));
            conv.push(Role::Assistant, format!("answer {i}"));
        }
        conv
    }

    #[test]
    fn rewind_does_not_split_a_tool_pair() {
        let mut conv = Conversation::new();
        conv.push(Role::User, "first");
        conv.push(Role::Assistant, "ack");
        conv.push(Role::User, "do it");
        conv.push_blocks(
            Role::Assistant,
            vec![state_store::Block::ToolUse {
                call_id: "c1".to_string(),
                name: "write".to_string(),
                input: serde_json::json!({"path": "a.txt"}),
            }],
        );
        conv.push_blocks(
            Role::User,
            vec![state_store::Block::ToolResult {
                call_id: "c1".to_string(),
                content: "wrote".to_string(),
                is_error: false,
                produced_at: None,
            }],
        );
        conv.push(
            Role::User,
            wavecode_wire::wrap_system_reminder("goal still open"),
        );
        conv.push(Role::Assistant, "done");

        assert_eq!(super::rewind_conversation(&mut conv, 1), 1);
        let entries = conv.snapshot();
        assert_eq!(entries.len(), 2, "only the first user turn remains");
        assert_eq!(entries[0].text(), "first");
        assert_eq!(entries[1].text(), "ack");
        assert!(
            !entries.iter().any(|entry| entry
                .blocks
                .iter()
                .any(|block| { matches!(block, state_store::Block::ToolUse { .. }) })),
            "undo must not leave a tool_use behind"
        );
    }

    fn rewind(turns: u32) -> Submission {
        Submission {
            id: "rewind-1".to_string(),
            op: Op::Rewind { turns },
        }
    }

    /// Drain events until `keep` (inclusive) returns it; events before
    /// it are dropped.
    async fn event_matching(client: &mut ActorClient, keep: impl Fn(&EventMsg) -> bool) -> Event {
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
                    .await
                    .unwrap()
                    .expect("event stream must stay open");
            if keep(&event.msg) {
                return event;
            }
        }
    }

    /// Event names the tap test compares against, so the assertion reads as
    /// protocol vocabulary rather than Rust debug output.
    fn event_kind(msg: &EventMsg) -> &'static str {
        match serde_json::to_value(msg)
            .ok()
            .and_then(|value| value["type"].as_str().map(str::to_string))
        {
            Some(kind) => match kind.as_str() {
                "turn_started" => "turn_started",
                "turn_completed" => "turn_completed",
                _ => "other",
            },
            None => "other",
        }
    }

    /// The metrics tap is installed on the client, so it must see exactly
    /// what the frontend sees, in the same order, without consuming
    /// anything from the stream.
    #[tokio::test]
    async fn installed_tap_mirrors_the_events_a_frontend_receives() {
        let seen: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let collector = seen.clone();
        let driver = FakeDriver {
            inputs: Mutex::new(Vec::new()),
            hold: false,
            release: Arc::new(Notify::new()),
            ended: Mutex::new(Vec::new()),
            inbox: InboxHandle::new(),
        };
        let mut client = SessionActor::spawn(
            driver,
            seeded_conversation(1),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        )
        .with_tap(Arc::new(move |event: &Event| {
            let kind = event_kind(&event.msg);
            collector
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(kind);
        }));
        client
            .submit(Submission {
                id: "p1".to_string(),
                op: Op::UserInput {
                    text: "hi".to_string(),
                    images: Vec::new(),
                },
            })
            .await
            .unwrap();

        let mut yielded = Vec::new();
        loop {
            let event = event_matching(&mut client, |_| true).await;
            let done = matches!(event.msg, EventMsg::TurnCompleted { .. });
            yielded.push(event_kind(&event.msg));
            if done {
                break;
            }
        }
        assert_eq!(yielded.first(), Some(&"turn_started"), "{yielded:?}");
        assert_eq!(yielded.last(), Some(&"turn_completed"), "{yielded:?}");
        // Copy the guard out before the next await: holding it into the
        // shutdown below would be a real deadlock risk, not just a lint.
        let tapped: Vec<&'static str> = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(tapped, yielded, "the tap must not drop or duplicate events");

        client
            .submit(Submission {
                id: "p-end".to_string(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        while let Some(event) = client.next_event().await {
            if matches!(event.msg, EventMsg::TurnCompleted { .. }) {
                break;
            }
        }
    }

    #[tokio::test]
    async fn rewind_drops_requested_user_turns() {
        let driver = FakeDriver {
            inputs: Mutex::new(Vec::new()),
            hold: false,
            release: Arc::new(Notify::new()),
            ended: Mutex::new(Vec::new()),
            inbox: InboxHandle::new(),
        };
        let mut client = SessionActor::spawn(
            driver,
            seeded_conversation(3),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        );
        client.submit(rewind(1)).await.unwrap();
        let event = event_matching(&mut client, |msg| {
            matches!(msg, EventMsg::HistoryRewound { .. })
        })
        .await;
        assert_eq!(
            event.msg,
            EventMsg::HistoryRewound { turns: 1 },
            "exactly the requested turn is removed"
        );
        // Teardown: shut down cleanly and drain the stream to closure;
        // the shrink itself is pinned by the event count above.
        client
            .submit(Submission {
                id: "bye".to_string(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        while tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .is_some()
        {}
    }

    #[tokio::test]
    async fn rewind_beyond_history_drops_every_user_turn() {
        let driver = FakeDriver {
            inputs: Mutex::new(Vec::new()),
            hold: false,
            release: Arc::new(Notify::new()),
            ended: Mutex::new(Vec::new()),
            inbox: InboxHandle::new(),
        };
        let mut client = SessionActor::spawn(
            driver,
            seeded_conversation(1),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        );
        client.submit(rewind(5)).await.unwrap();
        let event = event_matching(&mut client, |msg| {
            matches!(msg, EventMsg::HistoryRewound { .. })
        })
        .await;
        assert_eq!(event.msg, EventMsg::HistoryRewound { turns: 1 });
    }

    #[tokio::test]
    async fn rewind_with_nothing_to_drop_warns() {
        let driver = FakeDriver {
            inputs: Mutex::new(Vec::new()),
            hold: false,
            release: Arc::new(Notify::new()),
            ended: Mutex::new(Vec::new()),
            inbox: InboxHandle::new(),
        };
        let mut client = SessionActor::spawn(
            driver,
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        );
        client.submit(rewind(1)).await.unwrap();
        let event =
            event_matching(&mut client, |msg| matches!(msg, EventMsg::Warning { .. })).await;
        assert!(
            matches!(event.msg, EventMsg::Warning { ref message } if message.contains("nothing to rewind")),
            "{:?}",
            event.msg
        );
    }

    #[tokio::test]
    async fn mid_turn_rewind_is_rejected_with_a_warning() {
        let (mut client, release) = spawn_actor(true);
        client
            .submit(user_input("s0", "long running"))
            .await
            .unwrap();
        // Hold the turn inside the driver, then attempt the rewind.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        client.submit(rewind(1)).await.unwrap();
        let event =
            event_matching(&mut client, |msg| matches!(msg, EventMsg::Warning { .. })).await;
        assert!(
            matches!(event.msg, EventMsg::Warning { ref message } if message.contains("rewind rejected")),
            "{:?}",
            event.msg
        );
        release.notify_one();
    }

    fn user_input(id: &str, text: &str) -> Submission {
        Submission {
            id: id.to_string(),
            op: Op::UserInput {
                text: text.to_string(),
                images: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn client_steer_inject_cancel_ride_driver_inbox() {
        // The client carries the driver's inbox handle (captured at spawn),
        // so steering needs no wire Op and no running turn.
        let (client, _) = spawn_actor(false);
        assert!(client.steer("turn-steer", SteerTarget::NextTurn));
        assert!(client.steer("step-steer", SteerTarget::NextStep));
        assert!(client.inject("injected"));
        assert!(!client.steer("", SteerTarget::NextStep));
        assert!(!client.inject(""));
        // keep=true drops the two current-turn items, keeps next-turn.
        assert_eq!(client.cancel_inbox(true), 2);
        assert_eq!(client.cancel_inbox(false), 1);
        assert_eq!(client.cancel_inbox(false), 0);
    }

    #[tokio::test]
    async fn turn_events_flow_in_order() {
        let (mut client, _) = spawn_actor(false);
        client.submit(user_input("s1", "hello")).await.unwrap();
        let mut kinds = Vec::new();
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
                .await
                .unwrap()
        {
            kinds.push(
                serde_json::to_value(&event.msg)
                    .unwrap()
                    .get("type")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
            if kinds.last().unwrap() == "turn_completed" {
                break;
            }
        }
        assert_eq!(kinds, vec!["turn_started", "turn_completed"]);
    }

    #[tokio::test]
    async fn queue_overflow_rejects_with_own_id() {
        let (mut client, _) = spawn_actor(true);
        client.submit(user_input("s0", "first")).await.unwrap();
        // Let the first turn start and hold inside the driver.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        for i in 1..=(PENDING_QUEUE_CAP + 1) {
            client
                .submit(user_input(&format!("s{i}"), "queued"))
                .await
                .unwrap();
        }
        // Collect until the rejection arrives; the held turn never ends,
        // so the client is dropped (aborting the actor) afterwards.
        let mut rejection: Option<Event> = None;
        for _ in 0..(PENDING_QUEUE_CAP + 4) {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
                    .await
                    .unwrap()
                    .unwrap();
            if let EventMsg::Error { message, .. } = &event.msg
                && message.contains("queue is full")
            {
                rejection = Some(event);
                break;
            }
        }
        let rejection = rejection.expect("expected a queue-full rejection");
        assert_eq!(rejection.id, format!("s{}", PENDING_QUEUE_CAP + 1));
    }

    #[tokio::test]
    async fn idle_interrupt_emits_nothing() {
        let (mut client, _) = spawn_actor(false);
        client
            .submit(Submission {
                id: "s1".to_string(),
                op: Op::Interrupt,
            })
            .await
            .unwrap();
        let none =
            tokio::time::timeout(std::time::Duration::from_millis(150), client.next_event()).await;
        assert!(none.is_err());
    }

    #[tokio::test]
    async fn shutdown_closes_the_event_stream() {
        let (mut client, _) = spawn_actor(false);
        client
            .submit(Submission {
                id: "s1".to_string(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
            .await
            .unwrap();
        assert!(end.is_none());
    }

    #[tokio::test]
    async fn unknown_permission_modes_warn_instead_of_sticking() {
        let (mut client, _) = spawn_actor(false);
        client
            .submit(Submission {
                id: "s1".to_string(),
                op: Op::SetPermissionMode {
                    mode: "yolo".to_string(),
                },
            })
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event.msg,
            EventMsg::Warning { ref message } if message.contains("unknown permission mode")
        ));
    }

    #[tokio::test]
    async fn approval_reaches_the_parked_gate() {
        let gate = Arc::new(ApprovalGate::new());
        let driver = FakeDriver {
            inputs: Mutex::new(Vec::new()),
            hold: false,
            release: Arc::new(Notify::new()),
            ended: Mutex::new(Vec::new()),
            inbox: InboxHandle::new(),
        };
        let client = SessionActor::spawn(
            driver,
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            gate.clone(),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        );
        let waiter = gate.wait_for("c9").unwrap();
        client
            .submit(Submission {
                id: "s1".to_string(),
                op: Op::ExecApproval {
                    call_id: "c9".to_string(),
                    decision: WireDecision::AllowOnce,
                },
            })
            .await
            .unwrap();
        let decision = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decision, ApprovalDecision::AllowOnce);
    }

    #[tokio::test]
    async fn idle_late_approval_warns_with_submission_id() {
        let (mut client, _) = spawn_actor(false);
        client
            .submit(Submission {
                id: "s-late".to_string(),
                op: Op::ExecApproval {
                    call_id: "ghost-call".to_string(),
                    decision: WireDecision::AllowOnce,
                },
            })
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.id, "s-late");
        assert!(matches!(
            event.msg,
            EventMsg::Warning { ref message }
                if message.contains("ghost-call") && message.contains("no parked waiter")
        ));
    }

    #[tokio::test]
    async fn mid_turn_late_approval_warns_while_turn_held() {
        let (mut client, _) = spawn_actor(true);
        client.submit(user_input("s0", "first")).await.unwrap();
        // Let the first turn start and hold inside the driver.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        client
            .submit(Submission {
                id: "s-late".to_string(),
                op: Op::ExecApproval {
                    call_id: "ghost-call".to_string(),
                    decision: WireDecision::Deny {
                        reason: "nope".to_string(),
                    },
                },
            })
            .await
            .unwrap();
        // The held turn never ends, so scan events until the warning
        // arrives; dropping the client aborts the actor afterwards.
        let mut warning: Option<Event> = None;
        for _ in 0..8 {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
                    .await
                    .unwrap()
                    .unwrap();
            if let EventMsg::Warning { message } = &event.msg
                && message.contains("ghost-call")
            {
                warning = Some(event);
                break;
            }
        }
        let warning = warning.expect("expected a late-approval warning mid-turn");
        assert_eq!(warning.id, "s-late");
    }

    #[tokio::test]
    async fn shutdown_delivers_final_transcript_to_end_session() {
        let ended: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        struct RecordingDriver {
            ended: Arc<Mutex<Vec<Vec<String>>>>,
        }
        #[async_trait::async_trait]
        impl TurnDriver for RecordingDriver {
            async fn drive_turn(
                &self,
                _ctx: &RunContext,
                conv: &mut Conversation,
                input: TurnInput<'_>,
                _system: &str,
                on_event: &(dyn Fn(Event) + Send + Sync),
            ) -> StopReason {
                conv.push(Role::User, input.text.to_string());
                conv.push(Role::Assistant, "noted".to_string());
                on_event(Event {
                    id: "s1".to_string(),
                    msg: EventMsg::TurnCompleted { interrupted: false },
                });
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

            async fn end_session(&self, transcript: &[String]) {
                self.ended
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(transcript.to_vec());
            }
        }
        let mut client = SessionActor::spawn(
            RecordingDriver {
                ended: ended.clone(),
            },
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        );
        client
            .submit(user_input("s1", "remember the sky"))
            .await
            .unwrap();
        // Drain through the turn end, then shut down and drain to close.
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
                .await
                .unwrap()
        {
            if matches!(event.msg, EventMsg::TurnCompleted { .. }) {
                break;
            }
        }
        client
            .submit(Submission {
                id: "s2".to_string(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        while tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .is_some()
        {}
        let ended = ended.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(ended.len(), 1);
        assert!(
            ended[0]
                .iter()
                .any(|line| line.contains("remember the sky"))
        );
        assert!(ended[0].iter().any(|line| line.starts_with("user: ")));
    }

    /// Shutdown arriving while a compact is held must still end the
    /// session: the teardown at the compact exit runs the SessionEnd
    /// hooks and delivers the final transcript, same as the turn exit.
    #[tokio::test]
    async fn shutdown_during_held_compact_still_ends_session() {
        struct HoldingCompactDriver {
            release: Arc<Notify>,
            ended: Arc<Mutex<Vec<Vec<String>>>>,
        }
        #[async_trait::async_trait]
        impl TurnDriver for HoldingCompactDriver {
            async fn drive_turn(
                &self,
                _ctx: &RunContext,
                _conv: &mut Conversation,
                _input: TurnInput<'_>,
                _system: &str,
                _on_event: &(dyn Fn(Event) + Send + Sync),
            ) -> StopReason {
                StopReason::Completed
            }

            async fn drive_compact(
                &self,
                _conv: &mut Conversation,
                _trigger: CompactTrigger,
                on_event: &(dyn Fn(Event) + Send + Sync),
            ) -> Result<(), String> {
                on_event(Event {
                    id: String::new(),
                    msg: EventMsg::CompactStarted {
                        trigger: "manual".to_string(),
                    },
                });
                self.release.notified().await;
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

            async fn end_session(&self, transcript: &[String]) {
                self.ended
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(transcript.to_vec());
            }
        }
        let ended: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut client = SessionActor::spawn(
            HoldingCompactDriver {
                release: Arc::new(Notify::new()),
                ended: ended.clone(),
            },
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        );
        client
            .submit(Submission {
                id: "s1".to_string(),
                op: Op::Compact { instruction: None },
            })
            .await
            .unwrap();
        // The compact is held inside the driver once its start event lands.
        let started = tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(started.msg, EventMsg::CompactStarted { .. }));
        client
            .submit(Submission {
                id: "s2".to_string(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        // Teardown closes the event stream; end_session must have run.
        while tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .is_some()
        {}
        let ended = ended.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            ended.len(),
            1,
            "shutdown during a compact must still end the session"
        );
    }

    /// The bounded event channel drops (with a log line) once a stalled
    /// consumer has filled it, and never blocks the sender: memory stays
    /// capped and the actor keeps draining control ops.
    #[test]
    fn full_event_channel_drops_instead_of_growing() {
        let (tx, mut rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAP);
        for i in 0..EVENT_CHANNEL_CAP {
            try_send_event(
                &tx,
                Event {
                    id: format!("e{i}"),
                    msg: EventMsg::Warning {
                        message: String::new(),
                    },
                },
            );
        }
        // Over the cap: dropped synchronously, no panic, no growth.
        try_send_event(
            &tx,
            Event {
                id: "overflow".to_string(),
                msg: EventMsg::Warning {
                    message: String::new(),
                },
            },
        );
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, EVENT_CHANNEL_CAP);
    }

    fn durable_driver() -> FakeDriver {
        FakeDriver {
            inputs: Mutex::new(Vec::new()),
            hold: false,
            release: Arc::new(Notify::new()),
            ended: Mutex::new(Vec::new()),
            inbox: InboxHandle::new(),
        }
    }

    fn spawn_durable(root: &std::path::Path) -> ActorClient {
        SessionActor::spawn_with_durability(
            durable_driver(),
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
            DurabilityConfig {
                store: CheckpointStore::new(),
                root: root.to_path_buf(),
                policy: CheckpointPolicy::default(),
            },
        )
    }

    #[tokio::test]
    async fn startup_offers_resume_from_newest_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkpoints");
        state_checkpoint::durable_save(&root, "turn-1", "a").unwrap();
        state_checkpoint::durable_save(&root, "turn-2", "b").unwrap();
        let mut client = spawn_durable(&root);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event.msg,
            EventMsg::Warning { ref message }
                if message.contains("turn-2") && message.contains("resume available")
        ));
    }

    #[tokio::test]
    async fn turn_entry_checkpoints_before_acting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkpoints");
        let mut client = spawn_durable(&root);
        // Empty roots offer no resume, so the first event is the turn.
        client.submit(user_input("s1", "hello")).await.unwrap();
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
                .await
                .unwrap()
        {
            if matches!(event.msg, EventMsg::TurnCompleted { .. }) {
                break;
            }
        }
        // The pre-turn snapshot was durable before the turn acted.
        assert_eq!(
            state_checkpoint::durable_load(&root, "turn-1")
                .unwrap()
                .as_deref(),
            Some("")
        );
    }

    #[tokio::test]
    async fn disabled_policy_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("never-created");
        let mut client = SessionActor::spawn_with_durability(
            durable_driver(),
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            Arc::new(QuestionGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
            DurabilityConfig {
                store: CheckpointStore::new(),
                root: root.clone(),
                policy: CheckpointPolicy::disabled(),
            },
        );
        client.submit(user_input("s1", "hello")).await.unwrap();
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.next_event())
                .await
                .unwrap()
        {
            if matches!(event.msg, EventMsg::TurnCompleted { .. }) {
                break;
            }
        }
        assert!(
            !root.exists(),
            "disabled policy performs zero IO, not even mkdir"
        );
    }

    /// The `supervised` drain: when the last client drops mid-operation,
    /// the actor interrupts the work and waits at most `SHUTDOWN_DRAIN`
    /// for it to settle — an operation that never settles holds the exit
    /// for the full window, not forever. Paused time makes the window
    /// exact without real sleeping.
    #[tokio::test(start_paused = true)]
    async fn dropped_clients_drain_for_at_most_the_shutdown_window() {
        let (submit_tx, mut submit_rx) = mpsc::channel::<Submission>(CONTROL_CHANNEL_CAP);
        drop(submit_tx); // every client is gone
        let (event_tx, _event_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAP);
        let interrupt = InterruptHandle::new();
        let mut pending = VecDeque::new();
        let approvals = Arc::new(ApprovalGate::new());
        let questions = Arc::new(QuestionGate::new());

        let started = tokio::time::Instant::now();
        let exit = supervised(
            std::future::pending(),
            &mut submit_rx,
            &mut pending,
            &event_tx,
            &interrupt,
            &approvals,
            &questions,
        )
        .await;
        assert!(exit, "dropped clients must end the actor");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= SHUTDOWN_DRAIN,
            "the drain holds for the full window: {elapsed:?}"
        );
        assert!(
            elapsed < SHUTDOWN_DRAIN * 2,
            "the drain exits after the window, not much later: {elapsed:?}"
        );
        assert!(
            interrupt.is_triggered(),
            "the drain interrupts the in-flight work"
        );
    }

    /// An operation that settles inside the drain window ends the actor
    /// as soon as it completes — the drain never waits out the clock
    /// when the work has already stopped.
    #[tokio::test(start_paused = true)]
    async fn dropped_clients_exit_as_soon_as_the_operation_settles() {
        let (submit_tx, mut submit_rx) = mpsc::channel::<Submission>(CONTROL_CHANNEL_CAP);
        drop(submit_tx);
        let (event_tx, _event_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAP);
        let interrupt = InterruptHandle::new();
        let mut pending = VecDeque::new();
        let approvals = Arc::new(ApprovalGate::new());
        let questions = Arc::new(QuestionGate::new());

        let half_window = SHUTDOWN_DRAIN / 2;
        let started = tokio::time::Instant::now();
        let exit = supervised(
            tokio::time::sleep(half_window),
            &mut submit_rx,
            &mut pending,
            &event_tx,
            &interrupt,
            &approvals,
            &questions,
        )
        .await;
        assert!(exit, "dropped clients must end the actor");
        assert_eq!(
            started.elapsed(),
            half_window,
            "the actor exits when the operation settles, not at the window end"
        );
    }
}
