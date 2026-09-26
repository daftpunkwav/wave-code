# crates/frontends/ — 用户可见的三个界面层：控制台 UI、其渲染引擎、`wavecode` CLI 入口

[English](README.md) | 中文

| 子 crate | 职责 |
|---|---|
| `console/` | `console-ui` — 交互式终端前端：主题 chrome、模态对话框、斜杠命令、transcript 渲染（见其 README） |
| `engine/` | `tui-engine` — 内联终端渲染库：组件、editor、markdown/mermaid、差分屏幕渲染（见其 README） |
| `harness/` | `harness-cli` — `wavecode` 二进制：headless exec、REPL、控制台启动与 app-server 界面（见其 README） |

依赖方向只向下。`tui-engine` 是零内部 workspace 依赖的纯渲染库，由其
`lib.rs` 中的测试锁定。`console-ui` 只通过 `tui-engine` 渲染，且只经
wire 协议（`wavecode-wire`）与 actor 客户端（`operations-actor`）访问
会话；其内部依赖集合恰为五个 crate（`tui-engine`、`wavecode-wire`、
`wavecode-config`、`operations-actor`、`state-persistence`），同样由
测试锁定。`harness-cli` 通过 `operations-bootstrap` 组装会话，拥有
`wavecode` 二进制的全部命令行界面，包括 `serve` app-server。
