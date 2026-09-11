/*!
 * @file ToolPolicy
 * @description Declarative per-tool policy with deny-wins merging.
 *
 * Responsibilities:
 * - Match tool names against exact and prefix patterns.
 * - Merge overlapping rules with Deny beating Ask beating Allow.
 * - Attribute verdicts to the winning rules for audit and UX.
 * - Validate execution-wide ceilings before a session starts.
 *
 * This module must not depend on: any other workspace crate. Verdict
 * delivery (asking users, parking waits) belongs to upper layers.
 */

//! Static policy: what may run, decided from data, never from drumbeats.

/// Policy effect for one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Run without asking.
    Allow,
    /// Ask the user first.
    Ask,
    /// Refuse with the rule as reason.
    Deny,
}

/// One pattern rule: exact names, or `prefix*` wildcards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Pattern: `shell` matches exactly, `mcp__*` matches the prefix.
    pub pattern: String,
    /// Effect when the pattern matches.
    pub effect: Effect,
}

impl Rule {
    /// True when the pattern matches the tool name.
    pub fn matches(&self, tool: &str) -> bool {
        if let Some(prefix) = self.pattern.strip_suffix('*') {
            return tool.starts_with(prefix);
        }
        self.pattern == tool
    }
}

/// Ordered rule list evaluated with deny-wins merging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Rules in configuration order; order does not affect outcomes.
    pub rules: Vec<Rule>,
}

impl ToolPolicy {
    /// Create an empty policy (everything falls to the default).
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate one tool: the strongest matching effect wins
    /// (Deny over Ask over Allow); unmatched tools default to Ask so a
    /// missing rule can never silently permit execution.
    pub fn evaluate(&self, tool: &str) -> Effect {
        let mut effect: Option<Effect> = None;
        for rule in &self.rules {
            if !rule.matches(tool) {
                continue;
            }
            effect = Some(match (effect, rule.effect) {
                (Some(Effect::Deny), _) | (_, Effect::Deny) => Effect::Deny,
                (Some(Effect::Ask), _) | (_, Effect::Ask) => Effect::Ask,
                _ => Effect::Allow,
            });
        }
        effect.unwrap_or(Effect::Ask)
    }

    /// Evaluate one tool and name the rules behind the verdict.
    ///
    /// Returns the same effect as [`ToolPolicy::evaluate`] plus every
    /// matching rule in configuration order, so audit trails and denial
    /// messages can cite the exact rules instead of a bare verdict.
    /// Unmatched tools report the default Ask with an empty rule list.
    pub fn explain(&self, tool: &str) -> (Effect, Vec<&Rule>) {
        let matched: Vec<&Rule> = self.rules.iter().filter(|r| r.matches(tool)).collect();
        (self.evaluate(tool), matched)
    }
}

/// Execution-wide ceilings validated before a session starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPolicy {
    /// Tool dispatch rounds per turn.
    pub max_tool_rounds: u32,
    /// Approval wait timeout in seconds.
    pub approval_timeout_secs: u64,
    /// Consecutive tool errors before the turn aborts.
    pub max_consecutive_errors: u32,
}

/// Policy validation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// A ceiling is zero and would freeze or spin the loop.
    #[error("non-positive ceiling: {0}")]
    NonPositiveCeiling(&'static str),
}

impl ExecutionPolicy {
    /// Sensible production defaults.
    pub fn defaults() -> Self {
        Self {
            max_tool_rounds: 32,
            approval_timeout_secs: 120,
            max_consecutive_errors: 5,
        }
    }

    /// Reject degenerate ceilings that would freeze or spin the loop.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.max_tool_rounds == 0 {
            return Err(PolicyError::NonPositiveCeiling("max_tool_rounds"));
        }
        if self.approval_timeout_secs == 0 {
            return Err(PolicyError::NonPositiveCeiling("approval_timeout_secs"));
        }
        if self.max_consecutive_errors == 0 {
            return Err(PolicyError::NonPositiveCeiling("max_consecutive_errors"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ToolPolicy {
        ToolPolicy {
            rules: vec![
                Rule {
                    pattern: "mcp__*".to_string(),
                    effect: Effect::Ask,
                },
                Rule {
                    pattern: "shell".to_string(),
                    effect: Effect::Ask,
                },
                Rule {
                    pattern: "shell".to_string(),
                    effect: Effect::Deny,
                },
                Rule {
                    pattern: "read_file".to_string(),
                    effect: Effect::Allow,
                },
            ],
        }
    }

    #[test]
    fn deny_beats_ask_beats_allow_and_unknown_asks() {
        let policy = policy();
        assert_eq!(policy.evaluate("shell"), Effect::Deny);
        assert_eq!(policy.evaluate("mcp__playwright__click"), Effect::Ask);
        assert_eq!(policy.evaluate("read_file"), Effect::Allow);
        assert_eq!(policy.evaluate("brand_new_tool"), Effect::Ask);
    }

    #[test]
    fn explain_cites_winning_rules_in_order() {
        let policy = policy();
        let (effect, rules) = policy.explain("shell");
        assert_eq!(effect, Effect::Deny);
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().all(|r| r.pattern == "shell"));
        // Verdicts always agree with evaluate.
        let (effect, rules) = policy.explain("mcp__x__y");
        assert_eq!(effect, policy.evaluate("mcp__x__y"));
        assert_eq!(rules.len(), 1);
        let (effect, rules) = policy.explain("brand_new_tool");
        assert_eq!(effect, Effect::Ask);
        assert!(rules.is_empty());
    }

    #[test]
    fn ceilings_reject_degenerate_values() {
        assert!(ExecutionPolicy::defaults().validate().is_ok());
        let bad = ExecutionPolicy {
            max_tool_rounds: 0,
            ..ExecutionPolicy::defaults()
        };
        assert_eq!(
            bad.validate().unwrap_err(),
            PolicyError::NonPositiveCeiling("max_tool_rounds")
        );
    }
}
