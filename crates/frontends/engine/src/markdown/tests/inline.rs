//! Inline styling: prose, emphasis, headings, lists, quotes, rules,
//! links, math, and the preprocessing folds.

use super::*;

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
