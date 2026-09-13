/*!
 * @file StatusQueries
 * @description Frontend-facing read views over session-side state.
 *
 * Responsibilities:
 * - Name the on-demand status lookups slash commands need (plan, goal,
 *   snapshots) as display text, never as storage layout.
 * - Keep frontends storage-agnostic: they receive a trait object and
 *   render strings, so backend layout changes cannot silently degrade
 *   them.
 *
 * This module must not depend on: any workspace crate. It is a pure
 * seam; the only implementation lives in the composition root.
 */

//! Read-only status views over session-side state.
//!
//! The composition root (operations-bootstrap) owns the only
//! implementation; frontends receive it as a trait object (via
//! `TuiContext` / `SessionHandle`) and call it when a slash command
//! needs current state, so displays are never stale snapshots taken at
//! assembly time. Queries are total: missing or corrupt state degrades
//! to `None` / an empty list instead of failing the caller.

pub trait StatusQueries: Send + Sync {
    /// Reviewed-plan status text, or `None` when no proposal exists yet.
    fn plan_status(&self) -> Option<String>;
    /// Durable-goal status text, or `None` when no goal exists yet.
    fn goal_status(&self) -> Option<String>;
    /// Snapshot labels, sorted.
    fn snapshot_labels(&self) -> Vec<String>;
    /// One snapshot's display summary, or `None` for an unknown label.
    fn snapshot_summary(&self, label: &str) -> Option<String>;
}
