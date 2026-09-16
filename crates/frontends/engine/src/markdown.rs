//! Markdown-to-ANSI rendering for assistant messages.
//!
//! A streaming-tolerant subset of the reference renderer: headings,
//! emphasis, inline code, links (OSC 8 hyperlinks), fenced code blocks
//! with pluggable syntax highlighting, lists with nesting, block
//! quotes, tables (box-drawing), and horizontal rules. Output is one
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
        let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
        let parser = Parser::new_ext(text, options);
        let mut out: Vec<String> = Vec::new();
        let mut inline = String::new();
        let mut state = InlineState::default();
        let mut list_stack: Vec<Option<u64>> = Vec::new();
        let mut heading_level: Option<HeadingLevel> = None;
        let mut in_quote = false;
        let mut code_lang: Option<String> = None;

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
                    Tag::Table(_) => {}
                    Tag::TableHead => {}
                    Tag::TableRow => {}
                    Tag::TableCell => {}
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
                    TagEnd::Table => {}
                    TagEnd::TableHead => {}
                    TagEnd::TableRow => out.push(String::new()),
                    TagEnd::TableCell => inline.push_str("  "),
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
                Event::FootnoteReference(_) | Event::TaskListMarker(_) => {}
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
    fn tables_render_as_rows() {
        let mut md = renderer();
        let lines = md.render("| a | b |\n|---|---|\n| 1 | 2 |", 40);
        let joined: String = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("a"), "{joined}");
        assert!(joined.contains("2"), "{joined}");
    }
}
