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

use std::collections::HashMap;

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

/// Router layering explicit per-skill pins over a fallback router.
///
/// Operators pin known-heavy skills to [`SkillMode::Fork`] (or force tiny
/// helpers [`SkillMode::Inline`]); everything unpinned delegates to the
/// wrapped router unchanged. Pins always win so intent stays explicit.
#[derive(Debug, Clone)]
pub struct OverrideRouter<R = ThresholdRouter> {
    overrides: HashMap<String, SkillMode>,
    fallback: R,
}

impl<R> OverrideRouter<R> {
    /// Wrap a fallback router with no pins.
    pub fn new(fallback: R) -> Self {
        Self {
            overrides: HashMap::new(),
            fallback,
        }
    }

    /// Pin one skill to a mode, replacing any prior pin.
    pub fn pin(&mut self, skill: impl Into<String>, mode: SkillMode) {
        self.overrides.insert(skill.into(), mode);
    }

    /// Drop one pin, returning it when present.
    pub fn unpin(&mut self, skill: &str) -> Option<SkillMode> {
        self.overrides.remove(skill)
    }
}

impl<R: SkillRouter> SkillRouter for OverrideRouter<R> {
    fn route(&self, skill: &str, context_chars: usize) -> SkillRoute {
        if let Some(mode) = self.overrides.get(skill) {
            return SkillRoute {
                skill: skill.to_string(),
                mode: *mode,
                reason: format!("skill {skill:?} pinned to {mode:?} by operator override"),
            };
        }
        self.fallback.route(skill, context_chars)
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

    #[test]
    fn pins_win_and_unpinned_delegates() {
        let mut router = OverrideRouter::new(ThresholdRouter::new(1000));
        router.pin("heavy", SkillMode::Fork);
        router.pin("tiny", SkillMode::Inline);
        // Pins hold against the threshold in both directions.
        assert_eq!(router.route("heavy", 10).mode, SkillMode::Fork);
        assert_eq!(router.route("tiny", 99_999).mode, SkillMode::Inline);
        assert!(router.route("heavy", 10).reason.contains("pinned"));
        // Unpinned skills still delegate.
        assert_eq!(router.route("other", 10).mode, SkillMode::Inline);
        assert_eq!(router.route("other", 5000).mode, SkillMode::Fork);
        // Unpinning restores delegation.
        assert_eq!(router.unpin("heavy"), Some(SkillMode::Fork));
        assert_eq!(router.route("heavy", 10).mode, SkillMode::Inline);
        assert_eq!(router.unpin("ghost"), None);
    }
}
