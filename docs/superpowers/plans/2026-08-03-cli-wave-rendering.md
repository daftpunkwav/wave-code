# CLI 界面增强（波形品牌 + markdown 渲染）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 REPL/exec 的 human 输出升级为带波形品牌（滚动彩色动画横幅）与完整 markdown 渲染（标题/加粗/代码/对齐表格）的终端界面。

**Architecture:** `pulldown-cmark` 解析为事件流，自写 ANSI 渲染器（`markdown.rs` 纯函数）；`wave.rs` 生成正弦波形帧（RGB 渐变）；`render.rs` 状态机改为消息级缓冲渲染（delta 进缓冲区、complete 触发渲染、工具行实时着色）；`main.rs` 以 `select!` tick 驱动等待动画并打印横幅。颜色/降级由 `anstream` 统一处理（非 TTY 剥离、Windows VT、`NO_COLOR`）。

**Tech Stack:** pulldown-cmark 0.13、anstream 1.0、anstyle 1.0、unicode-width 0.2、terminal_size 0.4（后四者除 terminal_size 外已在 Cargo.lock 传递引入）。

**设计文档：** `docs/superpowers/specs/2026-08-03-cli-wave-rendering-design.md`（用户已批准：缓冲渲染 / 音频波形块横幅 / 波形滚动+彩色动画）。

**通用门禁（每个任务都要全绿才提交）：**

```bash
cargo test -p wavecode-cli
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

**约定：** 代码注释中文；commit 消息中文，格式 `界面增强 TN：…`；`--json` 输出路径（`render_jsonl` 与 main.rs JSONL 分支）**任何任务都不得触碰**；模型文本必须先过现有 `sanitize_terminal` 再进渲染管线。

---

## 共享接口契约（所有任务以此为准，禁止偏离）

### `crates/cli/src/markdown.rs`（新建）

```rust
/// 把一条完整助手消息渲染为带 ANSI 样式的终端文本。
/// `term_width` 为终端列宽（用于表格压缩）；输出末尾保证恰好一个换行。
pub fn render_markdown(input: &str, term_width: usize) -> String;
```

### `crates/cli/src/wave.rs`（新建）

```rust
/// 生成第 `phase` 帧、宽 `n` 格的波形（每格带 RGB 渐变 ANSI，末尾含 reset）。
/// 帧内块字符取自 '▁▂▃▄▅▆▇█'；相位递推形成流动效果。纯函数、确定性。
pub fn frame(n: usize, phase: f32) -> String;

/// REPL 启动横幅（定格帧）：波形 + `WaveCode v{version}` 加粗亮青 +
/// `{model} · {cwd}` 暗灰 + 提示行暗灰。`phase` 为定格相位（接启动动画末帧）。
pub fn banner(model: &str, cwd: &std::path::Path, version: &str, phase: f32) -> String;
```

### `crates/cli/src/render.rs`（改造，签名变化）

```rust
impl<W: std::io::Write> HumanRenderer<W> {
    /// animate：是否启用等待动画（human 模式 && stdout 为 TTY 时 true）
    pub fn new(out: W, animate: bool) -> Self;
    /// 处理一个事件（缓冲/渲染/工具行）。签名不变
    pub fn handle(&mut self, ev: &wavecode_protocol::Event) -> std::io::Result<()>;
    /// 等待动画 tick：处于 turn 中且无其他输出时重绘一帧波形（非 animate 时 no-op）
    pub fn tick_frame(&mut self) -> std::io::Result<()>;
    /// 是否处于"等待模型输出"（turn 内、自上次输出后无新内容打印）
    pub fn is_waiting_on_model(&self) -> bool;
}
```

`render_jsonl` / `sanitize_terminal` / `truncate_chars` 保持现有签名与语义不变；`human_tool_begin`/`human_tool_error` 增加颜色（返回串含 ANSI，测试断言方式同步改为显示宽度）。

### `crates/cli/src/main.rs`（改造）

- REPL：TTY 时先播放 12 帧启动动画（80ms/帧），定格后打印 `banner(model, cwd, version, phase)`；提示符改为亮青 `∿ `（rustyline 宽度若错位则退回无色 `∿ `，见 T6 Step 4）。
- 事件消费循环（exec 与 REPL 共用的 `consume_turn`）：human 模式且 TTY 时 `select!` 加 80ms tick 分支调 `renderer.tick_frame()?`；`--json` 与非 TTY 时结构不变。
- exec 模式：无横幅、动画同 REPL（TTY 即启用）；human 渲染输出流改用 `anstream::stdout()`（`--json` 时人类渲染走 `anstream::stderr()`）。

---

## Task 1: 依赖接线

**Files:**
- Modify: `Cargo.toml`（workspace.dependencies）
- Modify: `crates/cli/Cargo.toml`

- [ ] **Step 1: 修改 workspace 根 `Cargo.toml`**，在 `[workspace.dependencies]` 追加（按现有字母序与风格）：

```toml
anstream = "1.0"
anstyle = "1.0"
pulldown-cmark = "0.13"
terminal_size = "0.4"
unicode-width = "0.2"
```

- [ ] **Step 2: 修改 `crates/cli/Cargo.toml`**，`[dependencies]` 追加：

```toml
anstream.workspace = true
anstyle.workspace = true
pulldown-cmark.workspace = true
terminal_size.workspace = true
unicode-width.workspace = true
```

- [ ] **Step 3: 验证构建**

Run: `cargo build -p wavecode-cli 2>&1 | tail -3`
Expected: `Finished`（Cargo.lock 自动更新拉入 pulldown-cmark 与 terminal_size；若 pulldown-cmark 无 0.13 可用版本，取 crates.io 当前 0.12/0.13 最新并在 commit 中注明实际版本；T2 的 Tag 匹配以编译器报错为准微调）

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/cli/Cargo.toml Cargo.lock
git commit -m "界面增强 T1：markdown/ANSI/宽度渲染依赖接线（pulldown-cmark/anstream/anstyle/unicode-width/terminal_size）"
```

---

## Task 2: markdown.rs —— 核心渲染（不含表格）

**Files:**
- Create: `crates/cli/src/markdown.rs`
- Modify: `crates/cli/src/main.rs`（`mod markdown;` 声明）
- Test: 同文件 `#[cfg(test)]`

- [ ] **Step 1: 写失败测试**（先建空 `markdown.rs` 含 `pub fn render_markdown` 桩 `todo!()`，main.rs 加 `mod markdown;`）

```rust
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
        let plain = strip(&out);
        assert!(plain.contains("│ let a = 1;"), "实际：{plain:?}");
        assert!(plain.contains("│ let b = 2;"), "实际：{plain:?}");
    }

    #[test]
    fn unordered_and_ordered_lists() {
        let out = render_markdown("- 甲\n- 乙\n\n1. 一\n2. 二\n", 80);
        let plain = strip(&out);
        assert!(plain.contains("• 甲"), "实际：{plain:?}");
        assert!(plain.contains("• 乙"), "实际：{plain:?}");
        assert!(plain.contains("1. 一"), "实际：{plain:?}");
        assert!(plain.contains("2. 二"), "实际：{plain:?}");
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
        assert!(strip(&out).contains("第一段\n\n第二段"), "实际：{}", strip(&out));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p wavecode-cli markdown`
Expected: 编译错误或 `todo!()` panic（失败）

- [ ] **Step 3: 实现 `crates/cli/src/markdown.rs`**（完整代码如下；pulldown-cmark 0.13 的 Tag 为结构变体，若实际版本字段名不同以编译器为准微调 match 臂，行为不变）

```rust
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
    pub fn bold() -> Style {
        Style::new().bold()
    }
    pub fn italic() -> Style {
        Style::new().italic()
    }
    pub fn strike() -> Style {
        Style::new().strikethrough()
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
    let mut opts = Options::new();
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
            }
            _ => {} // html/footnote/task-list 标记等 M1 不渲染，忽略
        }
    }

    fn start_tag(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.table.is_none() {
                    self.block_gap();
                }
            }
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
                self.block_gap();
                self.list_stack.push(*start);
            }
            Tag::Item => {
                self.newline();
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
                self.block_gap();
                self.table = Some(TableBuilder::new(aligns.clone()));
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.begin_row();
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.begin_cell();
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
            TagEnd::List(_) => self.newline(),
            TagEnd::Item => {}
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    let lines = t.layout(self.width);
                    for line in lines {
                        self.out.push_str(&line);
                        self.out.push('\n');
                    }
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    t.end_row();
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    t.end_cell(self.inline);
                }
            }
            _ => {}
        }
    }

    /// 文本事件：表格内进单元格、代码块内加边线、正文加行首前缀
    fn text(&mut self, t: &str) {
        if self.in_code_block {
            for (i, seg) in t.split('\n').enumerate() {
                if i > 0 {
                    self.out.push('\n');
                    self.code_line_start = true;
                }
                if self.code_line_start && !seg.is_empty() {
                    self.push_styled("│ ", theme::bar());
                    self.code_line_start = false;
                }
                self.out.push_str(seg);
            }
            return;
        }
        self.emit(t);
    }

    /// 普通/表格内统一出口
    fn emit(&mut self, t: &str) {
        if let Some(tb) = &mut self.table {
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
        if style == Style::new() || text.is_empty() {
            self.out.push_str(text);
        } else {
            self.out.push_str(&format!("{}{}{}", style.render(), text, style.render_reset()));
        }
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

    fn finish(mut self) -> String {
        // 表格未闭合兜底（模型输出截断）：按已收集内容布局
        if let Some(t) = self.table.take() {
            for line in t.layout(self.width) {
                self.out.push_str(&line);
                self.out.push('\n');
            }
        }
        while self.out.ends_with("\n\n") {
            self.out.pop();
        }
        if !self.out.ends_with('\n') && !self.out.is_empty() {
            self.out.push('\n');
        }
        self.out
    }
}
```

（`TableBuilder` 在 T3 定义；本任务先给最小实现让测试编译——`layout` 暂按"纯文本逐行原样输出"占位**是计划失败**，因此 T2 的 TableBuilder 直接放 T3 的完整版代码：本任务文件先不含 `Tag::Table*` 四个分支与 `table` 字段，T3 再插入。即：T2 交付的 match 中删去上述 `Tag::Table*`/`TagEnd::Table*` 臂、`table` 字段及 `finish` 的表格兜底、`emit` 中的表格分支。）

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p wavecode-cli markdown`
Expected: 9 passed

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/markdown.rs crates/cli/src/main.rs
git commit -m "界面增强 T2：markdown 核心渲染（标题/粗斜删/行内码/代码块/列表/引用/链接/水平线）"
```

---

## Task 3: markdown.rs —— 表格渲染

**Files:**
- Modify: `crates/cli/src/markdown.rs`

- [ ] **Step 1: 写失败测试**（追加到 markdown.rs 测试模块）

```rust
    #[test]
    fn table_ascii_aligned_columns() {
        let md = "| 名称 | 类型 |\n|------|------|\n| foo | fn |\n| longer | let |\n";
        let plain = strip(&render_markdown(md, 80));
        let lines: Vec<&str> = plain.lines().filter(|l| l.contains('│')).collect();
        // 每行可见宽度一致（列对齐）
        let widths: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();
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
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "CJK 列未对齐：{plain}");
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

    #[test]
    fn table_alignment_right_and_center() {
        let md = "| L | R |\n|:---|---:|\n| a | 1 |\n| bbb | 22 |\n";
        let plain = strip(&render_markdown(md, 80));
        // 右对齐列：'1' 右侧被填充到列宽（'22' 之后紧跟框线，'1' 后有空格填充）
        let row1 = plain.lines().find(|l| l.contains("| a") || l.contains("│ a")).unwrap();
        assert!(row1.contains(" 1 │") || row1.contains(" 1 |"), "右对齐失效：{row1}");
    }

    #[test]
    fn unclosed_table_still_renders() {
        // 模型输出被截断（无闭合事件）也能出表
        let md = "| A |\n|---|\n| x |";
        let plain = strip(&render_markdown(md, 80));
        assert!(plain.contains('│'));
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p wavecode-cli markdown::tests::table`
Expected: 失败（无表格分支，`| A |` 原样输出无对齐/无压缩）

- [ ] **Step 3: 实现表格**（在 markdown.rs 中：①`Renderer` 加 `table: Option<TableBuilder>` 字段并在 `new` 初始化为 None；②`start_tag`/`end_tag` 插入 T2 省略的四个 `Tag::Table*`/`TagEnd::Table*` 臂；③`emit` 开头加表格分流；④`finish` 加未闭合兜底——均见 T2 代码中对应注释位置；⑤文件末尾追加以下完整代码）

```rust
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
            let floor = (avail / cols).min(MIN_COL_WIDTH).max(1);
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
```

注意：`Renderer::push_styled` 与自由函数 `styled` 存在重复——把 `Renderer::push_styled` 改为调用自由函数 `styled`（`self.out.push_str(&styled(text, style))`），保持 DRY。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p wavecode-cli markdown`
Expected: 全部通过（含 T2 的 9 个）

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/markdown.rs
git commit -m "界面增强 T3：markdown 表格渲染（CJK 宽度对齐/超宽压缩截断/对齐声明）"
```

---

## Task 4: wave.rs —— 波形帧与横幅

**Files:**
- Create: `crates/cli/src/wave.rs`
- Modify: `crates/cli/src/main.rs`（`mod wave;`）
- Test: 同文件 `#[cfg(test)]`

- [ ] **Step 1: 写失败测试**（先建桩）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_deterministic_and_phase_moves() {
        assert_eq!(frame(7, 0.0), frame(7, 0.0));
        assert_ne!(frame(7, 0.0), frame(7, 1.0), "相位应驱动波形变化");
    }

    #[test]
    fn frame_contains_block_chars_rgb_and_reset() {
        let f = frame(7, 0.0);
        // 7 个块字符（每个带 RGB ANSI 前缀）+ 末尾 reset
        assert_eq!(f.matches("\x1b[38;2;").count(), 7, "应为 7 段 RGB：{f:?}");
        assert!(f.contains('▁') || f.contains('▂') || f.contains('▃') || f.contains('▄') || f.contains('▅') || f.contains('▆') || f.contains('▇') || f.contains('█'));
        assert!(f.ends_with("\x1b[0m"), "末尾 reset：{f:?}");
    }

    #[test]
    fn palette_cycles_through_three_stops() {
        assert_eq!(palette(0.0), (64, 224, 208));
        assert_eq!(palette(1.0), (80, 140, 255));
        assert_eq!(palette(2.0), (186, 104, 255));
        assert_eq!(palette(3.0), palette(0.0), "调色板周期 3");
    }

    #[test]
    fn banner_contains_brand_model_cwd() {
        let b = banner("MiniMax-M3", std::path::Path::new("/tmp/demo"), "0.1.0", 4.2);
        assert!(b.contains("WaveCode v0.1.0"));
        assert!(b.contains("MiniMax-M3"));
        assert!(b.contains("/tmp/demo"));
        assert!(b.contains("\x1b[38;2;"), "波形应为 RGB 彩色：{b:?}");
        assert!(b.contains("提示"));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p wavecode-cli wave`
Expected: 编译错误/桩 panic

- [ ] **Step 3: 实现 `crates/cli/src/wave.rs`**

```rust
//! 波形动画：正弦采样块字符 + 青→蓝→紫 RGB 渐变。
//!
//! 品牌的"波形"动效内核：启动横幅动画、等待模型指示共用 [`frame`]。
//! 纯函数、确定性，ANSI 启用/剥离由输出侧 anstream 决定。

use anstyle::{Color, RgbAnsiColor, Style};

/// 八级块字符（由低到高）
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// 生成第 `phase` 帧、宽 `n` 格的波形（每格带 RGB ANSI，末尾 reset）。
pub fn frame(n: usize, phase: f32) -> String {
    let mut s = String::new();
    for i in 0..n {
        let h = ((i as f32 * 0.8 + phase * 0.9).sin() + 1.0) / 2.0;
        let ch = BLOCKS[(h * 7.0).round() as usize];
        let (r, g, b) = palette(i as f32 * 0.35 + phase * 0.25);
        let style = Style::new().fg_color(Some(Color::Rgb(RgbAnsiColor(r, g, b))));
        s.push_str(&format!("{}{}", style.render(), ch));
    }
    s.push_str(&Style::new().render_reset());
    s
}

/// 青→蓝→紫循环渐变：t 归一到 [0,3)，三段线性插值
fn palette(t: f32) -> (u8, u8, u8) {
    const STOPS: [(u8, u8, u8); 3] = [(64, 224, 208), (80, 140, 255), (186, 104, 255)];
    let tau = t.rem_euclid(3.0);
    let i = tau as usize;
    let f = tau - i as f32;
    let (a, b) = (STOPS[i], STOPS[(i + 1) % 3]);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// REPL 启动横幅（定格帧）。`phase` 取启动动画末帧相位，视觉无缝衔接。
pub fn banner(model: &str, cwd: &std::path::Path, version: &str, phase: f32) -> String {
    let wave = frame(7, phase);
    let name = Style::new()
        .fg_color(Some(Color::Ansi(anstyle::AnsiColor::BrightCyan)))
        .bold();
    let dim = Style::new().fg_color(Some(Color::Ansi(anstyle::AnsiColor::BrightBlack)));
    format!(
        "{wave}  {name}WaveCode v{version}{name:#}\n         {dim}{model} · {cwd}{dim:#}\n{dim}提示：直接输入任务；/quit 或 /exit 退出{dim:#}\n",
        cwd = cwd.display(),
    )
}
```

注意：`palette` 是私有的但测试直接调用——测试模块在同文件内（`use super::*`）可访问私有项，无需改可见性。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p wavecode-cli wave`
Expected: 4 passed

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/wave.rs crates/cli/src/main.rs
git commit -m "界面增强 T4：波形帧生成与彩色横幅（正弦采样 + RGB 渐变）"
```

---

## Task 5: render.rs —— 缓冲渲染状态机

**Files:**
- Modify: `crates/cli/src/render.rs`

- [ ] **Step 1: 改写/新增测试**（现有涉及"delta 直接输出"的断言全部按下述重写；`jsonl_one_event_per_line`、`truncate_*`、`sanitize_*` 保持不变）

```rust
    #[test]
    fn delta_is_buffered_until_complete_then_rendered_markdown() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        r.handle(&ev("s-1", EventMsg::TurnStarted { turn_id: "t".into() })).unwrap();
        r.handle(&ev("s-1", EventMsg::AgentMessageDelta { text: "**你好**".into() })).unwrap();
        assert!(r.out.is_empty(), "delta 不得直接输出");
        r.handle(&ev("s-1", EventMsg::AgentMessageComplete)).unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("你好"));
        assert!(!out.contains("**"), "markdown 记号应被渲染掉：{out}");
        assert!(out.contains("\x1b["), "应有样式：{out}");
    }

    #[test]
    fn interrupted_turn_renders_residual_buffer() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        r.handle(&ev("s-1", EventMsg::TurnStarted { turn_id: "t".into() })).unwrap();
        r.handle(&ev("s-1", EventMsg::AgentMessageDelta { text: "写了一半".into() })).unwrap();
        r.handle(&ev("s-1", EventMsg::TurnCompleted { stop_reason: StopReason::Interrupted })).unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("写了一半"), "中断残余应渲染：{out}");
        assert!(out.contains("（已中断）"));
        assert!(!out.contains("tokens:"), "中断无 tokens 行");
    }

    #[test]
    fn tool_lines_are_colored() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        r.handle(&ev("s-1", EventMsg::TurnStarted { turn_id: "t".into() })).unwrap();
        r.handle(&ev("s-1", EventMsg::ToolCallBegin { id: "1".into(), name: "write_file".into(), input: serde_json::json!({"path":"a.txt"}) })).unwrap();
        r.handle(&ev("s-1", EventMsg::ToolCallEnd { id: "1".into(), ok: false, output: "permission denied".into() })).unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("▸ write_file"));
        assert!(out.contains("✗ permission denied"));
        assert!(out.contains("\x1b["), "工具行应着色：{out}");
    }

    #[test]
    fn tick_frame_noop_when_not_animate() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        r.handle(&ev("s-1", EventMsg::TurnStarted { turn_id: "t".into() })).unwrap();
        assert!(r.is_waiting_on_model());
        r.tick_frame().unwrap();
        assert!(r.out.is_empty(), "非 animate 时 tick 为 no-op");
    }
```

（`ev()` 为现有测试辅助构造器，若名称不同以现有测试代码为准；`completed_turn_prints_latest_token_count` 保留语义——delta 改为经 complete 触发，tokens 行逻辑不变。）

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p wavecode-cli render`
Expected: 编译错误（`new` 签名/无 `tick_frame` 等）

- [ ] **Step 3: 实现状态机改造**（保留 `render_jsonl`/`sanitize_terminal`/`truncate_chars` 原样；其余按下述改造）

`HumanRenderer` 字段与构造：

```rust
pub struct HumanRenderer<W: Write> {
    out: W,
    /// 等待动画开关（human 模式 && TTY）
    animate: bool,
    /// 当前助手消息缓冲（delta 累积，complete/中断时渲染）
    msg_buf: String,
    /// 本 turn 最近一次 TokenCount（用于 tokens 行）
    last_usage: Option<(u64, u64)>,
    /// 波形相位（tick_frame 推进）
    phase: f32,
    /// 等待指示当前是否显示在终端上（下次输出前需 \r\x1b[K 清除）
    indicator_on: bool,
    /// 是否处于 turn 内
    in_turn: bool,
}

impl<W: Write> HumanRenderer<W> {
    pub fn new(out: W, animate: bool) -> Self {
        Self {
            out,
            animate,
            msg_buf: String::new(),
            last_usage: None,
            phase: 0.0,
            indicator_on: false,
            in_turn: false,
        }
    }

    pub fn is_waiting_on_model(&self) -> bool {
        self.in_turn
    }
}
```

`handle` 事件分发（关键臂；`EventMsg` 各变体字段以 protocol 实际为准）：

```rust
    pub fn handle(&mut self, ev: &Event) -> io::Result<()> {
        use wavecode_protocol::EventMsg::*;
        match &ev.msg {
            TurnStarted { .. } => {
                self.in_turn = true;
                self.msg_buf.clear();
                self.last_usage = None;
                self.phase = 0.0;
            }
            AgentMessageDelta { text } => {
                // 先剥控制字符（防终端注入），再进缓冲（不打印）
                let clean = sanitize_terminal(text);
                self.msg_buf.push_str(&clean);
            }
            AgentMessageComplete => {
                self.flush_message()?;
            }
            ToolCallBegin { name, input, .. } => {
                self.clear_indicator()?;
                // 工具行实时打印（即便有半截消息缓冲也先渲染掉，保持时序可读）
                self.flush_message()?;
                writeln!(self.out, "{}", human_tool_begin(name, input))?;
            }
            ToolCallEnd { ok: false, output, .. } => {
                self.clear_indicator()?;
                self.flush_message()?;
                writeln!(self.out, "{}", human_tool_error(output))?;
            }
            ToolCallEnd { .. } => {}
            TokenCount { used, window } => {
                self.last_usage = Some((*used, *window));
            }
            Warning { message } | Error { message } => {
                self.clear_indicator()?;
                self.flush_message()?;
                let style = if matches!(ev.msg, Warning { .. }) { theme_warn() } else { theme_err() };
                writeln!(self.out, "{}{}{}", style.render(), sanitize_terminal(message), style.render_reset())?;
            }
            TurnCompleted { stop_reason } => {
                self.clear_indicator()?;
                self.flush_message()?; // 中断路径的残余缓冲
                writeln!(self.out)?;
                if *stop_reason == StopReason::Interrupted {
                    writeln!(self.out, "{}（已中断）{}", theme_warn().render(), theme_warn().render_reset())?;
                } else if let Some((used, window)) = self.last_usage.take() {
                    let dim = theme_dim();
                    writeln!(self.out, "{}tokens: {used}/{window}{}", dim.render(), dim.render_reset())?;
                }
                self.in_turn = false;
            }
            _ => {} // non_exhaustive 兜底（保持现有注释风格）
        }
        Ok(())
    }

    /// 渲染缓冲消息（markdown）并清空；空缓冲 no-op
    fn flush_message(&mut self) -> io::Result<()> {
        if self.msg_buf.is_empty() {
            return Ok(());
        }
        let text = std::mem::take(&mut self.msg_buf);
        let width = terminal_width();
        write!(self.out, "{}", crate::markdown::render_markdown(&text, width))?;
        self.out.flush()
    }

    /// 清除等待指示行（若显示中）
    fn clear_indicator(&mut self) -> io::Result<()> {
        if self.indicator_on {
            write!(self.out, "\r\x1b[K")?;
            self.indicator_on = false;
        }
        Ok(())
    }

    /// 等待动画一帧（main 的 80ms tick 驱动）
    pub fn tick_frame(&mut self) -> io::Result<()> {
        if !self.animate || !self.in_turn {
            return Ok(());
        }
        self.phase += 0.35;
        write!(self.out, "\r{}", crate::wave::frame(14, self.phase))?;
        self.out.flush()?;
        self.indicator_on = true;
        Ok(())
    }
```

辅助（文件级，测试可用）：

```rust
/// 终端宽度：terminal_size 不可用时回退 80
fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

fn theme_tool() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::BrightCyan)))
}
fn theme_dim() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::BrightBlack)))
}
fn theme_warn() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Yellow)))
}
fn theme_err() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)))
}
```

`human_tool_begin`/`human_tool_error` 改为着色版（`▸` 与工具名亮青、input 摘要暗灰；`✗` 红、输出摘要红）：

```rust
pub fn human_tool_begin(name: &str, input: &serde_json::Value) -> String {
    let summary = truncate_chars(&input.to_string(), 80);
    let tool = theme_tool();
    let dim = theme_dim();
    format!(
        "▸ {}{}{} {}{}{}",
        tool.render(),
        name,
        tool.render_reset(),
        dim.render(),
        summary,
        dim.render_reset(),
    )
}
```

（`human_tool_error` 同理：`✗` 红 + `truncate_chars(output, 200)` 红。若现有测试 `human_render_tool_call_truncates_input` 断言字节长度，改为断言 `strip` 后的显示宽度 ≤ 81 或用 `truncate_chars` 的直接断言——同步修改。）

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p wavecode-cli`
Expected: 全绿（含改写的渲染测试与既有 JSONL/sanitize/截断测试）

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/render.rs
git commit -m "界面增强 T5：render 状态机改造——消息缓冲渲染、工具行/告警着色、等待动画帧"
```

---

## Task 6: main.rs —— 横幅、提示符与动画驱动

**Files:**
- Modify: `crates/cli/src/main.rs`

- [ ] **Step 1: 修改 `consume_turn`**——以现有函数体为底做一处结构改动：loop 顶部的 `let ev = client.next_event().await` 改为 `tokio::select!` 双分支，**其余逻辑（JSONL 的 writeln+flush+BrokenPipe 特判、is_completed 判定、`ConsumeOutcome` 归一、StreamEnded 返回）逐字保留**。目标结构：

```rust
let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
loop {
    tokio::select! {
        ev = client.next_event() => {
            // ……现有 match ev { Some(ev) => …, None => return Ok(ConsumeOutcome::StreamEnded) } 逐字保留……
        }
        _ = ticker.tick(), if !jsonl => {
            renderer.tick_frame()?;
        }
    }
}
```

（首个 interval tick 立即触发是 tokio 语义，此时 `in_turn=false`，`tick_frame` 为 no-op，无副作用。）

- [ ] **Step 2: REPL 横幅与提示符**

在 `run_repl`（或现有 REPL 入口函数）开头、打印横幅的位置替换原 `println!("WaveCode REPL — …")`：

```rust
// 启动横幅：TTY 时先播放 12 帧滚动动画（80ms/帧），定格后打印横幅；
// 非 TTY（管道）直接静态横幅（anstream 自动去色）
let version = env!("CARGO_PKG_VERSION");
let is_tty = std::io::stdout().is_terminal();
let mut phase = 0.0f32;
if is_tty {
    let mut out = anstream::stdout();
    for _ in 0..12 {
        phase += 0.35;
        write!(out, "\r{}", wave::frame(7, phase))?;
        out.flush()?;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    }
}
print!("{}", wave::banner(&cfg.model_name, &cfg.cwd, version, phase));
```

提示符：`editor.readline("wavecode> ")` → `editor.readline(PROMPT)`：

```rust
/// REPL 提示符：亮青波形符（rustyline 对含 ANSI 的 prompt 宽度计算经
/// 冒烟验证正常；若错位则退回同形无色版本，见 Step 4 验证）
const PROMPT: &str = "\x1b[96m∿\x1b[0m ";
```

- [ ] **Step 3: HumanRenderer 构造点更新**（`new(out, animate)`）：

- REPL：`HumanRenderer::new(anstream::stdout(), is_tty)`
- exec human：`HumanRenderer::new(anstream::stdout(), is_tty)`
- exec `--json`：人类渲染走 `anstream::stderr()`、`animate=false`；JSONL 仍走 `std::io::stdout().lock()`（**不动**）
- `use std::io::IsTerminal;` 引入

- [ ] **Step 4: 冒烟验证（本地，不调真实 API）**

```bash
cargo build -p wavecode-cli
# 无 config 错误路径仍退出码 2、指引无色（stderr 非 TTY）
./target/debug/wavecode.exe --config /nonexistent.toml exec hi; echo "exit=$?"
# REPL 管道冒烟：横幅应为静态（无动画）、提示符不出现于管道输出
printf '/quit\n' | ./target/debug/wavecode.exe
```

Expected: exit=2；管道输出含 `WaveCode v` 与波形字符、无 `\x1b` 转义残留（anstream 剥离）；cargo test 全绿。

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/main.rs
git commit -m "界面增强 T6：REPL 滚动彩色横幅、波形提示符、select! tick 等待动画"
```

---

## Task 7: SPEC 对账 + 真实 API 冒烟 + 全量门禁

**Files:**
- Modify: `docs/SPEC.md`（§15 CLI 渲染相关节）
- Test: 真实 API 冒烟

- [ ] **Step 1: 定位并更新 SPEC 渲染契约**

Run: `grep -n "渲染\|delta\|▸\|tokens" docs/SPEC.md | head -20`

将 CLI 渲染规则（M1 的"delta 直接 print"契约所在节）更新为：

```markdown
### CLI human 渲染契约（2026-08-03 起，取代 M1 裸流式）

- 助手消息：delta 经 sanitize 后缓冲，`AgentMessageComplete`（或中断残余）时经
  `markdown::render_markdown` 渲染（标题亮青加粗/粗斜删/行内码黄/代码块 `│ ` 边线/
  表格 CJK 对齐+超宽压缩）；--json 路径不受影响。
- 工具行：`▸ {工具名亮青} {input摘要≤80字符暗灰}`；失败 `✗ {output≤200字符}` 红；
  Warning 黄 / Error 红；tokens 行暗灰；`（已中断）` 黄。
- 品牌：REPL 启动横幅为音频波形块（TTY 播放 12 帧滚动彩色动画后定格），
  提示符亮青 `∿ `；等待模型期间显示滚动彩色波形指示（80ms/帧）。
- 降级：非 TTY 由 anstream 剥离 ANSI 且无动画；`NO_COLOR` 尊重；终端宽度
  取自 terminal_size，不可用回退 80。
```

- [ ] **Step 2: 真实 API 冒烟（markdown 重负载）**

```bash
mkdir -p /tmp/wavecode-ux && cd /tmp/wavecode-ux
RUST_LOG=off <repo>/target/debug/wavecode.exe exec "用 markdown 表格对比 Rust 和 Go 的 3 个差异（含中文列），再给一段 rust 的 hello world 代码块，最后用粗体总结一句话"
```

Expected: 退出码 0；输出中表格按列对齐（无 `|` 原文）、代码块带 `│ ` 前缀、粗体无 `**` 残留；tokens 行暗色。

REPL 管道冒烟：`printf '你好\n/quit\n' | wavecode`——横幅静态彩色被剥离、回复渲染正常、退出码 0。

- [ ] **Step 3: 全量门禁**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全绿

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "界面增强 T7：SPEC 渲染契约对账 + 真实 API 冒烟通过"
```

---

## 自查记录（writing-plans 自检）

**Spec 覆盖：** 设计文档 §5.1 渲染流程 → T5/T6；§5.2 样式表 → T2；§5.3 表格算法 → T3；§5.4 波形动画/横幅/提示符 → T4/T6；§5.5 降级 → T1(anstream)/T5(terminal_width)/T6；§6 测试 → 各任务 Step 1 + T7 冒烟；§7 文档对账 → T7。无遗漏。

**占位符扫描：** T2 的"先不含表格分支"是显式的任务切分说明（附完整插入位置与 T3 完整代码），非占位；pulldown-cmark Tag 变体差异已给出以编译器为准的微调口径。

**类型一致性：** `HumanRenderer::new(out, animate)`、`tick_frame`、`is_waiting_on_model` 在 T5 定义/T6 使用一致；`frame(n, phase)`/`banner(model, cwd, version, phase)` 在 T4 定义/T6 使用一致；`render_markdown(input, term_width)` T2 定义/T5 使用一致；`sanitize_terminal`/`truncate_chars` 沿用现有签名。

**修订（2026-08-03）：** T3 质量审查发现 `emit_cell` 截断分支丢弃宽字符剩余预算、截断行比列宽窄 1 格致 CJK 框线错位，已同步上方计划代码为补足填充版本（与 `crates/cli/src/markdown.rs` 实现一致）。
