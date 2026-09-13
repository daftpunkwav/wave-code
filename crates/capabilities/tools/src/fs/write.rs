//! Write tool: create or overwrite files with atomic write and size caps.

use super::*;

pub struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write a file inside the working directory, creating parent directories as needed. \
         Overwrites the whole file; use edit_file for partial changes."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to write, relative to the working directory"
                },
                "content": {
                    "type": "string",
                    "description": "Full content to write; the file is created or completely overwritten"
                }
            },
            "required": ["path", "content"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = match req_str(&input, "path") {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let content = match req_str(&input, "content") {
            Ok(c) => c,
            Err(out) => return Ok(out),
        };
        // Write-side guard: oversized content neither creates nor overwrites the file.
        if content.len() > MAX_WRITE_BYTES {
            return Ok(err_output(format!(
                "content too large ({} bytes, max {MAX_WRITE_BYTES})",
                content.len()
            )));
        }
        let path = match resolve_path(ctx, path)? {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
            // A directory path returns a self-correctable business message (same branching
            // shape as read_file / edit_file); tokio::fs::write on a directory returns an
            // implementation-level Err.
            return Ok(err_output(format!(
                "path is a directory, cannot write: {}",
                path.display()
            )));
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Atomic write: a mid-write failure leaves the existing file intact (temp+rename, see atomic_write).
        super::atomic_write(&path, content).await?;
        Ok(ok_output(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        )))
    }
}
