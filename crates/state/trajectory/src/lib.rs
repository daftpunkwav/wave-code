/*!
 * @file TrajectoryLog
 * @description Ordered action/observation log with structural replay.
 *
 * Responsibilities:
 * - Record every agent action with a monotonic sequence number.
 * - Attach observations to the steps that produced them.
 * - Render structural replays for debugging and evaluation.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Trajectory: the auditable spine of an agent run.
//!
//! Textual model output is summarized, never stored verbatim in full:
//! the trajectory records structure (what ran, what came back), while
//! the conversation store keeps the words.

/// What the agent did at one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKind {
    /// One model sample.
    Sample,
    /// One tool invocation by name.
    ToolCall {
        /// Tool name that was invoked.
        name: String,
    },
    /// One context compaction by trigger name.
    Compact {
        /// Trigger that caused the compaction.
        trigger: String,
    },
    /// A free-form note (plan update, steering, decision).
    Note,
}

/// One recorded step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Monotonic sequence number starting at 1.
    pub seq: u64,
    /// What happened.
    pub action: ActionKind,
    /// Short human-readable detail.
    pub detail: String,
}

/// One observation attached to a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Sequence number of the producing step.
    pub step_seq: u64,
    /// Observation payload summary.
    pub content: String,
    /// True when the observation reports a failure.
    pub is_error: bool,
}

/// Ordered log of steps and their observations.
#[derive(Debug, Default)]
pub struct Trajectory {
    steps: Vec<Step>,
    observations: Vec<Observation>,
}

impl Trajectory {
    /// Create an empty trajectory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one action, returning its sequence number.
    pub fn push_action(&mut self, action: ActionKind, detail: impl Into<String>) -> u64 {
        let seq = self.steps.len() as u64 + 1;
        self.steps.push(Step {
            seq,
            action,
            detail: detail.into(),
        });
        seq
    }

    /// Attach one observation to an existing step.
    pub fn push_observation(&mut self, step_seq: u64, content: impl Into<String>, is_error: bool) {
        self.observations.push(Observation {
            step_seq,
            content: content.into(),
            is_error,
        });
    }

    /// All recorded steps in order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Observations attached to one step.
    pub fn observations_for(&self, step_seq: u64) -> Vec<&Observation> {
        self.observations
            .iter()
            .filter(|o| o.step_seq == step_seq)
            .collect()
    }

    /// Render a structural replay: one line per step with error marks.
    pub fn replay(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|step| {
                let failed = self
                    .observations
                    .iter()
                    .any(|o| o.step_seq == step.seq && o.is_error);
                format!(
                    "#{} {}: {}{}",
                    step.seq,
                    action_name(&step.action),
                    step.detail,
                    if failed { " [error]" } else { "" }
                )
            })
            .collect()
    }
}

/// Stable short name for one action kind.
fn action_name(action: &ActionKind) -> String {
    match action {
        ActionKind::Sample => "sample".to_string(),
        ActionKind::ToolCall { name } => format!("tool:{name}"),
        ActionKind::Compact { trigger } => format!("compact:{trigger}"),
        ActionKind::Note => "note".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_are_monotonic_from_one() {
        let mut trajectory = Trajectory::new();
        assert_eq!(trajectory.push_action(ActionKind::Sample, "s1"), 1);
        assert_eq!(
            trajectory.push_action(
                ActionKind::ToolCall {
                    name: "shell".to_string()
                },
                "ls"
            ),
            2
        );
    }

    #[test]
    fn replay_marks_failed_steps() {
        let mut trajectory = Trajectory::new();
        let seq = trajectory.push_action(
            ActionKind::ToolCall {
                name: "shell".to_string(),
            },
            "ls",
        );
        trajectory.push_observation(seq, "denied", true);
        trajectory.push_action(ActionKind::Note, "done");
        let replay = trajectory.replay();
        assert_eq!(replay.len(), 2);
        assert!(replay[0].contains("[error]"));
        assert!(!replay[1].contains("[error]"));
        assert!(trajectory.observations_for(seq).len() == 1);
    }
}
