/*!
 * @file Gateway
 * @description RPC surfaces over the session contract: the ACP server
 * (JSON-RPC over stdio), the app server (REST + SSE over loopback HTTP),
 * and the MCP tool server (JSON-RPC over stdio).
 *
 * Responsibilities:
 * - Host the live RPC protocol loops as pure serving skins.
 * - Consume sessions through the `operations-actor` session contract,
 *   never through the composition root's concrete handle.
 * - Take assembly as a caller-supplied seam (production passes the
 *   composition root's `assemble_session`).
 *
 * This module must not depend on: frontends, or the composition root
 * (its tests use it as a dev-dependency for the hermetic assembly seam).
 */

//! Gateway: the RPC protocol surfaces over the session actor.
//!
//! Sessions enter through [`operations_actor::SessionSurface`]; the
//! servers stream wire events and route submissions without knowing how
//! a session was assembled.
pub mod acp;
pub mod app_server;
mod jsonrpc;
pub mod mcp_serve;

/// Shared stubs for this crate's server tests; never compiled into a
/// production build.
#[cfg(test)]
pub(crate) mod test_stubs;
