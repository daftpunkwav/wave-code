# crates/safety/guardrail/ — prompt 注入筛查与污点跟踪

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;无依赖 |
| `src/lib.rs` | 基于固定模式表的 `scan`/`Signal`/`Severity`、`judge`/`Verdict`(Allow/Warn/Block)、带 `combine` 的 `Taint` |

筛查是对一张简短、通用模式表做的 ASCII 大小写不敏感子串搜索;`judge` 把命中的最高严重级映射为唯一裁决,使所有调用方的判定一致——只要有一条 high 信号就阻塞,无论周围有多少正常文本。`Taint` 跟踪流入拼装 prompt 的不可信工具输出,一旦存在就保持污点。这些启发式是减速带而非高墙:它们拦截常见的注入与意外的指令泄露,针对性的攻击还需要在其上叠加分层审查。
