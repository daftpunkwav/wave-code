//! 自动提取产出的解析（SPEC §7.2 简化首版）。
//!
//! 提取子代理被要求按下面的线格式输出候选条目（见 core 侧的提取
//! preamble）；本模块把输出解析回 `(类别, 内容)` 列表，纯函数可单测。
//! 模型输出不可信：未知标签、空条目、标签前的闲聊一律丢弃——提取是
//! 尽力而为的后台任务，解析不出即不写入（core 侧失败静默记 warning）。
//!
//! ```text
//! [user] 偏好紧凑回复
//! [project] 仓库用 pnpm 管理
//! 多行内容直到下一个 [category] 标签或 EOF
//! ```

use crate::store::MemoryCategory;

/// 解析提取产出：`[user]` / `[feedback]` / `[project]` / `[reference]`
/// 标签行开始一个条目（标签与内容可同行，也可独占一行），后续行至
/// 下一标签或 EOF 为条目内容；标签前的内容忽略。`NONE` / 空输出 /
/// 无合法标签 → 空列表。
///
/// 已知边界：末条标签之后的闲聊会并入末条条目（与多行内容无法区分）——
/// 提取 preamble 要求子代理只输出条目行，见 core 侧提取流程。
pub fn parse_extracted_entries(text: &str) -> Vec<(MemoryCategory, String)> {
    let mut entries = Vec::new();
    let mut current: Option<(MemoryCategory, String)> = None;
    for line in text.lines() {
        match parse_tag_line(line) {
            Some((category, inline)) => {
                if let Some((cat, content)) = current.take() {
                    push_entry(&mut entries, cat, &content);
                }
                // 同行内容先占首行（带换行，与后续续行保持分隔一致）。
                let initial = if inline.is_empty() {
                    String::new()
                } else {
                    format!("{inline}\n")
                };
                current = Some((category, initial));
            }
            None => {
                if let Some((_, content)) = &mut current {
                    content.push_str(line);
                    content.push('\n');
                }
            }
        }
    }
    if let Some((cat, content)) = current {
        push_entry(&mut entries, cat, &content);
    }
    entries
}

/// 条目落库前的统一闸门：去首尾空白，空内容丢弃。
fn push_entry(
    entries: &mut Vec<(MemoryCategory, String)>,
    category: MemoryCategory,
    content: &str,
) {
    let trimmed = content.trim();
    if !trimmed.is_empty() {
        entries.push((category, trimmed.to_owned()));
    }
}

/// 解析标签行：`[category]` 起始，返回（类别， 标签后同行的内容）。
/// 标签后须为行尾或空白（防 `[userx]` 误命中）；非标签行返回 None。
fn parse_tag_line(line: &str) -> Option<(MemoryCategory, &str)> {
    let trimmed = line.trim_start();
    for category in MemoryCategory::ALL {
        let tag = format!("[{}]", category.as_str());
        if let Some(rest) = trimmed.strip_prefix(tag.as_str())
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return Some((category, rest.trim()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 基本解析：多类别、标签同行内容、续行内容、标签前闲聊忽略。
    #[test]
    fn parses_tagged_entries() {
        let output = "好的，以下是我提炼的记忆：\n[user] 偏好紧凑回复\n[project] 仓库用 pnpm 管理\n不要引入 yarn\n[feedback] 不要重构未坏代码";
        let entries = parse_extracted_entries(output);
        assert_eq!(
            entries,
            vec![
                (MemoryCategory::User, "偏好紧凑回复".to_owned()),
                (
                    MemoryCategory::Project,
                    "仓库用 pnpm 管理\n不要引入 yarn".to_owned()
                ),
                (MemoryCategory::Feedback, "不要重构未坏代码".to_owned()),
            ]
        );
    }

    /// 空输出 / NONE / 无合法标签 / 空条目 → 空列表（不写入）。
    #[test]
    fn no_entries_for_none_or_garbage() {
        assert!(parse_extracted_entries("").is_empty());
        assert!(parse_extracted_entries("NONE").is_empty());
        assert!(parse_extracted_entries("没什么值得记的。").is_empty());
        // 空条目（标签后无内容）丢弃；未知标签不是条目起始。
        assert!(parse_extracted_entries("[user]\n[project]  ").is_empty());
        assert!(parse_extracted_entries("[unknown] x").is_empty());
    }
}
