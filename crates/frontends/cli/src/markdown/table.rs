//! 表格子渲染器（阶段 4 拆分自 markdown.rs）：Cell / TableBuilder 与
//! CJK 对齐 / 超宽压缩逻辑。

use super::*;

pub(super) struct Cell {
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

pub(super) struct TableBuilder {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Cell>>, // rows[0] 为表头（pulldown 先给 TableHead）
    cur_row: Vec<Cell>,
    cur_spans: Vec<(Inline, String)>,
    cur_text: String,
    cur_inline: Inline,
}

impl TableBuilder {
    pub(super) fn new(aligns: Vec<Alignment>) -> Self {
        Self {
            aligns,
            rows: Vec::new(),
            cur_row: Vec::new(),
            cur_spans: Vec::new(),
            cur_text: String::new(),
            cur_inline: Inline::default(),
        }
    }

    pub(super) fn begin_row(&mut self) {
        self.cur_row = Vec::new();
    }

    pub(super) fn begin_cell(&mut self) {
        self.cur_spans = Vec::new();
        self.cur_text = String::new();
    }

    pub(super) fn push_text(&mut self, t: &str, inline: Inline) {
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

    pub(super) fn end_cell(&mut self, _last_inline: Inline) {
        if !self.cur_text.is_empty() {
            self.cur_spans
                .push((self.cur_inline, std::mem::take(&mut self.cur_text)));
        }
        self.cur_row.push(Cell {
            spans: std::mem::take(&mut self.cur_spans),
        });
    }

    pub(super) fn end_row(&mut self) {
        if !self.cur_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.cur_row));
        }
    }

    /// 布局：按终端宽度对齐/压缩，输出带框线的行（含表头下分隔线）
    pub(super) fn layout(&self, term_width: usize) -> Vec<String> {
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
