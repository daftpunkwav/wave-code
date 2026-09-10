/*!
 * @file CapabilityKit
 * @description Tool registry, tool attributes, and execution context.
 *
 * Responsibilities:
 * - Register tools under stable string identifiers.
 * - Carry declarative tool attributes (read-only, destructive, kind).
 * - Provide the execution context (working directory, denied env).
 *
 * This module must not depend on: runtime, state, safety, operations,
 * transport, or any orchestration layer.
 */

//! Capability registry with attribute-driven policy inputs.
//!
//! Policy decisions consume [`ToolAttrs`] structs passed by the caller. This
//! crate never matches tool names with string literals, so adding a tool
//! cannot silently drift the policy layer.

use std::collections::HashMap;
use std::path::PathBuf;

/// Functional kind of a tool, used for policy routing and observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Reads files, directories, or search indexes without mutation.
    Read,
    /// Creates, modifies, or deletes files.
    Write,
    /// Executes commands or code.
    Exec,
    /// Touches the network.
    Network,
    /// Anything that does not fit the kinds above.
    Other,
}

/// Declarative attributes of one tool.
///
/// The runtime passes this struct to the policy layer instead of letting
/// policy re-derive attributes from the tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolAttrs {
    /// True when the tool never mutates state.
    pub read_only: bool,
    /// True when the tool can destroy user data.
    pub destructive: bool,
    /// Functional kind of the tool.
    pub kind: ToolKind,
}

/// Execution context handed to every tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCtx {
    /// Working directory all relative paths resolve against.
    pub cwd: PathBuf,
    /// Environment variable names that must be scrubbed before execution.
    pub deny_env: Vec<String>,
}

impl ToolCtx {
    /// True when the variable must not leak into the tool environment.
    pub fn is_env_denied(&self, name: &str) -> bool {
        self.deny_env.iter().any(|d| d == name)
    }
}

/// Static description of one registered tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// Stable identifier; also the model-visible tool name.
    pub name: String,
    /// One-line description shown to the model.
    pub description: String,
    /// Declarative attributes consumed by the policy layer.
    pub attrs: ToolAttrs,
}

/// Registry errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A tool was registered twice under the same name.
    #[error("duplicate tool registration: {0}")]
    Duplicate(String),
}

/// Ordered registry of tool specifications.
#[derive(Debug, Default)]
pub struct Registry {
    order: Vec<String>,
    specs: HashMap<String, ToolSpec>,
}

impl Registry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one tool; duplicates are rejected to keep names stable.
    pub fn register(&mut self, spec: ToolSpec) -> Result<(), RegistryError> {
        if self.specs.contains_key(&spec.name) {
            return Err(RegistryError::Duplicate(spec.name));
        }
        self.order.push(spec.name.clone());
        self.specs.insert(spec.name.clone(), spec);
        Ok(())
    }

    /// Look up one tool by name.
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.specs.get(name)
    }

    /// All registered specs in registration order.
    pub fn specs(&self) -> Vec<&ToolSpec> {
        self.order
            .iter()
            .filter_map(|name| self.specs.get(name))
            .collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// True when no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "test tool".to_string(),
            attrs: ToolAttrs {
                read_only: true,
                destructive: false,
                kind: ToolKind::Read,
            },
        }
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mut registry = Registry::new();
        registry.register(read_spec("read_file")).unwrap();
        let err = registry.register(read_spec("read_file")).unwrap_err();
        assert_eq!(err, RegistryError::Duplicate("read_file".to_string()));
    }

    #[test]
    fn specs_keep_registration_order() {
        let mut registry = Registry::new();
        registry.register(read_spec("b")).unwrap();
        registry.register(read_spec("a")).unwrap();
        let names: Vec<_> = registry.specs().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a"]);
    }

    #[test]
    fn denied_env_names_match_exactly() {
        let ctx = ToolCtx {
            cwd: PathBuf::from("/tmp"),
            deny_env: vec!["SECRET_KEY".to_string()],
        };
        assert!(ctx.is_env_denied("SECRET_KEY"));
        assert!(!ctx.is_env_denied("SECRET_KEY_EXTRA"));
    }
}
