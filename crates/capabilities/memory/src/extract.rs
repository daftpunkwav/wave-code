//! Auto-extract output parsing (simplified first version).
//!
//! Extracting subagents are asked to emit candidate entries in the line format
//! below (see the core-side extraction preamble); this module parses that
//! output back into a `(category, content)` list as a pure, unit-testable
//! function. Model output is untrusted: unknown tags, empty entries, and
//! chatter before the tags are all dropped — extraction is a best-effort
//! background task, and unparseable output simply means nothing is written
//! (the core side logs a silent warning on failure).
//!
//! ```text
//! [user] prefers compact replies
//! [project] repo uses pnpm
//! multi-line content runs until the next [category] tag or EOF
//! ```

use crate::store::MemoryCategory;

/// Parse extraction output: a `[user]` / `[feedback]` / `[project]` /
/// `[reference]` tag line starts an entry (tag and content may share one line,
/// or the tag may stand alone); following lines up to the next tag or EOF form
/// the entry body. Content before the tags is ignored. `NONE` / empty output /
/// no valid tags -> empty list.
///
/// Known limitation: chatter after the last tag merges into the last entry
/// (indistinguishable from multi-line content) — the extraction preamble asks
/// subagents to emit entry lines only; see the core-side extraction flow.
pub fn parse_extracted_entries(text: &str) -> Vec<(MemoryCategory, String)> {
    let mut entries = Vec::new();
    let mut current: Option<(MemoryCategory, String)> = None;
    for line in text.lines() {
        match parse_tag_line(line) {
            Some((category, inline)) => {
                if let Some((cat, content)) = current.take() {
                    push_entry(&mut entries, cat, &content);
                }
                // Same-line content takes the first row (with a newline, keeping
                // the same separation as following continuation rows).
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

/// Single gate before persisting an entry: trim surrounding whitespace and
/// drop empty content.
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

/// Parse a tag line: starts with `[category]`, returns (category, same-line
/// content after the tag). The tag must be followed by end-of-line or
/// whitespace (so `[userx]` never matches); non-tag lines return None.
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

    /// Basic parsing: multiple categories, same-line and continuation content,
    /// pre-tag chatter ignored.
    #[test]
    fn parses_tagged_entries() {
        let output = "Sure, here are the memories I distilled:\n[user] prefers compact replies\n[project] repo uses pnpm\nnever introduce yarn\n[feedback] do not refactor working code";
        let entries = parse_extracted_entries(output);
        assert_eq!(
            entries,
            vec![
                (MemoryCategory::User, "prefers compact replies".to_owned()),
                (
                    MemoryCategory::Project,
                    "repo uses pnpm\nnever introduce yarn".to_owned()
                ),
                (
                    MemoryCategory::Feedback,
                    "do not refactor working code".to_owned()
                ),
            ]
        );
    }

    /// Empty output / NONE / no valid tags / empty entries -> empty list
    /// (nothing written).
    #[test]
    fn no_entries_for_none_or_garbage() {
        assert!(parse_extracted_entries("").is_empty());
        assert!(parse_extracted_entries("NONE").is_empty());
        assert!(parse_extracted_entries("nothing worth remembering.").is_empty());
        // Empty entries (no content after the tag) are dropped; unknown tags
        // never start an entry.
        assert!(parse_extracted_entries("[user]\n[project]  ").is_empty());
        assert!(parse_extracted_entries("[unknown] x").is_empty());
    }
}
