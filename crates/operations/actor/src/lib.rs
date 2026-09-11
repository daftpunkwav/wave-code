/*!
 * @file SessionActorRoot
 * @description Serial session driver: submission routing plus turn driving.
 *
 * Responsibilities:
 * - Own the pending queue, control routing, and the client handle.
 * - Drive turns and idle compactions with shared select discipline.
 * - Never name concrete capabilities; only the TurnDriver seam.
 *
 * This module must not depend on: tools, policy, hooks, models, memory,
 * skills, frontends, or any concrete capability implementation.
 */

//! Session actor: the serial driver behind the client handle.
//!
//! Transport notes for v1: the event channel is unbounded, so emitting
//! never blocks the turn loop. No event is ever dropped silently; fullness
//! backpressure returns with the networked transport, which can apply
//! real flow control instead of stalling execution.

pub mod actor;
pub mod client;
pub mod durable;

pub use actor::SessionActor;
pub use client::{ActorClient, SubmitError};
pub use durable::{
    CheckpointSink, DurabilityConfig, persist_checkpoint, persist_then_act, render_snapshot,
    resume_checkpoint, turn_label,
};
