//! 两个只读检索工具：`grep`（正则内容搜索）与 `glob`（路径模式匹配）。
//!
//! 路径约束与失败语义同 fs_tools：根路径/模式先经 [`crate::path_guard`] 词法
//! 校验约束在 `ToolCtx::cwd` 之下；遍历经 `glob` crate 展开（同步 API），每个
//! 命中路径再 canonicalize 复核仍在 cwd 真实路径内，防 symlink/junction 逃逸。
//! 遍历与文件读取为阻塞 IO，整体包 `spawn_blocking` 移出 executor 线程
//!（SPEC §19.3）；业务失败（正则无效、路径逃逸、无此路径等）返回
//! `Ok(is_error=true)` 回给模型，`Err` 仅用于实现级故障。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput, ToolsError};

/// grep 匹配行数上限：超过即停扫并标注截断。
const MAX_MATCHES: usize = 500;
/// grep 单行内容字节上限（截断回退到字符边界，对齐 read_file 风格）。
const MAX_LINE_BYTES: usize = 2000;
/// grep 总输出字节上限（对齐 read_file 的 50 KB）。
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
/// grep 单文件大小上限（对齐 read_file 的 4 MB 输入侧护栏）。
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// glob 返回路径数上限。
const MAX_PATHS: usize = 1000;

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

/// 校验 glob 类模式（glob 工具的 pattern 与 grep 的 glob 过滤）不得逃逸根目录：
/// 拒绝绝对路径与 `..` 组件——模式是词法展开，`..` 可让遍历越过 cwd。
fn validate_pattern(pattern: &str) -> std::result::Result<(), ToolOutput> {
    if pattern.is_empty() {
        return Err(err_output("glob pattern must not be empty"));
    }
    let p = Path::new(pattern);
    if p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(err_output(format!(
            "glob pattern escapes the working directory: {pattern}"
        )));
    }
    if let Err(e) = ::glob::Pattern::new(pattern) {
        return Err(err_output(format!("invalid glob pattern '{pattern}': {e}")));
    }
    Ok(())
}

/// 转 glob crate 模式串的目录前缀：glob 模式语法里 `\` 是转义符（Windows 路径
/// 分隔符会被误吃），统一替换为 `/`——Windows 文件 API 接受正斜杠，匹配按
/// path component 进行，与分隔符形态无关。目录名中的 glob 元字符
///（`[ ] { } * ?`）按模式语法转义——`D:\proj[1]\app` 这类目录不做转义
/// 会被当字符集解析，展开恒空（under_cwd 复核再过滤后即"永远无结果"）。
fn glob_prefix(dir: &Path) -> String {
    let unified = dir.to_string_lossy().replace('\\', "/");
    ::glob::Pattern::escape(&unified)
}

/// 命中路径复核：canonicalize 后必须在 cwd 真实路径之下（junction/symlink 指向
/// 外部时 glob 遍历会顺着走，词法校验挡不住，逐条复核兜底）。
fn under_cwd(path: &Path, cwd_canon: &Path) -> bool {
    path.canonicalize()
        .map(|c| c.starts_with(cwd_canon))
        .unwrap_or(false)
}

/// 相对 cwd 的展示路径，分隔符统一为 `/`（输出形态跨平台稳定）。
fn rel_display(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 正则内容搜索（只读）。
mod glob;
mod grep;

pub use glob::Glob;
pub use grep::Grep;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[tokio::test]
    async fn grep_matches_with_line_numbers_and_footer() {
        let (_d, c) = ctx();
        std::fs::create_dir(c.cwd.join("src")).unwrap();
        std::fs::write(c.cwd.join("src/a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        std::fs::write(c.cwd.join("b.txt"), "nothing here\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"fn main"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("src/a.rs:1:fn main() {}"));
        assert!(out.content.contains("[1 matches in 1 files]"));
    }

    #[tokio::test]
    async fn grep_glob_filter_limits_file_set() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.rs"), "target\n").unwrap();
        std::fs::write(c.cwd.join("b.txt"), "target\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"target","glob":"*.rs"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("a.rs:1:target"));
        assert!(!out.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_single_file_path() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.txt"), "hit\nmiss\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"hit","path":"a.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("a.txt:1:hit"));
    }

    #[tokio::test]
    async fn grep_path_escape_rejected() {
        let (_d, c) = ctx();
        let out = Grep
            .execute(serde_json::json!({"pattern":"x","path":"../outside"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        // glob 过滤同样不得逃逸
        let out = Grep
            .execute(serde_json::json!({"pattern":"x","glob":"../**/*.rs"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn grep_invalid_regex_is_error_output() {
        let (_d, c) = ctx();
        let out = Grep
            .execute(serde_json::json!({"pattern":"("}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn grep_rejects_empty_pattern() {
        // Empty regex matches every line: reject it instead of burning
        // the match cap on a full-tree dump.
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.txt"), "hello\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":""}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn grep_rejects_non_string_optional_params() {
        // Present-but-wrong-typed optionals must fail loudly: silently
        // defaulting would search a wider scope than the caller asked for.
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.txt"), "hit\n").unwrap();
        for bad in [
            serde_json::json!({"pattern":"hit","path":42}),
            serde_json::json!({"pattern":"hit","path":true}),
            serde_json::json!({"pattern":"hit","glob":42}),
            serde_json::json!({"pattern":"hit","glob":true}),
            serde_json::json!({"pattern":"hit","glob":["*.rs"]}),
        ] {
            let out = Grep.execute(bad.clone(), &c).await.unwrap();
            assert!(out.is_error, "input {bad} should be rejected");
        }
        // Explicit null keeps the defaults (JSON callers).
        let out = Grep
            .execute(
                serde_json::json!({"pattern":"hit","path":null,"glob":null}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("a.txt:1:hit"));
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.txt"), "hello\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"zzz"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "no matches");
    }

    #[tokio::test]
    async fn grep_truncates_at_match_cap() {
        let (_d, c) = ctx();
        let text: String = (0..MAX_MATCHES + 50).map(|i| format!("hit{i}\n")).collect();
        std::fs::write(c.cwd.join("many.txt"), text).unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"hit"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content
                .contains(&format!("stopped at {MAX_MATCHES} matches"))
        );
        let body = out.content.lines().take(MAX_MATCHES).count();
        assert_eq!(body, MAX_MATCHES);
    }

    #[tokio::test]
    async fn grep_truncates_long_lines_at_char_boundary() {
        let (_d, c) = ctx();
        // 多字节字符（'€' 3 字节）跨 2000 字节边界：截断不得切碎字符
        let line = format!("hit{}", "€".repeat(1000));
        std::fs::write(c.cwd.join("long.txt"), line).unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"hit"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("[truncated]"));
        assert!(!out.content.contains('\u{FFFD}'));
    }

    #[tokio::test]
    async fn glob_returns_sorted_relative_paths() {
        let (_d, c) = ctx();
        std::fs::create_dir_all(c.cwd.join("src/sub")).unwrap();
        std::fs::write(c.cwd.join("src/b.rs"), "x").unwrap();
        std::fs::write(c.cwd.join("src/sub/a.rs"), "x").unwrap();
        std::fs::write(c.cwd.join("src/c.txt"), "x").unwrap();
        let out = Glob
            .execute(serde_json::json!({"pattern":"src/**/*.rs"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines, vec!["src/b.rs", "src/sub/a.rs"]);
    }

    #[tokio::test]
    async fn glob_no_matches() {
        let (_d, c) = ctx();
        let out = Glob
            .execute(serde_json::json!({"pattern":"**/*.neverexist"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "no matches");
    }

    #[tokio::test]
    async fn glob_rejects_escape_patterns() {
        let (_d, c) = ctx();
        for bad in ["../**/*.rs", "../*.txt", "src/../../x"] {
            let out = Glob
                .execute(serde_json::json!({"pattern": bad}), &c)
                .await
                .unwrap();
            assert!(out.is_error, "pattern {bad} 应拒绝");
        }
        // 绝对路径模式：各平台上必然为绝对路径者
        #[cfg(windows)]
        let abs = "C:/Windows/*.ini";
        #[cfg(unix)]
        let abs = "/etc/*.conf";
        let out = Glob
            .execute(serde_json::json!({"pattern": abs}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn glob_truncates_over_path_cap() {
        let (_d, c) = ctx();
        for i in 0..MAX_PATHS + 5 {
            std::fs::write(c.cwd.join(format!("f{i:04}.txt")), "x").unwrap();
        }
        let out = Glob
            .execute(serde_json::json!({"pattern":"*.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.ends_with("[truncated: 5 more paths]"));
        let body = out
            .content
            .strip_suffix("\n[truncated: 5 more paths]")
            .unwrap();
        assert_eq!(body.lines().count(), MAX_PATHS);
    }

    /// 回归(cwd 元字符转义):目录名含 glob 元字符时,前缀必须按
    /// 模式语法转义——否则 `[1]` 被当字符集解析,展开恒空。
    #[tokio::test]
    async fn glob_works_when_cwd_contains_glob_metachars() {
        let outer = tempfile::tempdir().unwrap();
        let dir = outer.path().join("proj[1]_x");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.rs"), "fn main() {}")
            .await
            .unwrap();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            deny_env: Vec::new(),
        };
        let glob = crate::Registry::builtin().get("glob").unwrap();
        let out = glob
            .execute(serde_json::json!({"pattern": "*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("a.rs"),
            "应命中含元字符目录下的文件: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn registry_contains_grep_and_glob() {
        let reg = crate::Registry::builtin();
        assert!(reg.get("grep").unwrap().is_read_only());
        assert!(reg.get("glob").unwrap().is_read_only());
    }
}
