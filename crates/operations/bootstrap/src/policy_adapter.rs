/*!
 * @file PolicyAdapter
 * @description Adapts the legacy sandbox to the PolicyDecider seam.
 *
 * Responsibilities:
 * - Resolve tool attributes from the tool itself, never from names.
 * - Translate sandbox verdicts into policy verdict data objects.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::PolicyDecider`] implemented over `wavecode-sandbox`.
//!
//! Attribute sourcing rule: `read_only`/`destructive` come from the
//! [`wavecode_tools::Tool`] trait via registry lookup. The caller-supplied
//! flags are only a fallback for unregistered names, so policy can never
//! drift from the tool implementation by string matching.

use std::sync::Arc;

use infrastructure_base::{APPROVAL_DETAIL_TRUNCATION, truncate};
use runtime_runner::{AskKind, PolicyDecider, PolicyVerdict, ToolCall};

/// Decides policy through a legacy sandbox with registry-backed attributes.
pub struct PolicyAdapter {
    sandbox: wavecode_sandbox::Sandbox,
    registry: Arc<wavecode_tools::Registry>,
}

impl PolicyAdapter {
    /// Wrap a sandbox; the registry supplies authoritative tool attributes.
    pub fn new(
        sandbox: wavecode_sandbox::Sandbox,
        registry: Arc<wavecode_tools::Registry>,
    ) -> Self {
        Self { sandbox, registry }
    }
}

#[async_trait::async_trait]
impl PolicyDecider for PolicyAdapter {
    /// Switch the live sandbox mode; unknown names reject explicitly.
    fn set_permission_mode(&self, mode: &str) -> bool {
        match wavecode_protocol::PermissionMode::parse(mode) {
            Some(parsed) => {
                *self
                    .sandbox
                    .mode_handle()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = parsed;
                true
            }
            None => false,
        }
    }
    async fn decide(&self, call: &ToolCall) -> PolicyVerdict {
        let (read_only, destructive) = match self.registry.get(&call.name) {
            Some(tool) => (tool.is_read_only(), tool.is_destructive()),
            // Unknown names stay serial and destructive so dispatch and
            // policy both take the cautious path.
            None => (false, true),
        };
        match self
            .sandbox
            .decide(&call.name, &call.input, read_only, destructive)
        {
            wavecode_sandbox::Verdict::Allow => PolicyVerdict::Allow,
            wavecode_sandbox::Verdict::Ask { kind, detail } => PolicyVerdict::Ask {
                kind: match kind {
                    wavecode_protocol::ApprovalKind::Exec => AskKind::Exec,
                    wavecode_protocol::ApprovalKind::Write => AskKind::Write,
                    // `ApprovalKind` is non-exhaustive by protocol design:
                    // future kinds still need a prompt, so they route to
                    // the generic execution approval.
                    _ => AskKind::Exec,
                },
                detail: truncate(&detail, APPROVAL_DETAIL_TRUNCATION),
            },
            wavecode_sandbox::Verdict::Question { question, options } => {
                PolicyVerdict::Question {
                    question: truncate(&question, APPROVAL_DETAIL_TRUNCATION),
                    options,
                }
            }
            wavecode_sandbox::Verdict::Deny { reason } => PolicyVerdict::Deny { reason },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Arc<wavecode_tools::Registry> {
        Arc::new(wavecode_tools::Registry::builtin())
    }

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            name: name.to_string(),
            input,
        }
    }

    #[tokio::test]
    async fn read_only_tools_pass_without_rules() {
        let adapter = PolicyAdapter::new(
            wavecode_sandbox::Sandbox::without_rules(wavecode_protocol::PermissionMode::Guarded),
            registry(),
        );
        let verdict = adapter
            .decide(&call("read", serde_json::json!({"path": "a.txt"})))
            .await;
        assert_eq!(verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn permission_modes_switch_live_and_reject_garbage() {
        let sandbox =
            wavecode_sandbox::Sandbox::without_rules(wavecode_protocol::PermissionMode::Guarded);
        let adapter = PolicyAdapter::new(sandbox, registry());
        assert!(adapter.set_permission_mode("plan"));
        assert!(!adapter.set_permission_mode("yolo"));
    }

    #[tokio::test]
    async fn deny_rules_win_in_any_mode() {
        let adapter = PolicyAdapter::new(
            wavecode_sandbox::Sandbox::new(
                wavecode_protocol::PermissionMode::Auto,
                &[],
                &["Bash(*)".to_string()],
            )
            .unwrap(),
            registry(),
        );
        let verdict = adapter
            .decide(&call("shell", serde_json::json!({"command": "ls"})))
            .await;
        assert!(matches!(verdict, PolicyVerdict::Deny { .. }));
    }
}
