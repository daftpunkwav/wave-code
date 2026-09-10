/*!
 * @file CapabilityRegistry
 * @description Capability inventory with pluggable discovery sources.
 *
 * Responsibilities:
 * - Register capabilities under stable identifiers.
 * - List capabilities by kind for prompt and policy wiring.
 * - Merge discoveries with later sources winning identifier clashes.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Capability inventory: what the harness may offer, not how it runs.

/// Functional kind of one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityKind {
    /// Executable tool.
    Tool,
    /// Prompt skill pack.
    Skill,
    /// Language model route.
    Model,
    /// External transport (stdio, HTTP, gateway).
    Transport,
    /// Lifecycle hook bundle.
    Hook,
}

/// One discoverable capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// Stable identifier, e.g. a tool name or skill name.
    pub id: String,
    /// Functional kind.
    pub kind: CapabilityKind,
    /// One-line description for catalogs.
    pub description: String,
    /// Capability version for compatibility checks.
    pub version: u32,
}

/// Registry errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityError {
    /// The identifier is already registered.
    #[error("duplicate capability: {0}")]
    Duplicate(String),
}

/// Ordered registry of capabilities.
#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    order: Vec<String>,
    entries: std::collections::HashMap<String, Capability>,
}

impl CapabilityRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one capability; duplicates are rejected.
    pub fn register(&mut self, capability: Capability) -> Result<(), CapabilityError> {
        if self.entries.contains_key(&capability.id) {
            return Err(CapabilityError::Duplicate(capability.id));
        }
        self.order.push(capability.id.clone());
        self.entries.insert(capability.id.clone(), capability);
        Ok(())
    }

    /// Look up one capability by id.
    pub fn get(&self, id: &str) -> Option<&Capability> {
        self.entries.get(id)
    }

    /// All capabilities of one kind in registration order.
    pub fn list_by_kind(&self, kind: CapabilityKind) -> Vec<&Capability> {
        self.order
            .iter()
            .filter_map(|id| self.entries.get(id))
            .filter(|c| c.kind == kind)
            .collect()
    }

    /// Number of registered capabilities.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// True when nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// One discovery source, e.g. builtin tools, project skills, MCP servers.
pub trait CapabilitySource: Send + Sync {
    /// Enumerate the capabilities this source offers.
    fn discover(&self) -> Vec<Capability>;
}

/// Merge discoveries in order; later sources win identifier clashes.
///
/// Clashes are expected (project skills shadow builtin names), so they
/// resolve silently with source order as precedence.
pub fn merge_discoveries(sources: &[&dyn CapabilitySource]) -> Vec<Capability> {
    let mut merged: std::collections::HashMap<String, Capability> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for source in sources {
        for capability in source.discover() {
            if !merged.contains_key(&capability.id) {
                order.push(capability.id.clone());
            }
            merged.insert(capability.id.clone(), capability);
        }
    }
    order
        .into_iter()
        .filter_map(|id| merged.remove(&id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str) -> Capability {
        Capability {
            id: id.to_string(),
            kind: CapabilityKind::Tool,
            description: "t".to_string(),
            version: 1,
        }
    }

    struct StaticSource(Vec<Capability>);

    impl CapabilitySource for StaticSource {
        fn discover(&self) -> Vec<Capability> {
            self.0.clone()
        }
    }

    #[test]
    fn duplicates_rejected_and_kinds_listed() {
        let mut registry = CapabilityRegistry::new();
        registry.register(tool("shell")).unwrap();
        assert_eq!(
            registry.register(tool("shell")).unwrap_err(),
            CapabilityError::Duplicate("shell".to_string())
        );
        assert_eq!(registry.list_by_kind(CapabilityKind::Tool).len(), 1);
        assert!(registry.list_by_kind(CapabilityKind::Skill).is_empty());
    }

    #[test]
    fn later_sources_shadow_identifier_clashes() {
        let builtin = StaticSource(vec![tool("grep")]);
        let project = StaticSource(vec![Capability {
            id: "grep".to_string(),
            kind: CapabilityKind::Tool,
            description: "project override".to_string(),
            version: 2,
        }]);
        let merged = merge_discoveries(&[&builtin, &project]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].version, 2);
        assert_eq!(merged[0].description, "project override");
    }
}
