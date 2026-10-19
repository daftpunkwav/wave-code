//! Vertical rhythm between blocks: the single-blank rule, the heading
//! section break, and list hanging indents.

use super::*;

/// Assert exactly one blank line separates blocks: no two
/// consecutive blanks anywhere, and no leading blank. A heading
/// documents its own extra break and is not fed here.
fn assert_clean_rhythm(lines: &[String]) {
    assert!(
        lines.first().is_some_and(|l| !l.is_empty()),
        "document must not start blank: {lines:?}"
    );
    for pair in lines.windows(2) {
        assert!(
            !(pair[0].is_empty() && pair[1].is_empty()),
            "double blank line: {lines:?}"
        );
    }
}

/// Assert the heading rhythm: exactly two blanks before a heading
/// (the section break), at most one after it.
fn assert_heading_breaks(plain: &[String], heading: &str) {
    let at = plain
        .iter()
        .position(|l| l == heading)
        .unwrap_or_else(|| panic!("heading {heading:?} missing: {plain:?}"));
    assert!(
        at >= 2 && plain[at - 1].is_empty() && plain[at - 2].is_empty(),
        "two blanks before the heading: {plain:?}"
    );
    let doubles_below =
        at + 2 < plain.len() && plain[at + 1].is_empty() && plain[at + 2].is_empty();
    assert!(
        !doubles_below,
        "at most one blank after the heading: {plain:?}"
    );
}

#[test]
fn screenshot_corpus_list_heading_list_fence_has_rhythm() {
    let mut md = renderer();
    let text = "\
- ls - 列出目录内容
## Shell 与代码执行

- grep - 搜索文本
```bash
ls -la
```
";
    let lines = md.render(text, 60);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_heading_breaks(&plain, "Shell 与代码执行");
    assert_eq!(plain[0], "• ls - 列出目录内容");
    assert_eq!(plain[1], "", "section break before heading after list");
    assert_eq!(plain[2], "");
    assert_eq!(plain[3], "Shell 与代码执行");
    assert_eq!(plain[4], "", "blank after heading before list");
    assert_eq!(plain[5], "• grep - 搜索文本");
    assert_eq!(plain[6], "", "blank between list and fence");
    assert!(
        plain[7].starts_with("╭─ bash "),
        "frame after the blank: {plain:?}"
    );
    assert!(plain[8].starts_with("│ ls -la"));
    assert!(plain[9].starts_with("╰"));
}

#[test]
fn list_then_paragraph_gets_a_blank_line() {
    let mut md = renderer();
    // Without the blank line the paragraph would be a lazy
    // continuation of the last list item (correct CommonMark).
    let lines = md.render("- a\n- b\n\nparagraph", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["• a", "• b", "", "paragraph"]);
}

#[test]
fn paragraph_then_heading_gets_a_section_break() {
    let mut md = renderer();
    let lines = md.render("hello\n## Title", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["hello", "", "", "Title"]);
}

#[test]
fn heading_then_paragraph_has_exactly_one_blank() {
    let mut md = renderer();
    let lines = md.render("## Title\nhello", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["Title", "", "hello"]);
}

/// A heading opening the document stays flush: no leading blanks.
#[test]
fn leading_heading_stays_flush() {
    let mut md = renderer();
    let lines = md.render("## Title\nhello", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "Title", "no leading break: {plain:?}");
}

/// Wrapped list items hang: continuation lines align under the item
/// text, not under the marker.
#[test]
fn list_items_hang_their_continuation_lines() {
    let mut md = renderer();
    let lines = md.render("- one two three four five six seven eight", 20);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "• one two three fo", "first row: {plain:?}");
    assert_eq!(plain[1], "  ur five six seven", "hanging row: {plain:?}");
    assert_eq!(plain[2], "  eight", "hanging row: {plain:?}");
}

/// Ordered items hang by their wider marker: continuation lines align
/// under the text column ("1. " = three columns).
#[test]
fn ordered_items_hang_by_their_marker_width() {
    let mut md = renderer();
    let lines = md.render("1. one two three four five six", 20);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "1. one two three", "first row: {plain:?}");
    assert_eq!(plain[1], "   four five six", "hanging row: {plain:?}");
}

#[test]
fn paragraphs_and_code_blocks_stay_separated() {
    let mut md = renderer();
    let lines = md.render("first\n```py\nx = 1\n```\nlast", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "first");
    assert_eq!(plain[1], "", "blank before fence");
    assert!(plain[4].starts_with("╰"), "bottom frame: {plain:?}");
    assert_eq!(plain[5], "", "blank after fence");
    assert_eq!(plain[6], "last");
    assert_clean_rhythm(&plain);
}

#[test]
fn table_then_paragraph_gets_a_blank_line() {
    let mut md = renderer();
    // The blank line ends the table; a bare following line parses
    // as another table row.
    let lines = md.render("| a |\n|---|\n| 1 |\n\nafter", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    let last = plain.len() - 1;
    assert_eq!(plain[last], "after");
    assert_eq!(plain[last - 1], "", "blank between table and text");
    assert_eq!(plain[last - 2], "└───┘");
    assert_clean_rhythm(&plain);
}

#[test]
fn rule_separates_from_surrounding_blocks() {
    let mut md = renderer();
    let lines = md.render("- a\n---\n- b", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_clean_rhythm(&plain);
    assert_eq!(plain[1], "", "blank before rule");
    assert!(plain[2].starts_with("─"));
    assert_eq!(plain[3], "", "blank after rule");
    assert_eq!(plain[4], "• b");
}

#[test]
fn nested_list_items_do_not_open_separation() {
    let mut md = renderer();
    let lines = md.render("- a\n  - b\n- c", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["• a", "  • b", "• c"]);
}
