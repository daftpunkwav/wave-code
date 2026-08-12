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
    if let Err(e) = glob::Pattern::new(pattern) {
        return Err(err_output(format!("invalid glob pattern '{pattern}': {e}")));
    }
    Ok(())
}

/// 转 glob crate 模式串的目录前缀：glob 模式语法里 `\` 是转义符（Windows 路径
/// 分隔符会被误吃），统一替换为 `/`——Windows 文件 API 接受正斜杠，匹配按
/// path component 进行，与分隔符形态无关。
fn glob_prefix(dir: &Path) -> String {
    dir.to_string_lossy().replace('\\', "/")
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
pub struct Grep;

#[async_trait::async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression inside the working directory. \
         Output is one 'path:line:content' per match (paths relative to the working directory), \
         followed by a match/file count footer. Capped at 500 matches / 50KB; matching lines \
         are truncated at 2000 bytes. Binary, unreadable and over-4MB files are skipped."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression (regex crate syntax) to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search, relative to the working directory (default: the working directory itself)"
                },
                "glob": {
                    "type": "string",
                    "description": "Optional filename filter applied when searching a directory, e.g. \"*.rs\" or \"src/*.toml\" (use / as separator)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let pattern = match req_str(&input, "pattern") {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let regex = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return Ok(err_output(format!("invalid regex pattern: {e}"))),
        };
        let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let root = match resolve_path(ctx, path)? {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let filter = match input.get("glob").and_then(Value::as_str) {
            None => None,
            Some(f) => {
                if let Err(out) = validate_pattern(f) {
                    return Ok(out);
                }
                Some(f.to_owned())
            }
        };
        let cwd = ctx.cwd.clone();
        // 遍历 + 读文件 + 正则扫描为阻塞 IO，整体移出 executor 线程；
        // JoinError 仅在本模块 bug（panic）时触发，转为业务输出不中断 turn。
        match tokio::task::spawn_blocking(move || {
            grep_search(&regex, &root, filter.as_deref(), &cwd)
        })
        .await
        {
            Ok(content) => Ok(content),
            Err(e) => Ok(err_output(format!("grep failed: {e}"))),
        }
    }
}

/// grep 的阻塞主体：展开候选文件 → 逐文件扫描。返回完整输出（含统计尾行）。
fn grep_search(regex: &regex::Regex, root: &Path, filter: Option<&str>, cwd: &Path) -> ToolOutput {
    let cwd_canon = match cwd.canonicalize() {
        Ok(c) => c,
        Err(e) => return err_output(format!("failed to canonicalize working directory: {e}")),
    };
    // 候选文件清单：根为文件则只扫它；根为目录则经 glob 模式 `root/**/{filter}`
    // 展开（`**` 匹配任意深度，含根目录本身一层）。排序保证输出稳定。
    let files = if root.is_file() {
        vec![root.to_path_buf()]
    } else if root.is_dir() {
        let pattern = format!("{}/**/{}", glob_prefix(root), filter.unwrap_or("*"));
        let mut files: Vec<PathBuf> = match glob::glob(&pattern) {
            Ok(paths) => paths
                .filter_map(std::result::Result::ok)
                .filter(|p| p.is_file())
                .collect(),
            Err(e) => return err_output(format!("invalid glob pattern: {e}")),
        };
        files.sort();
        files
    } else {
        return err_output(format!("path not found: {}", root.display()));
    };

    let mut lines: Vec<String> = Vec::new();
    let mut matched_files = 0usize;
    let mut hit_cap = false;
    'files: for file in &files {
        // 逐条复核：junction/symlink 指向 cwd 之外的路径直接跳过
        if !under_cwd(file, &cwd_canon) {
            continue;
        }
        // 输入侧护栏对齐 read_file：超过 4 MB 不整读（跳过，不算匹配）
        let meta = match std::fs::metadata(file) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        // 读失败 / 非 UTF-8（二进制）静默跳过：grep 语义是搜文本
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let rel = rel_display(file, cwd);
        let mut file_had_match = false;
        for (idx, line) in text.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            file_had_match = true;
            let mut line = line.to_owned();
            if line.len() > MAX_LINE_BYTES {
                let mut cut = MAX_LINE_BYTES;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                line.truncate(cut);
                line.push_str("[truncated]");
            }
            lines.push(format!("{rel}:{}:{line}", idx + 1));
            if lines.len() >= MAX_MATCHES {
                hit_cap = true;
                break 'files;
            }
        }
        if file_had_match {
            matched_files += 1;
        }
    }

    if lines.is_empty() {
        return ok_output("no matches");
    }
    let mut content = lines.join("\n");
    let mut byte_truncated = false;
    if content.len() > MAX_OUTPUT_BYTES {
        let mut cut = MAX_OUTPUT_BYTES;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
        byte_truncated = true;
    }
    if byte_truncated {
        content.push_str("\n[truncated]");
    }
    content.push_str(&format!(
        "\n[{} matches in {} files{}]",
        lines.len(),
        matched_files,
        if hit_cap {
            format!(", stopped at {MAX_MATCHES} matches")
        } else {
            String::new()
        }
    ));
    ok_output(content)
}

/// 路径模式匹配（只读；返回相对 cwd 的匹配路径列表，按名排序）。
pub struct Glob;

#[async_trait::async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find paths matching a glob pattern inside the working directory, e.g. \"src/**/*.rs\" \
         (use / as separator; ** matches any depth). Returns matching paths relative to the \
         working directory, sorted by name, capped at 1000 entries."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern relative to the working directory, e.g. \"**/*.rs\""
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let pattern = match req_str(&input, "pattern") {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        if let Err(out) = validate_pattern(pattern) {
            return Ok(out);
        }
        let pattern = pattern.to_owned();
        let cwd = ctx.cwd.clone();
        // 同 grep：glob 遍历为阻塞 IO，包 spawn_blocking。
        match tokio::task::spawn_blocking(move || glob_search(&pattern, &cwd)).await {
            Ok(content) => Ok(content),
            Err(e) => Ok(err_output(format!("glob failed: {e}"))),
        }
    }
}

/// glob 的阻塞主体：以 cwd 为根展开模式，逐条复核后输出相对路径列表。
fn glob_search(pattern: &str, cwd: &Path) -> ToolOutput {
    let cwd_canon = match cwd.canonicalize() {
        Ok(c) => c,
        Err(e) => return err_output(format!("failed to canonicalize working directory: {e}")),
    };
    let full = format!("{}/{pattern}", glob_prefix(cwd));
    let paths = match glob::glob(&full) {
        Ok(p) => p,
        Err(e) => return err_output(format!("invalid glob pattern '{pattern}': {e}")),
    };
    let mut rels: Vec<String> = paths
        .filter_map(std::result::Result::ok)
        .filter(|p| under_cwd(p, &cwd_canon))
        .map(|p| rel_display(&p, cwd))
        .collect();
    rels.sort();
    let total = rels.len();
    rels.truncate(MAX_PATHS);
    if rels.is_empty() {
        return ok_output("no matches");
    }
    let mut content = rels.join("\n");
    if total > MAX_PATHS {
        content.push_str(&format!("\n[truncated: {} more paths]", total - MAX_PATHS));
    }
    ok_output(content)
}

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

    #[tokio::test]
    async fn registry_contains_grep_and_glob() {
        let reg = crate::Registry::builtin();
        assert!(reg.get("grep").unwrap().is_read_only());
        assert!(reg.get("glob").unwrap().is_read_only());
    }
}
