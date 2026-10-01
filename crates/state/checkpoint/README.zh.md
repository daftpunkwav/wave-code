# crates/state/checkpoint/ — 命名检查点与文件内容快照

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;依赖 `thiserror`、`serde_json`、`tokio`、`wavecode-config`(home 目录) |
| `src/lib.rs` | `Checkpoint`/`CheckpointStore`(内存态,回滚保留目标并丢弃更新者)、`CheckpointPolicy` 与 `durable_save`/`durable_load`/`list_resume_labels`(原子写入并 fsync 的 `<label>.json` 文件)、`SnapshotStore`(工作区快照,存于 `<home>/.wavecode/snapshots`,带配额与回退) |

快照 payload 是调用方自有的不透明字符串:store 从不解释它,"恢复意味着什么"属于 driver。label 会成为文件或目录名,因此必须匹配 `[A-Za-z0-9_-]{1,64}`;捕获有配额上限(单文件 512 KB、共 1000 个文件、64 MB 总量),跳过二进制内容与 `.git`/`target`/`node_modules`/`.venv`,并且存放在工作目录之外,因此回退从不依赖 git。`CheckpointPolicy` 默认 fail-closed——两个挂载点都做检查点——而完全禁用的策略则以零 IO 短路。
