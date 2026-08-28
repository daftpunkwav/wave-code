//! 搜索工具（阶段 3 拆分自 search_tools.rs）。

use super::*;

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
        let mut files: Vec<PathBuf> = match ::glob::glob(&pattern) {
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
                // 达上限跳出前当前文件已命中:计入文件数再退出,
                // 否则统计尾行少算一个文件。
                if file_had_match {
                    matched_files += 1;
                }
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
