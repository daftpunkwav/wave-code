/*!
 * @file PlanPreview
 * @description Dry-run rendering of model-planned actions.
 *
 * Responsibilities:
 * - Turn sampled blocks into human-readable plan lines.
 * - Never execute anything; preview is read-only analysis.
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
/// raw input. Nothing here touches executors, policy, or approvals.
pub fn render_plan(blocks: &[SampleBlock]) -> Vec<String> {
    blocks
        .iter()
        .map(|block| match block {
            SampleBlock::Text(text) => format!("say: {text}"),
            SampleBlock::ToolUse {
                call_id,
                name,
                input,
            } => {
                format!("run {name} ({call_id}) with {input}")
            }
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
}
