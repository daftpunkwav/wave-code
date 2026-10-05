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
use wavecode_wire::{Event, EventMsg, Op, Submission, UserImage, WireDecision};

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
    ///
    /// The turn-carrying ops (`UserInput`, `Compact`) and `Rewind`
    /// delegate to `run_*` helpers; control ops are handled inline.
    /// Helpers borrow disjoint fields of `self`, so their turn future
    /// and the routing arm never fight over a whole-`self` borrow.
    async fn run(mut self) {
        run_lifecycle_hook(&self.driver, &self.event_tx, HookPoint::SessionStart).await;

        // Offer resume-from-checkpoint: when durable labels exist, name
        // the newest so the frontend can restore it.
        if let Some(dur) = self.durability.as_ref() {
            let labels = list_resume_labels(&dur.root);
            if let Some(newest) = labels.last() {
                try_send_event(
                    &self.event_tx,
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
            let sub = match self.pending.pop_front() {
                Some(sub) => Some(sub),
                None => self.submit_rx.recv().await,
            };
            let Some(sub) = sub else {
                // All clients dropped: end the session best-effort.
                end_session_lifecycle(&self.driver, &self.event_tx, &self.conv).await;
                return;
            };
            match sub.op {
                Op::UserInput { text, images } => {
                    if self
                        .run_user_turn(&sub.id, text, images, &mut turn_seq)
                        .await
                    {
                        end_session_lifecycle(&self.driver, &self.event_tx, &self.conv).await;
                        return;
                    }
                }
                // Idle interrupt with no turn running: nothing to stop.
                Op::Interrupt => {}
                Op::ExecApproval { call_id, decision } => {
                    if !self.approvals.decide(&call_id, map_decision(decision)) {
                        // No parked waiter: the turn ended or never asked.
                        warn_late_decision(&self.event_tx, &sub.id, &call_id);
                    }
                }
                Op::QuestionAnswer { call_id, answer } => {
                    if !self.questions.answer(&call_id, answer) {
                        // No parked waiter: the turn ended or never asked.
                        warn_late_decision(&self.event_tx, &sub.id, &call_id);
                    }
                }
                Op::Compact { instruction } => {
                    if self.run_compact(&sub.id, instruction, &mut turn_seq).await {
                        end_session_lifecycle(&self.driver, &self.event_tx, &self.conv).await;
                        return;
                    }
                }
                Op::Rewind { turns } => self.run_rewind(&sub.id, turns),
                Op::Shutdown => {
                    self.interrupt.trigger();
                    end_session_lifecycle(&self.driver, &self.event_tx, &self.conv).await;
                    return;
                }
                Op::SetPermissionMode { mode } => {
                    // Live mode switch through the driver seam; unknown
                    // names warn so typos never silently stick.
                    if !self.driver.set_permission_mode(&mode) {
                        try_send_event(
                            &self.event_tx,
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
                    if !self.driver.set_model(&name) {
                        try_send_event(
                            &self.event_tx,
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
                    if !self.driver.set_thinking(&effort) {
                        try_send_event(
                            &self.event_tx,
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

    /// Drive one `UserInput` submission: fold in child notifications,
    /// checkpoint the pre-turn snapshot, then run the supervised turn.
    /// Returns true when the actor must exit (see [`supervised`]).
    async fn run_user_turn(
        &mut self,
        id: &str,
        text: String,
        images: Vec<UserImage>,
        turn_seq: &mut u64,
    ) -> bool {
        // Child completions re-enter as user messages before
        // the turn snapshot is taken.
        for note in self.children.drain_notifications() {
            self.conv.push(Role::User, note);
        }
        // Persist-then-act: the pre-turn snapshot is durable
        // before the driver broadcasts or executes anything.
        checkpoint_pre_turn(
            &mut self.durability,
            |policy| policy.checkpoint_before_model_request,
            turn_seq,
            &self.conv,
            &self.event_tx,
            id,
        )
        .await;
        let ctx = RunContext {
            run_id: id.to_string(),
            submission_id: id.to_string(),
            input: text.clone(),
            images,
        };
        let sink = forwarding_sink(&self.event_tx);
        let op = async {
            let _ = self
                .driver
                .drive_turn(
                    &ctx,
                    &mut self.conv,
                    TurnInput {
                        text: &ctx.input,
                        images: &ctx.images,
                    },
                    &self.system,
                    &sink,
                )
                .await;
        };
        supervised(
            op,
            &mut self.submit_rx,
            &mut self.pending,
            &self.event_tx,
            &self.interrupt,
            &self.approvals,
            &self.questions,
        )
        .await
    }

    /// Drive one manual `Compact` submission: checkpoint the pre-compact
    /// snapshot, then run the supervised compaction. Returns true when
    /// the actor must exit (see [`supervised`]).
    async fn run_compact(
        &mut self,
        id: &str,
        instruction: Option<String>,
        turn_seq: &mut u64,
    ) -> bool {
        // Compaction rewrites shared history (a state side
        // effect), so it honors the side-effect flag.
        checkpoint_pre_turn(
            &mut self.durability,
            |policy| policy.before_tool_side_effect,
            turn_seq,
            &self.conv,
            &self.event_tx,
            id,
        )
        .await;
        let tx = self.event_tx.clone();
        let sink_id = id.to_string();
        let sink = move |event: Event| {
            try_send_event(
                &tx,
                Event {
                    id: sink_id.clone(),
                    msg: event.msg,
                },
            );
        };
        let op = async {
            if let Err(cause) = self
                .driver
                .drive_compact(
                    &mut self.conv,
                    CompactTrigger::Manual { instruction },
                    &sink,
                )
                .await
            {
                sink(Event {
                    id: id.to_string(),
                    msg: EventMsg::Error {
                        message: cause,
                        recoverable: true,
                        code: Some("compact.failed".to_string()),
                    },
                });
            }
        };
        supervised(
            op,
            &mut self.submit_rx,
            &mut self.pending,
            &self.event_tx,
            &self.interrupt,
            &self.approvals,
            &self.questions,
        )
        .await
    }

    /// Apply an idle `Rewind`: drop the requested turns, settle stale
    /// usage, and report the outcome (warning when nothing was dropped).
    fn run_rewind(&mut self, id: &str, turns: u32) {
        // Rewind rewrites shared history, so it is idle-only
        // by contract: the frontend gates it, and running it
        // against an active snapshot would race the turn.
        let removed = rewind_conversation(&mut self.conv, turns);
        if removed == 0 {
            try_send_event(
                &self.event_tx,
                Event {
                    id: id.to_string(),
                    msg: EventMsg::Warning {
                        message: "nothing to rewind".to_string(),
                    },
                },
            );
        } else {
            // Stale usage must not outlive the dropped turns;
            // the next sample settles real numbers again.
            self.conv.settle(Usage::default());
            try_send_event(
                &self.event_tx,
                Event {
                    id: id.to_string(),
                    msg: EventMsg::HistoryRewound { turns: removed },
                },
            );
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
mod tests;
