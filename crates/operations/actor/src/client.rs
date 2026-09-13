/*!
 * @file ActorClient
 * @description In-process client handle for the session actor.
 *
 * Responsibilities:
 * - Submit operations and stream back events over channels.
 * - Interrupt and abort the actor task on drop without leaking it.
 *
 * This module must not depend on: concrete tools, policy, hooks, models,
 * memory, skills, frontends, or any capability implementation.
 */

//! Client handle: the only frontend-facing surface of the actor.

use infrastructure_base::InterruptHandle;
use runtime_runner::{InboxHandle, SteerTarget};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use wavecode_wire::{Event, Submission};

/// Submission failures: the actor is gone, so delivery is impossible.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The actor task exited and the submission channel closed.
    #[error("session actor exited; submission was not delivered")]
    ActorExited,
}

/// In-process client: submit submissions, receive events.
///
/// Mid-turn steering call path (no wire involved): [`ActorClient::steer`] /
/// [`ActorClient::inject`] / [`ActorClient::cancel_inbox`] write directly
/// to the [`InboxHandle`] the actor captured from
/// `TurnDriver::inbox_handle` at spawn; the driven run loop drains that
/// handle at its loop head (`NextTurn`) and before each sample (`NextStep`
/// plus injections). Drivers without an inbox make these a no-op
/// (`steer`/`inject` return false, `cancel_inbox` returns 0).
pub struct ActorClient {
    submit_tx: mpsc::Sender<Submission>,
    event_rx: mpsc::UnboundedReceiver<Event>,
    interrupt: InterruptHandle,
    inbox: Option<InboxHandle>,
    handle: JoinHandle<()>,
}

impl ActorClient {
    /// Build a client over live channels (called by the actor spawn path).
    pub(crate) fn new(
        submit_tx: mpsc::Sender<Submission>,
        event_rx: mpsc::UnboundedReceiver<Event>,
        interrupt: InterruptHandle,
        inbox: Option<InboxHandle>,
        handle: JoinHandle<()>,
    ) -> Self {
        Self {
            submit_tx,
            event_rx,
            interrupt,
            inbox,
            handle,
        }
    }

    /// Deliver one submission; fails when the actor already exited.
    pub async fn submit(&self, sub: Submission) -> Result<(), SubmitError> {
        self.submit_tx
            .send(sub)
            .await
            .map_err(|_| SubmitError::ActorExited)
    }

    /// Queue a steering message for the running turn; false when the
    /// driver exposes no inbox (or the text is empty).
    pub fn steer(&self, text: &str, target: SteerTarget) -> bool {
        if text.is_empty() {
            return false;
        }
        match &self.inbox {
            Some(inbox) => {
                inbox.steer(text.to_string(), target);
                true
            }
            None => false,
        }
    }

    /// Queue a user message for the running turn's next sample; false when
    /// the driver exposes no inbox (or the text is empty).
    pub fn inject(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        match &self.inbox {
            Some(inbox) => {
                inbox.inject(text.to_string());
                true
            }
            None => false,
        }
    }

    /// Drop queued inbox items, returning the dropped count (0 without an
    /// inbox). With `keep_next_turn`, next-turn steering survives.
    pub fn cancel_inbox(&self, keep_next_turn: bool) -> usize {
        self.inbox
            .as_ref()
            .map(|inbox| inbox.cancel(keep_next_turn))
            .unwrap_or(0)
    }

    /// Receive the next event; `None` once the actor exited and drained.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }

    /// Take one buffered event without waiting; `None` when empty.
    pub fn try_poll(&mut self) -> Option<Event> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for ActorClient {
    fn drop(&mut self) {
        // Best-effort graceful stop first so a turn parked at a safe point
        // can settle; abort guarantees no leaked task when the model stream
        // never reaches another safe point.
        self.interrupt.trigger();
        self.handle.abort();
    }
}
