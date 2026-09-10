/*!
 * @file CompositeExecutor
 * @description Merges registry tools and late-registered native tools.
 *
 * Responsibilities:
 * - Route execution to native tools first, registry tools second.
 * - Merge attributes and catalogs from both sources.
 * - Accept late native registrations behind interior mutability.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! Executor composition: one seam over two tool sources.
//!
//! Native tools register after RunLoop construction (child task tools need
//! the driver the loop owns), so the native side lives behind a mutex.
//! Dispatch itself never blocks on it beyond a short lock.

use std::sync::{Arc, Mutex};

use runtime_runner::{ToolCall, ToolExecutor, ToolRef, ToolResult};

use crate::native::NativeExecutor;

/// Executor merging a registry adapter with native tools.
pub struct CompositeExecutor {
    primary: super::ToolAdapter,
    native: Arc<Mutex<NativeExecutor>>,
    registry: Arc<wavecode_tools::Registry>,
}

impl CompositeExecutor {
    /// Merge a registry adapter, shared native tools, and the registry
    /// behind both for attribute lookups.
    pub fn new(
        primary: super::ToolAdapter,
        native: Arc<Mutex<NativeExecutor>>,
        registry: Arc<wavecode_tools::Registry>,
    ) -> Self {
        Self {
            primary,
            native,
            registry,
        }
    }

    /// Recover the native lock after a poison; registration is a single
    /// map insert with no half-written invariant.
    fn lock_native(&self) -> std::sync::MutexGuard<'_, NativeExecutor> {
        self.native.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register one native tool after construction (child task tools).
    pub fn register_native(&self, tool: crate::native::NativeTool) {
        self.lock_native().register(tool);
    }

    /// True when a native tool shadows the name.
    fn is_native(&self, tool: &str) -> bool {
        self.lock_native().contains(tool)
    }
}

#[async_trait::async_trait]
impl ToolExecutor for CompositeExecutor {
    async fn execute(&self, call: ToolCall) -> ToolResult {
        // Clone the native side out from under its lock: handlers run
        // without holding the registration mutex across awaits.
        let native = self.lock_native().clone();
        if native.contains(&call.name) {
            return native.execute(call).await;
        }
        self.primary.execute(call).await
    }

    fn is_read_only(&self, tool: &str) -> bool {
        if self.is_native(tool) {
            return self.lock_native().is_read_only(tool);
        }
        self.registry.get(tool).is_some_and(|t| t.is_read_only())
    }

    fn is_destructive(&self, tool: &str) -> bool {
        if self.is_native(tool) {
            return self.lock_native().is_destructive(tool);
        }
        self.registry.get(tool).is_none_or(|t| t.is_destructive())
    }

    fn available_tools(&self) -> Vec<ToolRef> {
        let mut refs: Vec<ToolRef> = self
            .registry
            .specs()
            .into_iter()
            .filter(|spec| !self.is_native(&spec.name))
            .map(|spec| ToolRef {
                name: spec.name.clone(),
                description: spec.description.clone(),
            })
            .collect();
        refs.extend(self.lock_native().available_tools());
        refs.sort_by(|a, b| a.name.cmp(&b.name));
        refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::NativeTool;

    fn composite() -> CompositeExecutor {
        let registry = Arc::new(wavecode_tools::Registry::builtin());
        let primary = crate::ToolAdapter::new(
            registry.clone(),
            wavecode_tools::ToolCtx {
                cwd: std::path::PathBuf::from("/tmp"),
                deny_env: Vec::new(),
            },
        );
        CompositeExecutor::new(
            primary,
            Arc::new(Mutex::new(NativeExecutor::new())),
            registry,
        )
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            name: name.to_string(),
            input: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn native_tools_shadow_and_fall_back() {
        let composite = composite();
        composite.register_native(NativeTool {
            name: "echo".to_string(),
            description: "echo".to_string(),
            read_only: true,
            destructive: false,
            handler: Arc::new(|_| ("native!".to_string(), false)),
        });
        // Native side executes with its own attributes and catalog slot.
        let out = composite.execute(call("echo")).await;
        assert_eq!(out.content, "native!");
        assert!(composite.is_read_only("echo"));
        assert!(composite.available_tools().iter().any(|t| t.name == "echo"));
        // Unknown names fall back to the registry adapter's error result.
        let missing = composite.execute(call("nope")).await;
        assert!(missing.is_error);
        // Registry attributes surface for registry tools.
        assert!(composite.is_read_only("read_file"));
    }
}
