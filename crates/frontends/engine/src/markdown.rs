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
use crate::component::Segment;
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
    /// Compose the span style. `quote` layers the block-quote look
    /// (dim italic) onto every span individually: an outer paint over
    /// the whole line would be cut at the first span's own reset.
    fn compose(&self, base: MarkdownStyle, quote: bool) -> Style {
        if self.code {
            return if quote {
                base.code.dim().italic()
            } else {
                base.code
            };
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
        if quote {
            style = style.dim().italic();
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
    cache: Option<(String, usize, Segment)>,
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
    pub fn render(&mut self, text: &str, columns: usize) -> Segment {
        if let Some((cached_text, cached_width, lines)) = &self.cache
            && cached_text == text
            && *cached_width == columns
        {
            return Segment::clone(lines);
        }
        let lines = Segment::new(self.render_uncached(text, columns));
        self.cache = Some((text.to_string(), columns, Segment::clone(&lines)));
        lines
    }

    /// Drop the rendered cache (theme or render-mode switches).
    pub fn clear_cache(&mut self) {
        self.cache = None;
    }

    /// Swap the inline style in place (theme switches): rendered lines
    /// repaint under the new style on the next render without touching
    /// the highlighter or fence renderers.
    pub fn set_style(&mut self, style: MarkdownStyle) {
        self.style = style;
        self.cache = None;
    }

    /// Swap the syntax highlighter (theme switches rebuild it: the
    /// syntax theme bakes into highlighted code at construction, and a
    /// stale highlighter repaints old code blocks in the old palette).
    pub fn set_highlighter(&mut self, highlighter: Box<dyn SyntaxHighlighter>) {
        self.highlighter = highlighter;
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
                            // Each span already carries the quote look
                            // (compose layers it per span); only the
                            // bar prefix needs its own paint here — an
                            // outer paint would be cut at the first
                            // inner reset.
                            out.push(format!("{}{}", self.style.quote.paint(prefix), line));
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
                        // The marker rides the text style: unpainted text
                        // inherits the terminal's default foreground, which
                        // a recolored (light) background leaves unreadable.
                        inline.push_str(&self.style.text.paint(&marker));
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
                            // Headings wrap like any other block: an
                            // overlong heading left unwrapped would be
                            // hard-truncated by the screen layer and
                            // lose its tail.
                            let lines: Vec<String> = width::wrap_line(&text, columns);
                            let last = lines.len().saturating_sub(1);
                            for (index, line) in lines.into_iter().enumerate() {
                                out.push(style.paint(&line));
                                if let Some(HeadingLevel::H1) = level
                                    && index == last
                                {
                                    // The rule sits under the heading's
                                    // last line, as wide as that line.
                                    let rule = self
                                        .style
                                        .rule
                                        .paint(&"─".repeat(columns.min(width::width(&line))));
                                    out.push(rule);
                                }
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
                    let styled = state
                        .compose(self.style, in_quote)
                        .paint(text_event.as_ref());
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
                Event::Html(html) | Event::InlineHtml(html) => {
                    // Pass-through HTML renders as literal text; paint it
                    // so it never inherits the terminal default color.
                    inline.push_str(&self.style.text.paint(html.as_ref()));
                }
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
                    // Painted like every other marker: unpainted text
                    // inherits the terminal default foreground, which a
                    // recolored (light) background leaves unreadable.
                    inline.push_str(&self.style.text.paint(if checked { "[x] " } else { "[ ] " }));
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

/// Advance the fenced-code state over one line; returns true when the
/// line sits outside a fenced block (the closing fence line itself
/// counts as outside — every gated transform is a no-op on a bare
/// fence marker). `fence` carries the opening marker between lines.
/// The single shared skeleton behind the preprocessing passes
/// ([`break_before_bold_lines`], [`highlight_to_bold`],
/// [`script_spans_to_unicode`], [`html_fixups`]).
fn step_fence(fence: &mut Option<char>, line: &str) -> bool {
    let trimmed = line.trim_start();
    match *fence {
        Some(open) if trimmed.starts_with(open) => *fence = None,
        Some(_) => {}
        None if trimmed.starts_with("```") => *fence = Some('`'),
        None if trimmed.starts_with("~~~") => *fence = Some('~'),
        None => {}
    }
    fence.is_none()
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
    let mut fence: Option<char> = None;
    for line in text.split('\n') {
        // Track fenced code by marker character: inside a fence only a
        // run of the same marker closes it, so `~~~` blocks (and backtick
        // runs inside them) never let a bold-led code line gain a
        // phantom break.
        if step_fence(&mut fence, line)
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
        if step_fence(&mut fence, line) {
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
        if step_fence(&mut fence, line) {
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
        if step_fence(&mut fence, line) {
            let t = line.trim_start();
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
                // Header cells repaint as one block: inner spans (code,
                // emphasis) would cut the heading emphasis at their
                // first reset, so they flatten to plain text here.
                style.heading.bold().paint(&width::strip_ansi(&cut))
            } else {
                // Body cells keep their inner span styling; an outer
                // paint would die at the first inner reset.
                cut
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
mod tests;
