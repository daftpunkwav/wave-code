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

/// Cross-stack serde contract: `wavecode_wire` and `wavecode_protocol`
/// are sibling vocabulary crates that cannot share a dependency edge, so
/// each names its own `ApprovalKind` — the wire event payload type and
/// the sandbox verdict type. Both spell `exec` / `write` in snake_case on
/// the wire; this test locks the two representations byte-equal so a tag
/// change on one side cannot ship without the other (both enums' doc
/// comments point here).
#[cfg(test)]
mod approval_kind_cross_stack {
    #[test]
    fn wire_and_protocol_approval_kinds_serialize_identically() {
        let pairs = [
            (
                wavecode_wire::ApprovalKind::Exec,
                wavecode_protocol::ApprovalKind::Exec,
            ),
            (
                wavecode_wire::ApprovalKind::Write,
                wavecode_protocol::ApprovalKind::Write,
            ),
        ];
        for (wire, protocol) in pairs {
            let wire_json = serde_json::to_value(wire).unwrap();
            let protocol_json = serde_json::to_value(protocol).unwrap();
            assert_eq!(
                wire_json, protocol_json,
                "ApprovalKind serde forms drifted between wire and protocol"
            );
            // Deserialize each side's form as the other: the tags are
            // interchangeable, not merely equal-by-coincidence.
            let wire_back: wavecode_wire::ApprovalKind =
                serde_json::from_value(protocol_json).unwrap();
            assert_eq!(wire_back, wire);
        }
    }
}
