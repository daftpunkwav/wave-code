/*!
 * @file HookAdapter
 * @description Adapts the legacy hook engine to the HookGateway seam.
 *
 * Responsibilities:
 * - Map lifecycle points onto hook event points one to one.
 * - Translate hook verdicts and warnings into gateway reports.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::HookGateway`] implemented over `wavecode-hooks`.

use std::path::PathBuf;
use std::sync::Arc;

use runtime_runner::{HookGateway, HookPoint, HookReport as GatewayReport};

/// Runs legacy hooks for run loop lifecycle points.
pub struct HookAdapter {
    engine: Arc<wavecode_hooks::HookEngine>,
    cwd: PathBuf,
}

impl HookAdapter {
    /// Wrap a shared engine; hooks execute with `cwd` as working directory.
    pub fn new(engine: Arc<wavecode_hooks::HookEngine>, cwd: PathBuf) -> Self {
        Self { engine, cwd }
    }

    /// Map a run loop lifecycle point onto a hook event point.
    fn event_point(point: HookPoint) -> wavecode_hooks::HookEventPoint {
        match point {
            HookPoint::PreToolUse => wavecode_hooks::HookEventPoint::PreToolUse,
            HookPoint::PostToolUse => wavecode_hooks::HookEventPoint::PostToolUse,
            HookPoint::PromptSubmit => wavecode_hooks::HookEventPoint::UserPromptSubmit,
            HookPoint::SessionStart => wavecode_hooks::HookEventPoint::SessionStart,
            HookPoint::SessionEnd => wavecode_hooks::HookEventPoint::SessionEnd,
            HookPoint::Stop => wavecode_hooks::HookEventPoint::Stop,
            HookPoint::PreCompact => wavecode_hooks::HookEventPoint::PreCompact,
            HookPoint::PostCompact => wavecode_hooks::HookEventPoint::PostCompact,
        }
    }

    /// Run hooks for tool-scoped points with full call context.
    ///
    /// The seam-level [`HookGateway::run`] cannot carry tool context (its
    /// payload has no tool slot in the legacy input shape), so tool points
    /// must go through this method to preserve matcher filtering.
    pub async fn run_for_tool(
        &self,
        point: HookPoint,
        tool_name: &str,
        tool_input: &serde_json::Value,
        tool_output: Option<&str>,
    ) -> GatewayReport {
        let input = wavecode_hooks::HookInput {
            cwd: &self.cwd,
            tool_name: Some(tool_name),
            tool_input: Some(tool_input),
            tool_output,
        };
        Self::translate(self.engine.run(Self::event_point(point), &input).await)
    }

    /// Translate a legacy hook report into a gateway report.
    fn translate(report: wavecode_hooks::HookReport) -> GatewayReport {
        match report.verdict {
            wavecode_hooks::HookVerdict::Allow => GatewayReport {
                allow: true,
                message: report.warnings.join("\n"),
            },
            wavecode_hooks::HookVerdict::Block(reason) => GatewayReport {
                allow: false,
                message: reason,
            },
        }
    }
}

#[async_trait::async_trait]
impl HookGateway for HookAdapter {
    async fn run(&self, point: HookPoint, _payload: &str) -> GatewayReport {
        // Session-scoped points carry no tool context; the payload is
        // reserved for a future input shape and intentionally unused.
        let input = wavecode_hooks::HookInput {
            cwd: &self.cwd,
            tool_name: None,
            tool_input: None,
            tool_output: None,
        };
        Self::translate(self.engine.run(Self::event_point(point), &input).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_engine_allows_everything() {
        let adapter = HookAdapter::new(
            Arc::new(wavecode_hooks::HookEngine::new(
                std::collections::HashMap::new(),
            )),
            std::path::PathBuf::from("/tmp"),
        );
        let report = adapter.run(HookPoint::PromptSubmit, "hello").await;
        assert!(report.allow);
        assert!(report.message.is_empty());
    }
}
