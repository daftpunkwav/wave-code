# Console UI 子系统

[English](ui.md) | 中文

交互式 console 是 wire + actor 接缝之上的两个 crate：

- `frontends/engine`（`tui-engine`）——纯渲染库，零内部依赖。组件对给定宽度按终端行产出一条 ANSI 字符串；屏幕层在帧间 diff 这些数组，只重写变化的区间（主屏幕内嵌模式，保留原生回滚缓冲，CSI 2026 同步输出）。
- `frontends/console`（`console-ui`）——主题化应用层。它只经 `wavecode-wire` 与 `operations-actor` 跨到会话（由 `dependency_matrix_locked` 锁定），从不引用能力 crate 或组合根。

## 字形身份

字形家族与活动类别一一对应。用户输入是唯一的非波浪标记（键入的 chevron）；动画化的工作骑在 loader 波形上：

| 字形 | 类别 | 出现位置 |
| --- | --- | --- |
| chevron `❯` | 用户输入（键入的笔画） | 编辑器提示符、用户项目符号、队列指针 |
| 三角扫描 `▁▃▅▇▅▃` | thinking（进行中） | 流式 thinking 头部 spinner |
| 锯齿坡 `▁▃▅▇` | 机器工作 | 工具/shell 运行点、压缩卡片脉冲 |
| 花形 `✻` | thinking（定稿） | 定格的 `Thought for Ns` 头部 |

结果标记（`●` 完成、`✗` 失败、`○` 待处理、`✓` 已完成）保持中性——它们报告结果，不报告活动种类。助手消息的项目符号是中性的 `●` 圆点：草稿流式输出时按固定节奏闪烁，消息完成后定格为常亮。

## 帧模型

一帧是完整的逻辑行数组：

```
transcript (welcome, user/assistant messages, thinking blocks, tool cards, shell cards, status)
todo panel               (todowrite mirror: ● in-progress / ✓ done / ○ pending)
queue pane               (queued user messages + steer hint)
editor box               (rounded frame, `❯` prompt, autocomplete popup below)
footer row 1             (▍mode model cwd ⎇ branch · rotating tip)
footer row 2             (transient exit hint ... context: N% (used/max))
```

一切都坐落在一列宽的 gutter 内，转录与 chrome 共享左边缘。随着转录增长，旧行滚入原生回滚缓冲，永不被重写；diff 渲染器追踪可重写基线，并把光标移动钳制在底边距内。两条屏幕层不变量保证帧在流式输出时保持稳定：

- **钉住的尾部**：帧的末尾行（编辑器框、自动补全弹窗、footer）在每个写入了内容的帧上都以绝对定位重新锚定到物理底部行，因此上方的流式输出或回滚缓冲扰动永远无法把输入拖出屏幕。仍然放得下屏幕的帧跳过钉住（原地 diff 已重写变化行）；完全相同的帧什么也不写。收缩的帧（对话框或自动补全弹窗关闭）会整屏重绘视口，并把可重写基线重新锚定到新帧底部，使重绘与钉住对输入区域的位置达成一致，屏幕中部不会残留一份过期的编辑器。
- **光标纪律**：硬件光标在整个帧期间隐藏，只在输入编辑器的插入符处（屏幕层解析到单元格的嵌入式标记）重新显示，因此任何转录、footer 或流式行都不可能停下一个闪烁的光标。终端模式在退出时（以及 panic 时）恢复，包括光标。

### 分段帧与写入时 gutter

`Component::render` 返回一个 `Segment`——一个引用计数的行数组（`Arc<Vec<String>>`）。一帧是段的列表，每个组件一段；在上屏途中没有东西被摊平成扁平的 `Vec<String>`。diff 渲染器同样把上一帧保存为段，这带来两件事：

- **引用计数存储**：保存上一帧是每段一次引用计数递增，不是逐行深拷贝。
- **指针相等跳过**：一个段的分配若与上一帧同区间的段相同，则按构造即相同——diff 跳过整个区间，一行都不比较。缓存命中的组件（已定稿的消息、welcome 卡）每帧交回同一个分配，因此空闲区域的成本是一次引用计数比较；重建的段（流式草稿、状态行）回退到具有扁平比较语义的逐行比较。

帧 gutter 在写入时施加，不在帧内：`Screen::set_margin` 在渲染器写每行时缩进它，并相应收缩截断预算，因此存储的行保持不填充，没有任何帧为逐行填充拷贝买单。缓存失效（主题切换、mermaid 开关）以 `invalidate_all` 遍历转录并丢弃流式草稿，因此缓存的段永远不会携带过期的调色板或模式。

## UI 消费的 wire 扩展

- `ToolCallEnd.output` — 工具结果的有界头部（`ToolCallPreview`，4 KiB，字符边界安全）。卡片渲染折叠的结果（≤3 行），Ctrl+O 展开。
- `TokenCount.context_window` / `context_used` — footer 上下文表（`context: 42% (82.0k/195k)`，1024 进制单位，百分比向上取整）。

两个字段都是 `Option` + `skip_serializing_if`；较旧的发送方与接收方保持兼容。

## 事件 → 组件映射

| 事件 | UI 效果 |
| --- | --- |
| `TurnStarted` | phase → waiting |
| `AgentThinkingDelta` | 活动中的 thinking 块（spinner + 最后 2 行，暗色斜体） |
| `AgentMessageDelta` | thinking 定稿；助手草稿流式输出（delta 批次把草稿标脏；帧在 50 ms 刷新间隔到期时或 100 ms tick 上重绘） |
| `AgentMessageComplete` | 草稿（或完成文本）落为 markdown 消息 |
| `ToolCallBegin` / `ToolCallEnd` | 工具卡片开/合（状态点、动词、关键参数、结果）；`edit` 卡片以聚类的 LCS diff 预览开头（每侧 2 000 行预算，超出显示摘要行；失败在 diff 下方显示应用错误）；`todowrite` 输入镜像进 todo 面板 |
| `ApprovalRequested` | 审批对话框（1/2 快速选择、Enter、Esc = 拒绝、Ctrl+C = 拒绝） |
| `QuestionRequested` | 提问对话框（编号选项 + 自由文本） |
| `TokenCount` | footer 上下文表 + 累计用量（由 `/usage` 显示） |
| `Compact*` / `Plan*` / `Goal*` | 状态行 |
| `Warning` / `Error` | 状态行（错误色） |
| `TurnCompleted` | 草稿折叠、转录修剪（15 回合 + 5 迟滞）、排队消息派发 |

## 输入

- 多行编辑器，带字素正确的移动、CJK 感知换行、kill ring、撤销、输入历史（Up/Down 带草稿恢复），以及 `~/.wavecode/input-history/console.jsonl` 下的持久 JSONL 历史。
- 括号粘贴，带大粘贴折叠（`[paste #N +L lines]` 标记在提交时原子展开）。
- Slash 命令（`/help /btw /new /clear /sessions /resume /fork /title
  /model /provider /doctor /agents /hooks /release-notes /effort
  /permissions /auto /wave /plan /init /mcp /settings
  /theme /usage /version /status /memory /snapshots /goal /compact
  /undo /editor /reload /copy /export /exit`），带模糊补全；未知的 `/tokens` 作为用户输入落空穿透（技能）。每个带参数的命令在裸调用时打开其交互面——`/theme` 打开主题选择器，`/effort` 打开级别列表，`/title`、`/editor`、`/export` 与 `/compact` 打开预填的自由文本提示，`/undo` 打开回退选择器，`/btw` 询问问题——而即时动作（`/clear`、`/new`、`/fork`、`/auto`、`/copy` 等）与纯信息命令（`/usage`、`/status`、`/mcp` 等）保持直接执行。
  `/usage` 渲染按严重性着色的上下文条加从 `TokenCount` 样本累计的 token 拆分。`/copy` 经 OSC 52 把最后一条助手消息放进剪贴板；`/export` 把完整未修剪的用户/助手对话写成 markdown（会提示输入路径；直接传一个则跳过提示）。模式与模型命令立即更新本地 chrome（没有 mode-changed wire 事件）。
- `/compact` 立即压缩上下文；提示接受可选指令来引导摘要（`CompactTrigger::Manual` 把焦点带给模型摘要器）。转录显示一张活动压缩卡片（锯齿波脉冲、已用秒数），落定为
  `● compacted: context <before>, summary <N> tokens`，其中 `before`
  是最近一次采样的上下文用量。失败的压缩不发出完成事件，因此卡片在
  证明它已死的错误上落定为
  `● compaction failed (…)`（空闲 `/compact`）或回合结束（回合内自动压缩）。
- `/undo` 打开覆盖最近回合的回退选择器（或 `/undo <n>`
  直接丢弃最后 n 个，默认 1，
  仅限空闲）：wire 的 `Rewind` op 截断 actor 的会话，
  `HistoryRewound` 事件修剪对话与转录，被截断的对话作为最新快照记入日志，resume
  回放回退后的对话。仅限会话层——被丢弃回合已做出的文件
  更改不会被撤销。双击 Esc
  （600 ms 窗口，仅限空闲）打开覆盖最近用户回合的同一回退选择器（最新优先，至多 8 行）；选中一行走同一 `/undo` 路径，因此 busy/shell 守卫原样适用。
- 会话：每次交互式启动把完成的回合（文本快照）记入
  `~/.wavecode/sessions/<uuid>.jsonl`，带共享的
  `index.json`（标题、cwd、时间戳、回合数）。`/sessions`（别名
  `/resume`）打开选择器（输入即搜索，Ctrl+A 切换 cwd/全部
  范围）并原地恢复：harness 重新装配一个以记录的历史播种的会话，转录回放它。
  `wavecode --session <id>` 与 `wavecode --continue`（-c，当前目录的最近
  会话）从 CLI 做同样的事。`/fork`
  把当前对话快照进一份可恢复的副本并留在原会话（打印 `--session` 命令）；`/title <title>` 改名；
  `/new` 经同一重装配路径开始新会话（新上下文与日志）；`/clear` 仍是软性屏幕重置。只要会话的 write-ahead 历史日志
  存在，resume 就是块级的（工具调用、工具结果、thinking 与图像都回来）；
  日志存在之前写入的会话回退到文本
  快照。见 `docs/subsystems/sessions-state.md`（中文版 [sessions-state.zh.md](sessions-state.zh.md)）。
- `/btw <question>` 在只读的侧会话中提一个旁边的问题（plan 模式，经同一会话工厂以主对话播种）：回答流式输出到编辑器上方的面板，从不进入主会话或日志。后续追问（面板打开时的 `/btw <question>`）骑同一侧会话；Esc 关闭面板并将其关闭。主会话全程保持可交互。
- `/model` 打开模型选择器：供应商标签页（Tab/Shift+Tab）、
  输入即搜索、↑/↓ 导航、`← current` 标记，以及 `N more` 折叠。条目来自配置的 `[models]` 表
  （`[models.<alias>]`，含 `provider`/`model`/可选
  `reasoning_effort`），外加模型目录 `~/.wavecode/models.json`
  （其模型以合成的 `catalog:<provider>` 供应商并入 `[models]`；同 id 的 config.toml 供应商获胜）以及配置的默认模型。Enter 把选择保存为默认（
  `~/.wavecode/console-settings.json` 中的 `default_model`/`default_provider`；CLI `--model` 仍然获胜；
  跨供应商的默认在下次启动时生效——活体切换仅限同供应商）。Alt+S 只把选择应用到本会话，同样仅限同供应商：跨供应商的选择不能上线，因此那里的 Alt+S 被拒绝并指向 Enter。Thinking 行
  （OpenAI 兼容供应商；预算驱动的 Anthropic thinking 会隐藏它）以 ←/→ 在 off/low/medium/high 间切换并实时派发
  `SetThinking`（仅限同供应商的选择，因此被选模型的 effort 不会泄漏到仍在运行的模型上）。`/effort <level>`
  直接设置级别；`/model <name>` 保留按名直接切换。
  目录本身经 `/provider` 编辑：裸调用打开 provider 面
  （已存 provider 及其模型数；选中一个会预填向导；新 provider 行
  从零开始），裸 `/provider add` 走逐步向导覆盖全部规格
  （provider、API 方言、端点、key——`env:NAME` 存环境变量名、裸值
  存内联 key、留空跳过——模型、别名、上下文/输出上限、思考档位、
  输入/输出模态，最后是复查；一次可通过"再加一个模型"在同一
  provider 上暂存多个模型）。`/provider add <alias> <kind>
  <provider> <base_url> <model> [context] [max_output]`
  按位置参数直接插入一条（命令不接受凭据；`api_key_env` 或内联
  `api_key` 可经向导或编辑文件设置，文件在 Unix 上保持
  owner-only），`/provider set <alias>
  <context|output|thinking|input> <value>` 修补单个字段，
  `/provider remove <alias>` 删除条目；旧的
  `/model add|list|set|remove` 拼写指向 `/provider` 而非静默无效。目录是可选的：文件缺失即空目录，而畸形的文件会报告并让子命令取消（普通 `/model <name>` 切换从不触碰该文件，因此坏目录无法连带弄坏活体切换）。
- `/permissions` 打开模式选择器（plan/auto/wave 带
  描述）；`/plan`、`/auto`、`/wave` 直接应用。
- `/help` 打开可滚动面板（键位绑定加每条命令及其描述；
  ↑/↓ 行滚动、PgUp/PgDn 翻页、Esc/q 关闭）。
- `/init` 发送固定的分析提示，让 agent 为仓库写 AGENTS.md。
  `/mcp` 列出配置的 MCP 服务器。`/status`
  打印聚合摘要（会话 id/标题、模型 + 供应商 +
  thinking、模式、cwd、分支、上下文、用量、mcp、版本）。
- `!cmd` shell 模式：前导 `!` 让命令在平台 shell（`cmd /C` / `sh -c`）下本地运行，输出实时显示在转录卡片中（暗色尾部、`(esc to cancel)`、失败时显示退出码行）。当缓冲以 `!` 开头时，编辑器把边框染成 shell 紫罗兰色并加 `! shell mode` 标签、给 `!cmd` token 上色。Esc 与
  Ctrl+C 先取消正在运行的命令（Windows 经 `taskkill /T` 杀掉整个
  `cmd` 树；其他平台用 `start_kill`）。Shell 命令
  从不抵达会话，可与忙碌回合并发运行，且同一时刻只有一个在跑。
- Shift+Tab 循环权限模式（plan → auto → wave）并
  重染编辑器边框（plan = primary，wave = warning）。
- 权限模式：`plan` 只读（非只读工具直接
  拒绝，无提示）、`auto` 放行编辑、仅对命令执行与破坏性工具询问、`wave` 允许
  一切。wave 拒绝清单（`~/.wavecode/console-settings.json` 中的
  `wave_denylist`，`Bash(pattern)` 规则语法或裸命令）在所有模式下作为 sandbox deny 规则强制执行：被禁命令无提示直接拒绝。
- `/settings` 打开交互面板（Up/Down 移动、Left/Right
  或 Enter 循环、更改持久化到
  `~/.wavecode/console-settings.json` 并应用到活动组件）：
  用户输入 markdown 渲染开/关、工具调用详细程度
  （names/summary/full）、edit 渲染（tool only/diff）、
  wave 拒绝清单条目数、thinking 默认展开、流式助手草稿开/关、
  页脚上下文计量条、页脚提示轮换、退出前二次确认，以及
  输入历史上限。
- 提交的用户输入默认在转录中渲染为 markdown（编辑器从不渲染）；该设置可将其关闭。
- 助手与用户 markdown 中的表格渲染为 box-drawing 网格，表头加粗、列均分收缩以适应宽度；参差的行填充到共享的显示宽度网格上，因此 CJK 单元格与对勾字形无法把边框拉歪。
- `@` 文件提及，带有界的 workspace 清单（2 000 条目，
  跳过 vendored/隐藏目录）。
- Ctrl+C 级联：忙碌时中断，否则武装双击退出
  （1 500 ms 窗口，footer 提示）；空编辑器上的 Ctrl+D 武装同一级联。Ctrl+O 切换展开、Ctrl+T 切换 todo
  面板、Ctrl+S 推进正在运行的回合（排队消息或编辑器文本）、Esc 在忙碌时中断（两种中断都以状态行确认），Alt+B/Alt+F 按词前后跳。空 shell 模式提示符上的 Esc 离开 shell 模式。Ctrl+G 把草稿交给外部编辑器：
  终端离开 raw 模式，编辑器经平台 shell 在临时文件上运行
  （`/editor <cmd>` 设置，否则 `$VISUAL`/`$EDITOR`），
  保存的文本替换草稿；空保存保留原文。
- 压缩显示一张活动卡片，落定为
  `● compacted (trigger): context <before>, summary <N> tokens`——或，
  当压缩器失败时（手动 `/compact` 报告一个无回合附带的可恢复
  错误）落定为
  `● compaction failed (trigger); context unchanged`。Console
  在回合结束或失败错误上让仍在运行的卡片落定，因此
  脉冲及其动画 tick 永远不会跑个不停。
- 文件写入审批载荷携带受影响的行（`-` 旧、
  `+` 新，来自 sandbox 的 `ask_detail`），审批对话框以 diff 颜色渲染它们——用户批准的是可见内容，不是一个光秃秃的路径。字符与行预算在 sandbox、wire 截断与对话框三处约束载荷。
- wire 驱动的模态（审批或提问）绝不比它的回合活得更久：
  在 `TurnCompleted` 上——以及不可恢复错误上——对话框以状态行关闭，因为挂起的门已随回合消亡，回答只会产生"迟到审批"警告。
  用户打开的对话框（设置、选择器）从不被回合事件触碰。
- 回合完成通知：每个完成的回合发一次 OSC 9 桌面通知，除非被中断或有排队的后续延续会话（`WAVECODE_NOTIFY=0` 禁用）。`WAVECODE_NOTIFY_STYLE`
  选择投递方式：`osc9`（默认）、`bell`（裸 BEL 铃）或
  `both`；在 tmux 下 OSC 9 载荷经 DCS 直通。
- 终端 chrome 序列：窗口标题（OSC 0，
  `WaveCode · model · session title`）跟随模型、`/title` 与
  会话切换；运行中的回合报告不定进度（OSC 9;4 state 2，每秒重发一次，回合结束与退出时清除）。序列走单槽 pending-sequence，绝不覆盖排队的通知。
- 助手 markdown 中的 fenced 代码块做语法高亮
  （syntect 加 two-face 额外语法集——TypeScript、TOML 等
  ——跑在纯 Rust 的 fancy-regex 后端上），位于引擎的
  `SyntaxHighlighter` 接缝之后；语法主题跟随活动 chrome
  主题的配对（synthwave → 捆绑的 synthwave-84 tmTheme、
  deepwave → base16-ocean.dark、light → base16-ocean.light）。超大
  块（>30 KB）与未知语言回退为普通行。
- footer 显示 workspace git 分支（⎇ 徽章），构造时与每个回合开始时直接从 `.git/HEAD` 读取（父目录上溯、worktree 的 `gitdir:` 文件形态、detached 时显示短 sha）——无子进程。
- welcome 卡片无边框且居中：`slant` figlet 字标
  （`WAVECODE`，用 pyfiglet 生成）自上而下从 primary 渐变到 accent，
  直接坐在盲文示波器轨迹上——一条 1 像素振幅调制的正弦线，从字母间流出——一个空白间隔，然后是左对齐的信息网格
  （model / dir + ⎇ branch / mode + mcp + version）。低于 56 列
  字标回退为带空格的 `W A V E C O D E` 行。
  调整窗口大小会搅动轨迹：它以 ease-out 尾巴向右滑动
  1.4 s，然后落定（一个 tick 驱动帧；`Welcome::is_rippling`
  报告状态）。一个 `#[ignore]` 快照测试（`welcome_snapshot`）
  把它打印出来供目测。
- 每个语义字形都来自 `chrome/symbols.rs`（单一来源）：
  用户输入的 `❯` chevron、`●`/`✓`/`○`/`✗` 中性结果、`⎇`
  分支。运动保持单单元格：回合运行时编辑器提示符每 400 ms 在 `❯`↔`›` 间脉冲（空闲时静止；shell 模式保持
  `!`）、流式助手草稿的 `●` 项目符号在落定前按固定节奏闪烁、运行中的工具或 shell 卡片脉冲锯齿坡帧、排队消息翻转 chevron 脉冲、活动 thinking 头部旋转三角扫描后冻结为
  `✻ Thought for Ns`。

## 输入净化

`ConsoleUi::push_status` / `push_user_message` / `push_assistant_message`
与 delta 处理器把每个来自 wire 的字符串经 `sanitize_terminal` 过滤后才进入组件；工具卡片以同样方式净化
名称与参数摘要。引擎加了第二道守卫：URL 携带控制字符的 markdown 超链接
降级为普通带样式文本（不发 OSC 8）。信任边界与 harness 其余部分一致：wire 字符串在净化前是敌意的。

## Steering 词汇说明

`SteerTarget` 起源于 `runtime-runner` 但经
`operations-actor` 再导出；console 只依赖 actor crate，这是既定接缝
（actor 本就包裹 runner）。

## 主题化

主题是纯数据：每个主题一个 `theme.json`。内置主题以捆绑文件发布
（`theme/themes/dark|deepwave|light.json`，经 `include_str!` 嵌入、只解析一次）；用户主题放入
`~/.wavecode/themes/<name>.json` 后成为 `/theme <name>`——编写指南见
`docs/themes.md`（中文版 [../themes.zh.md](../themes.zh.md)）。主题代码是解耦的，一个文件一个职责
（`theme/tokens.rs` 语义契约、`theme/file.rs` 格式 + 存储、
`theme/builtin.rs` 捆绑数据、`theme/active.rs` 全局 + 绘制辅助、`theme/detect.rs` 解析）；Rust 里没有任何颜色值。

所有调色板共享一个极简结构：四档中性文本坡道加纯灰 chrome，
**每主题一个强调色相**（dark 主题骑 azure、deepwave 骑 teal、light 骑 GitHub blue）承担
提示符、用户输入、行内代码与焦点 chrome，以及一条语义
带（绿/琥珀/红）被 diff 一对与 shell 模式复用。其他什么都没有颜色；代码块经各自的语法主题保持彩色。
用户输入行坐在一个微妙的更亮条带上。23 个语义 token，
十六进制值由测试对照解析后的数据文件锁定，
`light|dark|deepwave|auto` 解析（Unix 上的 OSC 11 探测——有界 `poll`，
绝不是阻塞读取线程，因为控制台输入上的字节读取会与 crossterm 的事件读取器竞争并偷走按键；Windows 完全跳过探测 → `COLORFGBG` → 默认），以及启动时一次性安装的全局主题。组件请求 token，绝不
请求原始颜色；引擎工作在 `Color`/`Style` 值上。颜色深度同样在启动时解析一次（`COLORTERM` truecolor、`TERM`
256 色，否则 16 经典 ANSI 色），每次绘制经同一角色映射降级。

两条调色板契约由测试锁定，主题因此不会退化回旧的薰衣草坡道：

- **无紫罗兰灰**：在每个中性坡道 token（text、dim、muted、
  border、neutral、diff gutter）上绿色保持不低于红色，通道递进保持均匀——蓝色超出绿色的量不超过绿超出红的步长——因此灰读作灰，绝不发紫。dark 调色板是真正的石墨坡道；deepwave 的 slate
  （均匀、偏蓝的递进）仍然被允许。
- **对比度锁定的墨色**：每档坡道相对自己主题的 `background` token 满足 WCAG 对比度——正文 ≥ 7:1、dim ≥ 4.5:1、muted
  ≥ 3.5:1——且输入条带保持该背景之上安静而可见的一档。

`background` token 同样是功能性的：light 主题把它的纸面 + 墨色 + 光标色应用到终端本身（OSC 11 背景、
OSC 10 前景、OSC 12 光标——尽力而为），从而在深色终端宿主上保持可读可用——否则那些终端近似纯白的默认
前景与光标会在纸面上消失。任何切换回暗主题——或退出时的终端恢复以及外部编辑器往返前后——都会把三者重置为终端
自己的颜色（OSC 110/111/112）。忽略这些序列的终端
不受影响。作为纵深防御，本会继承终端默认前景的 span——编辑器草稿正文、
自动补全弹窗、markdown 列表标记与编辑器滚动标签——都从主题绘制，消息渲染器在每次主题切换时重排自己的 markdown
（在 `dark` 下渲染的消息在 `light` 下重绘，而不是相反的操作顺序）。

**deepwave** 身份保持可选（`/theme deepwave` 或自定义主题的
`"base": "deepwave"`）：同样的极简结构，slate 坡道上的 teal 强调。

一个主题选择同时驱动 chrome 角色与语法
高亮：每个内置主题与一个 syntect 主题配对
（synthwave → 捆绑的 `synthwave-84` tmTheme、deepwave →
`base16-ocean.dark`、light → `base16-ocean.light`；见
`theme/syntax.rs`）。高亮器（`console/src/highlight.rs`）只从 syntect 主题读取前景色与字体样式——
背景从不渲染，因此代码块始终坐在终端
自己的背景上。

用户主题遵循同一文件格式（一个 `base`
（`dark`/`light`/`deepwave`）加 23 个 token 中任意子集的 `#rrggbb` 覆盖、可选的 `syntax_theme` 别名、可选的
`description` 显示在选择器中、可选的 `dark` 类别）。
未知 token 名、畸形颜色、未知别名与未知
字段被拒绝（拼写错误不得无声地渲染成 base），路径逃逸到不了文件系统，且 `/theme <name>` 实时应用一个——编辑器与弹窗样式从新
调色板重建，与内置主题一样，且重绘会清除回滚缓冲，
使前一调色板下绘制的行无法存活。

## Markdown 渲染约定

渲染器（`tui-engine/src/markdown.rs`）在块之间保持一个空行、绝不到两个：每个块（标题、段落、列表、
fence、引用、表格、分隔线）确保与之前的内容隔开，而不是推入尾部空行——因此列表后跟标题不再挤在一起，流式输出期间仍为空的中部文档标题也不会留下幻影空行。
紧凑列表项保持紧凑；松散列表（空行分隔）项按其源间隔渲染。

Fenced 代码块渲染为暗色圆角框——顶部 `╭─ lang ───`
（仅当 fence 携带语言标签时显示，白名单为语言名字符，因此 fence 信息无法注入转义
序列）、每行高亮文本前一条暗色 `│ ` 竖条、底部 `╰────`。字面的 ``` 标记从不渲染。框紧贴内容
（外加标签）并在宽终端上限制在 80 列；高亮文本本身不在 fence 样式内重新换行，因为
内部 ANSI 重置会把它取消。

### Fenced 块渲染器与行内修整

Fence 在语法高亮器之前咨询可插拔渲染器
（`FenceRenderer` 接缝；应用以 `Markdown::with_fence` 注册更多）：

- ```mermaid fence 渲染为 box-drawing 图（默认开启；
  console 中 Ctrl+M 切换源视图）。一个共享图引擎（节点 + 带样式边 + subgraph 框、分层带或列布局）承载 `graph`/`flowchart`（TD/TB/LR、subgraph
  分组、节点形状折叠为盒子）、经 flowchart 适配器的 `stateDiagram(-v2)`、`classDiagram`、`erDiagram`、
  `requirementDiagram` 与 `C4*` 家族。有自己几何的类别：`sequenceDiagram` 泳道、`gitGraph` 分支时间线、
  `mindmap`/`timeline` 树，以及 `pie`、`journey`、
  `quadrantChart` 与 `xychart-beta` 的图表行。任何不支持的
  （`block-beta`、`sankey-beta`）或超出列预算的，
  返回空，fence 回退为普通源视图。
- ```diff / ```patch fence 骑专用行样式：新增
  绿色、删除红色、文件头与 `@@` hunk 用元数据色调、上下文普通。

行内预处理（全部感知 fenced 代码、在解析前应用）：
`$…$` / `$$…$$` 数学转换为 unicode（`E = mc²`；块数学把
矩阵环境排布在对齐行上）、`==highlight==` 折叠为粗体、`^sup^` / `~sub~` 在 span 内每个字形都有对应码点时映射到上/下标码
点、`<details>`/`</details>` 行消失而 `<summary>X</summary>` 变为粗体 `▸ X` 标记
行、`<kbd>C</kbd>` 变为行内代码。无法映射的 span——
或处理集合之外的标签——原样通过。

## 本代刻意的非目标

后台任务与 plan 审批流需要尚不存在的 wire 或
后端支持。跨供应商的活体模型切换按设计需要会话重装配
（base URL 与凭据烘焙进供应商客户端），因此选择器为下次启动持久化一个默认值。
