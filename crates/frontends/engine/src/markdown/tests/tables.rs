//! Box-drawing tables: shared grids, alignment, control-character
//! flattening, and task-list checkboxes.

use super::*;

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
