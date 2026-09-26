# crates/operations/bootstrap/ — 组合根：把具体能力接到 run loop 的各 trait seam 之后

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；唯一具名具体能力 crate（`wavecode-tools`、`wavecode-sandbox`、`wavecode-hooks`、`wavecode-llm`、`wavecode-memory`、`wavecode-skills`、`wavecode-mcp`、`wavecode-context`）的地方 |
| `src/lib.rs` | crate 根；再导出各适配器与会话组装接口 |
| `src/session.rs` | `assemble_session` / `SessionHandle`：完整会话组装——配置、provider、权限（`Permissions`、`load_permissions`、`confinement_status`）、适配器、run loop、actor、client |
| `src/tool_adapter.rs` | `ToolAdapter`：以 `wavecode-tools` 注册表实现 `runtime_runner::ToolExecutor` |
| `src/policy_adapter.rs` | `PolicyAdapter`：以 `wavecode-sandbox` 实现 `PolicyDecider`，工具属性来自注册表 |
| `src/hook_adapter.rs` | `HookAdapter`：以 `wavecode-hooks` 实现 `HookGateway` |
| `src/model_adapter.rs` | `ModelAdapter`：以 `wavecode-llm` 实现 `ModelGateway`，把 provider 流折叠为响应块 |
| `src/gate_adapter.rs` | `GateApprovalSource` + `HeadlessDeny`：带超时地把审批/提问等待停在 `safety-gate` 上 |
| `src/compactor.rs` | `ContextCompactor`：以 `wavecode-context` 实现 `Compactor`，追加日志与任务列表脚注 |
| `src/evicting_gateway.rs` | `EvictingGateway`：gateway 装饰器，执行共享的保缓存工具结果淘汰 |
| `src/prune_adapter.rs` | `PruningExecutor`：executor 装饰器，把超长工具输出溢写到旁路存储 |
| `src/rate_limit.rs` | `RateLimitedModel`：聊天模型前的令牌桶限流 |
| `src/composite.rs` | `CompositeExecutor`：合并注册表工具与后注册的原生工具 |
| `src/native.rs` | `NativeExecutor` / `NativeTool`：以普通处理函数实现的进程内工具 |
| `src/plan_adapter.rs` | `TodoPlanTracker`：以既有 todo store 实现 `PlanTracker` |
| `src/goal_adapter.rs` | `GoalTrackerAdapter`：以会话 goal store 实现 `GoalTracker`（loop 侧只读） |
| `src/child_service.rs` | `TurnChildService`：把完整 turn 作为子任务运行，实现在 `action-tasks` seam 之后 |
| `src/memory_finish.rs` | `MemoryFinisher` / `SessionMemory`：会话结束时用模型蒸馏记忆 |
| `src/history_journal.rs` | `JournalSink`：把历史变更镜像进 write-ahead journal，并从中重建可恢复历史 |
| `src/grants_sink.rs` | `GrantSink`：把人工 "always allow" 决定持久化为授权记录 |
| `src/metrics_tap.rs` | `MetricsTap`：`EventTap`，把会话事件折叠进 `operations-observe` 的 ledger 样本 |
| `src/status_queries.rs` | `SessionStatus`：基于 plan/goal/snapshot store 实现 `operations_actor::StatusQueries` |
| `src/snapshot_tools.rs` | checkpoint store 之上的快照（只读）与恢复（需审批）工具 |
| `src/plugin_inventory.rs` | `PluginSummary`：把插件包发现映射为前端展示行 |
| `src/environment.rs` | `describe`：系统提示词中的事实性 host/cwd/date 段落 |
| `tests/workspace_layers.rs` | 通过 `cargo metadata` 读取真实依赖图并机械执行分层规则 |
| `tests/cache_prefix_stability.rs` | 在淘汰路径下守护 provider 提示词缓存前缀的回归 |

本 crate 位于依赖 DAG 顶端：runtime、state、action、safety 与
capability crate 都不依赖它。每个适配器把一个 `runtime-runner` trait
映射到一个具体 crate——策略留在既有 crate，映射留在这里，两侧互不
具名。缺少配置、provider 或凭据时组装硬失败；内存、技能、钩子、
目录等软降级则警告后继续。
