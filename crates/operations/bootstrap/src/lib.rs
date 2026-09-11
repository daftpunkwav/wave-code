/*!
 * @file Bootstrap
 * @description Composition root: adapters wiring concrete capabilities
 * behind the run loop anticorruption traits.
 *
 * Responsibilities:
 * - Own the only dependency edges to concrete capability crates.
 * - Adapt Tool/Policy/Hook/Model implementations to trait seams.
 * - Never contain execution policy itself; only wiring and mapping.
 *
 * This module must not be depended on by: runtime, state, action, safety,
 * capabilities, or any lower layer. It is the top of the DAG.
 */

//! Bootstrap adapters between legacy capabilities and the new run loop.
//!
//! Each adapter implements one `runtime-runner` trait by delegating to a
//! concrete legacy crate. Policy stays in the legacy crates; mapping stays
//! here, so neither side names the other directly.

pub mod child_service;
pub mod compactor;
pub mod composite;
pub mod gate_adapter;
pub mod hook_adapter;
pub mod job_tools;
pub mod mcp_bridge;
pub mod mcp_serve;
pub mod memory_finish;
pub mod memory_tool;
pub mod model_adapter;
pub mod native;
pub mod plan_adapter;
pub mod policy_adapter;
pub mod session;
pub mod skill_tool;
pub mod task_tools;
pub mod tool_adapter;

pub use child_service::TurnChildService;
pub use compactor::ContextCompactor;
pub use composite::CompositeExecutor;
pub use gate_adapter::{Approvals, GateApprovalSource, HeadlessDeny};
pub use hook_adapter::HookAdapter;
pub use mcp_bridge::{McpConnectReport, StdioMcpClient, connect_all};
pub use memory_finish::{MemoryFinisher, SessionMemory};
pub use memory_tool::MemoryWrite;
pub use model_adapter::ModelAdapter;
pub use native::{NativeExecutor, NativeTool};
pub use plan_adapter::TodoPlanTracker;
pub use policy_adapter::PolicyAdapter;
pub use session::{
    APPROVAL_TIMEOUT, AssembleOptions, DEFAULT_IDENTITY, DEFAULT_MAX_TOOL_ROUNDS, SessionError,
    SessionHandle, assemble_session,
};
pub use skill_tool::SkillTool;
pub use task_tools::{TaskOutputTool, TaskStopTool};
pub use tool_adapter::ToolAdapter;
