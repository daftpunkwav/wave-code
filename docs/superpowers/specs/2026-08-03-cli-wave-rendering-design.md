# WaveCode CLI 界面增强设计：波形品牌 + markdown 渲染

日期：2026-08-03 · 状态：已获用户批准（关键决策见 §2）

## 1. 背景与目标

M1 的 REPL/exec 输出为纯文本：模型回复的 markdown 原文（`**粗**`、表格 `|…|`）裸打印、无任何颜色与品牌识别。用户体验后提出三点诉求：

1. 界面契合 "WaveCode"（波形）品牌——美观、有辨识度、不复杂；
2. 重要内容（标题、加粗、代码）应有颜色或加粗；
3. markdown 表格在终端里对齐渲染（当前原文裸奔，中文列宽全乱）。

## 2. 关键决策（用户逐项批准）

| 决策点 | 结论 | 备选项（未选） |
|---|---|---|
| 渲染时机 | **消息级缓冲渲染**：模型消息完整到达后一次性渲染为美化 markdown；工具调用行维持实时；等待期有动态指示 | 逐字流式仅上色 / 文本流式+表格缓冲 |
| 横幅标识 | **音频波形块** `▁▃▅▇▅▃▁`（青蓝色系） | 波浪字符线 `∿∿∿` / 极简 `≋` 单行 |
| 波形动效 | **滚动 + 彩色**：启动横幅波形播放短动画后定格；等待模型指示复用同一动画组件；提示符 `∿` 静态亮青 | —（用户明确要求） |

## 3. 技术方案

`pulldown-cmark` 解析 markdown 为事件流 + 自写 ANSI 终端渲染器（外观完全自主可控）。弃选现成终端 md 库（termimad：外观不可控、依赖重）与逐行自解析（边界 case 多、造轮子）。

**依赖**（workspace 统一管理）：
- 新增 `pulldown-cmark`（启用 `ENABLE_TABLES` / `ENABLE_STRIKETHROUGH` Options，代码层开启而非 feature）；
- 提升已有传递依赖为直接依赖：`anstream`（1.0.0，自动 TTY 检测 / Windows VT / `NO_COLOR`）、`anstyle`（1.0.14）、`unicode-width`（0.2.2，CJK 宽字符列宽计算）。

## 4. 模块划分（均在 `crates/cli/src/`）

| 模块 | 职责 | 依赖 |
|---|---|---|
| `markdown.rs`（新） | md 事件流 → ANSI 样式文本的纯函数渲染器；表格列宽算法 | pulldown-cmark, anstyle, unicode-width |
| `wave.rs`（新） | 波形帧生成（正弦采样 + 调色板）、启动动画、等待动画、TTY/宽度降级 | anstyle, unicode-width |
| `render.rs`（改） | 事件状态机：delta 缓冲、complete 触发渲染、工具行实时着色、Warning/Error/tokens 着色 | markdown, wave, anstyle |
| `main.rs`（改） | REPL 横幅打印、提示符改 `∿`、等待动画的 `select!` tick 驱动 | wave |

`sanitize_terminal` 组合顺序不变：模型文本**先**剥控制字符（防注入），**再**由渲染器加入我们自己的 ANSI 样式。

## 5. 详细设计

### 5.1 渲染流程（HumanRenderer 状态机）

```
TurnStarted            → 清理状态；启动等待动画（仅 human+TTY）
AgentMessageDelta      → sanitize 后追加 msg_buf（不打印）
ToolCallBegin          → 停动画清行（\r\x1b[K）；实时打印 "▸ {工具名亮青} {input摘要暗灰}"
ToolCallEnd(ok)        → 无输出（保持 M1 契约）
ToolCallEnd(!ok)       → "✗ {output≤200字符}" 红色
AgentMessageComplete   → 停动画清行；markdown::render(msg_buf) 输出；清空 buf
TurnCompleted          → 若 msg_buf 非空（中断路径无 complete）先渲染残余；
                         换行；见过 TokenCount 打印 "tokens: u/w"（暗灰）；
                         Interrupted 打印 "（已中断）"（黄）
Warning/Error          → 黄/红原样打印
```

等待动画的驱动：`HumanRenderer` 暴露 `is_waiting_on_model()` 状态查询；`main.rs` 的事件消费循环由 `next_event().await` 改为 `tokio::select!`（事件 vs 80ms tick）；tick 到达且 `is_waiting_on_model()` 为真时重绘一帧。`--json` 模式、非 TTY、动画关闭时不插入 tick 分支（行为与现状一致）。

### 5.2 markdown.rs 样式表

| 元素 | 样式 |
|---|---|
| H1/H2/H3 | 亮青 + 加粗（不加 `#` 前缀） |
| 粗体/斜体/删除线 | ANSI bold / italic / strikethrough |
| 行内 `code` | 黄 |
| 代码块 | 每行前缀 `│ `（暗灰），正文默认色；不做语法高亮（YAGNI） |
| 表格 | 见 5.3 |
| 无序/有序列表 | `•` / 原编号，缩进 2 格，嵌套累进 |
| 引用块 | 行前缀 `│ ` 灰 |
| 链接 | 下划线 + 蓝（不附加 URL，终端可点击查看原文） |
| 水平线 | `────────`（暗灰，随终端宽度） |
| 段落 | 默认色，段间空行 |

### 5.3 表格算法

1. 解析表头/分隔行/数据行（pulldown-cmark `Table` 事件组）；
2. 每个单元格先按行内样式渲染为"文本+样式段"，显示宽度用 `unicode-width`（CJK=2、emoji=2）；
3. 列宽 = 各单元格最大显示宽度（含表头）；总宽 = Σ列宽 + 分隔符；
4. 总宽 ≤ 终端宽：直接对齐打印（`│` 分隔，表头下 `─` 线，对齐方式遵循 md 源 `:---:` 声明，默认左对齐）；
5. 总宽 > 终端宽：按比例压缩各列（保底 4 格），单元格显示宽度截断并补 `…`（按字符边界，不切碎多字节）；
6. 终端宽度：新增 `terminal_size` crate（极小、单一职责、维护活跃；winapi/ioctl 自写平台代码量远超引入它的成本——新增依赖理由即此，符合 SPEC §3 边界规则 3 的说明要求），不可用（重定向等）时回退 80。

### 5.4 波形动画（wave.rs）

- **帧生成**：宽度 N（横幅 7 格、等待指示 ~14 格），对每列采样 `▁▂▃▄▅▆▇█` 八级块字符：`height(i, t) = (sin(i·0.8 + t·0.9) + 1) / 2`，量化为块字符；相位 `t` 逐帧 +1 形成流动。
- **调色板**：青 → 蓝 → 紫渐变循环（anstyle RGB，每列颜色随相位偏移）；静态定格帧用同一渐变（无动画环境下也是彩色波形）。
- **启动横幅**（仅 REPL 且 TTY）：约 12 帧 × 80ms 滚动播放，随后定格打印：
  ```
  ▁▃▅▇▅▃▁  WaveCode v{version}      （波形渐变彩、名称加粗）
             {model} · {cwd}          （暗灰）
  提示：直接输入任务；/quit 或 /exit 退出（暗灰）
  ```
  非 TTY（管道）跳过动画直接静态横幅（仍彩色，anstream 管道下自动去色）；终端宽 < 20 时波形退化为 `≋`。
- **等待指示**：循环播放同组件动画（无限相位推进），消息渲染/工具行前 `\r\x1b[K` 清除。
- **提示符**：`∿ ` 亮青静态（rustyline 的 prompt 字符串含 ANSI 码，需验证 rustyline 对带色 prompt 的宽度计算——rustyline 支持 ANSI prompt，必要时用其 `highlight` 机制或预计算可见宽度；实现时验证，若宽度错位则提示符退回无色 `∿ `）。

### 5.5 颜色与降级

- 全部 human 输出走 `anstream::stdout()`（非 TTY 自动剥离 ANSI、Windows 启用 VT、尊重 `NO_COLOR`）；stderr 侧的人类渲染（--json 模式）走 `anstream::stderr()`。
- JSONL 输出路径保持原 `io::stdout().lock()` 纯 JSON，零样式代码触碰。
- exec 非交互模式：无横幅、无动画；颜色/markdown 渲染与 REPL 一致。

## 6. 测试

- `markdown.rs`：标题/粗斜删/行内码/代码块/列表/引用/链接/水平线 样式断言（含 ANSI 序列）；表格：ASCII 对齐、CJK 对齐（中文 2 格）、超宽压缩截断（不断裂多字节）、对齐声明生效；
- `wave.rs`：帧生成确定性（固定相位断言波形字符串与调色序列）、非 TTY 降级；
- `render.rs`：delta 不直接输出、complete 触发渲染、中断残余渲染、工具行着色、JSONL 不受污染（现有测试改造：裸 delta 断言 → 缓冲断言）；
- `main.rs` 横幅 smoke（buffer 断言含波形字符与版本号）；
- 门禁：`cargo test --workspace` / `fmt` / `clippy -D warnings` 全绿；手动冒烟（真实 API 一次 exec 看渲染效果 + REPL 管道冒烟）。

## 7. 文档对账

- `docs/SPEC.md` §15.4（CLI 渲染契约）更新：缓冲渲染流程、样式表、横幅/动画/提示符、降级策略；
- 设计文档即本文（docs/superpowers/specs/）；计划文件 M1 渲染契约为历史记录不动。

## 8. 范围外（YAGNI）

- 代码块语法高亮（syntect 重依赖，后续里程碑评估）；
- ratatui TUI（M2 另行设计，本渲染器语义可复用但实现独立）；
- exec 模式横幅；鼠标交互/超链接点击增强。
