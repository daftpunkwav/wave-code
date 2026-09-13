/*!
 * @file PlanTrackerAdapter
 * @description Exposes the legacy todo store as plan-steering state.
 *
 * Responsibilities:
 * - Report unfinished plan item counts.
 * - Render steering reminders from the plan snapshot.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::PlanTracker`] implemented over the legacy todo store.

use runtime_runner::PlanTracker;

/// Plan steering backed by a shared todo store.
pub struct TodoPlanTracker {
    store: wavecode_tools::TodoStore,
}

impl TodoPlanTracker {
    /// Wrap the session todo store.
    pub fn new(store: wavecode_tools::TodoStore) -> Self {
        Self { store }
    }
}

impl PlanTracker for TodoPlanTracker {
    fn unfinished(&self) -> usize {
        let (pending, in_progress) = self.store.unfinished();
        pending + in_progress
    }

    fn reminder(&self) -> String {
        // Wrapped as a system reminder so the nudge reads as harness
        // guidance, not user-authored input (tag owned by wavecode-context).
        wavecode_context::wrap_system_reminder(&format!(
            "Plan has unfinished items; update the plan before finishing:\n{}",
            wavecode_tools::format_todos(&self.store.snapshot())
        ))
    }
}
