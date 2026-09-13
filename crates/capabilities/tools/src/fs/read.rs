//! Read tool: file contents with line/byte budgets and truncation markers.

use super::*;

pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file inside the working directory. Output is capped at 2000 lines \
         / 50KB and marked [truncated] when cut. Use offset/limit to page through large files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to read, relative to the working directory"
                },
                "offset": {
                    "type": "integer",
                    "description": "0-based line number to start reading from (default 0)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default 2000, hard cap 2000)"
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
        // Stat metadata first: directory/file mismatches and oversized files branch explicitly into business output without guessing ErrorKind.
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(format!("file not found: {}", path.display())));
            }
            Err(e) => return Err(e.into()),
        };
        if meta.is_dir() {
            return Ok(err_output(format!(
                "path is a directory, use list_dir instead: {}",
                path.display()
            )));
        }
        // Input guard: files over 4 MB are not read wholesale; tell the model to page through them.
        if meta.len() > MAX_READ_BYTES {
            return Ok(err_output(format!(
                "file too large ({} bytes), use offset/limit to read parts: {}",
                meta.len(),
                path.display()
            )));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            // A TOCTOU window exists between metadata and read, so keep the NotFound branch.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(format!("file not found: {}", path.display())));
            }
            Err(e) => return Err(e.into()),
        };
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => {
                return Ok(err_output(format!(
                    "file is not valid UTF-8 (binary file?): {}",
                    path.display()
                )));
            }
        };

        let offset = match opt_usize(&input, "offset") {
            Ok(o) => o.unwrap_or(0),
            Err(out) => return Ok(out),
        };
        let limit = match opt_usize(&input, "limit") {
            Ok(l) => l.unwrap_or(MAX_LINES),
            Err(out) => return Ok(out),
        };
        if limit == 0 {
            return Ok(err_output("invalid parameter 'limit' (must be >= 1)"));
        }
        let limit = limit.min(MAX_LINES);
        let total = text.lines().count();
        // Bounds guard: an empty file with offset>0 also branches into a business failure here, ruling out a downstream usize underflow.
        if offset > 0 && offset >= total {
            return Ok(err_output(format!(
                "offset {offset} is beyond end of file ({total} lines)"
            )));
        }
        let end = offset.saturating_add(limit).min(total);
        // Without slicing return the text as-is, preserving original newlines and the ending; with slicing rejoin lines with \n.
        let mut content = if offset == 0 && end == total {
            text
        } else {
            text.lines()
                .skip(offset)
                .take(end.saturating_sub(offset))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut truncated = end < total;
        if content.len() > MAX_BYTES {
            let mut cut = MAX_BYTES;
            while !content.is_char_boundary(cut) {
                cut -= 1;
            }
            content.truncate(cut);
            truncated = true;
        }
        if truncated {
            content.push_str("\n[truncated]");
        }
        Ok(ok_output(content))
    }
}
