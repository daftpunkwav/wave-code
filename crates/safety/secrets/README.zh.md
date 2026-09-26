# crates/safety/secrets/ — 带脱敏的密钥存储

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;无依赖 |
| `src/lib.rs` | `SecretsStore`(`from_env`、`insert`、`get`、`names`)、使用 `REDACTED` 占位符的 `redact` |

值只能通过显式查找获取——环境变量读取只发生在 `from_env` 构造器中,记录与脱敏期间从不读取。`redact` 把每个已知值替换为 `***`,按长度从长到短处理以便重叠值被完整遮蔽;它是面向日志的尽力而为的卫生措施,不是安全边界——按定义,未知的值无法被遮蔽。缺失的环境变量被静默跳过,可选凭证不会让组装失败;空值从不存储,因此不可能把一切都脱敏掉。
