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

pub mod gate_adapter;
pub mod hook_adapter;
pub mod model_adapter;
pub mod plan_adapter;
pub mod policy_adapter;
pub mod tool_adapter;

pub use gate_adapter::GateApprovalSource;
pub use hook_adapter::HookAdapter;
pub use model_adapter::ModelAdapter;
pub use plan_adapter::TodoPlanTracker;
pub use policy_adapter::PolicyAdapter;
pub use tool_adapter::ToolAdapter;
