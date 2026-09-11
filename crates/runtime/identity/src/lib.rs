/*!
 * @file AgentIdentity
 * @description Agent profile, role, and persona declarations.
 *
 * Responsibilities:
 * - Declare who an agent acts as (role plus persona text).
 * - List the capability ids the profile may use.
 * - Validate profiles before a session starts.
 *
 * This module must not depend on: any other workspace crate. Behaviour
 * selection from profiles belongs to upper layers.
 */

//! Identity as data: validated declarations, no behaviour.

/// Functional role of one agent profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRole {
    /// General-purpose assistant.
    Assistant,
    /// Reviews plans and code without executing.
    Reviewer,
    /// Decomposes goals into plans only.
    Planner,
    /// Executes plans without replanning.
    Executor,
    /// Caller-defined role with a free-form name.
    Custom(String),
}

impl AgentRole {
    /// Stable string for logs and transcripts.
    pub fn as_str(&self) -> String {
        match self {
            Self::Assistant => "assistant".to_string(),
            Self::Reviewer => "reviewer".to_string(),
            Self::Planner => "planner".to_string(),
            Self::Executor => "executor".to_string(),
            Self::Custom(name) => format!("custom:{name}"),
        }
    }
}

/// Declared identity of one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfile {
    /// Profile name, unique per deployment.
    pub name: String,
    /// Functional role.
    pub role: AgentRole,
    /// Persona instructions prepended to the system prompt.
    pub persona: String,
    /// Capability ids this profile may use; empty means unrestricted.
    pub capabilities: Vec<String>,
}

impl AgentProfile {
    /// True when the named capability is available to this profile.
    pub fn may_use(&self, capability: &str) -> bool {
        self.capabilities.is_empty() || self.capabilities.iter().any(|c| c == capability)
    }

    /// Reject blank names, blank personas, and blank or duplicate
    /// capability entries before sessions start.
    pub fn validate(&self) -> Result<(), IdentityError> {
        if self.name.trim().is_empty() {
            return Err(IdentityError::BlankName);
        }
        if self.persona.trim().is_empty() {
            return Err(IdentityError::BlankPersona);
        }
        let mut seen = std::collections::HashSet::new();
        for capability in &self.capabilities {
            if capability.trim().is_empty() {
                return Err(IdentityError::BlankCapability);
            }
            if !seen.insert(capability) {
                return Err(IdentityError::DuplicateCapability(capability.clone()));
            }
        }
        Ok(())
    }
}

/// Identity validation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// Profile names must not be blank.
    #[error("agent profile name must not be blank")]
    BlankName,
    /// Personas must not be blank.
    #[error("agent persona must not be blank")]
    BlankPersona,
    /// Capability entries must not be blank.
    #[error("agent capability entry must not be blank")]
    BlankCapability,
    /// Capability entries must not repeat.
    #[error("duplicate agent capability: {0}")]
    DuplicateCapability(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> AgentProfile {
        AgentProfile {
            name: "reviewer-1".to_string(),
            role: AgentRole::Reviewer,
            persona: "You review plans.".to_string(),
            capabilities: vec!["read_file".to_string()],
        }
    }

    #[test]
    fn capability_gates_hold() {
        let profile = profile();
        assert!(profile.may_use("read_file"));
        assert!(!profile.may_use("shell"));
        let open = AgentProfile {
            capabilities: Vec::new(),
            ..profile
        };
        assert!(open.may_use("anything"));
    }

    #[test]
    fn blank_fields_fail_validation() {
        assert!(profile().validate().is_ok());
        assert_eq!(
            AgentProfile {
                name: "  ".to_string(),
                ..profile()
            }
            .validate()
            .unwrap_err(),
            IdentityError::BlankName
        );
        assert_eq!(
            AgentProfile {
                persona: String::new(),
                ..profile()
            }
            .validate()
            .unwrap_err(),
            IdentityError::BlankPersona
        );
    }

    #[test]
    fn roles_render_stable_names() {
        assert_eq!(AgentRole::Planner.as_str(), "planner");
        assert_eq!(
            AgentRole::Custom("auditor".to_string()).as_str(),
            "custom:auditor"
        );
    }

    #[test]
    fn capability_entries_validate_for_blanks_and_dupes() {
        assert_eq!(
            AgentProfile {
                capabilities: vec!["shell".to_string(), "  ".to_string()],
                ..profile()
            }
            .validate()
            .unwrap_err(),
            IdentityError::BlankCapability
        );
        assert_eq!(
            AgentProfile {
                capabilities: vec!["shell".to_string(), "shell".to_string()],
                ..profile()
            }
            .validate()
            .unwrap_err(),
            IdentityError::DuplicateCapability("shell".to_string())
        );
    }
}
