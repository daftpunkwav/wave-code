//! Permissions config (the `[permissions]` table): authored allow / deny rule
//! entries. Raw parsing only — entry syntax (`Bash(git *)` / `File(src/**)`)
//! and matching semantics belong to the sandbox crate, which is the single
//! authority on verdicts (same raw-parse discipline as hooks and mcp_servers).
//!
//! User-level only, by design: the planned project layer
//! (`.wavecode/config.toml` inside the working directory) stays unwired for
//! these two tables, because the agent can write that file itself. A
//! repo-scoped allow table would let a session grant its own future
//! self exemptions (a self-approval loop), so widened authority has exactly
//! two sources: a human editing the home config, and a human approving a call
//! ("always allow", persisted as literal grants by `state-persistence::grants`).

/// The `[permissions]` table.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct PermissionsConfig {
    /// Entries that skip approval when they hit (e.g. `Bash(cargo test *)`).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Entries that refuse the call in every permission mode
    /// (e.g. `Bash(rm -rf *)`).
    #[serde(default)]
    pub deny: Vec<String>,
}
