//! 持久记忆存储（SPEC §7.2）：`~/.wavecode/memories/` 下的 `MEMORY.md`
//! 索引 + 四类条目文件，Markdown 条目式（`- ` 列表项）。
//!
//! 纯逻辑、同步 IO：写入是单条追加（小文件），由 core 的 `memory_write`
//! 工具经 `spawn_blocking` 调用；读取发生在启动装配（一次性）。全部内容
//! 对用户透明可见、可直接编辑——本模块只做追加，不做整合（SPEC 的 24h+5
//! 会话门控整合为首版简化，见 crate 级文档）。

use std::path::{Path, PathBuf};

/// 索引文件名（记忆目录根下）。
pub const INDEX_FILE: &str = "MEMORY.md";

/// 记忆类别（SPEC §7.2 四类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryCategory {
    /// 用户画像（偏好、习惯、角色）。
    User,
    /// 反馈与纠正（用户明确的"以后这样做/不要这样做"）。
    Feedback,
    /// 项目知识（仓库约定、架构决策、环境事实）。
    Project,
    /// 外部参考（文档链接、外部系统要点）。
    Reference,
}

impl MemoryCategory {
    /// 全部类别（序固定，索引分组与测试遍历用）。
    pub const ALL: [Self; 4] = [Self::User, Self::Feedback, Self::Project, Self::Reference];

    /// 类别名（`memory_write` 工具参数 / 提取产出的标签词）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    /// 解析类别名；非法值返回 None（调用方转业务失败输出）。
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == raw)
    }

    /// 类别条目文件名。
    pub fn file_name(self) -> &'static str {
        match self {
            Self::User => "user.md",
            Self::Feedback => "feedback.md",
            Self::Project => "project.md",
            Self::Reference => "reference.md",
        }
    }
}

/// 持久记忆存储：根目录下的索引 + 类别文件。根目录可注入（测试用
/// tempfile；生产为 `~/.wavecode/memories/`）。
#[derive(Debug, Clone)]
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    /// 以指定根目录构造（不创建目录，首次写入时创建）。
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 默认根目录：`<home>/.wavecode/memories`。
    pub fn default_root(home: &Path) -> PathBuf {
        home.join(".wavecode").join("memories")
    }

    /// 存储根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 追加一条记忆：条目写入类别文件（`- ` 列表项，多行内容缩进续行
    /// 保持在同一列表项内），索引追加一行 `- [category] 摘要`。
    /// 摘要取内容首行，截断 80 字符——索引是导航，正文在类别文件。
    pub fn append(&self, category: MemoryCategory, content: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let entry = format_entry(content);
        append_line(&self.root.join(category.file_name()), &entry)?;
        let summary = summarize(content);
        append_line(
            &self.root.join(INDEX_FILE),
            &format!(
                "- [{}] {summary}（详见 {}）",
                category.as_str(),
                category.file_name()
            ),
        )
    }

    /// 读取索引全文；索引不存在返回空串（首次使用无记忆是正常形态，
    /// 不作为错误）。
    pub fn read_index(&self) -> std::io::Result<String> {
        match std::fs::read_to_string(self.root.join(INDEX_FILE)) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// 读取某类别条目全文（按需加载；不存在返回空串，语义同上）。
    pub fn read_category(&self, category: MemoryCategory) -> std::io::Result<String> {
        match std::fs::read_to_string(self.root.join(category.file_name())) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }
}

/// 用户 home 目录：`USERPROFILE`（Windows）优先，兜底 `HOME`。
/// 与 config crate 的解析同款——memory 无 workspace 内依赖（SPEC §3），
/// 此处自洽一份，不反向依赖 config。
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// 条目格式化：`- ` 列表项；多行内容的后续行缩进两格，保持在同一
/// Markdown 列表项内。
fn format_entry(content: &str) -> String {
    let mut out = String::from("- ");
    out.push_str(&content.trim().replace('\n', "\n  "));
    out
}

/// 索引摘要：内容首行，按字符截断 80（截断处补省略号）。
fn summarize(content: &str) -> String {
    let first_line = content.trim().lines().next().unwrap_or_default();
    const MAX: usize = 80;
    if first_line.chars().count() <= MAX {
        first_line.to_owned()
    } else {
        let mut s: String = first_line.chars().take(MAX - 1).collect();
        s.push('…');
        s
    }
}

/// 追加一行到文件末尾（文件不存在则创建）。
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 追加 → 类别文件与索引同步更新；二次追加累加（跨会话召回的
    /// 存储侧基础）。
    #[test]
    fn append_updates_category_file_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("memories"));

        store.append(MemoryCategory::User, "偏好紧凑回复").unwrap();
        store
            .append(MemoryCategory::Project, "仓库用 pnpm 管理")
            .unwrap();
        store.append(MemoryCategory::User, "长期使用 Rust").unwrap();

        let user = store.read_category(MemoryCategory::User).unwrap();
        assert_eq!(user, "- 偏好紧凑回复\n- 长期使用 Rust\n");
        let project = store.read_category(MemoryCategory::Project).unwrap();
        assert_eq!(project, "- 仓库用 pnpm 管理\n");

        let index = store.read_index().unwrap();
        let lines: Vec<&str> = index.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "- [user] 偏好紧凑回复（详见 user.md）");
        assert_eq!(lines[1], "- [project] 仓库用 pnpm 管理（详见 project.md）");
        assert_eq!(lines[2], "- [user] 长期使用 Rust（详见 user.md）");
    }

    /// 多行内容：续行缩进保持在同一列表项；索引摘要只取首行。
    #[test]
    fn multiline_entry_stays_in_one_bullet() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        store
            .append(MemoryCategory::Feedback, "不要重构未坏代码\n除非被要求")
            .unwrap();
        assert_eq!(
            store.read_category(MemoryCategory::Feedback).unwrap(),
            "- 不要重构未坏代码\n  除非被要求\n"
        );
        let index = store.read_index().unwrap();
        assert!(index.contains("- [feedback] 不要重构未坏代码（详见 feedback.md）"));
        assert!(!index.contains("除非被要求"), "索引摘要只取首行");
    }

    /// 空存储：索引 / 类别文件读取返回空串而非错误（首次使用形态）。
    #[test]
    fn empty_store_reads_as_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("nope"));
        assert_eq!(store.read_index().unwrap(), "");
        assert_eq!(store.read_category(MemoryCategory::Reference).unwrap(), "");
    }

    /// 类别名解析 roundtrip；非法值拒绝。
    #[test]
    fn category_parse_roundtrip() {
        for c in MemoryCategory::ALL {
            assert_eq!(MemoryCategory::parse(c.as_str()), Some(c));
        }
        assert_eq!(MemoryCategory::parse("nope"), None);
        assert_eq!(MemoryCategory::parse(""), None);
    }

    /// 长首行摘要截断 80 字符并补省略号。
    #[test]
    fn long_summary_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        let long = "长".repeat(200);
        store.append(MemoryCategory::User, &long).unwrap();
        let index = store.read_index().unwrap();
        let line = index.trim_end();
        // 摘要截断 80 字符（79 + 省略号），后接类别文件指引。
        assert!(line.contains('…'), "截断处应补省略号: {line}");
        assert!(line.ends_with("（详见 user.md）"));
        let summary = line
            .trim_start_matches("- [user] ")
            .trim_end_matches("（详见 user.md）");
        assert_eq!(summary.chars().count(), 80);
    }
}
