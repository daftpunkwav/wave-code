/*!
 * @file GoalAdapter
 * @description Adapts the session goal store to the run loop's seam.
 *
 * Responsibilities:
 * - Answer whether the durable objective still needs work.
 * - Render the continuation reminder from the goal's own status view.
 *
 * This module must not depend on: the actor, drivers, or any frontend. It
 * reads goal state and never writes it — the `goal` tool stays the single
 * writer, so the loop cannot race the model over the same record, and a
 * continuation can never mark its own work as done.
 */

use runtime_runner::GoalTracker;
use state_goal::GoalStatus;

use crate::goal_tools::{GoalStore, render_status};

impl GoalTracker for GoalStore {
    /// Open means an objective exists and the last mutation left it `Active`.
    ///
    /// `Blocked` / `Paused` / `Completed` all stop the steering: they are
    /// statements by the model or the operator that work should not continue,
    /// and a loop that ignored them would argue with them every round.
    fn open(&self) -> bool {
        let state = self.snapshot();
        !state.objective.trim().is_empty() && state.status == GoalStatus::Active
    }

    fn reminder(&self, budget: &str) -> String {
        let state = self.snapshot();
        // The envelope marks this as harness guidance rather than user input;
        // `render_status` is the model-facing goal view the `goal` tool's
        // `status` action shows too, so the reminder never invents a second
        // rendering of the same record.
        wavecode_context::wrap_system_reminder(&format!(
            "The session goal is still open: keep working it, or move it to \
             completed / blocked / paused if it cannot proceed. Do not stop \
             with it Active and nothing running.\n{budget}\n{}",
            render_status(&state)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use state_goal::GoalState;

    fn store(state: GoalState) -> GoalStore {
        // No home: memory-only store, which is all a read needs.
        GoalStore::new(state, None, "s1")
    }

    fn goal(status: GoalStatus, objective: &str) -> GoalState {
        GoalState {
            objective: objective.to_string(),
            status,
            ..GoalState::default()
        }
    }

    /// Only an Active objective with text behind it steers the loop.
    #[test]
    fn open_requires_an_active_objective() {
        assert!(store(goal(GoalStatus::Active, "ship it")).open());
        for status in [
            GoalStatus::Blocked,
            GoalStatus::Paused,
            GoalStatus::Completed,
        ] {
            assert!(
                !store(goal(status, "ship it")).open(),
                "{status} must not steer the loop"
            );
        }
        // No objective set yet: nothing to continue.
        assert!(!store(GoalState::default()).open());
        assert!(!store(goal(GoalStatus::Active, "   ")).open());
    }

    /// The reminder carries the objective, the loop's budget, and the
    /// harness-guidance envelope.
    #[test]
    fn reminder_renders_goal_and_budget() {
        let text = store(goal(GoalStatus::Active, "refactor the parser"))
            .reminder("Budget: 40 of 200000 context tokens used.");
        assert!(text.contains("refactor the parser"), "{text}");
        assert!(text.contains("Budget: 40 of"), "{text}");
        assert!(
            text.starts_with("<system-reminder>") && text.ends_with("</system-reminder>"),
            "{text}"
        );
    }
}
