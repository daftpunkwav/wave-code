//! 文件工具（阶段 3 拆分自 fs_tools.rs）。

use super::*;

pub struct EditFile;

#[async_trait::async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file inside the working directory. \
         old_string must match exactly one location; include more context to make it unique."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to edit, relative to the working directory"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace; must occur exactly once in the file"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text"
                }
            },
            "required": ["path", "old_string", "new_string"]
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
        let old_string = match req_str(&input, "old_string") {
            Ok(s) => s,
            Err(out) => return Ok(out),
        };
        let new_string = match req_str(&input, "new_string") {
            Ok(s) => s,
            Err(out) => return Ok(out),
        };
        if old_string.is_empty() {
            return Ok(err_output("old_string must not be empty"));
        }
        let path = match resolve_path(ctx, path)? {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        // 输入侧护栏：与 read_file 对称，超过 4 MB 不整读（edit 需要全文匹配，
        // 大文件改用分段重写策略）。
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(format!("file not found: {}", path.display())));
            }
            Err(e) => return Err(e.into()),
        };
        if meta.is_dir() {
            // 与 read_file 同形态的业务分流:目录路径返回可自我纠正的
            // 错误文案,而不是实现级 Err(读目录的 io 错误非 NotFound)。
            return Ok(err_output(format!(
                "path is a directory, cannot edit: {}",
                path.display()
            )));
        }
        if meta.len() > MAX_READ_BYTES {
            return Ok(err_output(format!(
                "file too large ({} bytes), refusing to edit: {}",
                meta.len(),
                path.display()
            )));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
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
        match text.matches(old_string).count() {
            0 => Ok(err_output(format!(
                "old_string not found in {}",
                path.display()
            ))),
            1 => {
                let updated = text.replacen(old_string, new_string, 1);
                tokio::fs::write(&path, updated).await?;
                Ok(ok_output(format!(
                    "replaced 1 occurrence in {}",
                    path.display()
                )))
            }
            n => Ok(err_output(format!(
                "old_string is not unique in {}: {n} matches",
                path.display()
            ))),
        }
    }
}
