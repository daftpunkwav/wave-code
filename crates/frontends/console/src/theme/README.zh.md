# crates/frontends/console/src/theme/ — 主题系统：语义 token、数据文件与解析

[English](README.md) | 中文

| 文件 | 职责 |
|---|---|
| `mod.rs` | 模块组织，外加 `apply_terminal_scheme`：OSC 10/11/12 同步，使 light 主题在深色终端宿主上保持可读 |
| `tokens.rs` | 语义颜色契约：`Token` 角色与解析后的 `Palette`；本身不含任何颜色值 |
| `file.rs` | `theme.json` 格式：用户主题（`~/.wavecode/themes/<name>.json`）的解析、校验与存储；未知字段、颜色名与语法别名一律拒绝 |
| `builtin.rs` | 内置主题，编译期从 `themes/*.json` 嵌入 |
| `active.rs` | 全局活动 `Theme`（`set`/`current`）与所有组件渲染所经的 paint 辅助函数 |
| `detect.rs` | 解析顺序：配置选择 → `NO_COLOR`/CI 环境 → OSC 11 终端背景探测 |
| `syntax.rs` | `SyntaxTheme` 别名，把 chrome 主题映射到代码高亮配色 |
| `themes/` | 内置主题数据文件：`dark.json`、`deepwave.json`、`light.json` |

颜色活在数据里，不在代码里：组件请求语义 `Token`，由活动 palette 解
析（共 23 个 token；主题可覆盖任意子集）。一次选择同时驱动界面
chrome 与代码块的语法高亮主题。
