//! Black-box integration tests for the public markdown render
//! contract: output shape, caching, rhythm, and the fence-renderer
//! seam. Only public API (`tui_engine::markdown::*`) is exercised.

use tui_engine::markdown::{FenceRenderer, Markdown, MarkdownStyle, PlainHighlighter};

fn renderer() -> Markdown {
    Markdown::new(MarkdownStyle::default(), Box::new(PlainHighlighter))
}

fn plain(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| tui_engine::width::strip_ansi(l))
        .collect()
}

/// One call returns one string per output row; identical input and
/// width hand back the same allocation (the cache), while a width
/// change re-renders.
#[test]
fn render_contract_shape_and_cache() {
    let mut md = renderer();
    let first = md.render("# Title\n\nbody text", 40);
    assert!(!first.is_empty());
    let again = md.render("# Title\n\nbody text", 40);
    assert!(
        std::sync::Arc::ptr_eq(&first, &again),
        "same input+width reuses the allocation"
    );
    let wider = md.render("# Title\n\nbody text", 80);
    assert!(
        !std::sync::Arc::ptr_eq(&first, &wider),
        "width change re-renders"
    );
    assert_eq!(plain(&first), plain(&wider)[..first.len()]);
}

/// Exactly one blank line separates blocks — never two, and the
/// output neither starts nor ends blank.
#[test]
fn blocks_separate_with_exactly_one_blank_line() {
    let mut md = renderer();
    let text =
        "para one\n\n## Heading\n\n- a\n- b\n\n> quote\n\n---\n\n| h |\n|---|\n| c |\n\nlast";
    let lines = plain(&md.render(text, 40));
    assert!(
        !lines.first().is_some_and(|l| l.is_empty()),
        "no leading blank"
    );
    assert!(
        !lines.last().is_some_and(|l| l.is_empty()),
        "no trailing blank"
    );
    for pair in lines.windows(2) {
        assert!(
            !(pair[0].is_empty() && pair[1].is_empty()),
            "double blank: {lines:?}"
        );
    }
}

/// Fence markers never leak, and every code row sits inside a frame
/// whose top and bottom rules share one width.
#[test]
fn code_fences_render_framed_without_markers() {
    let mut md = renderer();
    let lines = plain(&md.render("```rust\nfn a() {}\nfn b() {}\n```", 40));
    assert!(lines[0].starts_with("╭─ rust "), "{lines:?}");
    assert!(lines.last().unwrap().starts_with('╰'), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("│ fn a() {}")));
    assert!(!lines.iter().any(|l| l.contains("```")), "{lines:?}");
    let grid = tui_engine::width::width(&lines[0]);
    assert!(
        lines.iter().all(|l| tui_engine::width::width(l) == grid),
        "one shared frame grid: {lines:?}"
    );
}

/// An unclosed fence still renders (streaming): the frame closes
/// around what has arrived so far.
#[test]
fn unclosed_fences_render_streaming() {
    let mut md = renderer();
    let lines = plain(&md.render("```python\nx = 1", 40));
    assert!(lines[0].starts_with("╭─ python "), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("│ x = 1")));
    assert!(lines.last().unwrap().starts_with('╰'));
}

/// A custom fence renderer renders before the highlighter; other
/// languages (and a `None` from the renderer) fall through.
#[test]
fn custom_fences_plug_into_the_seam() {
    struct UpperFences;

    impl FenceRenderer for UpperFences {
        fn render_fence(&self, lang: &str, code: &str, _columns: usize) -> Option<Vec<String>> {
            (lang == "upper").then(|| vec![code.to_ascii_uppercase()])
        }
    }

    let mut md = Markdown::new(MarkdownStyle::default(), Box::new(PlainHighlighter))
        .with_fence(Box::new(UpperFences));
    let lines = plain(&md.render("```upper\nshout\n```", 40));
    assert!(lines.iter().any(|l| l.starts_with("│ SHOUT")), "{lines:?}");

    let lines = plain(&md.render("```text\nshout\n```", 40));
    assert!(lines.iter().any(|l| l.starts_with("│ shout")), "{lines:?}");
}

/// Tables render as box-drawing grids whose rows share one display
/// width; CJK cells cannot pull the borders out of alignment.
#[test]
fn tables_render_on_one_shared_grid() {
    let mut md = renderer();
    let text = "| 检查项 | 结果 |\n|---|---|\n| 编译通过 | ✅ |\n| 文档同步 | ❌ |";
    let lines = plain(&md.render(text, 60));
    let grid = tui_engine::width::width(&lines[0]);
    assert!(
        lines.iter().all(|l| tui_engine::width::width(l) == grid),
        "one grid: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains('├') && l.contains('┼')));
}

/// Clearing the cache re-renders to identical content.
#[test]
fn clear_cache_preserves_output() {
    let mut md = renderer();
    let first = md.render("hello **world**", 40);
    md.clear_cache();
    let second = md.render("hello **world**", 40);
    assert_eq!(*first, *second);
}
