//! Markdown-to-ANSI rendering for assistant messages.
//!
//! A streaming-tolerant subset of the reference renderer: headings,
//! emphasis, inline code, links (OSC 8 hyperlinks), fenced code blocks
//! with pluggable syntax highlighting, lists with nesting, block
//! quotes, tables (box-drawing), and horizontal rules. One deliberate
//! deviation: a standalone `**bold**` line always starts a new block
//! (models emit these as pseudo-headings; CommonMark would fold them
//! into the previous paragraph as a lazy continuation). Output is one
//! ANSI string per terminal line; wrapping happens per block at the
//! requested width.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

use crate::color::Style;
use crate::width;

/// Syntax highlighting seam: the engine ships a no-op implementation;
/// the application layer plugs in a real highlighter without this crate
/// growing a highlighting dependency.
pub trait SyntaxHighlighter {
    /// Highlight one code block; returns one string per output line.
    fn highlight(&self, code: &str, lang: Option<&str>) -> Vec<String>;
}

/// Pass-through highlighter (plain code, no color).
pub struct PlainHighlighter;

impl SyntaxHighlighter for PlainHighlighter {
    fn highlight(&self, code: &str, _lang: Option<&str>) -> Vec<String> {
        code.lines().map(|l| l.to_string()).collect()
    }
}

/// Visual style knobs for markdown rendering.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownStyle {
    /// Headings (h1/h2 also render bold regardless of this style).
    pub heading: Style,
    /// Links and inline code (accent color).
    pub code: Style,
    /// Link styling.
    pub link: Style,
    /// Code block fence rows (muted).
    pub fence: Style,
    /// Block quote text (dim italic).
    pub quote: Style,
    /// Horizontal rules.
    pub rule: Style,
}

impl Default for MarkdownStyle {
    fn default() -> Self {
        Self {
            heading: Style::new(),
            code: Style::new(),
            link: Style::new().underline(),
            fence: Style::new().dim(),
            quote: Style::new().dim().italic(),
            rule: Style::new().dim(),
        }
    }
}

/// Streaming inline style state: bold / italic / strikethrough.
#[derive(Debug, Default, Clone)]
struct InlineState {
    bold: bool,
    italic: bool,
    strike: bool,
    /// Inside inline code (its style replaces emphasis composition).
    code: bool,
    /// Inside a link (text part).
    link: bool,
    /// Current link target for OSC 8 emission.
    link_url: Option<String>,
}

impl InlineState {
    fn compose(&self, base: MarkdownStyle) -> Style {
        if self.code {
            return base.code;
        }
        let mut style = Style::new();
        if self.link {
            style = base.link;
        }
        if self.bold {
            style = style.bold();
        }
        if self.italic {
            style = style.italic();
        }
        if self.strike {
            style = style.dim();
        }
        style
    }
}

/// A markdown renderer with a per-(text, width) render cache.
pub struct Markdown {
    style: MarkdownStyle,
    highlighter: Box<dyn SyntaxHighlighter>,
    cache: Option<(String, usize, Vec<String>)>,
}

impl Markdown {
    /// A renderer with the given style and highlighter.
    pub fn new(style: MarkdownStyle, highlighter: Box<dyn SyntaxHighlighter>) -> Self {
        Self {
            style,
            highlighter,
            cache: None,
        }
    }

    /// Render markdown `text` to ANSI lines at `width`.
    pub fn render(&mut self, text: &str, columns: usize) -> Vec<String> {
        if let Some((cached_text, cached_width, lines)) = &self.cache
            && cached_text == text
            && *cached_width == columns
        {
            return lines.clone();
        }
        let lines = self.render_uncached(text, columns);
        self.cache = Some((text.to_string(), columns, lines.clone()));
        lines
    }

    fn render_uncached(&self, text: &str, columns: usize) -> Vec<String> {
        let text = break_before_bold_lines(text);
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        let parser = Parser::new_ext(&text, options);
        let mut out: Vec<String> = Vec::new();
        let mut inline = String::new();
        let mut state = InlineState::default();
        let mut list_stack: Vec<Option<u64>> = Vec::new();
        let mut heading_level: Option<HeadingLevel> = None;
        let mut in_quote = false;
        let mut code_lang: Option<String> = None;
        // Table collection: cells accumulate the styled inline buffer,
        // rows accumulate on row end, the table renders on table end.
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut table_row: Vec<String> = Vec::new();

        macro_rules! flush_inline {
            () => {
                if !inline.is_empty() {
                    let prefix = if in_quote { "│ " } else { "" };
                    for line in width::wrap_line(&inline, columns.saturating_sub(prefix.width())) {
                        if in_quote {
                            out.push(format!("{prefix}{}", self.style.quote.paint(&line)));
                        } else {
                            out.push(line);
                        }
                    }
                    inline.clear();
                }
            };
        }

        for event in parser {
            match event {
                Event::Start(tag) => match tag {
                    Tag::Heading { level, .. } => {
                        flush_inline!();
                        heading_level = Some(level);
                    }
                    Tag::Paragraph => {}
                    Tag::Emphasis => state.italic = true,
                    Tag::Strong => state.bold = true,
                    Tag::Strikethrough => state.strike = true,
                    Tag::Link { dest_url, .. } => {
                        state.link = true;
                        state.link_url = Some(dest_url.to_string());
                    }
                    Tag::Image { dest_url, .. } => {
                        state.link = true;
                        state.link_url = Some(dest_url.to_string());
                    }
                    Tag::List(start) => list_stack.push(start),
                    Tag::Item => {
                        flush_inline!();
                        let depth = list_stack.len().saturating_sub(1);
                        let marker = match (list_stack.last_mut(), depth) {
                            (Some(Some(number)), _) => {
                                let marker = format!("{number}. ");
                                *number += 1;
                                marker
                            }
                            _ => "• ".to_string(),
                        };
                        inline.push_str(&"  ".repeat(depth));
                        inline.push_str(&marker);
                    }
                    Tag::BlockQuote(_) => {
                        flush_inline!();
                        in_quote = true;
                    }
                    Tag::CodeBlock(kind) => {
                        flush_inline!();
                        if let CodeBlockKind::Fenced(info) = kind {
                            code_lang = info.split(' ').next().map(|s| s.to_string());
                        }
                        out.push(self.style.fence.paint("```"));
                    }
                    Tag::Table(_) => {
                        flush_inline!();
                        table_rows = Vec::new();
                    }
                    Tag::TableHead | Tag::TableRow => table_row = Vec::new(),
                    Tag::TableCell => inline = String::new(),
                    _ => {}
                },
                Event::End(tag_end) => match tag_end {
                    TagEnd::Heading(_) => {
                        if !inline.is_empty() {
                            let level = heading_level;
                            let text = std::mem::take(&mut inline);
                            let style = self.style.heading.bold();
                            if let Some(HeadingLevel::H1) = level {
                                out.push(style.paint(&text));
                                let rule = self
                                    .style
                                    .rule
                                    .paint(&"─".repeat(columns.min(width::width(&text))));
                                out.push(rule);
                            } else {
                                out.push(style.paint(&text));
                            }
                            out.push(String::new());
                            heading_level = None;
                        }
                    }
                    TagEnd::Paragraph => {
                        flush_inline!();
                        out.push(String::new());
                    }
                    TagEnd::Emphasis => state.italic = false,
                    TagEnd::Strong => state.bold = false,
                    TagEnd::Strikethrough => state.strike = false,
                    TagEnd::Link | TagEnd::Image => {
                        state.link = false;
                        state.link_url = None;
                    }
                    TagEnd::List(_) => {
                        list_stack.pop();
                        flush_inline!();
                    }
                    TagEnd::Item => {
                        flush_inline!();
                    }
                    TagEnd::BlockQuote(_) => {
                        flush_inline!();
                        in_quote = false;
                    }
                    TagEnd::CodeBlock => {
                        let lang = code_lang.take();
                        let code = std::mem::take(&mut inline);
                        for line in self.highlighter.highlight(&code, lang.as_deref()) {
                            out.push(format!("  {}", self.style.fence.paint(&line)));
                        }
                        out.push(self.style.fence.paint("```"));
                        out.push(String::new());
                    }
                    TagEnd::Table => {
                        flush_inline!();
                        render_table(&table_rows, columns, &self.style, &mut out);
                    }
                    TagEnd::TableHead | TagEnd::TableRow => {
                        table_rows.push(std::mem::take(&mut table_row));
                    }
                    TagEnd::TableCell => table_row.push(std::mem::take(&mut inline)),
                    _ => {}
                },
                Event::Text(text_event) => {
                    let styled = state.compose(self.style).paint(text_event.as_ref());
                    if state.link {
                        // Hyperlink targets must be control-character free
                        // or the link degrades to plain styled text.
                        let safe_url = state
                            .link_url
                            .as_ref()
                            .is_some_and(|url| !url.chars().any(char::is_control));
                        if let (true, Some(url)) = (safe_url, &state.link_url) {
                            inline.push_str(&format!("\x1b]8;;{url}\x07{styled}\x1b]8;;\x07"));
                        } else {
                            inline.push_str(&styled);
                        }
                    } else {
                        inline.push_str(&styled);
                    }
                }
                Event::Code(code) => {
                    inline.push_str(&self.style.code.paint(&format!("`{code}`")));
                }
                Event::SoftBreak => inline.push(' '),
                Event::HardBreak => {
                    flush_inline!();
                }
                Event::Rule => {
                    flush_inline!();
                    out.push(self.style.rule.paint("─".repeat(columns.min(80)).as_str()));
                }
                Event::Html(html) | Event::InlineHtml(html) => inline.push_str(html.as_ref()),
                Event::InlineMath(math) | Event::DisplayMath(math) => {
                    inline.push_str(&self.style.code.paint(math.as_ref()));
                }
                Event::TaskListMarker(checked) => {
                    // Lands right after the item bullet, before the text.
                    inline.push_str(if checked { "[x] " } else { "[ ] " });
                }
                Event::FootnoteReference(_) => {}
            }
        }
        flush_inline!();
        // Trailing blank lines never render.
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        out
    }
}

/// Insert a paragraph break before bold-led lines: model output uses a
/// standalone `**Heading**` line as a pseudo-heading, but CommonMark
/// folds it into the previous bullet/paragraph as a lazy continuation
/// line, gluing unrelated content onto one row. Each such line becomes
/// its own block instead. Fenced code blocks pass through untouched,
/// and a line that already follows a blank line is left alone.
fn break_before_bold_lines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.split('\n').any(is_bold_led) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(line);
            continue;
        }
        if !in_fence
            && is_bold_led(line)
            && out.last().is_some_and(|prev| !prev.trim().is_empty())
        {
            out.push("");
        }
        out.push(line);
    }
    std::borrow::Cow::Owned(out.join("\n"))
}

/// True when the line opens with a bold delimiter run: up to three
/// leading spaces (the CommonMark paragraph indent) then `**`.
fn is_bold_led(line: &str) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    indent <= 3 && line[indent..].starts_with("**")
}

/// Render collected table rows as a box-drawing table. Column widths
/// come from the widest visible cell; when the table exceeds `columns`
/// the columns shrink evenly (cells truncate). Row 0 is the header.
fn render_table(
    rows: &[Vec<String>],
    columns: usize,
    style: &MarkdownStyle,
    out: &mut Vec<String>,
) {
    if rows.is_empty() || rows[0].is_empty() {
        return;
    }
    let col_count = rows[0].len();
    let border = style.fence;
    // Natural widths per column, then an even shrink to fit the line.
    let mut widths: Vec<usize> = vec![1; col_count];
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(col_count) {
            widths[index] = widths[index].max(width::width(cell));
        }
    }
    let natural_total: usize = widths.iter().sum();
    // Row width = two outer borders plus two spaces around every cell,
    // so the columns share `columns - 2 - 2*cols`.
    let budget = columns.saturating_sub(2 + 2 * col_count).max(col_count);
    if natural_total > budget {
        let each = (budget / col_count).max(3);
        for w in widths.iter_mut() {
            *w = (*w).min(each);
        }
    }
    let rule = |out: &mut Vec<String>, left: &str, mid: &str, right: &str| {
        let segments: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        out.push(border.paint(&format!(
            "{left}{}{right}",
            segments.join(&border.paint(mid).to_string())
        )));
    };
    let render_row = |out: &mut Vec<String>, row: &[String], header: bool| {
        let mut line = String::from("│");
        for (index, cell) in row.iter().enumerate().take(col_count) {
            // Shrunken tables truncate overflowing cells to the column.
            let cut = width::truncate_to_width(cell, widths[index]);
            let cut = if width::width(&cut) < width::width(cell) {
                let mut trimmed = width::truncate_to_width(cell, widths[index].saturating_sub(1));
                trimmed.push('…');
                trimmed
            } else {
                cut
            };
            let pad = widths[index].saturating_sub(width::width(&cut));
            let styled = if header {
                style.heading.bold().paint(&cut)
            } else {
                cut
            };
            line.push_str(&format!(" {styled}{} │", " ".repeat(pad)));
        }
        out.push(border.paint(&line));
    };
    rule(out, "┌", "┬", "┐");
    if let Some(head) = rows.first() {
        render_row(out, head, true);
    }
    rule(out, "├", "┼", "┤");
    for row in rows.iter().skip(1) {
        render_row(out, row, false);
    }
    rule(out, "└", "┴", "┘");
    out.push(String::new());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::strip_ansi;

    fn renderer() -> Markdown {
        Markdown::new(MarkdownStyle::default(), Box::new(PlainHighlighter))
    }

    #[test]
    fn paragraphs_render_as_lines() {
        let mut md = renderer();
        let lines = md.render("hello world", 40);
        assert_eq!(strip_ansi(&lines[0]), "hello world");
    }

    #[test]
    fn bold_and_code_styled() {
        let mut md = renderer();
        let lines = md.render("**hi** `code`", 40);
        assert!(lines[0].contains("\x1b[1mhi\x1b[0m"), "{:?}", lines[0]);
        assert!(lines[0].contains("`code`"), "{:?}", lines[0]);
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
    fn code_blocks_render_fenced() {
        let mut md = renderer();
        let lines = md.render("```rust\nfn a() {}\n```", 40);
        assert_eq!(strip_ansi(&lines[0]), "```");
        assert_eq!(strip_ansi(&lines[1]), "  fn a() {}");
        assert_eq!(strip_ansi(&lines[2]), "```");
    }

    #[test]
    fn unclosed_code_block_still_renders_streaming() {
        let mut md = renderer();
        let lines = md.render("```rust\nfn a() {}", 40);
        assert_eq!(strip_ansi(&lines[0]), "```");
        assert_eq!(strip_ansi(&lines[1]), "  fn a() {}");
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
        assert!(lines[0].contains("\x1b[1m执行环境\x1b[0m"), "{:?}", lines[0]);
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
            lines[2].contains("\x1b[2m"),
            "strikethrough dims: {:?}",
            lines[2]
        );
        assert!(lines[4].contains("`code`"), "{:?}", lines[4]);
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
            !plain.iter().any(|l| l.contains("诊断信息") && l.contains("执行环境")),
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
        assert!(plain.contains(&"迷宫场景: 用 Three.js 拼出迷宫".to_string()), "{plain:?}");
        assert!(plain.contains(&"起点: 蓝色发光方块".to_string()), "{plain:?}");
        assert!(
            !plain.iter().any(|l| l.contains("先看环境。") && l.contains("迷宫场景")),
            "blocks must not glue: {plain:?}"
        );
    }

    #[test]
    fn bold_lines_inside_a_code_fence_are_left_alone() {
        let mut md = renderer();
        let lines = md.render("```\n**not a heading**\n```", 40);
        assert_eq!(strip_ansi(&lines[1]), "  **not a heading**");
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
            !plain.iter().any(|l| l.contains("诊断信息") && l.contains("执行环境")),
            "blocks must not glue: {plain:?}"
        );
    }
}
