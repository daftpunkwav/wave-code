# crates/action/browser/ — async tab seam 之后的浏览器自动化

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；无 workspace 依赖（仅 thiserror/async-trait/tokio） |
| `src/lib.rs` | `BrowserSession` trait（open/snapshot/click/fill/close），建立在驱动中立的 `TabState` 与 `BrowserError` 之上；附带 `FakeBrowser`：页面渲染预置文本、未知 tab id 与真实驱动一样报错的脚本化假实现 |

本 crate 就是数据加一条 seam：真实的协议驱动（CDP、WebDriver）实现
`BrowserSession` 即可，本 crate 无需改动。`FakeBrowser` 会追踪打开的
tab，使测试观察到的失败模式与真实驱动一致；其动作日志按调用顺序
记录每一次操作。
