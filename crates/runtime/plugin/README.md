# crates/runtime/plugin/ — manifest-ordered plugin registry with type-erased service injection

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; no workspace dependencies (serde/toml only) |
| `src/lib.rs` | `Plugin` / `AnyService` / `ServiceMap`: the plugin trait contributing type-erased services keyed by concrete `TypeId`; `Registry` starts plugins in manifest dependency order and stops them in reverse; `discover` / `load_and_start` read `runtime.toml` manifests with warn-and-skip degradation |

Duplicate names, missing dependencies, and dependency cycles are
explicit `PluginError` values, never panics, and a failed start starts
nothing partially. Manifests carry only identity (`name`, `version`,
`depends?`) — TOML carries no code — and every discovery failure
degrades to a startup warning instead of failing assembly. Hot removal
is `Registry::unload`; there is no file watching.
