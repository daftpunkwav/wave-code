//! 四个内置文件工具：`read_file` / `write_file` / `edit_file` / `list_dir`。
//! 所有路径经 [`crate::path_guard::resolve`] 约束在 `ToolCtx::cwd` 之下。
//! 失败语义：业务失败（文件不存在、匹配不唯一、参数缺失/类型错、路径逃逸）
//! 返回 `Ok(is_error=true)` 把原因回给模型；`Err` 仅用于 io 等实现级故障。

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput, ToolsError};

/// read_file 输出上限：2000 行 / 50 KB。
const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
/// read_file 输入侧硬上限：文件超过 4 MB 直接拒绝，避免整读占内存。
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
/// write_file 写入上限：content 超过 10 MB 直接拒绝。
const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024;
/// list_dir 单目录条目上限。
const MAX_ENTRIES: usize = 1000;

/// 构造正常输出。
fn ok_output(content: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: content.into(),
        is_error: false,
    }
}

/// 构造业务失败输出：原因回灌给模型自我纠正。
fn err_output(reason: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: reason.into(),
        is_error: true,
    }
}

/// 提取必填 string 参数；缺失或类型错误时给出业务失败输出。
fn req_str<'a>(input: &'a Value, key: &str) -> std::result::Result<&'a str, ToolOutput> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| {
        err_output(format!(
            "missing or invalid parameter '{key}' (string required)"
        ))
    })
}

/// 解析可选非负整数参数；存在但为负数/浮点/非数字类型时给出业务失败输出。
fn opt_usize(input: &Value, key: &str) -> std::result::Result<Option<usize>, ToolOutput> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(|n| Some(n as usize)).ok_or_else(|| {
            err_output(format!(
                "invalid parameter '{key}' (non-negative integer required)"
            ))
        }),
    }
}

/// 解析并校验路径：逃逸/无效输入转为业务失败输出回给模型，io 故障仍作为 `Err` 传播。
fn resolve_path(ctx: &ToolCtx, path: &str) -> Result<std::result::Result<PathBuf, ToolOutput>> {
    match crate::path_guard::resolve(ctx, path) {
        Ok(p) => Ok(Ok(p)),
        Err(e @ (ToolsError::InvalidInput { .. } | ToolsError::PathEscape { .. })) => {
            Ok(Err(err_output(e.to_string())))
        }
        Err(e) => Err(e),
    }
}

/// 读取文本文件（只读）。
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
        // 先取 metadata：目录/文件错配与超大文件显式分流为业务输出，不猜 ErrorKind。
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
        // 输入侧护栏：超过 4 MB 不整读，提示模型分页。
        if meta.len() > MAX_READ_BYTES {
            return Ok(err_output(format!(
                "file too large ({} bytes), use offset/limit to read parts: {}",
                meta.len(),
                path.display()
            )));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            // metadata 与 read 之间存在 TOCTOU 窗口，保留 NotFound 分流
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
        // 越界守卫：空文件 + offset>0 也在此分流为业务失败，杜绝下游 usize 下溢。
        if offset > 0 && offset >= total {
            return Ok(err_output(format!(
                "offset {offset} is beyond end of file ({total} lines)"
            )));
        }
        let end = offset.saturating_add(limit).min(total);
        // 无切片时原样返回，保留原始换行与结尾；有切片时按 \n 重新拼接。
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

/// 整文件写入（覆盖；自动创建父目录）。
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
        // 写入侧护栏：超限不创建/覆盖文件。
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
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await?;
        Ok(ok_output(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        )))
    }
}

/// 精确字符串替换编辑（old_string 必须唯一匹配）。
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

/// 列目录（只读；按名排序，目录带 `/` 后缀）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCtx};

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let (_d, c) = ctx();
        let out = WriteFile
            .execute(
                serde_json::json!({"path":"sub/hello.txt","content":"hi"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let out = ReadFile
            .execute(serde_json::json!({"path":"sub/hello.txt"}), &c)
            .await
            .unwrap();
        assert_eq!(out.content, "hi");
    }

    #[tokio::test]
    async fn read_missing_file_is_error_output_not_err() {
        let (_d, c) = ctx();
        let out = ReadFile
            .execute(serde_json::json!({"path":"nope.txt"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let (_d, c) = ctx();
        WriteFile
            .execute(
                serde_json::json!({"path":"a.txt","content":"foo bar foo"}),
                &c,
            )
            .await
            .unwrap();
        let dup = EditFile
            .execute(
                serde_json::json!({"path":"a.txt","old_string":"foo","new_string":"x"}),
                &c,
            )
            .await
            .unwrap();
        assert!(dup.is_error);
        let ok = EditFile
            .execute(
                serde_json::json!({"path":"a.txt","old_string":"bar foo","new_string":"baz"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!ok.is_error);
        let out = ReadFile
            .execute(serde_json::json!({"path":"a.txt"}), &c)
            .await
            .unwrap();
        assert_eq!(out.content, "foo baz");
    }

    #[tokio::test]
    async fn list_dir_marks_dirs() {
        let (_d, c) = ctx();
        std::fs::create_dir(c.cwd.join("d1")).unwrap();
        std::fs::write(c.cwd.join("f1.txt"), "x").unwrap();
        let out = ListDir
            .execute(serde_json::json!({"path":"."}), &c)
            .await
            .unwrap();
        assert!(out.content.contains("d1/"));
        assert!(out.content.contains("f1.txt"));
    }

    #[tokio::test]
    async fn registry_specs_sorted_and_have_schema() {
        let reg = crate::Registry::builtin();
        let specs = reg.specs();
        assert_eq!(specs.len(), 8);
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(specs.iter().all(|s| s.input_schema["type"] == "object"));
        assert!(reg.get("read_file").unwrap().is_read_only());
        assert!(!reg.get("write_file").unwrap().is_read_only());
    }

    #[tokio::test]
    async fn missing_param_is_error_output() {
        let (_d, c) = ctx();
        let out = WriteFile
            .execute(serde_json::json!({"path":"x.txt"}), &c)
            .await
            .unwrap(); // 缺 content
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_empty_file_with_offset_is_error_not_panic() {
        // 回归：空文件 + offset>0 曾触发 usize 下溢 panic
        let (_d, c) = ctx();
        WriteFile
            .execute(serde_json::json!({"path":"empty.txt","content":""}), &c)
            .await
            .unwrap();
        let out = ReadFile
            .execute(serde_json::json!({"path":"empty.txt","offset":5}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        // offset=0 读空文件仍正常返回空串
        let out = ReadFile
            .execute(serde_json::json!({"path":"empty.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "");
    }

    #[tokio::test]
    async fn read_rejects_oversized_file() {
        let (_d, c) = ctx();
        let big = vec![b'x'; (MAX_READ_BYTES + 1) as usize];
        std::fs::write(c.cwd.join("big.txt"), big).unwrap();
        let out = ReadFile
            .execute(serde_json::json!({"path":"big.txt"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_dir_path_is_error() {
        let (_d, c) = ctx();
        std::fs::create_dir(c.cwd.join("sub")).unwrap();
        let out = ReadFile
            .execute(serde_json::json!({"path":"sub"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn list_file_path_is_error() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("f.txt"), "x").unwrap();
        let out = ListDir
            .execute(serde_json::json!({"path":"f.txt"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn invalid_offset_limit_is_error() {
        let (_d, c) = ctx();
        WriteFile
            .execute(serde_json::json!({"path":"a.txt","content":"l1\nl2"}), &c)
            .await
            .unwrap();
        for bad in [
            serde_json::json!({"path":"a.txt","limit":0}),
            serde_json::json!({"path":"a.txt","offset":-1}),
            serde_json::json!({"path":"a.txt","limit":"100"}),
        ] {
            let out = ReadFile.execute(bad.clone(), &c).await.unwrap();
            assert!(out.is_error, "input {bad} 应返回 is_error");
        }
    }

    #[tokio::test]
    async fn write_rejects_oversized_content() {
        let (_d, c) = ctx();
        let big = "x".repeat(MAX_WRITE_BYTES + 1);
        let out = WriteFile
            .execute(serde_json::json!({"path":"big.txt","content":big}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        // 超限不创建文件
        assert!(!c.cwd.join("big.txt").exists());
    }

    #[tokio::test]
    async fn edit_rejects_oversized_file() {
        let (_d, c) = ctx();
        let big = vec![b'x'; (MAX_READ_BYTES + 1) as usize];
        std::fs::write(c.cwd.join("big.txt"), big).unwrap();
        let out = EditFile
            .execute(
                serde_json::json!({"path":"big.txt","old_string":"x","new_string":"y"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_truncates_at_line_cap() {
        // 3000 行：默认 limit 钳到 2000 行并标 [truncated]
        let (_d, c) = ctx();
        let text: String = (0..3000).map(|i| format!("line{i}\n")).collect();
        std::fs::write(c.cwd.join("many.txt"), text).unwrap();
        let out = ReadFile
            .execute(serde_json::json!({"path":"many.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.ends_with("\n[truncated]"));
        let body = out.content.strip_suffix("\n[truncated]").unwrap();
        assert_eq!(body.lines().count(), MAX_LINES);
        assert!(body.starts_with("line0"));
        assert!(body.ends_with("line1999"));
    }

    #[tokio::test]
    async fn read_truncates_at_byte_cap_on_char_boundary() {
        // 多字节字符（'€' 3 字节）压过 50 KB：截断落在字符边界，无乱码
        let (_d, c) = ctx();
        let text = "€".repeat(MAX_BYTES); // 3 * 50 KB 字节
        std::fs::write(c.cwd.join("euro.txt"), text).unwrap();
        let out = ReadFile
            .execute(serde_json::json!({"path":"euro.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.ends_with("\n[truncated]"));
        let body = out.content.strip_suffix("\n[truncated]").unwrap();
        assert!(body.len() <= MAX_BYTES);
        // String 类型保证合法 UTF-8；截断点必须是字符边界（'€' 完整，无 U+FFFD）
        assert!(!body.contains('\u{FFFD}'));
        assert_eq!(body.len() % "€".len(), 0);
    }

    #[tokio::test]
    async fn read_offset_limit_pages_correctly() {
        // offset/limit 范围内取片：内容与行号精确对应
        let (_d, c) = ctx();
        let text: String = (0..100).map(|i| format!("line{i}\n")).collect();
        std::fs::write(c.cwd.join("page.txt"), text).unwrap();
        let out = ReadFile
            .execute(
                serde_json::json!({"path":"page.txt","offset":10,"limit":5}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        // 未读到文件末尾，按规则附 [truncated] 标记
        assert_eq!(
            out.content,
            "line10\nline11\nline12\nline13\nline14\n[truncated]"
        );
    }

    #[tokio::test]
    async fn list_dir_truncates_over_entry_cap() {
        // 1005 个条目：截到 1000 并标 [truncated: N more entries]
        let (_d, c) = ctx();
        for i in 0..MAX_ENTRIES + 5 {
            std::fs::write(c.cwd.join(format!("f{i:04}.txt")), "x").unwrap();
        }
        let out = ListDir
            .execute(serde_json::json!({"path":"."}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.ends_with("[truncated: 5 more entries]"));
        let body = out
            .content
            .strip_suffix("\n[truncated: 5 more entries]")
            .unwrap();
        assert_eq!(body.lines().count(), MAX_ENTRIES);
    }
}
