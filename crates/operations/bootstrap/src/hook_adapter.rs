/*!
 * @file HookAdapter
 * @description Adapts the hook engine to the HookGateway seam.
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

/// Runs hooks for run loop lifecycle points.
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
    /// This backs the trait's [`HookGateway::run_tool`]: the default
    /// implementation drops the tool context, which would silently
    /// bypass matcher filtering, so tool points must go through here
    /// to preserve match semantics.
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

    /// Translate a hook report into a gateway report.
    fn translate(report: wavecode_hooks::HookReport) -> GatewayReport {
        let context = report.context;
        match report.verdict {
            wavecode_hooks::HookVerdict::Allow => GatewayReport {
                allow: true,
                message: report.warnings.join("\n"),
                context,
            },
            wavecode_hooks::HookVerdict::Block(reason) => GatewayReport {
                allow: false,
                message: reason,
                context,
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

    async fn run_tool(
        &self,
        point: HookPoint,
        tool: &str,
        input: &serde_json::Value,
        output: Option<&str>,
    ) -> GatewayReport {
        self.run_for_tool(point, tool, input, output).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Platform stdin-to-stdout copy for a piped hook (same convention
    /// as the wavecode-hooks test suite).
    fn echo_cmd() -> String {
        if cfg!(windows) { "more" } else { "cat" }.to_string()
    }

    /// Platform-independent command construction (see wavecode-hooks).
    fn exit_cmd(code: u32, stderr: &str) -> String {
        if stderr.is_empty() {
            format!("exit {code}")
        } else if cfg!(windows) {
            format!("echo {stderr} 1>&2 & exit {code}")
        } else {
            format!("echo {stderr} 1>&2; exit {code}")
        }
    }

    fn def(command: String) -> wavecode_hooks::HookDef {
        wavecode_hooks::HookDef {
            matcher: None,
            command,
            timeout_ms: wavecode_hooks::DEFAULT_TIMEOUT_MS,
            once: false,
        }
    }

    fn engine(
        defs: std::collections::HashMap<
            wavecode_hooks::HookEventPoint,
            Vec<wavecode_hooks::HookDef>,
        >,
    ) -> HookAdapter {
        // An existing working directory: hook spawns use it as their cwd
        // and fail to start on a nonexistent one.
        HookAdapter::new(
            Arc::new(wavecode_hooks::HookEngine::new(defs)),
            std::path::PathBuf::from("."),
        )
    }

    /// An adapter whose engine carries one prompt-type echo hook per
    /// given event point: prompt hooks capture their stdin payload as
    /// `context`, making what the engine received observable.
    fn echo_adapter(points: &[wavecode_hooks::HookEventPoint]) -> HookAdapter {
        let hook_engine = Arc::new(wavecode_hooks::HookEngine::new(
            std::collections::HashMap::new(),
        ));
        for point in points {
            hook_engine.register_prompt_hook(*point, def(echo_cmd()));
        }
        // An existing working directory: hook spawns use it as their cwd
        // and fail to start on a nonexistent one.
        HookAdapter::new(hook_engine, std::path::PathBuf::from("."))
    }

    #[tokio::test]
    async fn empty_engine_allows_everything() {
        let adapter = engine(std::collections::HashMap::new());
        let report = adapter.run(HookPoint::PromptSubmit, "hello").await;
        assert!(report.allow);
        assert!(report.message.is_empty());
    }

    /// Every gateway lifecycle point lands on its hook event point, so
    /// hooks observe the event they were configured for. A prompt hook
    /// echoing its stdin payload makes the mapping observable: the
    /// payload's `event` field names the event point the engine ran.
    /// `PromptSubmit` maps onto the hooks crate's `UserPromptSubmit`
    /// (the one non-identity name), and the rest map one to one.
    #[tokio::test]
    async fn lifecycle_points_map_one_to_one() {
        let adapter = echo_adapter(&wavecode_hooks::HookEventPoint::ALL);
        let cases = [
            (HookPoint::PreToolUse, "PreToolUse"),
            (HookPoint::PostToolUse, "PostToolUse"),
            (HookPoint::PromptSubmit, "UserPromptSubmit"),
            (HookPoint::SessionStart, "SessionStart"),
            (HookPoint::SessionEnd, "SessionEnd"),
            (HookPoint::Stop, "Stop"),
            (HookPoint::PreCompact, "PreCompact"),
            (HookPoint::PostCompact, "PostCompact"),
        ];
        for (point, expected) in cases {
            let report = adapter.run(point, "").await;
            assert!(report.allow, "{point:?} must not be vetoed by an echo");
            let payload: serde_json::Value = serde_json::from_str(&report.context)
                .unwrap_or_else(|error| panic!("{point:?} echoed one JSON payload: {error}"));
            assert_eq!(
                payload["event"], expected,
                "{point:?} must map onto the {expected} hook event point"
            );
        }
    }

    /// Tool-scoped points through the gateway seam carry the call
    /// context (tool name, input, output) into the hook payload — the
    /// default `HookGateway::run_tool` drops it, which would silently
    /// bypass matcher filtering, so the adapter must override it.
    #[tokio::test]
    async fn run_tool_carries_tool_context_to_hooks() {
        let adapter = echo_adapter(&[wavecode_hooks::HookEventPoint::PreToolUse]);
        let input = serde_json::json!({"command": "ls -la"});
        let report = adapter
            .run_tool(HookPoint::PreToolUse, "shell", &input, Some("total 4"))
            .await;
        let payload: serde_json::Value = serde_json::from_str(&report.context)
            .unwrap_or_else(|error| panic!("one JSON payload: {error}: {:?}", report.context));
        assert_eq!(payload["tool"], "shell", "{payload}");
        assert_eq!(payload["input"], input, "{payload}");
        assert_eq!(payload["output"], "total 4", "{payload}");
    }

    /// Populated engine: verdicts and warnings translate (a blocking
    /// exit 2 becomes `allow: false` with the stderr as the message, a
    /// failing exit 1 stays `allow: true` with the warning text), and
    /// matcher filtering runs on the tool name the gateway carries —
    /// a hook matched to `shell` never fires for `write_file`.
    #[tokio::test]
    async fn run_tool_applies_matchers_and_translates_verdicts() {
        let mut defs = std::collections::HashMap::new();
        defs.insert(
            wavecode_hooks::HookEventPoint::PreToolUse,
            vec![
                wavecode_hooks::HookDef {
                    matcher: Some("shell".to_string()),
                    ..def(exit_cmd(2, "blocked-by-policy"))
                },
                def(exit_cmd(1, "noisy-hook")),
            ],
        );
        let adapter = engine(defs);
        // Match: the first hook blocks and short-circuits the second.
        let report = adapter
            .run_tool(HookPoint::PreToolUse, "shell", &serde_json::json!({}), None)
            .await;
        assert!(!report.allow);
        assert_eq!(report.message, "blocked-by-policy");
        // Miss: the matcher never fires, the unfiltered hook warns only.
        let report = adapter
            .run_tool(
                HookPoint::PreToolUse,
                "write_file",
                &serde_json::json!({}),
                None,
            )
            .await;
        assert!(report.allow, "exit code 1 must not block");
        assert!(report.message.contains("exited with code 1"), "{report:?}");
        assert!(report.message.contains("noisy-hook"), "{report:?}");
        assert!(!report.message.contains("blocked-by-policy"), "{report:?}");
    }
}
