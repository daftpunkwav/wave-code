/*!
 * @file SkillRouting
 * @description Skill execution modes with threshold routing.
 *
 * Responsibilities:
 * - Declare inline versus fork execution modes.
 * - Route skills by caller-supplied context size.
 * - Leave discovery and parsing to the skills capability.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Skill routing: where a skill runs, decided from data.

/// Execution mode of one skill invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillMode {
    /// Run inline in the current turn.
    Inline,
    /// Fork into an isolated child task.
    Fork,
}

/// Routing decision for one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoute {
    /// Skill name that was routed.
    pub skill: String,
    /// Chosen execution mode.
    pub mode: SkillMode,
    /// Human-readable reason for the choice.
    pub reason: String,
}

/// Routes skills to execution modes.
pub trait SkillRouter: Send + Sync {
    /// Choose a mode for `skill` given the current context size in chars.
    fn route(&self, skill: &str, context_chars: usize) -> SkillRoute;
}

/// Threshold router: large contexts fork to protect the main window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThresholdRouter {
    /// Context sizes at or above this fork instead of inlining.
    pub fork_over_chars: usize,
}

impl ThresholdRouter {
    /// Create a router with the given fork threshold.
    pub fn new(fork_over_chars: usize) -> Self {
        Self { fork_over_chars }
    }
}

impl SkillRouter for ThresholdRouter {
    fn route(&self, skill: &str, context_chars: usize) -> SkillRoute {
        if context_chars >= self.fork_over_chars {
            SkillRoute {
                skill: skill.to_string(),
                mode: SkillMode::Fork,
                reason: format!(
                    "context {context_chars} chars reaches fork threshold {}",
                    self.fork_over_chars
                ),
            }
        } else {
            SkillRoute {
                skill: skill.to_string(),
                mode: SkillMode::Inline,
                reason: format!("context {context_chars} chars fits inline"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_splits_inline_from_fork() {
        let router = ThresholdRouter::new(1000);
        assert_eq!(router.route("s", 999).mode, SkillMode::Inline);
        assert_eq!(router.route("s", 1000).mode, SkillMode::Fork);
        assert!(router.route("s", 5000).reason.contains("fork threshold"));
    }
}
