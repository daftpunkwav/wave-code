//! markdown → ANSI 终端渲染器（缓冲渲染契约的渲染内核）。
//!
//! 输入一条完整助手消息（已过 sanitize_terminal），输出带样式的终端文本。
//! 表格按 unicode-width（CJK=2 格）对齐、超终端宽度按比例压缩（T3 落地）。
//! 本模块只做纯函数变换；ANSI 的启用/剥离由输出侧 anstream 决定。

use anstyle::{AnsiColor, Color, Style};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

/// 主题样式（青蓝波形色系；集中一处便于调色）
mod theme {
    use super::*;

    pub fn heading() -> Style {
        Style::new()
            .fg_color(Some(Color::Ansi(AnsiColor::BrightCyan)))
            .bold()
    }
    pub fn inline_code() -> Style {
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)))
    }
    /// 代码块/引用的左边线
    pub fn bar() -> Style {
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlack)))
    }
    pub fn link() -> Style {
        Style::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Blue)))
            .underline()
    }
    /// 表格框线与水平线
    pub fn frame() -> Style {
        Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlack)))
    }
}

/// 行内样式状态（可叠加；进入表格单元格时随 span 记录）
#[derive(Default, Clone, Copy, PartialEq)]
struct Inline {
    strong: bool,
    emphasis: bool,
    strike: bool,
    code: bool,
    link: bool,
}

impl Inline {
    fn style(self) -> Style {
        // code/link 提供前景色；粗斜删为叠加效果
        let mut s = if self.code {
            theme::inline_code()
        } else if self.link {
            theme::link()
        } else {
            Style::new()
        };
        if self.strong {
            s = s.bold();
        }
        if self.emphasis {
            s = s.italic();
        }
        if self.strike {
            s = s.strikethrough();
        }
        s
    }
}

/// 渲染入口：保证输出恰好以单个 '\n' 结尾
pub fn render_markdown(input: &str, term_width: usize) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let mut r = Renderer::new(term_width.max(20));
    for ev in Parser::new_ext(input, opts) {
        r.event(&ev);
    }
    r.finish()
}

struct Renderer {
    out: String,
    width: usize,
    inline: Inline,
    in_heading: bool,
    in_code_block: bool,
    /// 代码块内是否处于行首（写 `│ ` 前缀用）
    code_line_start: bool,
    quote_depth: usize,
    /// 正文行首标记（处理引用前缀）
    at_line_start: bool,
    /// 列表栈：Some(下一编号) 为有序
    list_stack: Vec<Option<u64>>,
    /// 段落是否已在输出中产生了内容（段间空行控制）
    started: bool,
    /// 构建中的表格（Some 时文本分流进单元格，见 emit）
    table: Option<TableBuilder>,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            out: String::new(),
            width,
            inline: Inline::default(),
            in_heading: false,
            in_code_block: false,
            code_line_start: false,
            quote_depth: 0,
            at_line_start: true,
            list_stack: Vec::new(),
            started: false,
            table: None,
        }
    }

    fn event(&mut self, ev: &Event<'_>) {
        match ev {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(t) => self.text(t),
            Event::Code(t) => {
                // 行内 code：强制 code 样式
                let saved = self.inline;
                self.inline.code = true;
                self.emit(t);
                self.inline = saved;
            }
            Event::SoftBreak | Event::HardBreak => self.newline(),
            Event::Rule => {
                self.block_gap();
                let line: String = std::iter::repeat_n('─', self.width.min(40)).collect();
                self.push_styled(&line, theme::frame());
                self.out.push('\n');
                // 水平线产生了可见内容，后续块间距据此判断
                self.started = true;
            }
            _ => {} // html/footnote/task-list 标记等 M1 不渲染，忽略
        }
    }

    fn start_tag(&mut self, tag: &Tag<'_>) {
        match tag {
            // 已知缺陷：loose list（如 `- 甲\n\n- 乙\n`）中 Item 内段落触发 block_gap，
            // 会产生 `• \n\n乙` 式破损——修复前既有缺陷，留待后续独立设计修复，勿在此误改
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
                self.code_line_start = true;
            }
            Tag::BlockQuote(..) => {
                self.block_gap();
                self.quote_depth += 1;
                self.at_line_start = true;
            }
            Tag::List(start) => {
                // 顶层列表前留空行；嵌套列表紧随其后只换行（Item 文本尚未收尾）
                if self.list_stack.is_empty() {
                    self.block_gap();
                } else {
                    self.newline();
                }
                self.list_stack.push(*start);
            }
            Tag::Item => {
                // 文档以列表开头时 out 为空，避免产生多余前导换行
                if !self.out.is_empty() {
                    self.newline();
                }
                let depth = self.list_stack.len();
                let indent = "  ".repeat(depth.saturating_sub(1));
                match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        self.out.push_str(&format!("{indent}{n}. "));
                        *n += 1;
                    }
                    _ => self.out.push_str(&format!("{indent}• ")),
                }
                self.at_line_start = false;
            }
            Tag::Table(aligns) => {
                self.table = Some(TableBuilder::new(aligns.clone()));
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(tb) = self.table.as_mut() {
                    tb.begin_row();
                }
            }
            Tag::TableCell => {
                if let Some(tb) = self.table.as_mut() {
                    tb.begin_cell();
                }
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: &TagEnd) {
        match tag {
            TagEnd::Paragraph => self.newline(),
            TagEnd::Heading(_) => {
                self.in_heading = false;
                self.newline();
            }
            TagEnd::Strong => self.inline.strong = false,
            TagEnd::Emphasis => self.inline.emphasis = false,
            TagEnd::Strikethrough => self.inline.strike = false,
            TagEnd::Link => self.inline.link = false,
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.newline();
            }
            TagEnd::BlockQuote(..) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.newline();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.newline();
            }
            TagEnd::Table => self.flush_table(),
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(tb) = self.table.as_mut() {
                    tb.end_row();
                }
            }
            TagEnd::TableCell => {
                if let Some(tb) = self.table.as_mut() {
                    tb.end_cell(self.inline);
                }
            }
            TagEnd::Item => {}
            _ => {}
        }
    }

    /// 文本事件：代码块内加边线、正文加行首前缀
    fn text(&mut self, t: &str) {
        if self.in_code_block {
            for (i, seg) in t.split('\n').enumerate() {
                if i > 0 {
                    self.out.push('\n');
                    self.code_line_start = true;
                }
                // 空行有意不补边线前缀：保持空白行零字符，避免尾随空格
                if self.code_line_start && !seg.is_empty() {
                    self.push_styled("│ ", theme::bar());
                    self.code_line_start = false;
                    // 代码块产生了可见内容，后续块间距据此判断
                    self.started = true;
                }
                self.out.push_str(seg);
            }
            return;
        }
        self.emit(t);
    }

    /// 普通文本出口
    fn emit(&mut self, t: &str) {
        // 表格构建中：文本进当前单元格（行内样式随 span 记录），不走主输出
        if let Some(tb) = self.table.as_mut() {
            tb.push_text(t, self.inline);
            return;
        }
        if self.at_line_start && self.quote_depth > 0 {
            for _ in 0..self.quote_depth {
                self.push_styled("│ ", theme::bar());
            }
        }
        self.at_line_start = false;
        let style = if self.in_heading {
            // 标题样式为基底，叠加斜体/删除线
            // 刻意取舍：标题内行内码/链接不单独着色，统一用标题样式，避免色彩堆叠
            let mut s = theme::heading();
            if self.inline.emphasis {
                s = s.italic();
            }
            if self.inline.strike {
                s = s.strikethrough();
            }
            s
        } else {
            self.inline.style()
        };
        self.push_styled(t, style);
        self.started = true;
    }

    fn push_styled(&mut self, text: &str, style: Style) {
        self.out.push_str(&styled(text, style));
    }

    /// 段间空行：仅当已有内容且末尾不是 \n\n
    fn block_gap(&mut self) {
        if !self.started {
            return;
        }
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        if !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
        self.at_line_start = true;
    }

    fn newline(&mut self) {
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        self.at_line_start = true;
    }

    /// 表格出表：布局后写入主输出（含块间距与 started 维护）
    fn flush_table(&mut self) {
        if let Some(tb) = self.table.take() {
            let lines = tb.layout(self.width);
            if lines.is_empty() {
                return;
            }
            self.block_gap();
            for line in lines {
                self.out.push_str(&line);
                self.out.push('\n');
            }
            // 表格产生了可见内容，后续块间距据此判断
            self.started = true;
        }
    }

    fn finish(mut self) -> String {
        // 模型输出被截断（无 TagEnd::Table）时未闭合表格兜底出表
        self.flush_table();
        while self.out.ends_with("\n\n") {
            self.out.pop();
        }
        if !self.out.ends_with('\n') && !self.out.is_empty() {
            self.out.push('\n');
        }
        self.out
    }
}

// ---------- 表格 ----------

/// 表格单元格最小列宽（压缩保底）
const MIN_COL_WIDTH: usize = 4;

/// 单元格：带行内样式的文本 span 序列（样式见 Inline）
struct Cell {
    spans: Vec<(Inline, String)>,
}

impl Cell {
    fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|(_, t)| UnicodeWidthStr::width(t.as_str()))
            .sum()
    }
}

struct TableBuilder {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Cell>>, // rows[0] 为表头（pulldown 先给 TableHead）
    cur_row: Vec<Cell>,
    cur_spans: Vec<(Inline, String)>,
    cur_text: String,
    cur_inline: Inline,
}

impl TableBuilder {
    fn new(aligns: Vec<Alignment>) -> Self {
        Self {
            aligns,
            rows: Vec::new(),
            cur_row: Vec::new(),
            cur_spans: Vec::new(),
            cur_text: String::new(),
            cur_inline: Inline::default(),
        }
    }

    fn begin_row(&mut self) {
        self.cur_row = Vec::new();
    }

    fn begin_cell(&mut self) {
        self.cur_spans = Vec::new();
        self.cur_text = String::new();
    }

    fn push_text(&mut self, t: &str, inline: Inline) {
        // 样式变化时 flush 前一个 span；单元格内换行折叠为空格
        let t = t.replace('\n', " ");
        if self.cur_text.is_empty() {
            self.cur_inline = inline;
            self.cur_text = t;
        } else if self.cur_inline == inline {
            self.cur_text.push_str(&t);
        } else {
            self.cur_spans
                .push((self.cur_inline, std::mem::take(&mut self.cur_text)));
            self.cur_inline = inline;
            self.cur_text = t;
        }
    }

    fn end_cell(&mut self, _last_inline: Inline) {
        if !self.cur_text.is_empty() {
            self.cur_spans
                .push((self.cur_inline, std::mem::take(&mut self.cur_text)));
        }
        self.cur_row.push(Cell {
            spans: std::mem::take(&mut self.cur_spans),
        });
    }

    fn end_row(&mut self) {
        if !self.cur_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.cur_row));
        }
    }

    /// 布局：按终端宽度对齐/压缩，输出带框线的行（含表头下分隔线）
    fn layout(&self, term_width: usize) -> Vec<String> {
        let cols = self
            .aligns
            .len()
            .max(self.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if cols == 0 {
            return Vec::new();
        }
        // 列宽 = 各单元格最大显示宽（缺单元格按空处理）
        let mut colw = vec![0usize; cols];
        for row in &self.rows {
            for (i, c) in row.iter().enumerate() {
                colw[i] = colw[i].max(c.width());
            }
        }
        // 格式 "│ c │ c │"：每列 3 格开销（"│ " + 尾部空格），结尾 "│" 1 格
        let budget = term_width.max(20);
        let overhead = 3 * cols + 1;
        if colw.iter().sum::<usize>() + overhead > budget {
            let avail = budget.saturating_sub(overhead);
            let sum = colw.iter().sum::<usize>().max(1);
            // 极端情形（超窄终端+多列+宽度悬殊）floor 总和可能超 avail：接受不回收，输出等宽但略超宽
            let floor = (avail / cols).clamp(1, MIN_COL_WIDTH);
            for w in &mut colw {
                *w = (*w * avail / sum).max(floor);
            }
        }
        let mut lines = Vec::new();
        for (ri, row) in self.rows.iter().enumerate() {
            lines.push(self.render_row(row, &colw));
            if ri == 0 {
                lines.push(self.separator(&colw));
            }
        }
        lines
    }

    fn align_of(&self, col: usize) -> Alignment {
        self.aligns.get(col).copied().unwrap_or(Alignment::None)
    }

    fn render_row(&self, row: &[Cell], colw: &[usize]) -> String {
        let mut s = String::new();
        for (i, w) in colw.iter().enumerate() {
            s.push_str(&styled("│ ", theme::frame()));
            let empty = Cell { spans: Vec::new() };
            let cell = row.get(i).unwrap_or(&empty);
            self.emit_cell(&mut s, cell, *w, self.align_of(i));
            s.push(' ');
        }
        s.push_str(&styled("│", theme::frame()));
        s
    }

    fn separator(&self, colw: &[usize]) -> String {
        let mut s = String::from("├");
        for (i, w) in colw.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push(if i + 1 == colw.len() { '┤' } else { '┼' });
        }
        styled(&s, theme::frame())
    }

    /// 单元格：按对齐填充到目标宽度；超宽截断补 `…`（span 感知，不切多字节）
    fn emit_cell(&self, out: &mut String, cell: &Cell, target: usize, align: Alignment) {
        let w = cell.width();
        let (pad_l, pad_r) = match align {
            Alignment::Right if w < target => (target - w, 0),
            Alignment::Center if w < target => ((target - w) / 2, target - w - (target - w) / 2),
            _ => (0, target.saturating_sub(w)),
        };
        out.push_str(&" ".repeat(pad_l));
        if w <= target {
            for (st, t) in &cell.spans {
                out.push_str(&styled(t, st.style()));
            }
        } else {
            // 截断：预算 target-1 给正文，末尾补 …
            let mut rest = target.saturating_sub(1);
            for (st, t) in &cell.spans {
                if rest == 0 {
                    break;
                }
                let mut take = String::new();
                let mut used = 0;
                for ch in t.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if used + cw > rest {
                        break;
                    }
                    used += cw;
                    take.push(ch);
                }
                out.push_str(&styled(&take, st.style()));
                rest -= used;
            }
            // 补足填充：宽字符放不进剩余预算时 rest 有剩余，截断行同样填满 target
            let taken = target - 1 - rest;
            out.push('…');
            out.push_str(&" ".repeat(target - taken - 1));
        }
        out.push_str(&" ".repeat(pad_r));
    }
}

/// 带样式包裹（空样式/空文本直出）
fn styled(text: &str, style: Style) -> String {
    if style == Style::new() || text.is_empty() {
        text.to_string()
    } else {
        format!("{}{}{}", style.render(), text, style.render_reset())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 去掉 ANSI 序列，便于断言可见文本（测试辅助）
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // 跳过 CSI：ESC [ ... 终字节 @-~
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('@'..='~').contains(&c2) {
                            break;
                        }
                    }
                    continue;
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn heading_is_bold_cyan_without_hash() {
        let out = render_markdown("# 标题\n", 80);
        assert!(out.contains("\x1b["), "应含 ANSI：{out:?}");
        assert!(strip(&out).contains("标题"));
        assert!(!strip(&out).contains('#'));
    }

    #[test]
    fn bold_and_strike_render_as_effects() {
        let out = render_markdown("**加粗** ~~删除~~\n", 80);
        let plain = strip(&out);
        assert!(plain.contains("加粗") && plain.contains("删除"));
        assert!(!plain.contains("**") && !plain.contains("~~"));
        assert!(out.matches("\x1b[").count() >= 4, "应有样式开闭：{out:?}");
    }

    #[test]
    fn inline_code_is_yellow() {
        let out = render_markdown("使用 `cargo build` 构建\n", 80);
        assert!(strip(&out).contains("cargo build"));
        assert!(!strip(&out).contains('`'));
        assert!(out.contains("\x1b["));
    }

    #[test]
    fn code_block_lines_have_bar_prefix() {
        let out = render_markdown("```\nlet a = 1;\nlet b = 2;\n```\n", 80);
        assert_eq!(strip(&out), "│ let a = 1;\n│ let b = 2;\n");
    }

    /// 代码块为文档首个块时，与后续段落之间仍保留空行（started 置位回归）
    #[test]
    fn code_block_then_paragraph_keeps_blank_line() {
        let out = render_markdown("```\ncode\n```\n\n解释\n", 80);
        assert_eq!(strip(&out), "│ code\n\n解释\n");
    }

    #[test]
    fn unordered_and_ordered_lists() {
        let out = render_markdown("- 甲\n- 乙\n\n1. 一\n2. 二\n", 80);
        assert_eq!(strip(&out), "• 甲\n• 乙\n\n1. 一\n2. 二\n");
    }

    /// 嵌套列表：内层结束后外层缩进/层级须恢复（list_stack 弹栈回归）
    #[test]
    fn nested_list_pops_stack() {
        let out = render_markdown("- 外一\n  - 内一\n  - 内二\n- 外二\n", 80);
        assert_eq!(strip(&out), "• 外一\n  • 内一\n  • 内二\n• 外二\n");
    }

    #[test]
    fn blockquote_has_bar() {
        let out = render_markdown("> 引用内容\n", 80);
        assert!(strip(&out).contains("│ 引用内容"), "实际：{}", strip(&out));
    }

    #[test]
    fn link_is_underlined_text_only() {
        let out = render_markdown("[文档](https://example.com)\n", 80);
        let plain = strip(&out);
        assert!(plain.contains("文档"));
        assert!(!plain.contains("https://"), "URL 不附加：{plain:?}");
    }

    #[test]
    fn rule_is_dim_line() {
        let out = render_markdown("上\n\n---\n\n下\n", 80);
        assert!(strip(&out).contains("────"), "实际：{}", strip(&out));
    }

    #[test]
    fn paragraph_spacing_preserved() {
        let out = render_markdown("第一段\n\n第二段\n", 80);
        assert!(
            strip(&out).contains("第一段\n\n第二段"),
            "实际：{}",
            strip(&out)
        );
    }

    #[test]
    fn table_ascii_aligned_columns() {
        let md = "| 名称 | 类型 |\n|------|------|\n| foo | fn |\n| longer | let |\n";
        let plain = strip(&render_markdown(md, 80));
        let lines: Vec<&str> = plain.lines().filter(|l| l.contains('│')).collect();
        // 每行可见宽度一致（列对齐）；表头含 CJK，须按显示宽度计（chars 会把中文算 1）
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| unicode_width::UnicodeWidthStr::width(*l))
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "列未对齐：{plain}");
        assert!(plain.contains("名称") && plain.contains("longer"));
    }

    #[test]
    fn table_cjk_width_counts_two_cells() {
        // 中文 2 格：两列含中文时仍按显示宽度对齐
        let md = "| 语言 | 速度 |\n|------|------|\n| 中文 | 快 |\n| rs | 快 |\n";
        let plain = strip(&render_markdown(md, 80));
        let lines: Vec<&str> = plain.lines().filter(|l| l.contains('│')).collect();
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| unicode_width::UnicodeWidthStr::width(*l))
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "CJK 列未对齐：{plain}"
        );
    }

    #[test]
    fn table_compresses_over_terminal_width() {
        let long = "很长的单元格内容".repeat(20);
        let md = format!("| A | B |\n|---|---|\n| {long} | {long} |\n");
        let plain = strip(&render_markdown(&md, 40));
        for line in plain.lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= 40,
                "超宽（40）：{line}"
            );
        }
        assert!(plain.contains('…'), "压缩应补省略号：{plain}");
    }

    /// 压缩截断后各行仍等宽（CJK 宽字符预算补足回归：rest 剩余转为 `…` 后填充）
    #[test]
    fn compressed_table_rows_stay_equal_width() {
        let md = "| 标题 |\n|------|\n| **很长的中文内容会被截断** |\n| 短 |\n";
        let plain = strip(&render_markdown(md, 24));
        let widths: Vec<usize> = plain
            .lines()
            .filter(|l| l.contains('│'))
            .map(unicode_width::UnicodeWidthStr::width)
            .collect();
        assert!(widths.len() >= 2, "应有多个表格行：{plain}");
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "截断后行宽不齐：{plain}"
        );
        assert!(plain.contains('…'), "应确实发生截断：{plain}");
    }

    #[test]
    fn table_alignment_right_and_center() {
        let md = "| L | R |\n|:---|---:|\n| a | 1 |\n| bbb | 22 |\n";
        let plain = strip(&render_markdown(md, 80));
        // 右对齐列：'1' 右侧被填充到列宽（'22' 之后紧跟框线，'1' 后有空格填充）
        let row1 = plain
            .lines()
            .find(|l| l.contains("| a") || l.contains("│ a"))
            .unwrap();
        assert!(
            row1.contains(" 1 │") || row1.contains(" 1 |"),
            "右对齐失效：{row1}"
        );
        // 居中对齐列：短内容两侧被填充（pad 均分，余量给右侧）
        let md_c = "| C |\n|:---:|\n| ab |\n| cdef |\n";
        let plain_c = strip(&render_markdown(md_c, 80));
        let row_c = plain_c.lines().find(|l| l.contains("ab")).unwrap();
        assert!(row_c.contains("│  ab  │"), "居中对齐失效：{row_c}");
    }

    #[test]
    fn unclosed_table_still_renders() {
        // 模型输出被截断（无闭合事件）也能出表
        let md = "| A |\n|---|\n| x |";
        let plain = strip(&render_markdown(md, 80));
        assert!(plain.contains('│'));
    }
}
