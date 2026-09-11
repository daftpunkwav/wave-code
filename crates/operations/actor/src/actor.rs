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
    CONTROL_CHANNEL_CAP, InterruptHandle, PENDING_QUEUE_CAP, SHUTDOWN_DRAIN,
};
use operations_wire::{Event, EventMsg, Op, Submission, WireDecision};
use runtime_child::ChildRuntime;
use runtime_runner::{HookPoint, RunContext, StopReason, TurnDriver};
use safety_gate::{ApprovalDecision, ApprovalGate};
use state_store::{CompactTrigger, Conversation, Role};
use tokio::sync::mpsc;

use crate::client::ActorClient;

/// Synthetic correlation id for session lifecycle warnings, which belong
/// to no submission.
const LIFECYCLE_ID: &str = "session-lifecycle";

/// Serial session driver, generic over the turn-driving seam.
pub struct SessionActor<D> {
    driver: D,
    conv: Conversation,
    children: Arc<ChildRuntime>,
    approvals: Arc<ApprovalGate>,
    interrupt: InterruptHandle,
    system: String,
    submit_rx: mpsc::Receiver<Submission>,
    event_tx: mpsc::UnboundedSender<Event>,
    pending: VecDeque<Submission>,
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
        interrupt: InterruptHandle,
        system: String,
    ) -> ActorClient {
        let (submit_tx, submit_rx) = mpsc::channel(CONTROL_CHANNEL_CAP);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let client_interrupt = interrupt.clone();
        let handle = tokio::spawn(
            Self {
                driver,
                conv,
                children,
                approvals,
                interrupt,
                system,
                submit_rx,
                event_tx,
                pending: VecDeque::new(),
            }
            .run(),
        );
        ActorClient::new(submit_tx, event_rx, client_interrupt, handle)
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
            interrupt,
            system,
            mut submit_rx,
            event_tx,
            mut pending,
        } = self;

        finish_lifecycle(&driver, &event_tx, HookPoint::SessionStart).await;

        loop {
            let sub = match pending.pop_front() {
                Some(sub) => Some(sub),
                None => submit_rx.recv().await,
            };
            let Some(sub) = sub else {
                // All clients dropped: end the session best-effort.
                finish_lifecycle(&driver, &event_tx, HookPoint::SessionEnd).await;
                end_session(&driver, &conv).await;
                return;
            };
            match sub.op {
                Op::UserInput { text } => {
                    // Child completions re-enter as user messages before
                    // the turn snapshot is taken.
                    for note in children.drain_notifications() {
                        conv.push(Role::User, note);
                    }
                    let ctx = RunContext {
                        run_id: sub.id.clone(),
                        submission_id: sub.id.clone(),
                        input: text.clone(),
                    };
                    let sink = unbounded_sink(&event_tx);
                    let op = async {
                        let _ = driver
                            .drive_turn(&ctx, &mut conv, &text, &system, &sink)
                            .await;
                    };
                    if supervised(
                        op,
                        &mut submit_rx,
                        &mut pending,
                        &event_tx,
                        &interrupt,
                        &approvals,
                    )
                    .await
                    {
                        finish_lifecycle(&driver, &event_tx, HookPoint::SessionEnd).await;
                        end_session(&driver, &conv).await;
                        return;
                    }
                }
                // Idle interrupt with no turn running: nothing to stop.
                Op::Interrupt => {}
                Op::ExecApproval { call_id, decision } => {
                    if !approvals.decide(&call_id, map_decision(decision)) {
                        // No parked waiter: the turn ended or never asked.
                        warn_late_approval(&event_tx, &sub.id, &call_id);
                    }
                }
                Op::Compact => {
                    let id = sub.id.clone();
                    let tx = event_tx.clone();
                    let sink = move |event: Event| {
                        let _ = tx.send(Event {
                            id: id.clone(),
                            msg: event.msg,
                        });
                    };
                    let op = async {
                        if let Err(cause) = driver
                            .drive_compact(&mut conv, CompactTrigger::Manual, &sink)
                            .await
                        {
                            sink(Event {
                                id: sub.id.clone(),
                                msg: EventMsg::Error {
                                    message: cause,
                                    recoverable: true,
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
                    )
                    .await
                    {
                        finish_lifecycle(&driver, &event_tx, HookPoint::SessionEnd).await;
                        return;
                    }
                }
                Op::Shutdown => {
                    interrupt.trigger();
                    finish_lifecycle(&driver, &event_tx, HookPoint::SessionEnd).await;
                    end_session(&driver, &conv).await;
                    return;
                }
                Op::SetPermissionMode { mode } => {
                    // Live mode switch through the driver seam; unknown
                    // names warn so typos never silently stick.
                    if !driver.set_permission_mode(&mode) {
                        let _ = event_tx.send(Event {
                            id: sub.id.clone(),
                            msg: EventMsg::Warning {
                                message: format!("unknown permission mode: {mode:?}"),
                            },
                        });
                    }
                }
            }
        }
    }
}

/// Run session lifecycle hooks, tagging warnings with the synthetic id.
async fn finish_lifecycle<D: TurnDriver>(
    driver: &D,
    event_tx: &mpsc::UnboundedSender<Event>,
    point: HookPoint,
) {
    let tx = event_tx.clone();
    driver
        .drive_hook(point, "", &|event| {
            let _ = tx.send(Event {
                id: LIFECYCLE_ID.to_string(),
                msg: event.msg,
            });
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
            format!("{role}: {}", entry.text)
        })
        .collect();
    driver.end_session(&transcript).await;
}

/// Forwarding sink over the unbounded event channel.
///
/// Unbounded send never blocks and only fails when the frontend is gone,
/// which is exactly when dropping is correct.
fn unbounded_sink(event_tx: &mpsc::UnboundedSender<Event>) -> impl Fn(Event) + '_ {
    move |event: Event| {
        let _ = event_tx.send(event);
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
    event_tx: &mpsc::UnboundedSender<Event>,
    interrupt: &InterruptHandle,
    approvals: &Arc<ApprovalGate>,
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
                        if route_extra(extra, pending, event_tx, interrupt, approvals) {
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
    event_tx: &mpsc::UnboundedSender<Event>,
    interrupt: &InterruptHandle,
    approvals: &Arc<ApprovalGate>,
) -> bool {
    match sub.op {
        Op::UserInput { .. } | Op::Compact => queue_or_reject(pending, sub, event_tx),
        Op::Interrupt => interrupt.trigger(),
        Op::SetPermissionMode { .. } => {
            // Mode switches apply at the next turn boundary; queue one
            // marker so ordering with inputs is preserved. Control-class
            // immediacy is unnecessary: policy reads the mode per call.
            queue_or_reject(pending, sub, event_tx);
        }
        Op::ExecApproval { call_id, decision } => {
            if !approvals.decide(&call_id, map_decision(decision)) {
                warn_late_approval(event_tx, &sub.id, &call_id);
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
    event_tx: &mpsc::UnboundedSender<Event>,
) {
    if pending.len() >= PENDING_QUEUE_CAP {
        tracing::warn!(id = %sub.id, "submission queue is full; rejecting");
        let _ = event_tx.send(Event {
            id: sub.id,
            msg: EventMsg::Error {
                message: format!(
                    "submission queue is full ({PENDING_QUEUE_CAP} pending); retry later"
                ),
                recoverable: true,
            },
        });
    } else {
        pending.push_back(sub);
    }
}

/// Warn the frontend about a late approval decision with no parked waiter.
///
/// Decisions are one-shot slots, so dropping is safe; staying silent is
/// not: without this, a stale or mistyped call id fails invisibly.
fn warn_late_approval(
    event_tx: &mpsc::UnboundedSender<Event>,
    id: &str,
    call_id: &str,
) {
    tracing::warn!(%call_id, "late approval with no parked waiter; dropped");
    let _ = event_tx.send(Event {
        id: id.to_string(),
        msg: EventMsg::Warning {
            message: format!("late approval for {call_id}: no parked waiter; dropped"),
        },
    });
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
        StopReason::Error(_) => "turn failed after settle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use state_store::CompactTrigger;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    struct FakeDriver {
        inputs: Mutex<Vec<String>>,
        hold: bool,
        release: Arc<Notify>,
        ended: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl TurnDriver for FakeDriver {
        async fn drive_turn(
            &self,
            ctx: &RunContext,
            _conv: &mut Conversation,
            input: &str,
            _system: &str,
            on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> StopReason {
            self.inputs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(input.to_string());
            on_event(Event {
                id: ctx.submission_id.clone(),
                msg: EventMsg::TurnStarted,
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
    }

    fn spawn_actor(hold: bool) -> (ActorClient, Arc<Notify>) {
        let release = Arc::new(Notify::new());
        let driver = FakeDriver {
            inputs: Mutex::new(Vec::new()),
            hold,
            release: release.clone(),
            ended: Mutex::new(Vec::new()),
        };
        let client = SessionActor::spawn(
            driver,
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            InterruptHandle::new(),
            "sys".to_string(),
        );
        (client, release)
    }

    fn user_input(id: &str, text: &str) -> Submission {
        Submission {
            id: id.to_string(),
            op: Op::UserInput {
                text: text.to_string(),
            },
        }
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
        };
        let client = SessionActor::spawn(
            driver,
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            gate.clone(),
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
                input: &str,
                _system: &str,
                on_event: &(dyn Fn(Event) + Send + Sync),
            ) -> StopReason {
                conv.push(Role::User, input.to_string());
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
            InterruptHandle::new(),
            "sys".to_string(),
        );
        client.submit(user_input("s1", "remember the sky")).await.unwrap();
        // Drain through the turn end, then shut down and drain to close.
        while let Some(event) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.next_event(),
        )
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
        assert!(ended[0].iter().any(|line| line.contains("remember the sky")));
        assert!(ended[0].iter().any(|line| line.starts_with("user: ")));
    }
}
