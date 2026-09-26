# crates/operations/ — 会话生命周期：actor、组合根、RPC 网关与可观测性

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `actor/` | `operations-actor` — 会话串行驱动器与进程内客户端句柄，外加 `SessionSurface` 契约 |
| `bootstrap/` | `operations-bootstrap` — 组合根，把具体能力接到 `runtime-runner` 各 trait seam 之后 |
| `eval/` | `operations-eval` — 脚本化基准、录制会话回放与任务级评分 |
| `gateway/` | `operations-gateway` — RPC 服务外壳：stdio 上的 ACP、loopback 的 REST + SSE app 服务端、MCP 工具服务端 |
| `observe/` | `operations-observe` — wire 事件之上的只读指标折叠与 append-only turn 台账 |
| `simulate/` | `operations-simulate` — 模型计划动作的 dry-run 渲染 |

这一层的依赖边全部单向：`gateway` 只经 `actor` 的 `SessionSurface`
trait 消费会话；`bootstrap` 负责组装会话，并且是本层唯一允许具名
具体能力 crate 的成员（它位于 workspace DAG 顶端，本层没有任何
crate 依赖它）；`actor` 经泛型 `runtime-runner::TurnDriver` seam 驱动
turn。`observe` 与 `simulate` 是纯观察者——前者只依赖 wire 协议，
后者只依赖 runner 的 seam 类型——因此二者都无法影响执行。
