//! CJK-adjacent emphasis and the bold-line prepass: wide glyphs must
//! not split spans, and pseudo-headings must break out of paragraphs.

use super::*;

/// Assert the plain text carries no literal asterisk/tilde markers
/// (the delimiter run was consumed as emphasis, not leaked).
fn assert_no_leaked_markers(lines: &[String]) {
    for line in lines {
        let plain = strip_ansi(line);
        assert!(
            !plain.contains("**") && !plain.contains("~~"),
            "leaked emphasis markers: {plain:?}"
        );
    }
}

#[test]
fn cjk_bold_flanked_by_cjk_characters() {
    let mut md = renderer();
    let lines = md.render("中文**加粗**中文", 40);
    assert!(lines[0].contains("\x1b[1m加粗\x1b[0m"), "{:?}", lines[0]);
    assert_no_leaked_markers(&lines);
}

#[test]
fn cjk_bold_with_fullwidth_punctuation_on_both_sides() {
    let mut md = renderer();
    let lines = md.render("前文：**执行环境**。后文", 40);
    assert!(
        lines[0].contains("\x1b[1m执行环境\x1b[0m"),
        "{:?}",
        lines[0]
    );
    assert_no_leaked_markers(&lines);
}

#[test]
fn cjk_bold_at_line_start_and_after_a_bullet_marker() {
    let mut md = renderer();
    let lines = md.render("**起点**: 蓝色发光方块\n\n- **终点**: 绿色方块", 40);
    assert!(lines[0].contains("\x1b[1m起点\x1b[0m"), "{:?}", lines[0]);
    assert!(
        lines[2].contains("\x1b[1m终点\x1b[0m"),
        "bold after bullet: {:?}",
        lines[2]
    );
    assert_eq!(strip_ansi(&lines[2]), "• 终点: 绿色方块");
    assert_no_leaked_markers(&lines);
}

#[test]
fn cjk_italic_strikethrough_and_code_adjacent_to_cjk() {
    let mut md = renderer();
    let lines = md.render("中文*斜体*中文\n\n中文~~删除~~中文\n\n中文`code`中文", 40);
    assert!(lines[0].contains("\x1b[3m斜体\x1b[0m"), "{:?}", lines[0]);
    assert!(lines[2].contains("删除"), "{:?}", lines[2]);
    assert!(
        lines[2].contains("\x1b[9m"),
        "strikethrough uses SGR 9: {:?}",
        lines[2]
    );
    assert!(
        !lines[4].contains('`'),
        "cjk-adjacent code spans carry no quotes: {:?}",
        lines[4]
    );
    assert_no_leaked_markers(&lines);
}

// --- Block boundaries: standalone bold lines ---
//
// Models emit `**Heading**` on its own line as a pseudo-heading.
// CommonMark folds such a line into the previous bullet/paragraph
// as a lazy continuation; the renderer must break the block so the
// line lands on its own output line.

#[test]
fn bold_line_after_a_bullet_starts_a_new_block() {
    let mut md = renderer();
    let lines = md.render("- lsp_diagnostics - 查看代码诊断信息\n**执行环境**", 60);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "• lsp_diagnostics - 查看代码诊断信息");
    assert!(
        plain.iter().any(|l| l == "执行环境"),
        "bold heading on its own line: {plain:?}"
    );
    assert!(
        !plain
            .iter()
            .any(|l| l.contains("诊断信息") && l.contains("执行环境")),
        "blocks must not glue: {plain:?}"
    );
}

#[test]
fn bold_line_after_a_paragraph_starts_a_new_block() {
    let mut md = renderer();
    let lines = md.render("查看代码诊断信息\n**执行环境**", 60);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "查看代码诊断信息");
    assert_eq!(plain[2], "执行环境", "own block: {plain:?}");
}

#[test]
fn bold_colon_lines_break_out_of_a_paragraph() {
    let mut md = renderer();
    let text = "先看环境。\n**迷宫场景**: 用 Three.js 拼出迷宫\n**起点**: 蓝色发光方块";
    let lines = md.render(text, 60);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert!(
        plain.contains(&"迷宫场景: 用 Three.js 拼出迷宫".to_string()),
        "{plain:?}"
    );
    assert!(
        plain.contains(&"起点: 蓝色发光方块".to_string()),
        "{plain:?}"
    );
    assert!(
        !plain
            .iter()
            .any(|l| l.contains("先看环境。") && l.contains("迷宫场景")),
        "blocks must not glue: {plain:?}"
    );
}

#[test]
fn bold_lines_inside_a_code_fence_are_left_alone() {
    let mut md = renderer();
    let lines = md.render("```\n**not a heading**\n```", 40);
    assert!(strip_ansi(&lines[1]).starts_with("│ **not a heading**"));
}

/// Tilde fences are fenced code too: a bold-led line inside a `~~~`
/// block must keep its exact content (no phantom blank line), and a
/// backtick run inside it must not close the tilde fence early.
#[test]
fn bold_lines_inside_a_tilde_fence_are_left_alone() {
    let mut md = renderer();
    let text = "intro\n~~~\n**not a heading**\n```\n**still code**\n```\n~~~";
    let lines = md.render(text, 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert!(
        plain.iter().any(|l| l.starts_with("│ **not a heading**")),
        "tilde-fenced bold line intact: {plain:?}"
    );
    assert!(
        plain.iter().any(|l| l.starts_with("│ **still code**")),
        "inner backtick run does not close the tilde fence: {plain:?}"
    );
    // The break-before pass must not inject a blank code line: every
    // bar line carries content.
    for line in &plain {
        assert!(
            !line.starts_with("│") || line.trim_start_matches('│').trim() != "",
            "no phantom blank inside the fence: {plain:?}"
        );
    }
}

#[test]
fn screenshot_session_lines_render_without_leaked_emphasis() {
    let mut md = renderer();
    let text = "\
搭建说明如下。
- lsp_diagnostics - 查看代码诊断信息
**执行环境**

**迷宫场景**: 用 Three.js 拼出迷宫
**起点**: 蓝色发光方块

- 中文`code`内联与**加粗**混排";
    let lines = md.render(text, 60);
    assert_no_leaked_markers(&lines);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert!(
        plain.iter().any(|l| l == "执行环境"),
        "standalone bold line owns a line: {plain:?}"
    );
    assert!(
        !plain
            .iter()
            .any(|l| l.contains("诊断信息") && l.contains("执行环境")),
        "blocks must not glue: {plain:?}"
    );
}
