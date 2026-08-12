//! markdown → ratatui [`Line`] 渲染（消息流中助手消息的渲染内核）。
//!
//! 语义对齐 SPEC §15.5 与 cli/src/markdown.rs（那边产出 ANSI 字符串，
//! 这里产出带样式的 ratatui 行；tui 不能依赖 cli，为同源不同形的重写）：
//! - 标题：亮青加粗；
//! - 粗体 / 斜体 / 删除线：对应 modifier 叠加；
//! - 行内码：黄；
//! - 代码块 / 引用：`│ ` 左边线（暗灰）；
//! - 链接：蓝色下划线（只显文本，不显 URL）；
//! - 水平线：暗灰 `─`。
//!
//! 已知取舍：表格首版按纯文本退化（单元格以 ` │ ` 分隔），cli 侧的
//! CJK 对齐 + 超宽压缩留待后续；输入应已过 `sanitize_terminal`（app 侧
//! 在 delta 入缓冲时净化）。

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 主题色（与 cli 渲染同一色系：亮青强调 / 黄行内码 / 暗灰弱化）。
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

/// 行内样式状态（可叠加）。
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

/// 渲染入口：一条完整助手消息 → 消息流条目行集（末尾无多余空行）。
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
    /// 代码块内当前行是否已写 `│ ` 前缀。
    code_line_started: bool,
    quote_depth: usize,
    /// 列表栈：Some(下一编号) 为有序。
    list_stack: Vec<Option<u64>>,
    in_table: bool,
}

impl Renderer {
    fn event(&mut self, ev: &Event<'_>) {
        match ev {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(t) => self.text(t),
            // 行内码：黄色，叠加行内 modifier。
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
            // HTML / 脚注 / 图片首版不渲染（图片 alt 已由 pulldown 以 Text 给出）。
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
            // 表格退化形态：单元格以 ` │ ` 分隔。
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
            // 代码块文本按行切分，每行挂 `│ ` 左边线（可跨 Text 事件续行）。
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

    /// 行首前缀：引用深度的 `│ ` 边线（表格单元格内不挂）。
    fn ensure_prefix(&mut self) {
        if self.spans.is_empty() && !self.in_table {
            for _ in 0..self.quote_depth {
                self.spans.push(Span::styled("│ ", bar_style()));
            }
        }
    }

    /// 结束当前行（空 spans 不产行，避免段间出现空行叠加）。
    fn flush(&mut self) {
        if !self.spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }

    /// 块间空行：已有内容且当前行非空时补一个空行。
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

    /// 标题 / 粗体 / 行内码 / 代码块边线 / 列表 / 引用的文本形态锁定。
    #[test]
    fn renders_heading_code_list_quote() {
        let md = "# 标题\n\n正文 **粗体** `code`\n\n```rust\nfn main() {}\n```\n\n- 甲\n- 乙\n\n> 引用\n";
        let lines = render_markdown(md);
        let text = plain(&lines);
        assert!(text.contains("标题"));
        assert!(text.contains("正文 粗体 code"));
        assert!(text.contains("│ fn main() {}"), "代码块边线: {text:?}");
        assert!(text.contains("- 甲"));
        assert!(text.contains("│ 引用"), "引用边线: {text:?}");
        // 样式断言：标题亮青加粗；行内码黄；粗体叠加 BOLD。
        let heading = &lines[0];
        assert_eq!(heading.spans[0].style.fg, Some(Color::LightCyan));
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
        let body = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("粗体")))
            .unwrap();
        let bold = body.spans.iter().find(|s| s.content == "粗体").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = body.spans.iter().find(|s| s.content == "code").unwrap();
        assert_eq!(code.style.fg, Some(Color::Yellow));
    }

    /// 有序列表编号自增；段落之间恰好一个空行；末尾无空行。
    #[test]
    fn ordered_list_and_blank_lines() {
        let lines = render_markdown("甲\n\n乙\n\n1. 一\n2. 二\n");
        let text = plain(&lines);
        assert!(text.contains("1. 一"));
        assert!(text.contains("2. 二"));
        assert!(!text.ends_with('\n'));
        assert!(!text.contains("\n\n\n"), "段间至多一个空行: {text:?}");
    }

    /// 链接只显文本不显 URL；删除线 modifier 生效。
    #[test]
    fn link_and_strikethrough() {
        let lines = render_markdown("[站点](https://example.com) ~~旧~~");
        let text = plain(&lines);
        assert!(text.contains("站点"));
        assert!(!text.contains("https://"));
        let strike = lines[0].spans.iter().find(|s| s.content == "旧").unwrap();
        assert!(strike.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }
}
