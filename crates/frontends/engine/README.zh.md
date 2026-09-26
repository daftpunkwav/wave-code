# crates/frontends/engine/ — 零内部依赖的内联终端渲染库（tui-engine）

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | 清单；边界注释记录了零内部依赖规则 |
| `src/` | 库源码：组件、editor、markdown/mermaid/math 渲染器与差分屏幕渲染（见其 README） |
| `tests/` | 只测公开 API 的黑盒集成测试：`markdown_contract.rs`（渲染契约、缓存、fence seam）与 `mermaid_dispatch.rs`（图形分发与 `MermaidFences`） |

引擎是零内部 workspace 依赖的纯渲染库——只有外部 crate
（`crossterm`、`unicode-width`、`unicode-segmentation`、
`pulldown-cmark`，unix 上另加 `libc`），由 `src/lib.rs` 中的
`dependency_matrix_locked` 测试锁定。渲染模型：给定宽度下，每个组件
为每个终端行产出一个 ANSI 字符串，`screen` 在帧间对这些数组做差分，
只重写变化的部分，同时保留原生 scrollback（从不使用备用屏幕）。语义
主题与会话接线都活在其上的应用层；引擎对两者一无所知。
