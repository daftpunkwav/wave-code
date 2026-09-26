use super::*;
use crate::color::Color;
use crate::width::strip_ansi;

fn renderer() -> Markdown {
    Markdown::new(MarkdownStyle::default(), Box::new(PlainHighlighter))
}

/// One entry per display column: wide glyphs repeat into both of
/// their cells, so plain indexing matches what a terminal shows.
fn display_columns(line: &str) -> Vec<char> {
    let mut cols = Vec::new();
    for ch in line.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1);
        cols.push(ch);
        for _ in 1..w {
            cols.push(ch);
        }
    }
    cols
}

#[test]
fn paragraphs_render_as_lines() {
    let mut md = renderer();
    let lines = md.render("hello world", 40);
    assert_eq!(strip_ansi(&lines[0]), "hello world");
}

#[test]
fn plain_prose_rides_the_text_style() {
    // Body prose takes the configured text style (near-white in the
    // console themes), never an accent hue.
    let style = MarkdownStyle {
        text: Style::new().fg(Color::rgb(1, 2, 3)),
        ..MarkdownStyle::default()
    };
    let mut md = Markdown::new(style, Box::new(PlainHighlighter));
    let lines = md.render("plain text", 40);
    assert!(
        lines[0].contains("[38;2;1;2;3mplain text"),
        "{:?}",
        lines[0]
    );
    // List bullets are prose too.
    let lines = md.render("- item", 40);
    assert!(lines[0].contains("[38;2;1;2;3mitem"), "{:?}", lines[0]);
    // Table body cells follow; header cells keep the heading style.
    let lines = md.render(
        "| h |
|---|
| c |",
        40,
    );
    assert!(
        lines[3].contains("[38;2;1;2;3m") && lines[1].contains("[1m"),
        "{lines:?}"
    );
}

#[test]
fn bold_and_code_styled() {
    let mut md = renderer();
    let lines = md.render("**hi** `code`", 40);
    assert!(lines[0].contains("\x1b[1mhi\x1b[0m"), "{:?}", lines[0]);
    assert!(
        !lines[0].contains('`'),
        "code spans carry no visible quotes: {:?}",
        lines[0]
    );
}

#[test]
fn heading_h1_is_bold_and_underlined() {
    let mut md = renderer();
    let lines = md.render("# Title", 40);
    assert!(lines[0].contains("Title"));
    assert!(lines[0].contains("\x1b[1m"), "bold: {:?}", lines[0]);
    assert_eq!(strip_ansi(&lines[1]), "─────");
}

#[test]
fn code_blocks_render_framed_without_fence_markers() {
    let mut md = renderer();
    let lines = md.render("```rust\nfn a() {}\n```", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert!(plain[0].starts_with("╭─ rust "), "top frame: {plain:?}");
    assert!(plain[0].ends_with('╮'), "top frame closes: {plain:?}");
    assert!(
        plain[1].starts_with("│ fn a() {}") && plain[1].trim_end().ends_with('│'),
        "bar + code: {plain:?}"
    );
    assert!(plain[2].starts_with("╰─"), "bottom frame: {plain:?}");
    assert!(plain[2].ends_with('╯'), "bottom frame closes: {plain:?}");
    assert!(
        !plain.iter().any(|l| l.contains("```")),
        "no literal fence markers: {plain:?}"
    );
    // The frame rules share one width so top and bottom align, and
    // every row spans the full frame (right border included).
    let grid = width::width(&plain[0]);
    assert!(plain.iter().all(|l| width::width(l) == grid), "{plain:?}");
}

#[test]
fn code_frame_carries_language_tag_only_when_known() {
    let mut md = renderer();
    let tagged = md.render("```python\nx = 1\n```", 40);
    assert!(
        strip_ansi(&tagged[0]).starts_with("╭─ python "),
        "tagged frame: {:?}",
        strip_ansi(&tagged[0])
    );
    let untagged = md.render("```\nx = 1\n```", 40);
    assert!(
        strip_ansi(&untagged[0]).starts_with("╭──"),
        "plain frame: {:?}",
        strip_ansi(&untagged[0])
    );
    assert!(!strip_ansi(&untagged[0]).contains("python"));
}

#[test]
fn language_tag_drops_unsafe_characters() {
    let mut md = renderer();
    // Control characters and punctuation in the info string must
    // never reach the rendered frame.
    let lines = md.render("```py\x1b[31m!@\n x = 1\n```", 40);
    let plain = strip_ansi(&lines[0]);
    assert!(
        !plain.contains('\x1b') && !plain.contains('!') && !plain.contains('@'),
        "sanitized tag: {plain:?}"
    );
    assert!(plain.contains("py31m"), "whitelisted chars stay: {plain:?}");
}

#[test]
fn unclosed_code_block_still_renders_streaming() {
    let mut md = renderer();
    let lines = md.render("```rust\nfn a() {}", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert!(plain[0].starts_with("╭─ rust "), "{plain:?}");
    assert!(plain[1].starts_with("│ fn a() {}"));
}

#[test]
fn lists_render_bullets_and_numbers() {
    let mut md = renderer();
    let lines = md.render("- a\n- b", 40);
    assert_eq!(strip_ansi(&lines[0]), "• a");
    assert_eq!(strip_ansi(&lines[1]), "• b");
    let lines = md.render("1. x\n2. y", 40);
    assert_eq!(strip_ansi(&lines[0]), "1. x");
    assert_eq!(strip_ansi(&lines[1]), "2. y");
}

#[test]
fn quotes_render_with_bar_prefix() {
    let mut md = renderer();
    let lines = md.render("> quoted text", 40);
    assert_eq!(strip_ansi(&lines[0]), "│ quoted text");
}

#[test]
fn rule_renders_dashes() {
    let mut md = renderer();
    let lines = md.render("---", 40);
    assert_eq!(strip_ansi(&lines[0]), "─".repeat(40));
}

#[test]
fn links_carry_osc8() {
    let mut md = renderer();
    let lines = md.render("[text](https://x.y)", 40);
    assert!(
        lines[0].contains("\x1b]8;;https://x.y\x07"),
        "osc8 link: {:?}",
        lines[0]
    );
    assert!(lines[0].contains("text"));
}

#[test]
fn links_with_control_chars_degrade_to_text() {
    let mut md = renderer();
    let lines = md.render("[x](badurl\x1b)", 40);
    assert!(
        !lines[0].contains("\x1b]8;;"),
        "unsafe url must not become a hyperlink: {lines:?}"
    );
}

#[test]
fn cache_returns_same_output() {
    let mut md = renderer();
    let first = md.render("cached", 40);
    let second = md.render("cached", 40);
    assert_eq!(first, second);
}

#[test]
fn tables_render_as_box_drawing() {
    let mut md = renderer();
    let lines = md.render("| a | bb |\n|---|---|\n| 1 | 2 |", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "┌───┬────┐");
    assert_eq!(plain[1], "│ a │ bb │");
    assert_eq!(plain[2], "├───┼────┤");
    assert_eq!(plain[3], "│ 1 │ 2  │");
    assert_eq!(plain[4], "└───┴────┘");
}

#[test]
fn task_lists_render_checkboxes() {
    let mut md = renderer();
    let lines = md.render("- [x] done\n- [ ] pending", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain[0], "• [x] done");
    assert_eq!(plain[1], "• [ ] pending");
}

#[test]
fn wide_tables_shrink_to_fit() {
    let mut md = renderer();
    let long = "x".repeat(60);
    let lines = md.render(&format!("| {long} |\n|---|\n| 1 |"), 40);
    assert!(
        lines.iter().all(|l| width::width(l) <= 40),
        "table fits: {lines:?}"
    );
}

/// The screenshot regression: a two-column CJK table with checkmark
/// glyphs must land on one shared grid — every line the same display
/// width, one line per row, no row merged into a neighbor.
#[test]
fn cjk_checkmark_table_renders_one_grid() {
    let mut md = renderer();
    let text = "\
| 检查项 | 结果 |
|---|---|
| 编译通过 | ✅ |
| 测试全绿 | ✔ |
| 文档同步 | ❌ |
| 依赖锁定 | ✅ |
| lint 干净 | ✅ |
| 格式化 | ✅ |";
    let lines = md.render(text, 60);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    // Top rule, header, mid rule, six body rows, bottom rule.
    assert_eq!(plain.len(), 10, "one line per row: {plain:?}");
    let grid = width::width(&plain[0]);
    assert!(
        plain.iter().all(|l| width::width(l) == grid),
        "every line shares the grid width {grid}: {plain:?}"
    );
    // Every line's border glyphs sit at the same display columns:
    // the ┬ seam of the top rule lines up with the │ of every row.
    let seam = display_columns(&plain[0])
        .into_iter()
        .position(|c| c == '┬')
        .expect("seam in the top rule");
    for line in &plain {
        let cols = display_columns(line);
        if line.starts_with('│') {
            assert_eq!(cols[seam], '│', "seam aligned: {line:?}");
        } else {
            assert!(
                matches!(cols[seam], '┬' | '┼' | '┴'),
                "rule crossing aligned: {line:?}"
            );
        }
    }
    // Body rows stay separate.
    assert!(plain[3].contains("编译通过"), "{plain:?}");
    assert!(plain[4].contains("测试全绿"), "{plain:?}");
    assert!(plain[8].contains("格式化"), "{plain:?}");
}

/// Ragged rows (fewer cells than the widest row) pad with empty
/// cells instead of pulling the closing border out of alignment.
#[test]
fn ragged_rows_pad_onto_the_shared_grid() {
    let mut md = renderer();
    let lines = md.render("| a | b |\n|---|---|\n| only |", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    let grid = width::width(&plain[0]);
    assert!(
        plain.iter().all(|l| width::width(l) == grid),
        "one grid: {plain:?}"
    );
    assert_eq!(plain[3], "│ only │   │", "{plain:?}");
}

/// Control whitespace inside a cell flattens to spaces; a raw
/// newline must never shatter a row across terminal lines.
#[test]
fn cell_control_whitespace_flattens() {
    assert_eq!(flat_cell("a\nb"), "a b");
    assert_eq!(flat_cell("a\r\rb"), "a  b");
    assert_eq!(flat_cell("a\tb"), "a b");
    assert_eq!(flat_cell("clean"), "clean");
}

/// Code block content rides raw into the highlighter: escape
/// sequences embedded in the source (a model echoing terminal
/// output) are sanitized instead of reaching the frame.
#[test]
fn code_content_escapes_are_sanitized() {
    let mut md = renderer();
    let lines = md.render("```\n\x1b[97mbright\x1b[0m\n```", 40);
    let plain = strip_ansi(&lines[1]);
    assert!(plain.starts_with("│ bright"), "{plain:?}");
    assert!(!plain.contains("[97m"), "residue: {plain:?}");
}

/// Math segments convert to unicode ($E = mc^2$ renders the
/// superscript, no raw $ markers) — math, not source.
#[test]
fn math_segments_render_as_unicode() {
    let mut md = renderer();
    let lines = md.render("$E = mc^2$ is famous", 60);
    let plain = strip_ansi(&lines[0]);
    assert!(plain.contains("E = mc"), "{plain:?}");
    assert!(plain.contains("\u{00b2}"), "{plain:?}");
    assert!(!plain.contains('$'), "no raw dollars: {plain:?}");
}

/// A ```diff fence rides the diff styles: + green, - red, @@ in
/// the meta tone, context plain — no syntax highlighting.
#[test]
fn diff_fences_use_dedicated_line_styles() {
    let style = MarkdownStyle {
        diff_added: Style::new().fg(Color::rgb(10, 200, 10)),
        diff_removed: Style::new().fg(Color::rgb(200, 10, 10)),
        diff_meta: Style::new().fg(Color::rgb(100, 100, 100)),
        ..MarkdownStyle::default()
    };
    let mut md = Markdown::new(style, Box::new(PlainHighlighter));
    let text = "```diff\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n context\n```";
    let lines = md.render(text, 60);
    assert!(
        lines[5].contains("\u{1b}[38;2;10;200;10m"),
        "+ line green: {:?}",
        lines[5]
    );
    assert!(
        lines[4].contains("\u{1b}[38;2;200;10;10m"),
        "- line red: {:?}",
        lines[4]
    );
    assert!(
        lines[3].contains("\u{1b}[38;2;100;100;100m"),
        "hunk meta: {:?}",
        lines[3]
    );
    assert!(
        !lines[6].contains("\u{1b}[38;2;"),
        "context plain: {:?}",
        lines[6]
    );
}

/// Markdown column alignment centers `:---:` columns and
/// right-aligns `---:` columns.
#[test]
fn table_column_alignment_pads_by_alignment() {
    let mut md = renderer();
    let text = "| head | head |\n|:---:|---:|\n| ab | cd |";
    let lines = md.render(text, 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    // Column content width is 4 (from the header); the centered
    // `ab` sits one space off each side, the right-aligned `cd`
    // hugs the right border.
    assert!(
        plain[3].starts_with("│  ab ") && plain[3].ends_with("   cd │"),
        "{plain:?}"
    );
    // Header cells follow the same alignment as their columns.
    assert!(
        plain[1].starts_with("│ head ") && plain[1].ends_with("head │"),
        "{plain:?}"
    );
}

/// `==highlight==` folds onto bold instead of leaking markers.
#[test]
fn highlight_folds_onto_bold() {
    let mut md = renderer();
    let lines = md.render("==marked== text", 40);
    assert!(
        lines[0].contains("\u{1b}[1mmarked\u{1b}[0m"),
        "bold: {:?}",
        lines[0]
    );
    assert!(!lines[0].contains("=="), "{:?}", lines[0]);
}

/// `x^2^` maps onto superscript code points, intraword included;
/// unmappable spans stand.
#[test]
fn superscript_maps_to_unicode() {
    let mut md = renderer();
    let lines = md.render("x^2^ big", 40);
    assert!(
        strip_ansi(&lines[0]).contains("x\u{00b2} big"),
        "{:?}",
        lines[0]
    );
    let lines = md.render("H~2~O is water", 40);
    assert!(
        strip_ansi(&lines[0]).contains("H\u{2082}O is water"),
        "{:?}",
        lines[0]
    );
    // `~~del~~` is strikethrough, not subscript.
    let lines = md.render("~~del~~", 40);
    assert!(strip_ansi(&lines[0]).contains("del"), "{:?}", lines[0]);
    // Unmappable inner text stands.
    let lines = md.render("x^q^ big", 40);
    assert!(strip_ansi(&lines[0]).contains("x^q^ big"), "{:?}", lines[0]);
}

/// Code frames hug their content: a short snippet gets a narrow
/// frame instead of a full-width one.
#[test]
fn code_frames_hug_their_content() {
    let mut md = renderer();
    let lines = md.render("```rust\nfn a() {}\n```", 80);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    let frame_w = width::width(&plain[0]);
    assert!(frame_w < 30, "content-fit frame, got {frame_w}: {plain:?}");
    assert!(
        plain.iter().all(|l| width::width(l) == frame_w),
        "one grid: {plain:?}"
    );
}

/// Table rules and rows paint each span separately: every SGR open
/// is matched by exactly one reset, so a border can never lose its
/// style mid-line to a nested paint.
#[test]
fn table_spans_stay_balanced() {
    let style = MarkdownStyle {
        text: Style::new().fg(Color::rgb(1, 2, 3)),
        fence: Style::new().fg(Color::rgb(4, 5, 6)),
        ..MarkdownStyle::default()
    };
    let mut md = Markdown::new(style, Box::new(PlainHighlighter));
    let lines = md.render("| a | b |\n|---|---|\n| 1 | 2 |", 40);
    for line in lines.iter() {
        let opens = line.matches("\x1b[").count();
        let resets = line.matches("\x1b[0m").count();
        assert_eq!(opens, resets * 2, "balanced spans: {line:?}");
    }
}

/// A code span inside a heading rides raw: painting it separately
/// would drop an inner reset mid-heading that cuts the heading
/// style short over the tail of the line.
#[test]
fn heading_inline_code_stays_raw_so_the_heading_style_holds() {
    let style = MarkdownStyle {
        heading: Style::new().bold(),
        code: Style::new().fg(Color::rgb(9, 9, 9)),
        ..MarkdownStyle::default()
    };
    let mut md = Markdown::new(style, Box::new(PlainHighlighter));
    let lines = md.render("# Run `cargo test` First", 60);
    // One paint around the whole heading, no inner spans.
    assert_eq!(
        lines[0].matches("\x1b[").count(),
        2,
        "open + reset only: {:?}",
        lines[0]
    );
    assert!(lines[0].contains("cargo test"), "{:?}", lines[0]);
}

/// An empty heading (`##` alone) must not leave the heading state
/// stuck: later text and code spans go back to their normal styled
/// rendering.
#[test]
fn empty_heading_releases_the_raw_text_path() {
    let style = MarkdownStyle {
        text: Style::new().fg(Color::rgb(1, 2, 3)),
        code: Style::new().fg(Color::rgb(9, 9, 9)),
        ..MarkdownStyle::default()
    };
    let mut md = Markdown::new(style, Box::new(PlainHighlighter));
    let lines = md.render("##\n\nafter `code` tail", 60);
    let body = lines
        .iter()
        .find(|l| strip_ansi(l).contains("after"))
        .expect("body line");
    assert!(
        body.contains("\x1b[38;2;1;2;3m"),
        "prose styled again: {body:?}"
    );
    assert!(
        body.contains("\x1b[38;2;9;9;9m"),
        "code span styled again: {body:?}"
    );
}

// --- CJK-adjacent emphasis (regression locks) ---
//
// CommonMark flanking treats CJK ideographs as word characters, so
// emphasis flanked by CJK text or full-width punctuation must still
// parse; the locks below keep the behavior from regressing.

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

// --- Vertical rhythm between blocks ---
//
// The screenshot complaint: a list followed by a section heading
// rendered with zero blank line. Every block must separate from
// the previous one with exactly one blank line (none at the top).

/// Assert exactly one blank line separates blocks: no two
/// consecutive blanks anywhere, and no leading blank.
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
    assert_clean_rhythm(&plain);
    assert_eq!(plain[0], "• ls - 列出目录内容");
    assert_eq!(plain[1], "", "blank before heading after list");
    assert_eq!(plain[2], "Shell 与代码执行");
    assert_eq!(plain[3], "", "blank after heading before list");
    assert_eq!(plain[4], "• grep - 搜索文本");
    assert_eq!(plain[5], "", "blank between list and fence");
    assert!(
        plain[6].starts_with("╭─ bash "),
        "frame after the blank: {plain:?}"
    );
    assert!(plain[7].starts_with("│ ls -la"));
    assert!(plain[8].starts_with("╰"));
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
fn paragraph_then_heading_has_exactly_one_blank() {
    let mut md = renderer();
    let lines = md.render("hello\n## Title", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["hello", "", "Title"]);
}

#[test]
fn heading_then_paragraph_has_exactly_one_blank() {
    let mut md = renderer();
    let lines = md.render("## Title\nhello", 40);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["Title", "", "hello"]);
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

#[test]
fn streamed_full_buffer_rerender_is_stable() {
    let mut md = renderer();
    let full = "- ls - 列出目录内容\n## Shell\n```bash\nls\n```\n";
    // Live drafts arrive prefix by prefix; results are discarded.
    let _ = md.render("- ls - 列出目录内容", 60);
    let _ = md.render("- ls - 列出目录内容\n## Shell", 60);
    let streamed = md.render(full, 60);
    let mut fresh = renderer();
    assert_eq!(streamed, fresh.render(full, 60));
}

#[test]
fn streamed_bold_line_breaks_once_the_buffer_completes() {
    let mut md = renderer();
    // Live draft: only the bullet line has arrived so far.
    let _ = md.render("- lsp_diagnostics - 查看代码诊断信息", 60);
    // The bold heading arrives in a later delta; the accumulated
    // buffer must re-render with the block break.
    let lines = md.render("- lsp_diagnostics - 查看代码诊断信息\n**执行环境**", 60);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert!(
        plain.iter().any(|l| l == "执行环境"),
        "final buffer re-renders with the break: {plain:?}"
    );
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

/// A custom fence renderer plugs through the seam: its body lands
/// inside the standard frame, other languages (and a `None`) fall
/// through to the plain highlighter.
struct UpperFences;

impl FenceRenderer for UpperFences {
    fn render_fence(&self, lang: &str, code: &str, _columns: usize) -> Option<Vec<String>> {
        (lang == "upper").then(|| vec![code.to_ascii_uppercase()])
    }
}

#[test]
fn fence_renderers_plug_through_the_seam() {
    let mut md = Markdown::new(MarkdownStyle::default(), Box::new(PlainHighlighter))
        .with_fence(Box::new(UpperFences));
    let lines = md.render("```upper\nshout\n```", 40);
    assert!(strip_ansi(&lines[1]).starts_with("│ SHOUT"), "{lines:?}");
    // Untagged for the seam: the highlighter path stands.
    let lines = md.render("```text\nshout\n```", 40);
    assert!(strip_ansi(&lines[1]).starts_with("│ shout"), "{lines:?}");
}
