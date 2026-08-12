//! wavecode-core — agent 引擎。
//!
//! 职责：session 生命周期、turn 状态机循环（组装上下文 → 流式采样 →
//! 工具编排 → hooks → 状态驱动下一轮）、任务模型（Regular / Compact /
//! Review / Goal）、slash 指令分发、goal / plan 模式、subagent 编排与
//! 后台任务。
//!
//! 只依赖 [`wavecode_protocol`] / [`wavecode_llm`] / [`wavecode_tools`] /
//! [`wavecode_sandbox`]（P2 审批与安全模型）/ [`wavecode_context`]（P3
//! 上下文管线：预算核算 / 三级阈值 / 压缩）/ `wavecode_memory`（P6 记忆
//! 系统）/ `wavecode_skills` + `wavecode_hooks`（P7）/ `wavecode_config`
//!（P7，[hooks] 原始配置转换）/ `wavecode_mcp`（P9，MCP 接口边界与
//! 工具桥）的公开接口（auth 按 SPEC §3 矩阵留给后续里程碑引回），
//! 不感知任何前端形态；与前端的唯一交互通道是 [`wavecode_protocol`]
//! 的 Submission / Event。
//!
//! M1 已落地：[`session`]（Session 生命周期与 turn 状态机循环，mock 模型
//! 验证）；P2 落地 AwaitApproval 审批管道；P3 落地 PreTurn 三级阈值 /
//! reactive compact / max_tokens 续写 / `/compact`；P4 落地规划系统
//!（[`prompt`] 提示词分层组装、todo 清单注入、stop steering）；P5 落地
//! 子代理（[`subagent`]：task / task_output / task_stop 工具、独立 Session
//! 隔离上下文、`<task-notification>` 回注）；P6 落地记忆系统（[`memory`]：
//! WAVECODE.md 槽位注入、memory_write 工具、会话结束自动提取）；P7 落地
//! skills 与 hooks（[`skills`]：SKILL.md 清单注入 / `skill` 工具 / inline
//! 展开与 fork 派生 / `/name` slash 直调；[`hooks`]：command hook 引擎
//! 装配与八个事件点挂接）；P9 落地 MCP 预留接口（[`mcp`]：client/server
//! trait 边界再导出、`mcp__{server}__{tool}` 工具桥、`[mcp_servers]`
//! 配置转换；真实 transport 留待后续迭代）。

//! P10 落地会话持久化（[`rollout`]：rollout jsonl 追加写、构造即
//! replay 的 resume 恢复、`list_threads` 列表；SQLite 索引降级与 fork
//! 占位见模块注释）。

pub mod hooks;
pub mod mcp;
pub mod memory;
pub mod prompt;
pub mod rollout;
pub mod session;
pub mod skills;
pub mod subagent;

pub use memory::MemorySessionConfig;
pub use rollout::RolloutConfig;
pub use session::{ApprovalGate, Session, SessionConfig};
pub use skills::SkillSessionConfig;
pub use subagent::{SubagentManager, SubagentType, TaskResult, TaskState};
