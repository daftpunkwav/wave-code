//! Self-contained surfaces: local files in, report out.
//!
//! Every submodule here is a single CLI surface that never assembles a
//! session: `doctor` validates local state, `metrics` aggregates the
//! local ledger, `grants` inspects and revokes persisted always-allow
//! grants, and `update_cmd` drives the self-update flow (the only one
//! that talks to the network).

pub(crate) mod doctor;
pub(crate) mod grants;
pub(crate) mod metrics;
pub(crate) mod update_cmd;
