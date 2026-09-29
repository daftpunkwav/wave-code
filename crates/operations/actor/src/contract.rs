/*!
 * @file SessionContract
 * @description Session contract shared by the composition root and the
 * RPC gateway: assembly inputs, assembly failures, the fallback identity,
 * and the serving surface the RPC servers consume.
 *
 * Responsibilities:
 * - Define the assembly inputs (`AssembleOptions`) and failures
 *   (`SessionError`) both the composition root and gateway name.
 * - Define `SessionSurface`, exactly the per-session abilities the RPC
 *   servers consume (submissions, events, gates, interrupt, mode).
 *
 * This module must not depend on: tools, hooks, models, memory, skills,
 * frontends, or the composition root.
 */

//! Session contract: the vocabulary below both bootstrap and gateway.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use infrastructure_base::InterruptHandle;
use safety_gate::{ApprovalGate, QuestionGate};
use wavecode_wire::{Event, Submission};

use crate::client::SubmitError;

/// Fallback identity block when the caller supplies no base prompt.
pub const DEFAULT_IDENTITY: &str = "You are WaveCode, a precise coding agent.";

/// Assembly failures: hard stops, never silent degradation.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Configuration loading or provider resolution failed.
    #[error(transparent)]
    Config(#[from] wavecode_config::ConfigError),
    /// The provider client itself could not be built (HTTP/TLS init
    /// failure). No model means no session: this aborts assembly instead
    /// of degrading into a session that cannot sample.
    #[error("model client initialization failed: {0}")]
    Model(String),
}

/// Assembly inputs, all caller-owned.
///
/// Version policy (on record, deliberate): this is a **workspace-internal**
/// seam, not a published-stable API — every constructor lives in this repo
/// (harness frontends, gateway servers, and this crate's tests), and each
/// builds the struct by exhaustive literal. Adding a field therefore breaks
/// compilation at every construction site on purpose: the compiler, not a
/// changelog, drives the same-commit review of each caller, and there is no
/// out-of-tree consumer a break could surprise. `#[non_exhaustive]` or a
/// builder would trade that compile-time exhaustiveness for defaults that
/// have no meaningful value here (`cwd`, `identity`), so neither is used;
/// renaming or re-typing a field remains a breaking change for the
/// workspace and is updated in the same commit.
pub struct AssembleOptions {
    /// Config file path; `None` loads the user-level config.
    pub config_path: Option<PathBuf>,
    /// `--model` override winning over the configured model.
    pub model_override: Option<String>,
    /// Provider id override winning over the configured `model_provider`
    /// (e.g. a saved default model that lives on another provider).
    /// Unknown names warn and fall back to the configured provider
    /// instead of failing assembly.
    pub provider_override: Option<String>,
    /// `--permission-mode` override winning over the configured mode.
    pub permission_override: Option<String>,
    /// Reasoning-effort override (saved picker default) winning over the
    /// provider's configured `reasoning_effort`; OpenAI-compatible
    /// providers only.
    pub thinking_override: Option<String>,
    /// Working directory for tools and relative paths.
    pub cwd: PathBuf,
    /// Home directory; `None` degrades memory without failing.
    pub home: Option<PathBuf>,
    /// Identity block prepended to the system prompt.
    pub identity: String,
    /// True for non-interactive drivers: approvals deny openly instead
    /// of parking on a gate nobody answers.
    pub headless: bool,
    /// Seed history as (from_model, text) pairs, e.g. from resume import.
    /// Empty starts a fresh conversation.
    pub initial_history: Vec<(bool, String)>,
    /// `wave`-mode denylist entries (`Bash(pattern)` rule syntax): parsed
    /// into sandbox deny rules so a banned command is refused in every
    /// mode without a prompt. Malformed entries land in the startup
    /// warnings instead of failing assembly. `None` loads the shared
    /// store every surface reads (`~/.wavecode/wave-denylist.json`,
    /// owned by the config layer); `Some` overrides explicitly (tests,
    /// embedders).
    pub wave_denylist: Option<Vec<String>>,
    /// Session id the frontends record the turn journal under, when they
    /// minted one. Journaling itself is a frontend duty today (console
    /// and exec journal their own turns; RPC-served sessions do not
    /// persist) — the id lets compaction append a pointer to that
    /// journal so a post-compaction turn can look up exact earlier
    /// output instead of guessing; `None` (unknown id, no home) keeps
    /// the pointer out.
    #[doc(hidden)]
    pub session_id: Option<String>,
}

/// The per-session surface the RPC servers serve: turn/exec submissions,
/// the wire event stream, the approval and question gates, the shared
/// interrupt, and the effective permission mode.
///
/// Implemented by the composition root's `SessionHandle`; gateway servers
/// take this trait so they never name the handle or its adapters. Async
/// methods take `&mut self` because a session is owned exclusively by the
/// server task driving it (the event receiver is not shareable).
#[async_trait]
pub trait SessionSurface: Send {
    /// Deliver one submission to the session; fails when the session
    /// actor already exited.
    async fn submit(&mut self, submission: Submission) -> Result<(), SubmitError>;

    /// Receive the next wire event; `None` when the session actor exited.
    async fn next_event(&mut self) -> Option<Event>;

    /// Effective permission mode wire name for status displays.
    fn permission_mode(&self) -> &str;

    /// Shared approval gate behind parked decisions.
    fn approvals(&self) -> Arc<ApprovalGate>;

    /// Shared question gate behind parked interactive questions.
    fn questions(&self) -> Arc<QuestionGate>;

    /// Shared interrupt handle for stops and drops.
    fn interrupt(&self) -> InterruptHandle;
}
