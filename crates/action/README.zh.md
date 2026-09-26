# crates/action/ — 可被模型调用的能力 seam：tasks、jobs、workflow、browser、retrieval

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `browser/` | `action-browser` — async tab seam 之后的浏览器自动化，附带脚本化假实现 |
| `jobs/` | `action-jobs` — 后台 shell 作业（spawn/wait/cancel/output）及 `job_*` 工具 |
| `retrieval/` | `action-retrieval` — 分块文档上的词项重叠检索 |
| `tasks/` | `action-tasks` — 能力中立的任务生命周期 seam 及其测试假实现 |
| `workflow/` | `action-workflow` — 校验式 DAG 执行、Ralph 循环、持久化调度及 `workflow_run`/`ralph_run`/`schedule` 工具 |

分层契约直接体现在清单里：`browser`、`retrieval`、`tasks` 完全没有
workspace 依赖——它们只定义 seam 与词汇。执行层负责实现这些 seam
（组合根把 `tasks` 映射到 child runtime），而一个 crate 要变得可被
模型调用时，只增加唯一一条指向 `wavecode-tools` 的能力边
（`jobs/tools.rs`、`workflow/tools.rs`）。向下的依赖边也只落在被动
机制上：`workflow` 可以具名 vocabulary 层的 `runtime-scheduler`，
`jobs` 可以用 `runtime-child` 的完成通道，但任何 action crate 都
不会具名 state、operations 或 frontend。
