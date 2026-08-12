//! 指令记忆（`WAVECODE.md`）的发现、拼接与 `@path` 引用展开（SPEC §7.1）。
//!
//! 纯逻辑、同步 IO：收集是一次性启动期动作（cli bootstrap），不在 turn
//! 循环热路径上，故直接用 `std::fs`，可 100% 单测（tempfile 构造目录树）。
//!
//! 收集顺序（"全局在前、局部在后"拼接）：
//!
//! ```text
//! 用户级 ~/.wavecode/WAVECODE.md → 项目根 WAVECODE.md + .wavecode/rules/*.md
//! → cwd WAVECODE.md + .wavecode/rules/*.md
//! ```
//!
//! 项目根以 `.git`（目录或文件，兼容 worktree 形态）向上定位；cwd 与项目根
//! 相同或不在其下时按实际路径去重，同一份文件不会拼入两次。
//!
//! 取舍（首版范围外，SPEC §7.1 的其余条目留待后续）：`WAVECODE.override.md`
//! 覆盖项目级、fallback 文件名（CLAUDE.md/AGENTS.md）均未实现。

use std::path::{Path, PathBuf};

/// 指令记忆文件名。
pub const INSTRUCTION_FILE: &str = "WAVECODE.md";

/// `@path` 引用递归展开的深度上限（SPEC §7.1）：WAVECODE.md 本身为
/// 深度 0，其内引用的文件为深度 1，依此类推；深度超过上限的文件内引用
/// 不再展开，按字面文本保留。
pub const MAX_INCLUDE_DEPTH: usize = 5;

/// 指令记忆收集结果。
#[derive(Debug, Clone, Default)]
pub struct InstructionMemory {
    /// 拼接产物（全局在前、局部在后，各来源以标题分节）；无内容时为空串。
    pub combined: String,
    /// 参与拼接的来源文件（按拼接序；调试与展示用）。
    pub sources: Vec<PathBuf>,
}

/// 向上定位项目根：从 `cwd` 起逐级找含 `.git`（目录或文件——worktree 为
/// 文件）的祖先目录；找到即返回，找不到返回 None（非仓库环境只收集
/// 用户级与 cwd 两级）。
pub fn find_project_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// 收集指令记忆：用户级（`home/.wavecode/WAVECODE.md`）→ 项目根 → cwd，
/// 逐级拼接；项目根与 cwd 级各自附带 `.wavecode/rules/*.md`（按文件名
/// 排序并入）。`home` 为 None 时跳过用户级。所有文件经 `@path` 引用展开
/// 后拼接；读不到的文件静默跳过（记忆是增强项，缺失不阻塞启动）。
pub fn collect(home: Option<&Path>, cwd: &Path) -> InstructionMemory {
    let mut mem = InstructionMemory::default();
    let mut seen: Vec<PathBuf> = Vec::new();

    // 一级来源：一份 WAVECODE.md + 该级 rules 目录的 *.md（按文件名排序）。
    // 用户级与项目级的目录形态不同（~/.wavecode/WAVECODE.md +
    // ~/.wavecode/rules vs <dir>/WAVECODE.md + <dir>/.wavecode/rules），
    // 两个路径由调用方显式给出。
    let collect_level = |instr_file: PathBuf,
                         rules_dir: PathBuf,
                         mem: &mut InstructionMemory,
                         seen: &mut Vec<PathBuf>| {
        let mut files = vec![instr_file];
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            let mut rules: Vec<PathBuf> = entries
                .filter_map(std::result::Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
                .collect();
            rules.sort();
            files.extend(rules);
        }
        for file in files {
            // 去重：cwd == 项目根等形态下同一文件只拼一次（以规范化路径判等，
            // 失败回退原始路径——判等只需稳定，无需真实解析）。
            let key = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
            if seen.contains(&key) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue; // 文件不存在 / 读失败：跳过
            };
            seen.push(key);
            let base = file.parent().map(Path::to_path_buf).unwrap_or_default();
            let expanded = expand_at_refs(&content, &base, 0, &mut Vec::new());
            if !mem.combined.is_empty() {
                mem.combined.push_str("\n\n");
            }
            mem.combined.push_str(&format!("## {}\n\n", file.display()));
            mem.combined.push_str(expanded.trim_end());
            mem.sources.push(file);
        }
    };

    if let Some(home) = home {
        let user_dir = home.join(".wavecode");
        collect_level(
            user_dir.join(INSTRUCTION_FILE),
            user_dir.join("rules"),
            &mut mem,
            &mut seen,
        );
    }
    if let Some(root) = find_project_root(cwd) {
        collect_level(
            root.join(INSTRUCTION_FILE),
            root.join(".wavecode").join("rules"),
            &mut mem,
            &mut seen,
        );
    }
    collect_level(
        cwd.join(INSTRUCTION_FILE),
        cwd.join(".wavecode").join("rules"),
        &mut mem,
        &mut seen,
    );
    mem
}

/// 展开内容中的 `@path` 引用：引用文件内容就地替换标记（带来源标题），
/// 递归展开至 [`MAX_INCLUDE_DEPTH`]；`visited` 记录已展开文件（规范化
/// 路径），重复 / 成环引用不再展开，按字面文本保留（防环 + 防重复）。
/// 文件缺失 / 读取失败的引用同样按字面保留——诚实呈现，不静默丢弃。
fn expand_at_refs(
    content: &str,
    base_dir: &Path,
    depth: usize,
    visited: &mut Vec<PathBuf>,
) -> String {
    let mut out = String::with_capacity(content.len());
    for token in content.split_inclusive(char::is_whitespace) {
        let (body, trail_ws) = split_trailing_whitespace(token);
        match parse_at_ref(body) {
            Some(reference) => {
                let path = base_dir.join(reference);
                let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let expanded = if depth < MAX_INCLUDE_DEPTH && !visited.contains(&key) {
                    std::fs::read_to_string(&path).ok().map(|inner| {
                        visited.push(key);
                        let inner_base = path.parent().unwrap_or(base_dir);
                        let inner = expand_at_refs(&inner, inner_base, depth + 1, visited);
                        format!("### {reference}\n\n{}", inner.trim_end())
                    })
                } else {
                    None // 超深度 / 已展开（含成环）：按字面保留
                };
                match expanded {
                    Some(text) => {
                        out.push_str(&text);
                        out.push_str(trail_ws);
                    }
                    None => out.push_str(token),
                }
            }
            None => out.push_str(token),
        }
    }
    out
}

/// 拆分 token 的尾部空白（`split_inclusive` 保留的分隔符），返回
/// （主体， 尾部空白）。
fn split_trailing_whitespace(token: &str) -> (&str, &str) {
    let body = token.trim_end();
    (body, &token[body.len()..])
}

/// 解析 `@path` 引用标记：`@` 起始、后跟非空路径；剥离常见尾随标点
/// （`.` `,` `;` `:` `)` `]`），路径含 `@` 之外的空白不合法（token 已按
/// 空白切分，天然满足）。非引用返回 None。
fn parse_at_ref(token: &str) -> Option<&str> {
    let body = token.strip_prefix('@')?;
    let path = body.trim_end_matches(['.', ',', ';', ':', ')', ']']);
    if path.is_empty() {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// 拼接顺序（P6 验收）：用户级 → 项目根 → cwd，全局在前。
    #[test]
    fn concat_order_global_first() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let root = dir.path().join("repo");
        let cwd = root.join("crates/xyz");
        write(&home.join(".wavecode/WAVECODE.md"), "USER-LEVEL");
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join("WAVECODE.md"), "PROJECT-ROOT");
        write(&cwd.join("WAVECODE.md"), "CWD-LEVEL");

        let mem = collect(Some(&home), &cwd);
        let (u, r, c) = (
            mem.combined.find("USER-LEVEL").unwrap(),
            mem.combined.find("PROJECT-ROOT").unwrap(),
            mem.combined.find("CWD-LEVEL").unwrap(),
        );
        assert!(
            u < r && r < c,
            "拼接顺序应为 用户级→项目根→cwd:\n{}",
            mem.combined
        );
        assert_eq!(mem.sources.len(), 3);
    }

    /// 项目根定位：嵌套 cwd 向上找 `.git`；cwd == 项目根时文件不重复拼入。
    #[test]
    fn project_root_detection_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join("WAVECODE.md"), "ROOT");
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_root(&nested).as_deref(), Some(root.as_path()));
        // cwd == 项目根：同一份 WAVECODE.md 只拼一次。
        let mem = collect(None, &root);
        assert_eq!(mem.combined.matches("ROOT").count(), 1);
        assert_eq!(mem.sources.len(), 1);
    }

    /// @引用展开：基本替换 + 相对引用文件的目录解析。
    #[test]
    fn at_ref_expansion_basic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&root.join("docs/extra.md"), "EXTRA-CONTENT");
        write(&root.join("WAVECODE.md"), "前\n@docs/extra.md\n后");

        let mem = collect(None, &root);
        assert!(
            mem.combined.contains("EXTRA-CONTENT"),
            "引用应展开:\n{}",
            mem.combined
        );
        assert!(!mem.combined.contains("@docs/extra.md"), "标记应被替换");
        // 缺失文件的引用按字面保留（诚实呈现）。
        write(&root.join("WAVECODE.md"), "见 @docs/missing.md 说明");
        let mem = collect(None, &root);
        assert!(mem.combined.contains("@docs/missing.md"));
    }

    /// @引用深度上限（P6 验收）：链式引用 f0→f1→…→f7，深度超过 5 的
    /// 引用不再展开，按字面保留。
    #[test]
    fn at_ref_expansion_depth_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        for i in 0..=7 {
            let content = if i == 7 {
                format!("LEAF-{i}")
            } else {
                format!("LV{i}\n@f{}.md", i + 1)
            };
            write(&root.join(format!("f{i}.md")), &content);
        }
        write(&root.join("WAVECODE.md"), "@f0.md");

        let mem = collect(None, &root);
        // WAVECODE.md 为深度 0 → f0..f4（深度 1..=5）展开；f4（深度 5）
        // 内的 @f5.md 已达上限，不再展开，按字面保留。
        for i in 0..=4 {
            assert!(
                mem.combined.contains(&format!("LV{i}")),
                "LV{i} 应展开:\n{}",
                mem.combined
            );
        }
        assert!(
            mem.combined.contains("@f5.md"),
            "达上限的引用应按字面保留:\n{}",
            mem.combined
        );
        assert!(!mem.combined.contains("LV5"), "深度 6 的内容不得出现");
    }

    /// @引用防环（P6 验收）：a ↔ b 互引必须终止；已展开文件再次引用按
    /// 字面保留（visited 判重）。
    #[test]
    fn at_ref_expansion_cycle_terminates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&root.join("a.md"), "A-CONTENT\n@b.md");
        write(&root.join("b.md"), "B-CONTENT\n@a.md");
        write(&root.join("WAVECODE.md"), "@a.md");

        let mem = collect(None, &root);
        assert!(mem.combined.contains("A-CONTENT"));
        assert!(mem.combined.contains("B-CONTENT"));
        // b 内对 a 的回环引用按字面保留（a 已在 visited）。
        assert!(mem.combined.contains("@a.md"));
    }

    /// rules 目录并入（P6 验收）：`.wavecode/rules/*.md` 按文件名排序拼接。
    #[test]
    fn rules_dir_merged_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&root.join("WAVECODE.md"), "ROOT");
        write(&root.join(".wavecode/rules/02-style.md"), "RULE-STYLE");
        write(&root.join(".wavecode/rules/01-test.md"), "RULE-TEST");
        write(&root.join(".wavecode/rules/skip.txt"), "NOT-MD");

        let mem = collect(None, &root);
        let (t, s) = (
            mem.combined.find("RULE-TEST").unwrap(),
            mem.combined.find("RULE-STYLE").unwrap(),
        );
        assert!(t < s, "rules 应按文件名排序并入:\n{}", mem.combined);
        assert!(!mem.combined.contains("NOT-MD"), "非 .md 文件不并入");
        // 来源：WAVECODE.md + 两个 rules 文件。
        assert_eq!(mem.sources.len(), 3);
    }

    /// 非仓库环境（无 .git）：只收集用户级与 cwd 两级。
    #[test]
    fn non_repo_collects_user_and_cwd_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("plain/sub");
        write(&home.join(".wavecode/WAVECODE.md"), "USER-LEVEL");
        write(&cwd.join("WAVECODE.md"), "CWD-LEVEL");
        // tempdir 祖先若恰为 git 仓库会误定位——断言前显式确认。
        if cwd.ancestors().all(|p| !p.join(".git").exists()) {
            let mem = collect(Some(&home), &cwd);
            assert!(mem.combined.contains("USER-LEVEL"));
            assert!(mem.combined.contains("CWD-LEVEL"));
            assert_eq!(mem.sources.len(), 2);
        }
    }

    /// 用户级规则目录是 `~/.wavecode/rules/*.md`（与项目级的
    /// `<dir>/.wavecode/rules` 形态不同——回归锁定，曾误算成
    /// `~/.wavecode/.wavecode/rules`）。
    #[test]
    fn user_level_rules_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("plain");
        write(&home.join(".wavecode/WAVECODE.md"), "USER-LEVEL");
        write(&home.join(".wavecode/rules/01-global.md"), "GLOBAL-RULE");
        write(&cwd.join("WAVECODE.md"), "CWD-LEVEL");
        if cwd.ancestors().all(|p| !p.join(".git").exists()) {
            let mem = collect(Some(&home), &cwd);
            assert!(
                mem.combined.contains("GLOBAL-RULE"),
                "用户级 rules 应并入:\n{}",
                mem.combined
            );
            let (u, g) = (
                mem.combined.find("USER-LEVEL").unwrap(),
                mem.combined.find("GLOBAL-RULE").unwrap(),
            );
            assert!(u < g, "用户级 rules 排在本级 WAVECODE.md 之后");
        }
    }
}
