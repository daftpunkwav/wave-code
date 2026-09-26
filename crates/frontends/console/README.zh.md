# crates/frontends/console/ — 交互式终端控制台 UI（console-ui）

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；边界注释记录了锁定的依赖集合 |
| `src/` | crate 源码：UI orchestrator、chrome、对话框与主题系统（见其 README） |
| `assets/` | 随二进制发布的数据：`synthwave-84.tmTheme`，内置的语法高亮主题 |
| `tests/` | 集成测试目标目录（当前为空；覆盖在 `src/` 单元测试中） |

控制台只经 wire 协议（`wavecode-wire`）与 actor 客户端
（`operations-actor`）访问会话，所有渲染都委托给 `tui-engine`。其内部
依赖被锁定为恰五个——`tui-engine`、`wavecode-wire`、
`wavecode-config`、`operations-actor`、`state-persistence`——由
`src/lib.rs` 中的 `dependency_matrix_locked` 测试保证，该测试将每个
依赖表的每个键对照白名单检查。它不得依赖 runtime、action、safety、
transport crate 或组合根。
