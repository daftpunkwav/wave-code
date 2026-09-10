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
use operations_wire::{Event, Submission};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Submission failures: the actor is gone, so delivery is impossible.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The actor task exited and the submission channel closed.
    #[error("session actor exited; submission was not delivered")]
    ActorExited,
}

/// In-process client: submit submissions, receive events.
pub struct ActorClient {
    submit_tx: mpsc::Sender<Submission>,
    event_rx: mpsc::UnboundedReceiver<Event>,
    interrupt: InterruptHandle,
    handle: JoinHandle<()>,
}

impl ActorClient {
    /// Build a client over live channels (called by the actor spawn path).
    pub(crate) fn new(
        submit_tx: mpsc::Sender<Submission>,
        event_rx: mpsc::UnboundedReceiver<Event>,
        interrupt: InterruptHandle,
        handle: JoinHandle<()>,
    ) -> Self {
        Self {
            submit_tx,
            event_rx,
            interrupt,
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
