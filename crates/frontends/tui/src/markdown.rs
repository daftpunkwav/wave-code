//! markdown to ratatui [`Line`] rendering (rendering core for assistant messages in the message stream).
//!
//! Semantics align with SPEC section 15.5 and cli/src/markdown.rs (which emits ANSI strings,
//! while this emits styled ratatui rows; the tui cannot depend on cli, so this is a same-source rewrite):
//! - Headings: bright cyan, bold;
//! - Bold / italic / strikethrough: stacked modifiers;
//! - Inline code: yellow;
//! - Code blocks / quotes: `│ ` left border (dark gray);
//! - Links: blue underline (text only, no URL);
//! - Rules: dark gray `─`.
//!
//! Known tradeoff: tables degrade to plain text in v1 (cells joined with ` │ `);
//! CJK alignment + over-wide squashing are follow-ups; input must pass `sanitize_terminal` (
//! sanitized on the app side when deltas enter the buffer).

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Theme colors (same family as CLI rendering: bright cyan accent / yellow inline code / dimmed gray).
fn heading_style() -> Style {
    Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD)
}

fn inline_code_style() -> Style {
    Style::default().fg(Color::Yellow)
}

fn bar_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn link_style() -> Style {
    Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::UNDERLINED)
}

/// Inline style state (stackable).
#[derive(Default, Clone, Copy)]
struct Inline {
    strong: bool,
    emphasis: bool,
    strike: bool,
    link: bool,
}

impl Inline {
    fn style(self, in_heading: bool) -> Style {
        let mut s = if in_heading {
            heading_style()
        } else if self.link {
            link_style()
        } else {
            Style::default()
        };
        if self.strong {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.emphasis {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s
    }
}

/// Render entry: one complete assistant message into message-stream rows (no trailing blank line).
pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let mut r = Renderer::default();
    for ev in Parser::new_ext(input, opts) {
        r.event(&ev);
    }
    r.finish()
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    inline: Inline,
    in_heading: bool,
    in_code_block: bool,
    /// Whether the current line in a code block already has the `│ ` prefix.
    code_line_started: bool,
    quote_depth: usize,
    /// List stack: Some(next number) means ordered.
    list_stack: Vec<Option<u64>>,
    in_table: bool,
}

impl Renderer {
    fn event(&mut self, ev: &Event<'_>) {
        match ev {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(t) => self.text(t),
            // Inline code: yellow, stacked with inline modifiers.
            Event::Code(t) => {
                self.ensure_prefix();
                let mut s = inline_code_style();
                if self.inline.strong {
                    s = s.add_modifier(Modifier::BOLD);
                }
                self.spans.push(Span::styled(t.to_string(), s));
            }
            Event::SoftBreak | Event::HardBreak => self.flush(),
            Event::Rule => {
                self.block_gap();
                self.lines
                    .push(Line::from(Span::styled("─".repeat(24), bar_style())));
            }
            Event::TaskListMarker(checked) => {
                self.ensure_prefix();
                self.spans
                    .push(Span::raw(if *checked { "☑ " } else { "☐ " }));
            }
            // HTML / footnotes / images do not render in v1 (image alt already arrives as Text via pulldown).
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block_gap(),
            Tag::Heading { .. } => {
                self.block_gap();
                self.in_heading = true;
            }
            Tag::Strong => self.inline.strong = true,
            Tag::Emphasis => self.inline.emphasis = true,
            Tag::Strikethrough => self.inline.strike = true,
            Tag::Link { .. } => self.inline.link = true,
            Tag::CodeBlock(_) => {
                self.block_gap();
                self.in_code_block = true;
                self.code_line_started = false;
            }
            Tag::BlockQuote(..) => {
                self.block_gap();
                self.quote_depth += 1;
            }
            Tag::List(start) => {
                self.block_gap();
                self.list_stack.push(*start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.list_stack.len();
                let indent = "  ".repeat(depth.saturating_sub(1));
                let bullet = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let b = format!("{indent}{n}. ");
                        *n += 1;
                        b
                    }
                    _ => format!("{indent}- "),
                };
                self.ensure_prefix();
                self.spans.push(Span::raw(bullet));
            }
            Tag::Table(_) => {
                self.block_gap();
                self.in_table = true;
            }
            Tag::TableHead | Tag::TableRow => self.flush(),
            // Degraded table form: cells joined with ` │ `.
            Tag::TableCell if !self.spans.is_empty() => {
                self.spans.push(Span::styled(" │ ", bar_style()));
            }
            Tag::TableCell => {}
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: &TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.in_heading = false;
            }
            TagEnd::Strong => self.inline.strong = false,
            TagEnd::Emphasis => self.inline.emphasis = false,
            TagEnd::Strikethrough => self.inline.strike = false,
            TagEnd::Link => self.inline.link = false,
            TagEnd::CodeBlock => {
                self.flush();
                self.in_code_block = false;
            }
            TagEnd::BlockQuote(..) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.flush();
                self.list_stack.pop();
            }
            TagEnd::Item => self.flush(),
            TagEnd::Table => {
                self.flush();
                self.in_table = false;
            }
            TagEnd::TableHead | TagEnd::TableRow => self.flush(),
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        if self.in_code_block {
            // Split code block text per line, each with a `│ ` left border (may span Text events).
            let mut parts = t.split('\n');
            let mut first = true;
            for part in &mut parts {
                if !first {
                    self.flush();
                    self.code_line_started = false;
                }
                first = false;
                if !self.code_line_started {
                    self.spans.push(Span::styled("│ ", bar_style()));
                    self.code_line_started = true;
                }
                self.spans.push(Span::raw(part.to_string()));
            }
            return;
        }
        self.ensure_prefix();
        let style = self.inline.style(self.in_heading);
        self.spans.push(Span::styled(t.to_string(), style));
    }

    /// Line-start prefix: `│ ` borders per quote depth (not inside table cells).
    fn ensure_prefix(&mut self) {
        if self.spans.is_empty() && !self.in_table {
            for _ in 0..self.quote_depth {
                self.spans.push(Span::styled("│ ", bar_style()));
            }
        }
    }

    /// End the current line (empty spans emit nothing, avoiding stacked blank lines).
    fn flush(&mut self) {
        if !self.spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }

    /// Blank line between blocks: add one when content exists and the current line is non-empty.
    fn block_gap(&mut self) {
        self.flush();
        if self.lines.last().is_some_and(|l| !l.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        while self.lines.last().is_some_and(|l| l.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Locks the text shape of headings / bold / inline code / code borders / lists / quotes.
    #[test]
    fn renders_heading_code_list_quote() {
        let md = "# Title\n\nBody **bold** `code`\n\n```rust\nfn main() {}\n```\n\n- A\n- B\n\n> quote\n";
        let lines = render_markdown(md);
        let text = plain(&lines);
        assert!(text.contains("Title"));
        assert!(text.contains("Body bold code"));
        assert!(text.contains("│ fn main() {}"), "code block border: {text:?}");
        assert!(text.contains("- A"));
        assert!(text.contains("│ quote"), "quote border: {text:?}");
        // Style assertions: bright cyan bold headings; yellow inline code; bold stacked.
        let heading = &lines[0];
        assert_eq!(heading.spans[0].style.fg, Some(Color::LightCyan));
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
        let body = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("bold")))
            .unwrap();
        let bold = body.spans.iter().find(|s| s.content == "bold").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = body.spans.iter().find(|s| s.content == "code").unwrap();
        assert_eq!(code.style.fg, Some(Color::Yellow));
    }

    /// Ordered lists auto-increment; exactly one blank line between paragraphs; no trailing blank line.
    #[test]
    fn ordered_list_and_blank_lines() {
        let lines = render_markdown("A\n\nB\n\n1. one\n2. two\n");
        let text = plain(&lines);
        assert!(text.contains("1. one"));
        assert!(text.contains("2. two"));
        assert!(!text.ends_with('\n'));
        assert!(!text.contains("\n\n\n"), "at most one blank line between blocks: {text:?}");
    }

    /// Links show text without URL; strikethrough modifier applies.
    #[test]
    fn link_and_strikethrough() {
        let lines = render_markdown("[Site](https://example.com) ~~old~~");
        let text = plain(&lines);
        assert!(text.contains("Site"));
        assert!(!text.contains("https://"));
        let strike = lines[0].spans.iter().find(|s| s.content == "old").unwrap();
        assert!(strike.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }
}
