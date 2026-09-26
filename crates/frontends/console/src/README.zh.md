# crates/frontends/console/src/ — console-ui 源码文件地图

[English](README.md) | 中文

| 文件 | 职责 |
|---|---|
| `lib.rs` | crate 根：模块组织、re-export，以及锁定五个内部依赖的 dependency-matrix 测试 |
| `ui.rs` | UI orchestrator：帧组装、按键/粘贴/resize 分发与会话事件泵；panic 与 BrokenPipe 时的终端恢复 |
| `dialogs.rs` | 模态对话框——工具审批与结构化提问；对话框打开期间独占全部按键输入 |
| `slash.rs` | 斜杠命令的注册表、解析与分发效果；未知 token 按普通输入落空 |
| `complete.rs` | 斜杠命令与 `@` 文件提及的补全 provider（有界 workspace 扫描） |
| `diff.rs` | 文件编辑的聚类 LCS diff 渲染：`+N -M` 头部、gutter 网格、流式安全 |
| `highlight.rs` | 挂在引擎 `SyntaxHighlighter` seam 上的 syntect 语法高亮；内置 SynthWave '84 tmTheme |
| `history.rs` | 持久化输入历史：每个前端 surface 一份 append-only JSONL |
| `settings.rs` | 持久化 UI 偏好（`~/.wavecode/console-settings.json`），经单一 `Arc<Mutex<UiSettings>>` 共享 |
| `state.rs` | chrome 组件共享的 `AppState`：会话身份、streaming 阶段、上下文用量、排队输入、todos |
| `transcript.rs` | 按轮次分组的 transcript 缓冲，带窗口化裁剪（保留最新 15 轮，滞回 5 轮） |
| `welcome.rs` | 欢迎卡片：braille 示波器轨迹上的 figlet 字标，resize 时泛起涟漪 |
| `git_info.rs` | 分支徽章：直接读 `.git/HEAD`，不起子进程 |
| `chrome/` | 持久边框元素：footer 两行、外部 status line、通知、tab 进度、符号表、todo 面板 |
| `controllers/` | 会话事件与 UI 状态之间的粘合：流式合并、`/btw` 侧会话、`!` shell 任务 |
| `messages/` | transcript 消息组件：user/assistant/status、thinking、工具调用、compaction、shell、usage |
| `panes/` | transcript 与编辑器之间的成框区域：排队消息 pane |
| `theme/` | 主题系统：语义 token、`themes/*.json` 数据文件、解析（见其 README） |
| `ui/` | `ui.rs` 的测试专属模块：行为测试、`/model` catalog 测试与 ANSI showcase 转储 |

帧组装、输入分发与会话事件泵都在 `ui.rs`；其余文件要么是它组合的
组件（`messages/`、`chrome/`、`panes/`、dialogs、transcript），要么是
支撑状态（settings、history、theme）。渲染只跨 `tui-engine` 类型；会
话交互只跨 `wavecode-wire` 事件/操作与 `operations-actor` 客户端。
