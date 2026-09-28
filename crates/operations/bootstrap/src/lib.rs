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

pub mod agents_instructions;
pub mod child_service;
pub mod compaction_tool;
pub mod compactor;
pub mod composite;
pub mod environment;
pub mod evicting_gateway;
pub mod gate_adapter;
pub mod goal_adapter;
pub mod grants_sink;
pub mod history_journal;
pub mod hook_adapter;
pub mod memory_finish;
pub mod metrics_tap;
pub mod model_adapter;
pub mod native;
pub mod plan_adapter;
pub mod plugin_inventory;
pub mod policy_adapter;
pub mod prune_adapter;
pub mod rate_limit;
pub mod session;
pub mod snapshot_tools;
pub mod status_queries;
pub mod tool_adapter;

pub use child_service::TurnChildService;
pub use compactor::ContextCompactor;
pub use composite::CompositeExecutor;
pub use gate_adapter::{Approvals, GateApprovalSource, HeadlessDeny};
pub use grants_sink::GrantSink;
pub use hook_adapter::HookAdapter;
pub use memory_finish::{MemoryFinisher, SessionMemory};
pub use model_adapter::ModelAdapter;
pub use native::{NativeExecutor, NativeTool};
pub use plan_adapter::TodoPlanTracker;
pub use policy_adapter::PolicyAdapter;
pub use session::{
    APPROVAL_TIMEOUT, AssembleOptions, DEFAULT_IDENTITY, DEFAULT_MAX_TOOL_ROUNDS, Permissions,
    SessionError, SessionHandle, assemble_session, confinement_status, load_permissions,
};
pub use status_queries::SessionStatus;
pub use tool_adapter::ToolAdapter;
