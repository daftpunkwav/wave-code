# crates/safety/audit/ — append-only 审计轨迹

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;无依赖 |
| `src/lib.rs` | `AuditVerdict`(Allow/Ask/Deny/Error)、`AuditEvent`(seq、actor、action、target、verdict)、`AuditLog` 的 `append` / `by_actor` / `all` |

每条记录获得单调递增的序号,日志保持写入顺序,因此 `by_actor` 能为事件复盘提供稳定的过滤视图:谁对哪个工具做了什么、策略如何裁决。轨迹仅存于内存,本 crate 不依赖任何其他 workspace crate。
