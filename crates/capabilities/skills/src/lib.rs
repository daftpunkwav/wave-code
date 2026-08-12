//! wavecode-skills — skills 系统（SPEC §8，P7 落地）。
//!
//! SKILL.md（YAML frontmatter + Markdown 正文）的发现、解析与清单注入：
//! - 发现：`<root>/skills/<name>/SKILL.md`，来源按优先级（低→高，同名覆盖）
//!   builtin < `~/.wavecode/skills` < `.wavecode/skills`（MCP 暴露的 skill
//!   为 SPEC §8.1 的第四来源，随 P9 MCP 落地，本版留占位）；
//! - frontmatter 字段取 SPEC §8.1 表交集：`description`（必填）/ `when_to_use` /
//!   `allowed-tools` / `context: inline | fork` / `user-invocable` /
//!   `argument-hint` / `paths`；
//! - 清单注入：[`SkillSet::catalog`] 渲染 name + description + when_to_use
//!   清单，预算（上下文窗口 1%）由调用方（core）以字符额度传入，超限降级
//!   截断（先去 when_to_use，再截断描述）；
//! - 执行展开：[`Skill::expand`] 替换 `$ARGUMENTS` 占位与
//!   `${WAVECODE_SKILL_DIR}` 变量；inline / fork 的执行编排在 core
//!   （本 crate 无 workspace 内依赖，SPEC §3 矩阵）。
//!
//! **frontmatter 解析取舍**：引入 `serde_yaml` 而非手写最小解析——frontmatter
//! 是 YAML（字段值可含冒号、列表、多行串），手写解析的边界 case（引号、
//! 缩进列表）会无声劣化；serde_yaml 已加进 workspace 根 `[workspace.dependencies]`
//! 统一版本（SPEC §3 纪律）。SPEC §8.1 表内字段命名混用 kebab-case
//! （`allowed-tools`）与 snake_case（`when_to_use`），解析面两种拼写都接受
//! （serde alias），写出侧不做约束。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `$ARGUMENTS` 占位符（inline 展开时替换为调用参数）。
const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";
/// skill 目录变量（展开为 SKILL.md 所在目录的绝对路径）。
const SKILL_DIR_VARIABLE: &str = "${WAVECODE_SKILL_DIR}";

/// skill 来源（优先级低→高；同名 skill 高优先级来源覆盖低优先级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillSource {
    /// 随二进制分发的内置技能集。
    Builtin,
    /// 用户级 `~/.wavecode/skills`。
    User,
    /// 项目级 `<cwd>/.wavecode/skills`。
    Project,
    /// MCP server 暴露的 prompt 转换的 inline skill（SPEC §8.1 第四来源 /
    /// §10，优先级最高）。P9 仅落地枚举占位；真实转换需 `prompts/get`
    /// 拉取内容，随 MCP 真实 transport 在 core 侧接线。
    Mcp,
}

impl SkillSource {
    /// 来源名（诊断 / 警告文本用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
            Self::Mcp => "mcp",
        }
    }
}

/// 执行模式（frontmatter `context` 字段，SPEC §8.1）：inline 展开进当前
/// 会话；fork 以独立 subagent 运行。缺省 inline。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillContext {
    /// 正文展开为 user 消息进入当前会话。
    #[default]
    Inline,
    /// 以 skill 正文为指令派生独立 subagent。
    Fork,
}

/// SKILL.md frontmatter（SPEC §8.1 字段交集）。
///
/// 字段命名混用 kebab / snake（SPEC 表原文如此），两种拼写均接受；
/// 未知字段忽略（向前兼容，新增字段不炸旧版本）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SkillMeta {
    /// 一句话能力描述（必填），进入清单注入。
    pub description: String,
    /// 模型自动触发判断依据，进入清单注入。
    #[serde(alias = "when-to-use")]
    pub when_to_use: Option<String>,
    /// 限定 skill 激活期间可用的工具名白名单（空 = 不限）。
    #[serde(rename = "allowed-tools", alias = "allowed_tools", default)]
    pub allowed_tools: Vec<String>,
    /// 执行模式（inline / fork）。
    #[serde(default)]
    pub context: SkillContext,
    /// 是否允许 `/name` 直调（默认 true）。
    #[serde(
        rename = "user-invocable",
        alias = "user_invocable",
        default = "default_true"
    )]
    pub user_invocable: bool,
    /// 参数提示（补全用）。
    #[serde(rename = "argument-hint", alias = "argument_hint")]
    pub argument_hint: Option<String>,
    /// 命中文件操作时条件激活的 glob 列表（首版仅记录，不参与触发）。
    #[serde(default)]
    pub paths: Vec<String>,
}

/// `user_invocable` 的 serde 默认值（SPEC：默认 true）。
fn default_true() -> bool {
    true
}

/// 一个已解析的 skill：目录名 + frontmatter + 正文。
#[derive(Debug, Clone)]
pub struct Skill {
    /// skill 名（SKILL.md 所在目录名）。
    pub name: String,
    /// SKILL.md 所在目录（`${WAVECODE_SKILL_DIR}` 展开目标）。
    pub dir: PathBuf,
    /// 来源（决定覆盖优先级）。
    pub source: SkillSource,
    /// frontmatter。
    pub meta: SkillMeta,
    /// Markdown 正文（frontmatter 之后的全部内容，去首尾空白）。
    pub body: String,
}

impl Skill {
    /// 解析一个 skill 目录（`<dir>/SKILL.md`）。
    ///
    /// SKILL.md 缺失 / 读取失败 / frontmatter 非法（含缺 `description`）均
    /// 返回 Err——调用方（[`discover`]）转为警告跳过，单点坏文件不炸发现。
    pub fn parse(dir: &Path, source: SkillSource) -> Result<Self, SkillError> {
        let path = dir.join("SKILL.md");
        let raw = std::fs::read_to_string(&path).map_err(|e| SkillError::Read {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        let (frontmatter, body) = split_frontmatter(&raw).ok_or_else(|| SkillError::Parse {
            path: path.clone(),
            reason: "缺少 YAML frontmatter（以 --- 分隔的头部块）".to_owned(),
        })?;
        let meta: SkillMeta = serde_yaml::from_str(frontmatter).map_err(|e| SkillError::Parse {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        if meta.description.trim().is_empty() {
            return Err(SkillError::Parse {
                path: path.clone(),
                reason: "description 为必填字段且不得为空".to_owned(),
            });
        }
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| SkillError::Parse {
                path: path.clone(),
                reason: "无法从目录路径取 skill 名".to_owned(),
            })?;
        Ok(Self {
            name,
            dir: dir.to_path_buf(),
            source,
            meta,
            body: body.trim().to_owned(),
        })
    }

    /// inline 展开（SPEC §8.2）：`$ARGUMENTS` 替换为调用参数，
    /// `${WAVECODE_SKILL_DIR}` 替换为 skill 目录路径。
    ///
    /// 正文无 `$ARGUMENTS` 占位而调用方给了参数时，参数追加在正文末尾
    /// （对齐 Claude Code 行为：占位缺失不等于丢弃参数）。
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        let mut out = self
            .body
            .replace(SKILL_DIR_VARIABLE, &self.dir.display().to_string());
        if out.contains(ARGUMENTS_PLACEHOLDER) {
            out = out.replace(ARGUMENTS_PLACEHOLDER, args);
        } else if !args.is_empty() {
            out.push_str("\n\n");
            out.push_str(args);
        }
        out
    }
}

/// 拆分 frontmatter 与正文：文件以 `---` 行起首、下一个 `---` 行收尾之间
/// 为 YAML frontmatter，其余为正文。返回 None = 无 frontmatter。
fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw.splitn(2, '\n');
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let rest = lines.next()?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    let body = rest[end + 4..]
        .strip_prefix('\n')
        .or_else(|| rest[end + 4..].strip_prefix("\r\n"))
        .unwrap_or(&rest[end + 4..]);
    // `---` 收尾行后若还有同行内容（如 `--- x`）不属于合法 frontmatter；
    // 宽松处理：以行为单位，收尾行只取到行尾前的部分不影响 body。
    Some((frontmatter, body))
}

/// skill 解析 / 读取错误（发现阶段转为警告）。
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// SKILL.md 读取失败。
    #[error("无法读取 {}: {reason}", .path.display())]
    Read {
        /// 出错文件。
        path: PathBuf,
        /// 底层原因。
        reason: String,
    },
    /// frontmatter 解析失败（含缺必填字段）。
    #[error("解析 {} 失败: {reason}", .path.display())]
    Parse {
        /// 出错文件。
        path: PathBuf,
        /// 底层原因。
        reason: String,
    },
}

/// 一个发现根：来源 + 目录（`<dir>/<name>/SKILL.md`）。
#[derive(Debug, Clone)]
pub struct SkillRoot {
    /// 来源（优先级）。
    pub source: SkillSource,
    /// skills 根目录（其下每个含 SKILL.md 的子目录是一个 skill）。
    pub dir: PathBuf,
}

/// 标准发现根（SPEC §8.1 优先级，低→高）：builtin（若有）<
/// `~/.wavecode/skills` < `<cwd>/.wavecode/skills`。
pub fn standard_roots(builtin: Option<PathBuf>, home: Option<&Path>, cwd: &Path) -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    if let Some(dir) = builtin {
        roots.push(SkillRoot {
            source: SkillSource::Builtin,
            dir,
        });
    }
    if let Some(home) = home {
        roots.push(SkillRoot {
            source: SkillSource::User,
            dir: home.join(".wavecode").join("skills"),
        });
    }
    roots.push(SkillRoot {
        source: SkillSource::Project,
        dir: cwd.join(".wavecode").join("skills"),
    });
    roots
}

/// 发现产物：技能集 + 警告（坏文件逐个警告跳过，不炸整体发现）。
#[derive(Debug, Default)]
pub struct Discovery {
    /// 覆盖消解后的技能集。
    pub set: SkillSet,
    /// 发现期警告（读取 / 解析失败）。
    pub warnings: Vec<String>,
}

/// 按优先级顺序发现全部 skill：`roots` 须按优先级低→高传入，同名 skill
/// 后者覆盖前者（SPEC §8.1）。根目录不存在 / 不可读静默跳过（无该来源是
/// 正常形态）；单个 skill 坏文件记警告继续。
pub fn discover(roots: &[SkillRoot]) -> Discovery {
    let mut discovery = Discovery::default();
    for root in roots {
        let entries = match std::fs::read_dir(&root.dir) {
            Ok(entries) => entries,
            // 根目录不存在 / 不可读：该来源缺席，非错误。
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("SKILL.md").is_file() {
                continue;
            }
            match Skill::parse(&dir, root.source) {
                Ok(skill) => {
                    // 同名覆盖：高优先级来源（后处理）替换低优先级。
                    discovery.set.skills.insert(skill.name.clone(), skill);
                }
                Err(e) => {
                    discovery
                        .warnings
                        .push(format!("[{}] skill 跳过: {e}", root.source.as_str()));
                }
            }
        }
    }
    discovery
}

/// 覆盖消解后的技能集（按名有序，迭代输出稳定）。
#[derive(Debug, Default)]
pub struct SkillSet {
    skills: BTreeMap<String, Skill>,
}

impl SkillSet {
    /// 直接插入一个 skill（同名覆盖）。发现管线之外的注入点：单测构造、
    /// 后续 MCP 暴露 skill 的并入（P9）。
    pub fn add(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// 按名查找。
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// 迭代（按名字典序）。
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// skill 数。
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// 渲染清单注入文本（SPEC §8.2：name + description + when_to_use；
    /// `max_chars` 为字符额度——预算 = 上下文窗口 1%，由 core 换算传入）。
    ///
    /// 超限降级策略（逐级）：全量（含 when_to_use）→ 去掉 when_to_use →
    /// 描述按均摊额度截断（`…` 结尾）→ 硬截断保总额。额度为 0 或无 skill
    /// 返回空串（调用方省略注入槽位）。
    pub fn catalog(&self, max_chars: usize) -> String {
        if self.skills.is_empty() || max_chars == 0 {
            return String::new();
        }
        let full = self.render(true, None);
        if full.len() <= max_chars {
            return full;
        }
        let no_when = self.render(false, None);
        if no_when.len() <= max_chars {
            return no_when;
        }
        // 均摊额度：每条目 "- : \n" 约 8 字符开销；下限 16 防过度截断。
        let per_entry = (max_chars / self.skills.len()).saturating_sub(8).max(16);
        let truncated = self.render(false, Some(per_entry));
        if truncated.len() <= max_chars {
            return truncated;
        }
        let mut cut = max_chars.saturating_sub(40);
        while !truncated.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n…(skills catalog truncated)", &truncated[..cut])
    }

    /// 清单渲染：`include_when` 控制 when_to_use 后缀；`desc_limit` 为单条
    /// 描述的截断额度（None 不截断）。
    fn render(&self, include_when: bool, desc_limit: Option<usize>) -> String {
        let mut out = String::new();
        for skill in self.skills.values() {
            let desc = match desc_limit {
                Some(limit) => truncate_chars(skill.meta.description.trim(), limit),
                None => skill.meta.description.trim().to_owned(),
            };
            out.push_str("- ");
            out.push_str(&skill.name);
            out.push_str(": ");
            out.push_str(&desc);
            if include_when && let Some(when) = &skill.meta.when_to_use {
                let when = when.trim();
                if !when.is_empty() {
                    out.push_str(" (when: ");
                    out.push_str(when);
                    out.push(')');
                }
            }
            out.push('\n');
        }
        out.trim_end().to_owned()
    }
}

/// 按字符截断（超限去尾加 `…`；UTF-8 边界安全）。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut t: String = text.chars().take(limit.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("{frontmatter}\n{body}")).unwrap();
    }

    // —— frontmatter 解析 ——

    #[test]
    fn parses_full_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "commit",
            r#"---
description: 创建规范 git 提交
when_to_use: 用户要求提交代码时
allowed-tools:
  - shell
  - read_file
context: fork
user-invocable: false
argument-hint: "[message]"
paths:
  - "src/**"
---"#,
            "正文：按规范提交。",
        );
        let root = SkillRoot {
            source: SkillSource::Project,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        let skill = discovery.set.get("commit").unwrap();
        assert_eq!(skill.meta.description, "创建规范 git 提交");
        assert_eq!(
            skill.meta.when_to_use.as_deref(),
            Some("用户要求提交代码时")
        );
        assert_eq!(skill.meta.allowed_tools, vec!["shell", "read_file"]);
        assert_eq!(skill.meta.context, SkillContext::Fork);
        assert!(!skill.meta.user_invocable);
        assert_eq!(skill.meta.argument_hint.as_deref(), Some("[message]"));
        assert_eq!(skill.meta.paths, vec!["src/**"]);
        assert_eq!(skill.body, "正文：按规范提交。");
        assert_eq!(skill.source, SkillSource::Project);
    }

    #[test]
    fn defaults_and_alias_spellings() {
        let dir = tempfile::tempdir().unwrap();
        // snake_case 拼写（SPEC 表混用 kebab/snake，两种都接受）+ 缺省值。
        write_skill(
            dir.path(),
            "review",
            "---\ndescription: 评审代码\nwhen-to-use: 提到评审时\nallowed_tools: [grep]\n---",
            "评审正文",
        );
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        let skill = discovery.set.get("review").unwrap();
        assert_eq!(skill.meta.when_to_use.as_deref(), Some("提到评审时"));
        assert_eq!(skill.meta.allowed_tools, vec!["grep"]);
        // 缺省：inline / user_invocable=true / 无 hint / 无 paths。
        assert_eq!(skill.meta.context, SkillContext::Inline);
        assert!(skill.meta.user_invocable);
        assert!(skill.meta.argument_hint.is_none());
        assert!(skill.meta.paths.is_empty());
    }

    #[test]
    fn missing_or_empty_description_is_warning_skip() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "nodesc", "---\nwhen_to_use: x\n---", "正文");
        write_skill(
            dir.path(),
            "emptydesc",
            "---\ndescription: \"\"\n---",
            "正文",
        );
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert_eq!(discovery.set.len(), 0);
        assert_eq!(discovery.warnings.len(), 2);
        assert!(
            discovery.warnings[0].contains("description")
                || discovery.warnings[1].contains("description")
        );
    }

    #[test]
    fn file_without_frontmatter_is_warning_skip() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "plain", "没有头部", "正文");
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert_eq!(discovery.set.len(), 0);
        assert_eq!(discovery.warnings.len(), 1);
    }

    // —— 来源优先级 ——

    /// SPEC §8 验收：同名覆盖，builtin < user < project。
    #[test]
    fn higher_priority_source_overrides_same_name() {
        let builtin = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        for (root, desc) in [
            (builtin.path(), "builtin 版"),
            (user.path(), "user 版"),
            (project.path(), "project 版"),
        ] {
            write_skill(
                root,
                "lint",
                &format!("---\ndescription: {desc}\n---"),
                "正文",
            );
        }
        // 只存在于低优先级来源的 skill 保留。
        write_skill(
            user.path(),
            "only-user",
            "---\ndescription: 仅用户级\n---",
            "正文",
        );
        let roots = [
            SkillRoot {
                source: SkillSource::Builtin,
                dir: builtin.path().to_path_buf(),
            },
            SkillRoot {
                source: SkillSource::User,
                dir: user.path().to_path_buf(),
            },
            SkillRoot {
                source: SkillSource::Project,
                dir: project.path().to_path_buf(),
            },
        ];
        let discovery = discover(&roots);
        let lint = discovery.set.get("lint").unwrap();
        assert_eq!(lint.meta.description, "project 版");
        assert_eq!(lint.source, SkillSource::Project);
        let only_user = discovery.set.get("only-user").unwrap();
        assert_eq!(only_user.source, SkillSource::User);
        // 不存在的根目录静默跳过。
        let missing = discover(&[SkillRoot {
            source: SkillSource::User,
            dir: user.path().join("nope"),
        }]);
        assert!(missing.set.is_empty() && missing.warnings.is_empty());
    }

    // —— inline 展开 ——

    /// SPEC §8 验收：$ARGUMENTS 替换与 ${WAVECODE_SKILL_DIR} 变量。
    #[test]
    fn expand_replaces_arguments_and_skill_dir() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "fix",
            "---\ndescription: 修问题\n---",
            "修复 $ARGUMENTS，参考 ${WAVECODE_SKILL_DIR}/notes.md",
        );
        let root = SkillRoot {
            source: SkillSource::Project,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        let skill = discovery.set.get("fix").unwrap();
        let expanded = skill.expand("崩溃问题");
        assert_eq!(
            expanded,
            format!("修复 崩溃问题，参考 {}/notes.md", skill.dir.display())
        );
        // 无参数：占位替换为空串。
        assert!(skill.expand("").contains("修复 ，参考"));
    }

    #[test]
    fn expand_appends_args_when_placeholder_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "plain",
            "---\ndescription: 无占位\n---",
            "按规范执行。",
        );
        let root = SkillRoot {
            source: SkillSource::Project,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        let skill = discovery.set.get("plain").unwrap();
        assert_eq!(skill.expand("额外参数"), "按规范执行。\n\n额外参数");
        assert_eq!(skill.expand(""), "按规范执行。");
    }

    // —— 清单注入 ——

    fn catalog_set(entries: &[(&str, &str, Option<&str>)]) -> SkillSet {
        let mut skills = BTreeMap::new();
        for (name, desc, when) in entries {
            skills.insert(
                name.to_string(),
                Skill {
                    name: name.to_string(),
                    dir: PathBuf::from("/tmp/x"),
                    source: SkillSource::Project,
                    meta: SkillMeta {
                        description: desc.to_string(),
                        when_to_use: when.map(str::to_owned),
                        allowed_tools: vec![],
                        context: SkillContext::Inline,
                        user_invocable: true,
                        argument_hint: None,
                        paths: vec![],
                    },
                    body: String::new(),
                },
            );
        }
        SkillSet { skills }
    }

    #[test]
    fn catalog_renders_name_description_when() {
        let set = catalog_set(&[
            ("commit", "创建提交", Some("用户要求提交时")),
            ("review", "评审代码", None),
        ]);
        let catalog = set.catalog(10_000);
        assert!(catalog.contains("- commit: 创建提交 (when: 用户要求提交时)"));
        assert!(catalog.contains("- review: 评审代码"));
        // 空集 / 零额度 → 空串（槽位省略）。
        assert!(SkillSet::default().catalog(10_000).is_empty());
        assert!(set.catalog(0).is_empty());
    }

    /// SPEC §8 验收：预算超限截断——先去 when_to_use，再截断描述，
    /// 任意额度下结果不超限。
    #[test]
    fn catalog_truncates_to_budget() {
        let long_desc = "这是一段很长很长的能力描述，用来撑爆注入预算。";
        let long_when = "这段 when_to_use 同样很长，也应为预算让路。";
        let entries: Vec<(String, String, Option<String>)> = (0..20)
            .map(|i| {
                (
                    format!("skill-{i:02}"),
                    format!("{long_desc}{i}"),
                    Some(format!("{long_when}{i}")),
                )
            })
            .collect();
        let refs: Vec<(&str, &str, Option<&str>)> = entries
            .iter()
            .map(|(n, d, w)| (n.as_str(), d.as_str(), w.as_deref()))
            .collect();
        let set = catalog_set(&refs);
        let full = set.catalog(100_000);
        assert!(
            full.len() > 600,
            "全量清单应足够长以触发降级: {}",
            full.len()
        );
        for budget in [600usize, 400, 200] {
            let catalog = set.catalog(budget);
            assert!(
                catalog.len() <= budget,
                "预算 {budget} 超支: {} > {budget}",
                catalog.len()
            );
            assert!(!catalog.is_empty());
        }
        // 宽裕预算下先丢 when_to_use 保描述。
        let no_when_budget = set.render(false, None).len() + 10;
        let catalog = set.catalog(no_when_budget);
        assert!(!catalog.contains("(when:"));
        assert!(catalog.contains(long_desc));
    }
}
