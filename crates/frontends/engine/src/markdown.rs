//! Markdown-to-ANSI rendering for assistant messages.
//!
//! A streaming-tolerant subset of the reference renderer: headings,
//! emphasis, inline code, links (OSC 8 hyperlinks), fenced code blocks
//! with pluggable fence renderers (mermaid diagrams) and syntax
//! highlighting, framed rendering, lists with nesting, block quotes,
//! tables (box-drawing), and horizontal rules.
//! One deliberate deviation: a standalone `**bold**` line always starts
//! a new block (models emit these as pseudo-headings; CommonMark would
//! fold them into the previous paragraph as a lazy continuation).
//! Output is one ANSI string per terminal line; wrapping happens per
//! block at the requested width.
//!
//! Vertical rhythm: every block ensures exactly one blank line between
//! itself and the previous block (none at the top of the document), so
//! blocks never cram together and two blank lines never appear.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

use crate::color::Style;
use crate::sanitize::sanitize_terminal;
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

/// Fenced-block seam: renders one language-tagged fence body before the
/// syntax highlighter runs. The engine ships the mermaid diagram
/// renderer through this seam ([`crate::mermaid::MermaidFences`]);
/// applications register more with [`Markdown::with_fence`] instead of
/// this module growing a special case per language.
pub trait FenceRenderer {
    /// Render the body of the fence tagged `lang` within `columns`
    /// display columns (the frame's inner budget); `None` falls through
    /// to the next renderer — diff handling, then the highlighter.
    fn render_fence(&self, lang: &str, code: &str, columns: usize) -> Option<Vec<String>>;
}

/// Visual style knobs for markdown rendering.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownStyle {
    /// Plain body prose (paragraphs, list items, table body cells):
    /// near-white in the console themes — never an accent hue.
    pub text: Style,
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
    /// Diff fence additions (`+` lines).
    pub diff_added: Style,
    /// Diff fence removals (`-` lines).
    pub diff_removed: Style,
    /// Diff metadata (file headers, `@@` hunks).
    pub diff_meta: Style,
}

impl Default for MarkdownStyle {
    fn default() -> Self {
        Self {
            text: Style::new(),
            heading: Style::new(),
            code: Style::new(),
            link: Style::new().underline(),
            fence: Style::new().dim(),
            quote: Style::new().dim().italic(),
            rule: Style::new().dim(),
            diff_added: Style::new(),
            diff_removed: Style::new(),
            diff_meta: Style::new().dim(),
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

/// Ensure one blank line of separation before a block: pushes a blank
/// only when the output has content and does not already end blank, so
/// blocks never cram together and two blank lines never appear.
fn ensure_blank(out: &mut Vec<String>) {
    if out.last().is_some_and(|line| !line.is_empty()) {
        out.push(String::new());
    }
}

/// The render width of code block frame rules: full width on narrow
/// terminals, capped so wide terminals get modest frames.
fn frame_width(columns: usize) -> usize {
    columns.clamp(6, 80)
}

impl InlineState {
    fn compose(&self, base: MarkdownStyle) -> Style {
        if self.code {
            return base.code;
        }
        // Plain prose rides the text style (near-white in the console
        // themes); emphasis only adds flags on top.
        let mut style = if self.link { base.link } else { base.text };
        if self.bold {
            style = style.bold();
        }
        if self.italic {
            style = style.italic();
        }
        if self.strike {
            style = style.strikethrough();
        }
        style
    }
}

impl MarkdownStyle {
    /// Wrap this style plus a highlighter into a renderer.
    pub fn into_markdown(self, highlighter: Box<dyn SyntaxHighlighter>) -> Markdown {
        Markdown::new(self, highlighter)
    }
}

/// A markdown renderer with a per-(text, width) render cache.
pub struct Markdown {
    style: MarkdownStyle,
    highlighter: Box<dyn SyntaxHighlighter>,
    /// Fence renderers consulted before the highlighter; the default
    /// set carries the engine-builtin mermaid diagrams.
    fences: Vec<Box<dyn FenceRenderer>>,
    cache: Option<(String, usize, std::sync::Arc<Vec<String>>)>,
}

impl Markdown {
    /// A renderer with the given style and highlighter. The default
    /// fence-renderer set (mermaid diagrams) rides along; applications
    /// extend it with [`Markdown::with_fence`].
    pub fn new(style: MarkdownStyle, highlighter: Box<dyn SyntaxHighlighter>) -> Self {
        Self {
            style,
            highlighter,
            fences: vec![Box::new(crate::mermaid::MermaidFences)],
            cache: None,
        }
    }

    /// Register one more fenced-block renderer, kept for this
    /// renderer's lifetime.
    pub fn with_fence(mut self, fence: Box<dyn FenceRenderer>) -> Self {
        self.fences.push(fence);
        self
    }

    /// The dim rule opening a code block, with a language tag when one
    /// is known: `╭─ python ─────╮`. Raw fence info never reaches the
    /// output — the tag keeps only a safe character whitelist.
    fn frame_top(&self, lang: Option<&str>, total: usize) -> String {
        let tag = lang.and_then(lang_tag);
        let head = match tag {
            Some(tag) => format!("╭─ {tag} "),
            None => "╭─".to_string(),
        };
        let fill = total.saturating_sub(width::width(&head) + 1);
        self.style
            .fence
            .paint(&format!("{head}{}╮", "─".repeat(fill)))
    }

    /// The dim rule closing a code block: `╰──────╯`.
    fn frame_bottom(&self, total: usize) -> String {
        self.style
            .fence
            .paint(&format!("╰{}╯", "─".repeat(total.saturating_sub(2))))
    }

    /// One framed code row: `│ {content padded to the frame} │`. The
    /// content line is already ANSI-painted (highlighter or plain) and
    /// must not be re-wrapped — overlong lines truncate at the frame.
    fn frame_row(&self, line: &str, total: usize) -> String {
        let inner = total.saturating_sub(4);
        let cut = width::truncate_to_width(line, inner);
        format!(
            "{}{}{}",
            self.style.fence.paint("│ "),
            width::pad_to_width(&cut, inner),
            self.style.fence.paint(" │")
        )
    }

    /// Render markdown `text` to ANSI lines at `width`. Cache hits
    /// share the rendered array by reference (one refcount bump).
    pub fn render(&mut self, text: &str, columns: usize) -> std::sync::Arc<Vec<String>> {
        if let Some((cached_text, cached_width, lines)) = &self.cache
            && cached_text == text
            && *cached_width == columns
        {
            return std::sync::Arc::clone(lines);
        }
        let lines = std::sync::Arc::new(self.render_uncached(text, columns));
        self.cache = Some((text.to_string(), columns, std::sync::Arc::clone(&lines)));
        lines
    }

    /// Drop the rendered cache (theme or render-mode switches).
    pub fn clear_cache(&mut self) {
        self.cache = None;
    }

    /// Paint a ```diff fence: whole lines ride the diff colors —
    /// additions green, removals red, file headers and `@@` hunks in
    /// the metadata tone, context plain. Generic syntax highlighting
    /// cannot express a diff.
    fn diff_lines(&self, code: &str) -> Vec<String> {
        code.lines()
            .map(|line| {
                let line = line.strip_suffix('\r').unwrap_or(line);
                // File headers and `@@` hunks share the metadata tone;
                // +/- lines carry the added/removed colors; context is
                // plain prose.
                let style =
                    if line.starts_with("@@") || line.starts_with("+++") || line.starts_with("---")
                    {
                        self.style.diff_meta
                    } else if line.starts_with('+') {
                        self.style.diff_added
                    } else if line.starts_with('-') {
                        self.style.diff_removed
                    } else {
                        self.style.text
                    };
                style.paint(line)
            })
            .collect()
    }

    fn render_uncached(&self, text: &str, columns: usize) -> Vec<String> {
        let highlighted = highlight_to_bold(text);
        let scripted = script_spans_to_unicode(&highlighted);
        let fixed_html = html_fixups(&scripted);
        let text = break_before_bold_lines(&fixed_html);
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_MATH
            | Options::ENABLE_FOOTNOTES;
        let parser = Parser::new_ext(&text, options);
        let mut out: Vec<String> = Vec::new();
        let mut inline = String::new();
        let mut state = InlineState::default();
        let mut list_stack: Vec<Option<u64>> = Vec::new();
        let mut heading_level: Option<HeadingLevel> = None;
        let mut in_quote = false;
        let mut in_code = false;
        let mut code_lang: Option<String> = None;
        // Table collection: cells accumulate the styled inline buffer,
        // rows accumulate on row end, the table renders on table end.
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut table_row: Vec<String> = Vec::new();
        let mut table_aligns: Vec<pulldown_cmark::Alignment> = Vec::new();

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
                    Tag::Paragraph => ensure_blank(&mut out),
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
                    Tag::List(start) => {
                        let top_level = list_stack.is_empty();
                        list_stack.push(start);
                        // Separation belongs around whole lists; items
                        // inside stay tight.
                        if top_level {
                            ensure_blank(&mut out);
                        }
                    }
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
                        ensure_blank(&mut out);
                        in_quote = true;
                    }
                    Tag::CodeBlock(kind) => {
                        flush_inline!();
                        ensure_blank(&mut out);
                        if let CodeBlockKind::Fenced(info) = kind {
                            code_lang = info.split(' ').next().map(|s| s.to_string());
                        }
                        in_code = true;
                    }
                    Tag::FootnoteDefinition(label) => {
                        ensure_blank(&mut out);
                        inline.push_str(&self.style.code.paint(&format!("[{label}]: ")));
                    }
                    Tag::Table(alignments) => {
                        flush_inline!();
                        ensure_blank(&mut out);
                        table_rows = Vec::new();
                        table_aligns = alignments;
                    }
                    Tag::TableHead | Tag::TableRow => table_row = Vec::new(),
                    Tag::TableCell => inline = String::new(),
                    _ => {}
                },
                Event::End(tag_end) => match tag_end {
                    TagEnd::Heading(_) => {
                        // Take the level unconditionally: an empty heading
                        // (`##` alone) emits no text, and a level left
                        // parked here would push every later span onto
                        // the raw unstyled path.
                        let level = heading_level.take();
                        if !inline.is_empty() {
                            ensure_blank(&mut out);
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
                        }
                    }
                    TagEnd::Paragraph => {
                        flush_inline!();
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
                        in_code = false;
                        let lang = code_lang.take();
                        // Code content rides raw through the inline
                        // buffer (no style paint) and is sanitized once
                        // here: model-sourced escapes must never reach
                        // the highlighter or the frame.
                        let taken = std::mem::take(&mut inline);
                        let code = sanitize_terminal(&taken).into_owned();
                        // Fence renderers get first pass at the body
                        // (the mermaid seam); then diff fences; the
                        // rest rides the syntax highlighter.
                        let body_budget = frame_width(columns).saturating_sub(4);
                        let diagram = lang.as_deref().and_then(|lang| {
                            self.fences
                                .iter()
                                .find_map(|fence| fence.render_fence(lang, &code, body_budget))
                        });
                        // Diff fences get dedicated +- and hunk coloring
                        // instead of generic syntax highlighting.
                        let is_diff = matches!(lang.as_deref(), Some("diff") | Some("patch"));
                        let body = match diagram {
                            Some(body) => body,
                            None if is_diff => self.diff_lines(&code),
                            None => self.highlighter.highlight(&code, lang.as_deref()),
                        };
                        // The frame hugs its content (plus the tag) and
                        // only stretches to the width cap on wide rows —
                        // a full-width frame starves the right side.
                        let content = body
                            .iter()
                            .map(|line| width::width(line))
                            .max()
                            .unwrap_or(0);
                        let tag_width = lang
                            .as_deref()
                            .and_then(lang_tag)
                            .map(|tag| width::width(&tag) + 6)
                            .unwrap_or(0);
                        let total = content
                            .max(tag_width)
                            .saturating_add(4)
                            .min(frame_width(columns));
                        out.push(self.frame_top(lang.as_deref(), total));
                        for line in &body {
                            out.push(self.frame_row(line, total));
                        }
                        out.push(self.frame_bottom(total));
                    }
                    TagEnd::FootnoteDefinition => {
                        flush_inline!();
                    }
                    TagEnd::Table => {
                        flush_inline!();
                        render_table(&table_rows, &table_aligns, columns, &self.style, &mut out);
                    }
                    TagEnd::TableHead | TagEnd::TableRow => {
                        table_rows.push(std::mem::take(&mut table_row));
                    }
                    TagEnd::TableCell => table_row.push(std::mem::take(&mut inline)),
                    _ => {}
                },
                Event::Text(text_event) => {
                    // Code block and heading content stays raw: code is
                    // painted by the highlighter, and heading text is
                    // painted once at the heading end — a pre-painted
                    // buffer would double-paint and its inner reset
                    // would cancel the outer style.
                    if in_code || heading_level.is_some() {
                        inline.push_str(text_event.as_ref());
                        continue;
                    }
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
                    // Inside a heading the span rides raw with the rest
                    // of the heading text: painting here would drop an
                    // inner reset mid-heading that cuts the heading
                    // style short (the heading paints once at its end).
                    if heading_level.is_some() {
                        inline.push_str(code.as_ref());
                        continue;
                    }
                    // Color alone marks the span; visible quotes would leak
                    // into the transcript as content the model never wrote.
                    inline.push_str(&self.style.code.paint(code.as_ref()));
                }
                Event::SoftBreak => inline.push(' '),
                Event::HardBreak => {
                    flush_inline!();
                }
                Event::Rule => {
                    flush_inline!();
                    ensure_blank(&mut out);
                    out.push(self.style.rule.paint("─".repeat(columns.min(80)).as_str()));
                }
                Event::Html(html) | Event::InlineHtml(html) => inline.push_str(html.as_ref()),
                Event::InlineMath(math) => {
                    let converted = crate::math::render(math.as_ref());
                    inline.push_str(&self.style.text.paint(&converted));
                }
                Event::DisplayMath(math) => {
                    // Block math becomes its own block: matrix
                    // environments lay out over multiple aligned lines.
                    flush_inline!();
                    ensure_blank(&mut out);
                    for line in crate::math::render_block(math.as_ref(), columns) {
                        out.push(self.style.text.paint(&line));
                    }
                    out.push(String::new());
                }
                Event::TaskListMarker(checked) => {
                    // Lands right after the item bullet, before the text.
                    inline.push_str(if checked { "[x] " } else { "[ ] " });
                }
                Event::FootnoteReference(name) => {
                    // Footnote markers render as bracketed references;
                    // the definition renders at its own site below.
                    inline.push_str(&self.style.code.paint(&format!("[{name}]")));
                }
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
    // Track fenced code by marker character: inside a fence only a run
    // of the same marker closes it, so `~~~` blocks (and backtick runs
    // inside them) never let a bold-led code line gain a phantom break.
    let mut fence: Option<char> = None;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        match fence {
            Some(open) if trimmed.starts_with(open) => fence = None,
            Some(_) => {}
            None if trimmed.starts_with("```") => fence = Some('`'),
            None if trimmed.starts_with("~~~") => fence = Some('~'),
            None => {}
        }
        if fence.is_none()
            && is_bold_led(line)
            && out.last().is_some_and(|prev| !prev.trim().is_empty())
        {
            out.push("");
        }
        out.push(line);
    }
    std::borrow::Cow::Owned(out.join("\n"))
}

/// Fold `==highlight==` spans onto bold: the highlight marker is not
/// CommonMark, and an unparsed `==` reads as leaked markup. Fenced
/// code passes through untouched, and `===` setext underlines (no
/// paired close on the line) are left alone.
fn highlight_to_bold(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("==") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut fence: Option<char> = None;
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len().saturating_sub(1);
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        match fence {
            Some(open) if trimmed.starts_with(open) => fence = None,
            Some(_) => {}
            None if trimmed.starts_with("```") => fence = Some('`'),
            None if trimmed.starts_with("~~~") => fence = Some('~'),
            None => {}
        }
        if fence.is_none() {
            out.push_str(&highlight_line(line));
        } else {
            out.push_str(line);
        }
        if index < last {
            out.push('\n');
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Rewrite the outermost `==...==` pair on one line to `**...**`.
fn highlight_line(line: &str) -> std::borrow::Cow<'_, str> {
    let Some(open) = find_marker(line, 0) else {
        return std::borrow::Cow::Borrowed(line);
    };
    let Some(close) = find_marker(line, open + 2) else {
        return std::borrow::Cow::Borrowed(line);
    };
    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..open]);
    out.push_str("**");
    out.push_str(&line[open + 2..close]);
    out.push_str("**");
    out.push_str(&line[close + 2..]);
    std::borrow::Cow::Owned(out)
}

/// The next `==` run at or after `from`.
fn find_marker(line: &str, from: usize) -> Option<usize> {
    line[from..].find("==").map(|position| from + position)
}

/// Fold `^sup^` / `~sub~` spans onto unicode script code points
/// when every glyph in the span has one (`x^2^` becomes `x²`,
/// `H~2~O` becomes `H₂O`); spans with unmappable characters stay
/// verbatim. CommonMark has no superscript/subscript syntax, and
/// pulldown-cmark only parses them at word boundaries — the prepass
/// covers the intraword forms models actually emit. Fenced code passes
/// through untouched, and `~~strike~~` / `===` rules are never
/// touched (a script span opens with exactly one marker).
fn script_spans_to_unicode(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('^') && !text.contains('~') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut fence: Option<char> = None;
    let mut out = String::with_capacity(text.len());
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len().saturating_sub(1);
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        match fence {
            Some(open) if trimmed.starts_with(open) => fence = None,
            Some(_) => {}
            None if trimmed.starts_with("```") => fence = Some('`'),
            None if trimmed.starts_with("~~~") => fence = Some('~'),
            None => {}
        }
        if fence.is_none() {
            out.push_str(&script_line(line));
        } else {
            out.push_str(line);
        }
        if index < last {
            out.push('\n');
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Rewrite script spans on one line. A span opens with `^` or `~`
/// (not doubled), holds 1-16 whitespace-free characters without its own
/// marker, and closes with the same marker; every character must map
/// onto a script code point or the span stands.
fn script_line(line: &str) -> String {
    const MAX_SPAN: usize = 16;
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    while i < chars.len() {
        let marker = chars[i];
        let sub = marker == '~';
        let doubled = i + 1 < chars.len() && chars[i + 1] == marker;
        if (marker != '^' && marker != '~') || doubled {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut close = None;
        for (offset, c) in chars
            .iter()
            .enumerate()
            .take(chars.len().min(i + 1 + MAX_SPAN))
            .skip(i + 1)
        {
            if *c == marker {
                close = Some(offset);
                break;
            }
            if c.is_whitespace() || *c == '^' || *c == '~' {
                break;
            }
        }
        let Some(close) = close else {
            out.push(chars[i]);
            i += 1;
            continue;
        };
        let inner: String = chars[i + 1..close].iter().collect();
        let mapped = if sub {
            crate::math::to_subscript(&inner)
        } else {
            crate::math::to_superscript(&inner)
        };
        match mapped {
            Some(scripted) => {
                out.push_str(&scripted);
                i = close + 1;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Fold the HTML constructs models emit onto markdown the terminal
/// can show: `<details>`/`</details>` lines vanish (a terminal cannot
/// collapse), `<summary>X</summary>` becomes a bold `▸ X` marker row,
/// and `<kbd>C</kbd>` becomes inline code (`C`) — a key cap. Fenced
/// code passes through untouched. Unknown tags keep flowing through
/// the existing HTML passthrough.
fn html_fixups(text: &str) -> std::borrow::Cow<'_, str> {
    if !(text.contains("<details") || text.contains("<summary") || text.contains("<kbd")) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut fence: Option<char> = None;
    let mut out: Vec<String> = Vec::with_capacity(text.lines().count());
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        match fence {
            Some(open) if trimmed.starts_with(open) => fence = None,
            Some(_) => {}
            None if trimmed.starts_with("```") => fence = Some('`'),
            None if trimmed.starts_with("~~~") => fence = Some('~'),
            None => {}
        }
        if fence.is_none() {
            let t = trimmed;
            if t == "<details>" || t == "</details>" {
                continue; // drop the container rows entirely
            }
            if t.starts_with("<summary>") && t.ends_with("</summary>") {
                let inner = &t["<summary>".len()..t.len() - "</summary>".len()];
                out.push(format!("**\u{25b8} {inner}**"));
                continue;
            }
            if t.contains("<kbd>") {
                out.push(kbd_line(line));
                continue;
            }
        }
        out.push(line.to_string());
    }
    std::borrow::Cow::Owned(out.join("\n"))
}

/// Replace every `<kbd>key</kbd>` span on the line with `key` inline
/// code.
fn kbd_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("<kbd>") {
        out.push_str(&rest[..start]);
        rest = &rest[start + "<kbd>".len()..];
        match rest.find("</kbd>") {
            Some(end) => {
                let key = &rest[..end];
                out.push_str(&format!("`{key}`"));
                rest = &rest[end + "</kbd>".len()..];
            }
            None => {
                out.push_str("<kbd>");
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// True when the line opens with a bold delimiter run: up to three
/// leading spaces (the CommonMark paragraph indent) then `**`.
fn is_bold_led(line: &str) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    indent <= 3 && line[indent..].starts_with("**")
}

/// The display tag for a fenced block info string: the first word with
/// every character outside the language-name whitelist dropped. Fence
/// info is model-sourced text, so control characters and escapes must
/// never reach the frame. `None` when nothing readable remains.
fn lang_tag(info: &str) -> Option<String> {
    let word = info.split_whitespace().next()?;
    let clean: String = word
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '#' | '_'))
        .collect();
    if clean.is_empty() { None } else { Some(clean) }
}

/// Render collected table rows as a box-drawing table. Column widths
/// come from the widest visible cell; when the table exceeds `columns`
/// the columns shrink evenly (cells truncate). Row 0 is the header.
///
/// Every row renders on one shared column grid: ragged rows (fewer or
/// more cells than the widest row) pad and clip to the grid instead of
/// pulling the borders out of alignment, and control whitespace inside
/// a cell flattens to spaces so a row can never shatter across
/// multiple terminal lines.
fn render_table(
    rows: &[Vec<String>],
    aligns: &[pulldown_cmark::Alignment],
    columns: usize,
    style: &MarkdownStyle,
    out: &mut Vec<String>,
) {
    if rows.is_empty() || rows[0].is_empty() {
        return;
    }
    // The grid spans the widest row, so no cell is ever dropped; short
    // rows gain empty cells.
    let col_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            (0..col_count)
                .map(|index| flat_cell(row.get(index).map(String::as_str).unwrap_or_default()))
                .collect()
        })
        .collect();
    let border = style.fence;
    // Natural widths per column, then an even shrink to fit the line.
    let mut widths: Vec<usize> = vec![1; col_count];
    for row in &rows {
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
    // Rules and rows paint every segment separately: a styled span
    // carries its own reset, so wrapping one paint inside another would
    // cancel the outer style mid-line and leave the border half-painted.
    let rule = |out: &mut Vec<String>, left: &str, mid: &str, right: &str| {
        let segments: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        out.push(border.paint(&format!("{left}{}{right}", segments.join(mid))));
    };
    let render_row = |out: &mut Vec<String>, row: &[String], header: bool| {
        let mut line = border.paint("│");
        for (index, w) in widths.iter().enumerate() {
            let cell = row.get(index).map(String::as_str).unwrap_or_default();
            // Shrunken tables truncate overflowing cells to the column.
            let cut = width::truncate_to_width(cell, *w);
            let cut = if width::width(&cut) < width::width(cell) {
                let mut trimmed = width::truncate_to_width(cell, w.saturating_sub(1));
                trimmed.push('…');
                trimmed
            } else {
                cut
            };
            let pad = w.saturating_sub(width::width(&cut));
            // Markdown column alignment (`:---:`, `---:`) controls where
            // the slack lands; the default hugs the left.
            let (left, right) = match aligns.get(index) {
                Some(pulldown_cmark::Alignment::Center) => (pad / 2, pad - pad / 2),
                Some(pulldown_cmark::Alignment::Right) => (pad, 0),
                _ => (0, pad),
            };
            let styled = if header {
                style.heading.bold().paint(&cut)
            } else {
                style.text.paint(&cut)
            };
            line.push_str(&format!(
                " {}{}{} ",
                " ".repeat(left),
                styled,
                " ".repeat(right)
            ));
            line.push_str(&border.paint("│"));
        }
        out.push(line);
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
}

/// Flatten a table cell onto one line: newlines, carriage returns, and
/// tabs become spaces. A raw control character inside a cell would
/// embed a line break mid-row and shatter the box-drawing grid.
fn flat_cell(cell: &str) -> String {
    if cell.chars().any(|c| matches!(c, '\n' | '\r' | '\t')) {
        cell.split(['\n', '\r', '\t']).collect::<Vec<_>>().join(" ")
    } else {
        cell.to_string()
    }
}

#[cfg(test)]
mod tests {
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

    /// Table rules and rows paint each span separately: every SGR open
    /// is matched by exactly one reset, so a border can never lose its
    /// style mid-line to a nested paint.
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
}
