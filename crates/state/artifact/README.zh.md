# crates/state/artifact/ — 运行产出物的版本化登记表

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;无依赖 |
| `src/lib.rs` | `ArtifactKind`、`Artifact`(`<name>@<version>` id、kind、digest)、`ArtifactStore` 的 `publish` / `get` / `latest` / `list` |

`publish` 对同名产出物自动递增版本号,重复发布不会破坏历史;`latest` 解析浮动指针,`get` 钉住某个带版本的 id。payload 留在产出方一侧——store 只记录身份、类别与生产方提供的完整性 digest(格式不透明),并且不依赖任何其他 workspace crate。
