/*!
 * @file SnapshotTools
 * @description Snapshot (read-only) and restore (destructive) tools.
 *
 * Responsibilities:
 * - Expose the checkpoint file-content snapshot store as model tools.
 * - Keep snapshot policy flags explicit (snapshot is read-only, restore
 *   is destructive and approval-gated).
 * - Re-export the store surface for session assembly and slash display.
 *
 * This module must not depend on: git, the network, or any crate beyond
 * the tools crate's existing dependencies plus state-checkpoint.
 */

//! Snapshot tools: thin [`Tool`] adapters over the checkpoint crate's
//! file-content snapshot store; the walk, caps, and rewind semantics live
//! in state-checkpoint, this module only maps tool input/output.

use std::path::PathBuf;

use serde_json::{Value, json};

pub use state_checkpoint::{
    MAX_FILE_BYTES, MAX_FILE_COUNT, MAX_TOTAL_BYTES, SnapshotCaps, SnapshotCreateReport,
    SnapshotInfo, SnapshotRestoreReport, SnapshotStore, default_snapshot_store_root,
    snapshot_store_root_for_session, validate_snapshot_label,
};

use crate::{Result, Tool, ToolCtx, ToolOutput, ToolsError, err_output, req_str};

/// Map a store error onto the tools error surface: business failures
/// (bad label, unknown snapshot, corrupt manifest) feed back to the
/// model, implementation failures propagate as `Err`.
fn map_store_error(e: state_checkpoint::SnapshotError) -> Result<ToolOutput> {
    match e {
        state_checkpoint::SnapshotError::InvalidInput { message } => Ok(err_output(message)),
        state_checkpoint::SnapshotError::Io(e) => Err(ToolsError::Io(e)),
    }
}

/// Capture a recoverable file-content snapshot of the working directory.
/// Read-only: it only reads `cwd` files and writes outside `cwd` to the
/// home-scoped store, so planning agents can capture freely before risk.
pub struct SnapshotTool {
    store: SnapshotStore,
}

impl SnapshotTool {
    /// Build with an explicit store root.
    pub fn new(store_root: PathBuf) -> Self {
        Self {
            store: SnapshotStore::new(store_root),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SnapshotTool {
    fn name(&self) -> &str {
        "snapshot"
    }

    fn description(&self) -> &str {
        "Capture a recoverable file-content snapshot of the working directory (text files only; \
         binaries, .git, target, node_modules and .venv are skipped, per-file 512KB / 1000 files / \
         64MB caps apply). Snapshots live outside the working directory and never depend on git. \
         Call this before risky edits so 'restore' can rewind afterwards; the result reports file \
         counts and any caps hit."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Snapshot label ([A-Za-z0-9_-], max 64 chars); re-capturing a label overwrites it"
                }
            },
            "required": ["label"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let label = match req_str(&input, "label") {
            Ok(label) => label.to_owned(),
            Err(out) => return Ok(out),
        };
        match self.store.create(&ctx.cwd, &label).await {
            Ok(report) => Ok(ToolOutput {
                content: report.summary(),
                is_error: false,
            }),
            Err(e) => map_store_error(e),
        }
    }
}

/// Rewind working-directory files to a named snapshot. Destructive (it
/// overwrites files), so policy approval gates it; files changed after the
/// snapshot are refused unless `force` is true, and every file is reported.
pub struct RestoreTool {
    store: SnapshotStore,
}

impl RestoreTool {
    /// Build with an explicit store root.
    pub fn new(store_root: PathBuf) -> Self {
        Self {
            store: SnapshotStore::new(store_root),
        }
    }
}

#[async_trait::async_trait]
impl Tool for RestoreTool {
    fn name(&self) -> &str {
        "restore"
    }

    fn description(&self) -> &str {
        "Rewind working-directory files to a named file-content snapshot (see the 'snapshot' \
         tool). Files created after the snapshot are left alone; files whose content changed \
         after the snapshot are refused unless 'force' is true. The result reports per-file \
         outcomes (written, unchanged, skipped, missing)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Snapshot label to rewind to"
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite files changed after the snapshot (default false)"
                }
            },
            "required": ["label"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let label = match req_str(&input, "label") {
            Ok(label) => label.to_owned(),
            Err(out) => return Ok(out),
        };
        let force = match input.get("force") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(force)) => *force,
            Some(_) => return Ok(err_output("invalid parameter 'force' (boolean required)")),
        };
        match self.store.restore(&ctx.cwd, &label, force).await {
            Ok(report) => Ok(ToolOutput {
                content: report.summary(),
                is_error: false,
            }),
            Err(e) => map_store_error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_ctx(cwd: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: cwd.to_path_buf(),
            deny_env: Vec::new(),
        }
    }

    fn write_file(path: &std::path::Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Tool flags: snapshot is read-only and safe; restore is destructive.
    #[test]
    fn tool_policy_flags() {
        let root = PathBuf::from("/tmp/snapshots-test");
        assert!(SnapshotTool::new(root.clone()).is_read_only());
        assert!(!SnapshotTool::new(root.clone()).is_destructive());
        assert!(!RestoreTool::new(root.clone()).is_read_only());
        assert!(RestoreTool::new(root).is_destructive());
    }

    /// Tools round trip through the Tool trait with counts in the output.
    #[tokio::test]
    async fn tools_report_counts() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = tool_ctx(cwd.path());
        let snapshot = SnapshotTool::new(root.path().to_path_buf());
        write_file(&cwd.path().join("a.txt"), b"v1");
        let out = snapshot
            .execute(json!({"label": "t1"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("1 files"));
        let restore = RestoreTool::new(root.path().to_path_buf());
        write_file(&cwd.path().join("a.txt"), b"v2");
        let out = restore.execute(json!({"label": "t1"}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("1 skipped"));
    }

    /// Invalid labels and unknown snapshots surface as business failures;
    /// mistyped `force` is rejected before touching the filesystem.
    #[tokio::test]
    async fn tool_input_errors_are_business_failures() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = tool_ctx(cwd.path());
        let snapshot = SnapshotTool::new(root.path().to_path_buf());
        let bad = snapshot
            .execute(json!({"label": "../x"}), &ctx)
            .await
            .unwrap();
        assert!(bad.is_error);
        let restore = RestoreTool::new(root.path().to_path_buf());
        let unknown = restore
            .execute(json!({"label": "missing"}), &ctx)
            .await
            .unwrap();
        assert!(unknown.is_error);
        assert!(unknown.content.contains("unknown snapshot"));
        write_file(&cwd.path().join("a.txt"), b"a");
        snapshot
            .execute(json!({"label": "two"}), &ctx)
            .await
            .unwrap();
        let bad_force = restore
            .execute(json!({"label": "two", "force": "yes"}), &ctx)
            .await
            .unwrap();
        assert!(bad_force.is_error);
    }
}
