# crates/safety/ — 策略层安全原语

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `audit/` | `safety-audit`——内存中的 append-only 审计轨迹,记录 allow/ask/deny/error 裁决 |
| `gate/` | `safety-gate`——按 call id 停靠审批与提问请求,无竞态、一次性投递 |
| `guardrail/` | `safety-guardrail`——启发式 prompt 注入筛查与污点跟踪 |
| `secrets/` | `safety-secrets`——具名密钥存储,并为日志与转录做脱敏 |

这一层只做策略裁决:操作系统级隔离(landlock、seatbelt、ACL)不在其范围内;权限模式是 `wavecode-protocol` 拥有的 wire 词汇,这里从不重新定义它们。依赖边刻意保持最小——`audit`、`guardrail`、`secrets` 零依赖,`gate` 只需要 `tokio`(oneshot channel)与 `thiserror`。这些 crate 对自身局限直言不讳:guardrail 的启发式是减速带而非高墙,密钥脱敏是日志卫生而非安全边界。
