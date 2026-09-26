# crates/foundation/auth/ — 按 provider 索引、以显式值传递的凭据

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | `Credential`（API-key/bearer，`Debug` 脱敏，`expose()`）、`AuthStore`（按 provider 查找，`from_env`、`insert`/`remove`/`get`/`get_or`）、`Scheme`、`AuthError`、`REDACTED` |

查找始终按 provider 名称隔离，两个 provider 不可能意外共享凭据材料；
空值在构造时即被拒绝，配置错误在组装期就失败而不是在深夜才暴露。
原始密钥材料只经 `Credential::expose` 离开本 crate；`Debug` 只输出
scheme 并以 `REDACTED` 代替真实值，因此日志与父结构的 `{:#?}` 转储
都不会泄漏密钥。`from_env` 对缺失或空的环境变量直接跳过而非失败，
可选 provider 不会阻断启动。持久化存储（OS keyring、vault）不在
本 crate 范围内：调用方通过 `AuthStore::insert` 或 `from_env` 把值
推入，transcript 中的掩码处理由这些调用点对接的 secrets store 负责。
本 crate 零依赖（连外部依赖也没有），当前 workspace 内也没有其他
crate 依赖它。
