//! 文件工具（阶段 3 拆分自 fs_tools.rs）。

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
        // 先取 metadata：文件/目录错配显式分流为业务输出，不猜 ErrorKind
        // （Windows 上对文件 read_dir 落 os error 267，kind 不稳定）。
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
            // metadata 与 read_dir 之间存在 TOCTOU 窗口，保留 NotFound 分流
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
