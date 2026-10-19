//! Fenced code blocks: framing, language tags, sanitization, diff
//! styles, and the diagram seam.

use super::*;

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

/// The code-frame width cap still holds for highlighted code: wide
/// terminals get modest frames.
#[test]
fn code_frames_still_cap_at_eighty_columns() {
    let mut md = renderer();
    let lines = md.render("```rs\nfn a() {}\n```\n", 130);
    let frame = lines
        .iter()
        .find(|l| strip_ansi(l).contains('╭'))
        .expect("a frame top");
    assert!(
        width::width(&strip_ansi(frame)) <= 80,
        "code frame capped: {frame:?}"
    );
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

/// Diagram bodies are painted with the text style: unpainted spans
/// inherit the terminal default, which a recolored terminal background
/// leaves unreadable.
#[test]
fn diagram_rows_carry_the_text_style() {
    let ink = Style::new().fg(Color::rgb(1, 2, 3));
    let mut md = Markdown::new(
        MarkdownStyle {
            text: ink,
            ..MarkdownStyle::default()
        },
        Box::new(PlainHighlighter),
    );
    let lines = md.render("```mermaid\nflowchart TD\n a --> b\n```", 60);
    let diagram_row = lines
        .iter()
        .find(|l| strip_ansi(l).contains('┌'))
        .expect("a diagram frame row");
    assert!(
        diagram_row.contains("\x1b[38;2;1;2;3m"),
        "diagram row painted with the text token: {diagram_row:?}"
    );
}

/// Diagrams draw on the full terminal width: the code-frame width cap
/// would strand a wide diagram on a narrow frame no matter how wide
/// the terminal is.
#[test]
fn wide_diagrams_use_the_full_body_width() {
    // Three 34-column boxes plus gutters need ~108 columns: past the
    // 80-column code-frame cap, inside a 120-column body budget.
    let a = format!("start-{}", "x".repeat(24));
    let b = format!("middle-{}", "y".repeat(23));
    let c = format!("final-{}", "z".repeat(24));
    let src = format!("```mermaid\nflowchart LR\n A[{a}] --> B[{b}] --> C[{c}]\n```");
    let mut md = renderer();
    // At code-frame width the diagram cannot fit and falls back to the
    // source view (the raw fence text, no box drawing).
    let narrow = md.render(&src, 80);
    assert!(
        !narrow.iter().any(|l| strip_ansi(l).contains('┌')),
        "wide diagram falls back at frame width: {narrow:?}"
    );
    // The same diagram renders once the terminal is wide enough.
    let wide = md.render(&src, 120);
    assert!(
        wide.iter().any(|l| strip_ansi(l).contains('┌')),
        "wide diagram renders on a wide terminal: {wide:?}"
    );
    assert!(
        wide.iter().any(|l| strip_ansi(l).contains('▶')),
        "side arrows present: {wide:?}"
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
