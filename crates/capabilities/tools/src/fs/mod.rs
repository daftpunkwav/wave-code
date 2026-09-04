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

/// 原子覆盖写（write_file / edit_file 共用）：先写同目录临时文件，再
/// rename 替换目标（同卷 rename 原子；Windows 为 MOVEFILE_REPLACE_EXISTING）。
/// 直接 `tokio::fs::write` 覆盖既有文件在写中途失败时会把目标截断为半截
/// 内容且无备份；temp+rename 下目标要么是旧内容要么是新内容。
///
/// 临时名带进程 id + 进程内序号：同批并行写同一目录不冲突。写入或
/// rename 失败时清理临时文件后传播错误（不留垃圾、不吞错）。
pub(super) async fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = tokio::fs::write(&tmp, content).await {
        // 磁盘满 / 权限等写失败：半截 .tmp 同样不留垃圾。
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

/// 读取文本文件（只读）。
mod edit;
mod list;
mod read;
mod write;

pub use edit::EditFile;
pub use list::ListDir;
pub use read::ReadFile;
pub use write::WriteFile;

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

    /// 原子写：覆盖后内容正确，且同目录不残留 .tmp 临时文件
    ///（temp+rename 成功路径 rename 即转正，失败路径清理）。
    #[tokio::test]
    async fn write_overwrites_without_temp_leftovers() {
        let (_d, c) = ctx();
        WriteFile
            .execute(serde_json::json!({"path":"a.txt","content":"v1"}), &c)
            .await
            .unwrap();
        WriteFile
            .execute(
                serde_json::json!({"path":"a.txt","content":"v2-longer"}),
                &c,
            )
            .await
            .unwrap();
        EditFile
            .execute(
                serde_json::json!({"path":"a.txt","old_string":"v2","new_string":"v3"}),
                &c,
            )
            .await
            .unwrap();
        let out = ReadFile
            .execute(serde_json::json!({"path":"a.txt"}), &c)
            .await
            .unwrap();
        assert_eq!(out.content, "v3-longer");
        let leftovers: Vec<String> = std::fs::read_dir(c.cwd.join("."))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件: {leftovers:?}");
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
        // builtin 不含 todo_write（由会话装配 via with_todo_write 注入）。
        assert_eq!(specs.len(), 7);
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(specs.iter().all(|s| s.input_schema["type"] == "object"));
        assert!(reg.get("read_file").unwrap().is_read_only());
        assert!(!reg.get("write_file").unwrap().is_read_only());
        assert!(reg.get("todo_write").is_none());

        let (full, _todos) = crate::Registry::builtin_with_todos();
        assert_eq!(full.specs().len(), 8);
        assert!(full.get("todo_write").is_some());
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
