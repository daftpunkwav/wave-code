//! 搜索工具（阶段 3 拆分自 search_tools.rs）。

use super::*;

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
    let paths = match ::glob::glob(&full) {
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
