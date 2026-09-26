# crates/frontends/engine/src/ — tui-engine 源码文件地图

[English](README.md) | 中文

| 文件 | 职责 |
|---|---|
| `lib.rs` | crate 根：渲染模型文档、模块组织、re-export，以及零内部依赖测试 |
| `component.rs` | 组件模型：`Component`/`Container`/`Focusable`，以及 `Segment`——跨帧共享的引用计数行数组 |
| `screen.rs` | 内联差分屏幕渲染器：最小重写、原生 scrollback、synchronized-output 标记、pinned 输入尾区、写时 margin |
| `editor.rs` | 多行输入编辑器：grapheme 级正确编辑、CJK 感知换行、历史、kill ring + undo、bracketed paste、补全弹窗、提交契约 |
| `autocomplete.rs` | 补全管线：`CompletionProvider` trait、触发检测与弹窗 |
| `select_list.rs` | 斜杠命令、文件提及、picker 与对话框共用的弹窗选择列表 |
| `fuzzy.rs` | 用于补全排序的模糊子序列评分 |
| `markdown.rs` | Markdown 到 ANSI 的渲染，带 `SyntaxHighlighter` 与 `FenceRenderer` seam（mermaid 渲染器由此接入） |
| `mermaid.rs` | mermaid 围栏块的终端渲染：公共图 IR + 分层带状布局，sequence/gitGraph/mindmap/图表类各有专用布局 |
| `math.rs` | 数学片段的极简 TeX 到 Unicode 转换（上下标、分数、根号） |
| `width.rs` | ANSI 感知的可见宽度测量、截断与按 grapheme 边界的折行 |
| `color.rs` | `Color`/`Style` 与 SGR 输出，带 `ColorDepth` 降级（truecolor / 256 / 16） |
| `keys.rs` | 归一化按键模型（`Key`、`KeyEvent`、`Mods`），组件代码无需解析终端序列 |
| `terminal.rs` | raw mode、bracketed paste、焦点上报、Kitty keyboard 协议、OSC 11 背景探测、配色方案同步 |
| `border.rs` | 圆角框绘制，editor 边框、欢迎卡片与对话框共用 |
| `loader.rs` | 按活动类型区分的 spinner 帧（sine = composing，triangle = thinking，saw = 机器工作） |
| `text.rs` | 静态预着色文本组件 |
| `typing_burst.rs` | 无 bracketed paste 终端的粘贴检测（burst 期间 Enter 改写为 Shift+Enter） |
| `sanitize.rs` | 在模型/工具来源的字符串到达终端前剥离 C0/C1 控制字符与 ESC 序列 |
| `markdown/` | `markdown.rs` 的 `#[cfg(test)]` 测试 |
| `screen/` | `screen.rs` 的 `#[cfg(test)]` 测试 |

组件只讲 ANSI 字符串与引擎自有的 `Color` 值；screen 层在帧间对它们的
行数组做差分。扩展 seam（`SyntaxHighlighter`、`FenceRenderer`、
`CompletionProvider`）让应用层接入高亮、图形与补全来源，而本 crate
不因此增加依赖。
