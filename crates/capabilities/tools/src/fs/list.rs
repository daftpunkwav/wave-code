//! List tool: directory listings with entry limits and metadata.

use super::*;

pub struct ListDir;

#[async_trait::async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List entries of a directory inside the working directory, sorted by name; \
         directories are suffixed with /. Output is capped at 1000 entries."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list, relative to the working directory"
                }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = match req_str(&input, "path") {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let path = match resolve_path(ctx, path)? {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        // Stat metadata first: file/directory mismatches branch explicitly into business
        // output without guessing ErrorKind (on Windows, read_dir on a file yields
        // os error 267 with an unstable kind).
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(format!(
                    "directory not found: {}",
                    path.display()
                )));
            }
            Err(e) => return Err(e.into()),
        };
        if meta.is_file() {
            return Ok(err_output(format!(
                "path is a file, use read_file instead: {}",
                path.display()
            )));
        }
        let mut read_dir = match tokio::fs::read_dir(&path).await {
            Ok(r) => r,
            // A TOCTOU window exists between metadata and read_dir, so keep the NotFound branch.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(format!(
                    "directory not found: {}",
                    path.display()
                )));
            }
            Err(e) => return Err(e.into()),
        };
        let mut entries: Vec<String> = Vec::new();
        while let Some(entry) = read_dir.next_entry().await? {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await?.is_dir() {
                name.push('/');
            }
            entries.push(name);
        }
        entries.sort();
        let total = entries.len();
        entries.truncate(MAX_ENTRIES);
        let mut content = entries.join("\n");
        if total > MAX_ENTRIES {
            content.push_str(&format!(
                "\n[truncated: {} more entries]",
                total - MAX_ENTRIES
            ));
        }
        Ok(ok_output(content))
    }
}
