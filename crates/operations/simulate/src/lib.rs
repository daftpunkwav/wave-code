/*!
 * @file PlanPreview
 * @description Dry-run rendering of model-planned actions.
 *
 * Unwired by intent: zero dependents — not reachable from the
 * `wavecode` binary. Kept as a deliberate seed; see the "Wiring
 * status" section of docs/architecture.md before citing or wiring.
 *
 * Library-only plan preview; not a product feature yet.
 * Responsibilities:
 * - Turn sampled blocks into human-readable plan lines.
 * - Never execute anything; preview is read-only analysis.
 * - Summarize plans by kind and list tool names in order.
 * - Share the block vocabulary with the run loop seam.
 *
 * This module must not depend on: any workspace crate except the run
 * loop seam types it renders.
 */

//! Simulation as honest preview: what WOULD run, printed plainly.

use runtime_runner::SampleBlock;

/// Render planned blocks as one dry-run line each.
///
/// Text blocks preview as speech, tool blocks as invocations with their
/// raw input. Result blocks never appear in a plan (results exist only
/// after execution) and are skipped. Nothing here touches executors,
/// policy, or approvals.
pub fn render_plan(blocks: &[SampleBlock]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|block| match block {
            SampleBlock::Text(text) => Some(format!("say: {text}")),
            SampleBlock::ToolUse {
                call_id,
                name,
                input,
            } => Some(format!("run {name} ({call_id}) with {input}")),
            SampleBlock::ToolResult { .. }
            | SampleBlock::Image { .. }
            | SampleBlock::Thinking { .. } => None,
        })
        .collect()
}

/// Counts of one dry-run plan, in block order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanSummary {
    /// Speech (text) blocks.
    pub speeches: usize,
    /// Tool invocation blocks.
    pub tool_calls: usize,
}

impl PlanSummary {
    /// Total blocks summarized.
    pub fn total(&self) -> usize {
        self.speeches + self.tool_calls
    }

    /// True when the plan holds no blocks.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Count one plan by block kind without rendering any lines.
pub fn summarize(blocks: &[SampleBlock]) -> PlanSummary {
    let mut summary = PlanSummary::default();
    for block in blocks {
        match block {
            SampleBlock::Text(_) => summary.speeches += 1,
            SampleBlock::ToolUse { .. } => summary.tool_calls += 1,
            SampleBlock::ToolResult { .. }
            | SampleBlock::Image { .. }
            | SampleBlock::Thinking { .. } => {}
        }
    }
    summary
}

/// Tool names of one plan in block order (repeats kept).
///
/// Lets reviewers see at a glance which tools WOULD run; speech and
/// result blocks contribute nothing.
pub fn tool_names(blocks: &[SampleBlock]) -> Vec<&str> {
    blocks
        .iter()
        .filter_map(|block| match block {
            SampleBlock::ToolUse { name, .. } => Some(name.as_str()),
            SampleBlock::Text(_)
            | SampleBlock::ToolResult { .. }
            | SampleBlock::Image { .. }
            | SampleBlock::Thinking { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_blocks_render_in_order() {
        let lines = render_plan(&[
            SampleBlock::Text("checking".to_string()),
            SampleBlock::ToolUse {
                call_id: "c1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"command": "ls"}),
            },
        ]);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("say: "));
        assert!(lines[1].contains("run shell (c1)"));
    }

    #[test]
    fn empty_plans_render_empty() {
        assert!(render_plan(&[]).is_empty());
    }

    #[test]
    fn summaries_count_by_kind_and_name_tools_in_order() {
        let blocks = [
            SampleBlock::Text("checking".to_string()),
            SampleBlock::ToolUse {
                call_id: "c1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"command": "ls"}),
            },
            SampleBlock::ToolUse {
                call_id: "c2".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"command": "pwd"}),
            },
        ];
        let summary = summarize(&blocks);
        assert_eq!(
            summary,
            PlanSummary {
                speeches: 1,
                tool_calls: 2,
            }
        );
        assert_eq!(summary.total(), 3);
        assert!(!summary.is_empty());
        assert_eq!(tool_names(&blocks), vec!["shell", "shell"]);
        assert!(summarize(&[]).is_empty());
        assert!(tool_names(&[]).is_empty());
    }
}
