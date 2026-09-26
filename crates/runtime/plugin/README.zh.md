# crates/runtime/plugin/ — 按清单依赖序启动的插件注册表与类型擦除服务注入

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；无 workspace 依赖（仅 serde/toml） |
| `src/lib.rs` | `Plugin` / `AnyService` / `ServiceMap`：插件 trait 向以具体 `TypeId` 为键的容器贡献类型擦除的服务；`Registry` 按清单依赖序启动插件、按逆序停止；`discover` / `load_and_start` 读取 `runtime.toml` 清单，失败则警告并跳过 |

重名、缺失依赖与依赖环都是显式的 `PluginError`，绝不 panic；启动
失败时不会留下部分启动的状态。清单只承载身份（`name`、`version`、
`depends?`）——TOML 不携带代码——所有发现失败都降级为启动警告，
不会让组装失败。热移除即 `Registry::unload`；没有文件监视。
