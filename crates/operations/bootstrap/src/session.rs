/*!
 * @file SessionAssembly
 * @description Full-session composition root for the new harness stack.
 *
 * Responsibilities:
 * - Load configuration and resolve the model provider.
 * - Assemble memory, skills, hooks, registry, policy, and adapters.
 * - Build the run loop, child service, native tools, actor, and client.
 * - Collect startup warnings instead of failing on soft degradation.
 *
 * This module must not be depended on by: runtime, state, action, safety,
 * capabilities, or any lower layer. It is the top of the DAG.
 */

//! Session assembly: config file to a live client handle.
//!
//! Two-phase tool wiring: the composite executor enters the run loop
//! first, then child task tools register against the built driver. Hard
//! failures (missing config, provider, credentials) abort assembly;
//! soft degradation (memory, skills, hooks, catalog) warns and continues.

use action_tasks::TaskService;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use operations_actor::{ActorClient, SessionActor};
use runtime_child::ChildRuntime;
use runtime_prompt::{DEFAULT_CATALOG_BUDGET, PromptSlots, build_system};
use runtime_runner::{RunConfig, RunInterrupts, RunLoop, ToolExecutor};
use safety_gate::{ApprovalGate, QuestionGate};
use state_store::Conversation;

use crate::child_service::TurnChildService;
use crate::compactor::ContextCompactor;
use crate::composite::CompositeExecutor;
use crate::gate_adapter::GateApprovalSource;
use crate::hook_adapter::HookAdapter;
use crate::memory_finish::{MemoryFinisher, SessionMemory};
use crate::memory_tool::MemoryWrite;
use crate::model_adapter::ModelAdapter;
use crate::native::{NativeExecutor, NativeTool};
use crate::plan_adapter::TodoPlanTracker;
use crate::policy_adapter::PolicyAdapter;
use crate::prune_adapter::PruningExecutor;
use crate::skill_tool::SkillTool;
use crate::task_tools::{TaskContinueTool, TaskOutputTool, TaskStopTool};
use crate::tool_adapter::ToolAdapter;

/// Approval wait timeout applied to parked decisions.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Tool dispatch rounds per turn; single-sourced from the runner so the
/// loop's ceiling and the assembly default cannot drift apart.
pub use runtime_runner::DEFAULT_MAX_TOOL_ROUNDS;
/// Fallback identity block when the caller supplies no base prompt.
pub const DEFAULT_IDENTITY: &str = "You are WaveCode, a precise coding agent.";

/// Assembly failures: hard stops, never silent degradation.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Configuration loading or provider resolution failed.
    #[error(transparent)]
    Config(#[from] wavecode_config::ConfigError),
}

/// Assembled live session.
pub struct SessionHandle {
    /// Client for submitting operations and streaming events.
    pub client: ActorClient,
    /// Shared approval gate behind parked decisions.
    pub approvals: Arc<ApprovalGate>,
    /// Shared question gate behind parked interactive questions.
    pub questions: Arc<QuestionGate>,
    /// Shared interrupt handle for stops and drops.
    pub interrupt: infrastructure_base::InterruptHandle,
    /// Assembled system prompt (also injected into every turn).
    pub system: String,
    /// Resolved model name for status displays.
    pub model_name: String,
    /// Provider id the primary client resolves through.
    pub provider_id: String,
    /// Effective reasoning-effort level for status displays (OpenAI-
    /// compatible providers only; `None` for budget-driven thinking).
    pub thinking_effort: Option<String>,
    /// Effective permission mode wire name for status displays.
    pub permission_mode: String,
    /// Directly invokable skill names for completion sources.
    pub skill_names: Vec<String>,
    /// Persistent memory index text (empty when memory degraded).
    pub memory_index: String,
    /// Configured MCP servers as one display line each.
    pub mcp_servers: Vec<String>,
    /// Startup warnings in assembly order.
    pub warnings: Vec<String>,
    /// Shared tool registry for late-registered tools (skills, MCP).
    tools_registry: Arc<wavecode_tools::Registry>,
    /// Started runtime plugins owned for the session's lifetime so their
    /// services stay reachable and unload runs on teardown. Service
    /// injection into the run loop is not wired yet; access via
    /// [`SessionHandle::plugins`].
    plugins: runtime_plugin::Registry,
    /// On-demand status views over plan / goal / snapshot state, shared
    /// with frontends so slash commands never touch storage layout.
    pub status: Arc<dyn operations_actor::StatusQueries>,
    /// MCP servers awaiting live connection (sorted by name).
    mcp_pending: Vec<(String, wavecode_config::McpServerRaw)>,
}

impl SessionHandle {
    /// Live-connect pending MCP servers and bridge their tools.
    ///
    /// Replaces the configured-only status lines with live results and
    /// appends degradation warnings; idempotent once connected. Run it
    /// after assembly and before the first turn so forked tools exist
    /// before the model samples.
    ///
    /// Snapshot semantics: the system prompt and the child tool-surface
    /// policy were built during assembly, before this runs, so MCP tools
    /// appear in the live sampling catalog but not in the prompt's tool
    /// list, and children never inherit them.
    pub async fn connect_mcp_servers(&mut self) {
        if self.mcp_pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.mcp_pending);
        let report = crate::mcp_bridge::connect_all(&pending, &self.tools_registry).await;
        self.mcp_servers = report.lines;
        self.warnings.extend(report.warnings);
    }

    /// Started runtime plugins (service map and lifecycle).
    pub fn plugins(&self) -> &runtime_plugin::Registry {
        &self.plugins
    }
}

/// Assembly inputs, all caller-owned.
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
    /// warnings instead of failing assembly.
    pub wave_denylist: Vec<String>,
    /// Session id whose turn journal this assembly will be recorded to, when
    /// the caller already minted one. Compaction appends a pointer to that
    /// journal so a post-compaction turn can look up exact earlier output
    /// instead of guessing; `None` (unknown id, no home) keeps the pointer
    /// out.
    #[doc(hidden)]
    pub session_id: Option<String>,
}

/// Resolve the effective permission mode: CLI override wins over config.
///
/// Unknown values warn and fall back to `Auto` so a typo never locks
/// the session into a mode the policy rejected. Legacy mode names parse
/// onto their successor but warn, so silent behavior drift for old
/// config files is at least visible in the startup warnings.
pub fn resolve_permission_mode(
    config_value: Option<&str>,
    cli_override: Option<&str>,
    warnings: &mut Vec<String>,
) -> wavecode_protocol::PermissionMode {
    if let Some(raw) = cli_override {
        return wavecode_protocol::PermissionMode::parse(raw).unwrap_or_else(|| {
            warnings.push(format!(
                "unrecognized --permission-mode {raw:?}; falling back to auto"
            ));
            wavecode_protocol::PermissionMode::Auto
        });
    }
    config_value
        .and_then(|raw| {
            let parsed = wavecode_protocol::PermissionMode::parse(raw);
            if parsed.is_some() && matches!(raw, "default" | "acceptEdits" | "bypassPermissions") {
                warnings.push(format!(
                    "permission_mode {raw:?} is a legacy name; use plan, auto, or wave"
                ));
            }
            parsed.or_else(|| {
                warnings.push(format!(
                    "unrecognized permission_mode {raw:?}; falling back to auto"
                ));
                None
            })
        })
        .unwrap_or(wavecode_protocol::PermissionMode::Auto)
}

/// The startup permission tables plus what a human needs to know about them.
///
/// One builder serves both consumers: session assembly takes the tables,
/// `wavecode doctor` takes the counts and findings. A separate audit path
/// could drift from what sessions actually load, which is worse than no
/// audit at all.
pub struct Permissions {
    /// Validated allow rules in source order (authored entries, then grants).
    pub allow: Vec<wavecode_sandbox::Rule>,
    /// Validated deny rules in source order (authored entries, then the
    /// session denylist). Deny-first ordering lives in the sandbox, not here.
    pub deny: Vec<wavecode_sandbox::Rule>,
    /// Authored `[permissions] allow` entries that parsed.
    pub authored_allow: usize,
    /// Persisted "always allow" grants that loaded.
    pub persisted_grants: usize,
    /// One line per entry needing a human: invalid syntax, or an allow a
    /// deny rule provably shadows. Assembly surfaces these as startup
    /// warnings; doctor fails on them.
    pub findings: Vec<String>,
}

/// Build the startup permission tables.
///
/// Allow comes from two sources: entries a human authored in the user-level
/// config (`[permissions] allow`) and the literal grants earlier sessions
/// persisted through "always allow". Deny is config plus the session
/// denylist.
///
/// Config entries must already be `Scope(pattern)`; only denylist entries get
/// bare-command Bash scoping (they come from a settings field of command
/// fragments, not rule syntax). Entries validate one at a time: a typo costs
/// only its own line and surfaces as a finding, rather than failing the table
/// it sits in — losing the deny table over a bad allow entry would silently
/// widen authority. Matching semantics stay entirely in the sandbox.
pub fn load_permissions(
    config: &wavecode_config::Config,
    home: Option<&std::path::Path>,
    session_denylist: &[String],
) -> Permissions {
    let mut findings = Vec::new();
    let mut allow_entries = config.permissions.allow.clone();
    let mut grants = 0usize;
    if let Some(home) = home {
        let stored = state_persistence::grants::load_grants(home);
        if stored.malformed > 0 {
            findings.push(format!(
                "{} always-allow grant line(s) in {} are unreadable and were skipped",
                stored.malformed,
                state_persistence::grants::grants_path(home).display()
            ));
        }
        grants = stored.grants.len();
        allow_entries.extend(stored.grants.into_iter().map(|grant| grant.rule));
    }
    let deny_entries: Vec<String> = config
        .permissions
        .deny
        .iter()
        .cloned()
        .chain(session_denylist.iter().map(String::as_str).map(bash_scope))
        .collect();
    let allow = validate_entries(&allow_entries, "allow", &mut findings);
    let deny = validate_entries(&deny_entries, "deny", &mut findings);
    // Deny-first means an allow a deny rule fully covers can never change a
    // verdict: still harmless, but the human writing it expects otherwise.
    for rule in &allow {
        if let Some(ban) = deny.iter().find(|ban| rule.is_covered_by(ban)) {
            findings.push(format!(
                "allow rule {rule} can never apply: {ban} denies everything it matches"
            ));
        }
    }
    let authored_allow = allow.len().saturating_sub(grants);
    Permissions {
        allow,
        deny,
        authored_allow,
        persisted_grants: grants,
        findings,
    }
}

/// Parse entries one at a time, reporting each failure instead of stopping.
fn validate_entries(
    entries: &[String],
    table: &str,
    findings: &mut Vec<String>,
) -> Vec<wavecode_sandbox::Rule> {
    entries
        .iter()
        .filter_map(|entry| {
            wavecode_sandbox::Rule::parse(entry)
                .map_err(|error| findings.push(format!("invalid {table} rule: {error}")))
                .ok()
        })
        .collect()
}

/// One line on the OS confinement a session's shell spawns will get.
///
/// Frontends read this through the composition root so they never name the
/// sandbox crate. The Windows job backend gets its documented gap appended:
/// a status line saying "available" would otherwise overstate what the
/// platform boundary actually holds.
pub fn confinement_status() -> String {
    let backend = wavecode_sandbox::Sandbox::detect_backend();
    let line = wavecode_sandbox::status_line(&backend);
    if backend.backend_name() == "job"
        && matches!(
            backend.enforcement(),
            wavecode_sandbox::EnforcementLevel::Partial
        )
    {
        return format!("{line}; {JOB_GAP}");
    }
    line
}

/// The job backend's gap, phrased for a status line. A test pins every claim
/// here against the sandbox's own documented reason, so the short form
/// cannot drift into overstating what the platform boundary holds.
const JOB_GAP: &str = "process-tree lifetime control and process-count limits only: no filesystem write boundary, no network policy";

/// Bare denylist entries get the Bash scope; already-scoped ones pass
/// through untouched.
fn bash_scope(entry: &str) -> String {
    let trimmed = entry.trim();
    if trimmed.starts_with("Bash(") || trimmed.starts_with("File(") {
        entry.to_string()
    } else {
        format!("Bash({entry})")
    }
}

/// Assemble a live session: config to client handle.
///
/// Must be called inside a tokio runtime (the actor task spawns here).
/// Drives no turns; sampling starts on the first submitted input, so
/// assembly itself needs no network access.
pub fn assemble_session(options: AssembleOptions) -> Result<SessionHandle, SessionError> {
    let AssembleOptions {
        config_path,
        model_override,
        provider_override,
        permission_override,
        thinking_override,
        cwd,
        home,
        identity,
        headless,
        initial_history,
        wave_denylist,
        session_id,
    } = options;
    let mut warnings = Vec::new();

    // 1. Configuration and provider resolution (hard failure surface).
    let config = match config_path {
        Some(path) => wavecode_config::Config::load_from(&path)?,
        None => wavecode_config::Config::load()?,
    };
    // A provider override (saved default model on another provider)
    // degrades to the configured provider with a warning when unknown,
    // matching the permission-mode fallback style: a stale saved default
    // must never brick startup. The reported provider id always names
    // the provider actually resolved to.
    let (provider, api_key, provider_id) = match provider_override.as_deref() {
        Some(name) => match config.resolve_named_provider(name) {
            Ok((provider, api_key)) => (provider, api_key, name.to_string()),
            Err(error) => {
                warnings.push(format!(
                    "provider override {name:?} unusable ({error}); using configured provider"
                ));
                let (provider, api_key) = config.resolve_provider()?;
                (provider, api_key, config.model_provider.clone())
            }
        },
        None => {
            let (provider, api_key) = config.resolve_provider()?;
            (provider, api_key, config.model_provider.clone())
        }
    };
    if is_insecure_http_url(&provider.base_url) {
        warnings.push(format!(
            "base_url uses plain http to a non-loopback host ({}); credentials travel in cleartext",
            provider.base_url
        ));
    }

    // 2. Provider model client; the model-independent remainder lives in
    // `assemble_session_with_model` so tests can inject a stub model.
    // Production behavior is unchanged: this resolves config and builds
    // the provider client, then delegates everything below.
    // Effort display state: only OpenAI-compatible providers carry a
    // switchable string level; Anthropic budgets stay config-only.
    let thinking_effort = if provider.kind.carries_reasoning_effort() {
        thinking_override
            .clone()
            .or_else(|| provider.reasoning_effort.clone())
    } else {
        None
    };
    let model_name = model_override.unwrap_or_else(|| config.model.clone());
    // Primary plus ordered fallbacks share one constructor; each fallback
    // resolves its own provider entry and key, so credentials never cross
    // providers. Unresolvable fallbacks (unknown name, missing key) warn
    // and skip instead of failing the session.
    let primary: Arc<dyn wavecode_llm::ChatModel> = crate::model_adapter::build_chat_model(
        provider,
        api_key,
        &model_name,
        thinking_override.as_deref(),
    );
    let mut chain: Vec<Arc<dyn wavecode_llm::ChatModel>> = vec![primary];
    for name in &provider.fallback_providers {
        match config.resolve_named_provider(name) {
            Ok((fallback_provider, fallback_key)) => {
                chain.push(crate::model_adapter::build_chat_model(
                    fallback_provider,
                    fallback_key,
                    &model_name,
                    thinking_override.as_deref(),
                ))
            }
            Err(error) => warnings.push(format!("skipping fallback provider {name:?}: {error}")),
        }
    }
    // In-layer transient-failure retries (backoff + deadline + auth
    // fail-fast); cross-provider failover stays in FallbackModel, so the
    // two never amplify each other. Without this a single 5xx/429 at
    // request establishment fails the whole turn.
    let chain: Vec<Arc<dyn wavecode_llm::ChatModel>> = chain
        .into_iter()
        .map(|model| {
            Arc::new(wavecode_llm::retry::RetryingModel::new(
                model,
                wavecode_llm::retry::RetryPolicy::default(),
            )) as Arc<dyn wavecode_llm::ChatModel>
        })
        .collect();
    let model: Arc<dyn wavecode_llm::ChatModel> = if chain.len() > 1 {
        Arc::new(crate::model_adapter::FallbackModel::new(chain))
    } else {
        chain
            .into_iter()
            .next()
            .expect("primary model always present")
    };
    let deny_env = provider
        .env_key
        .as_deref()
        .filter(|name| !name.is_empty())
        .map(|name| vec![name.to_owned()])
        .unwrap_or_default();
    // Effective model limits: explicit config wins; OpenAI-compatible
    // providers without explicit limits consult the capability table so
    // DeepSeek-class models sample with their real window instead of the
    // Anthropic-shaped default.
    let (context_window, max_output_tokens) = if provider.kind.carries_reasoning_effort()
        && provider.context_window.is_none()
        && provider.max_output_tokens.is_none()
    {
        let caps = wavecode_llm::ModelCapabilities::resolve_or(
            &model_name,
            provider.context_window(),
            provider.max_output_tokens(),
        );
        (caps.context_window, caps.max_output_tokens)
    } else {
        (provider.context_window(), provider.max_output_tokens())
    };
    Ok(assemble_session_with_model(WithModel {
        session_id: session_id.clone(),
        config,
        model,
        model_name,
        provider_id,
        thinking_effort,
        deny_env,
        context_window,
        max_output_tokens,
        permission_override,
        cwd,
        home,
        identity,
        headless,
        initial_history,
        wave_denylist,
        warnings,
    }))
}

/// Model-independent half of session assembly (test seam carrier).
///
/// Production fills this from config plus the provider-built client in
/// [`assemble_session`]; tests fill it directly around a stub model so
/// prompt paths run hermetically. Changing these fields must not change
/// what production assembles for the same inputs.
pub(crate) struct WithModel {
    /// Full config for hooks, permission mode, and MCP descriptions.
    pub config: wavecode_config::Config,
    /// Chat model: the provider client in production, a stub in tests.
    pub model: Arc<dyn wavecode_llm::ChatModel>,
    /// Effective model name for status displays and sampling.
    pub model_name: String,
    /// Provider id the primary client resolves through.
    pub provider_id: String,
    /// Effective reasoning-effort level for status displays.
    pub thinking_effort: Option<String>,
    /// Env names hidden from tools, resolved from the provider.
    pub deny_env: Vec<String>,
    /// Effective context window, resolved from the provider.
    pub context_window: u64,
    /// Effective output cap, resolved from the provider.
    pub max_output_tokens: u32,
    /// `--permission-mode` override winning over the configured mode.
    pub permission_override: Option<String>,
    /// Working directory for tools and relative paths.
    pub cwd: PathBuf,
    /// Home directory; `None` degrades memory without failing.
    pub home: Option<PathBuf>,
    /// Identity block prepended to the system prompt.
    pub identity: String,
    /// True for non-interactive drivers: approvals deny openly.
    pub headless: bool,
    /// Seed history as (from_model, text) pairs.
    pub initial_history: Vec<(bool, String)>,
    /// `wave`-mode denylist entries (Bash rule syntax).
    pub wave_denylist: Vec<String>,
    /// Session id whose turn journal this assembly records to, when the
    /// caller already minted one (see [`AssembleOptions::session_id`]).
    pub session_id: Option<String>,
    /// Warnings accumulated before the model-independent half.
    pub warnings: Vec<String>,
}

/// Assemble the model-independent remainder: registries to live client.
///
/// Must be called inside a tokio runtime (the actor task spawns here).
/// Shares its body with [`assemble_session`]; the split is purely a
/// seam for hermetic tests, never a behavior fork.
pub(crate) fn assemble_session_with_model(parts: WithModel) -> SessionHandle {
    let WithModel {
        config,
        model,
        model_name,
        provider_id,
        thinking_effort,
        deny_env,
        context_window,
        max_output_tokens,
        permission_override,
        cwd,
        home,
        identity,
        headless,
        initial_history,
        wave_denylist,
        session_id,
        mut warnings,
    } = parts;
    let (registry, todos) = wavecode_tools::Registry::builtin_with_todos();
    // `memory_write` shares the prompt index root so model-written entries
    // surface in the next session without a restart. No home means no
    // memory at all (same gate as `assemble_memory` below).
    if let Some(home) = home.as_deref() {
        registry.register(Arc::new(MemoryWrite::new(
            wavecode_memory::MemoryStore::new(wavecode_memory::MemoryStore::default_root(home)),
        )));
    }
    let registry = Arc::new(registry);

    // 3. Policy: permission mode with explicit fallback, rules unconfigured.
    let permission_mode = resolve_permission_mode(
        config.permission_mode.as_deref(),
        permission_override.as_deref(),
        &mut warnings,
    );
    // Effective wire name for status displays (post-fallback, so the UI
    // never shows a mode the policy rejected). `as_str` is an exhaustive
    // match, so a future PermissionMode variant fails compilation here
    // instead of silently displaying under the wrong mode name.
    let permission_mode_raw = permission_mode.as_str().to_string();
    let permissions = load_permissions(&config, home.as_deref(), &wave_denylist);
    warnings.extend(permissions.findings);
    let sandbox =
        wavecode_sandbox::Sandbox::from_rules(permission_mode, permissions.allow, permissions.deny);

    // 4. Context sources with warn-and-continue degradation.
    let (instruction_memory, memory_index) = assemble_memory(home.as_deref(), &cwd, &mut warnings);
    let (skill_catalog, skill_names, skill_set) =
        assemble_skills(home.as_deref(), &cwd, &mut warnings);
    let hooks = assemble_hooks(&config, &mut warnings);
    let mcp_servers = describe_mcp_servers(&config);

    // 5. Seam adapters (pure wiring, no policy inside).
    let tools = ToolAdapter::new(
        registry.clone(),
        wavecode_tools::ToolCtx {
            cwd: cwd.clone(),
            deny_env,
        },
    );
    let policy = match (home.as_deref(), session_id.as_deref()) {
        (Some(home), Some(session_id)) => PolicyAdapter::new(sandbox, registry.clone())
            .with_grants(crate::grants_sink::GrantSink::new(home, session_id)),
        // No home or no session identity: "always allow" stays in-session,
        // because a grant needs somewhere to live and a writer to attribute.
        _ => PolicyAdapter::new(sandbox, registry.clone()),
    };
    // Tool-result eviction rides the gateway seam: every sample crosses
    // the pass, stored history keeps original payloads.
    let model_adapter = crate::evicting_gateway::EvictingGateway::new(ModelAdapter::new(
        model.clone(),
        model_name.clone(),
        max_output_tokens,
        registry.clone(),
    ));
    let approvals = Arc::new(ApprovalGate::new());
    let questions = Arc::new(QuestionGate::new());
    // The session interrupt feeds the approval/question gates too: a
    // mid-wait Ctrl+C ends a parked decision as Interrupted instead of
    // holding the turn for the full approval timeout.
    let interrupt = infrastructure_base::InterruptHandle::new();
    let gate_source = if headless {
        crate::gate_adapter::Approvals::Headless(crate::gate_adapter::HeadlessDeny)
    } else {
        crate::gate_adapter::Approvals::Gate(GateApprovalSource::new(
            approvals.clone(),
            questions.clone(),
            APPROVAL_TIMEOUT,
            interrupt.clone(),
        ))
    };
    let plans = TodoPlanTracker::new(todos.clone());
    // Compaction footers: the journal pointer needs a home plus the session
    // id; the live plan shares the todo store the todo tool writes.
    let journal = home.as_deref().and_then(|home| {
        let id = session_id.as_deref()?;
        state_persistence::sessions::session_journal_file(home, id)
    });
    let compactor = ContextCompactor::new(model.clone(), model_name.clone())
        .with_footers(crate::compactor::CompactionFooters { journal })
        .with_plans(todos);

    // 6. Run loop behind the shared driver pointer.
    let native = Arc::new(Mutex::new(NativeExecutor::new()));
    // Oversized tool outputs spill to the side-store instead of flowing
    // into history unbounded; the `spill` tool reads them back.
    let executor = PruningExecutor::new(
        CompositeExecutor::new(tools, native.clone(), registry.clone()),
        wavecode_context::spill::SpillStore::new(
            wavecode_context::spill::default_spill_store_root(),
        ),
    );
    // One clock read seeds both the environment paragraph and the loop's
    // midnight-rollover check, so the two can never disagree about the date.
    let session_now = std::time::SystemTime::now();
    let session_date = crate::environment::format_date(session_now);
    let worker = Arc::new(RunLoop::new(
        executor,
        policy,
        HookAdapter::new(hooks, cwd.clone()),
        model_adapter,
        gate_source,
        plans,
        compactor,
        RunConfig {
            model_name: model_name.clone(),
            context_window,
            max_output_tokens,
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
            max_continuations: runtime_runner::MAX_CONTINUATIONS,
            max_plan_nudges: runtime_runner::MAX_PLAN_NUDGES,
            max_stop_blocks: runtime_runner::MAX_STOP_BLOCKS,
            max_reactive_compacts: runtime_runner::MAX_REACTIVE_COMPACTS,
            max_repeat_streak: runtime_runner::MAX_REPEAT_STREAK,
            max_wire_images: runtime_runner::MAX_WIRE_IMAGES,
            session_date: Some(session_date),
        },
        interrupt.clone(),
    ));
    // Per-run tool allowlist for fork-scoped skill surfaces; the handle
    // is Arc-backed, so grabbing it before `worker` moves is enough.
    let run_allowlist = worker.run_allowlist();
    // Run-scoped interrupt registry: each child turn gets its own handle,
    // so a `task_stop` bridges into that child only and never flips the
    // session-wide flag the parent turn and siblings share.
    let run_interrupts: RunInterrupts = worker.run_interrupts();
    let driver = {
        // Session-end memory extraction rides the driver seam: the actor
        // calls `end_session` with the final transcript on teardown. No
        // home means no memory at all (same gate as the write tool above
        // and `assemble_memory` below).
        let finisher = home.as_deref().map(|root| {
            MemoryFinisher::new(
                model.clone(),
                model_name.clone(),
                wavecode_memory::MemoryStore::new(wavecode_memory::MemoryStore::default_root(root)),
            )
        });
        Arc::new(SessionMemory::new(worker, finisher))
    };

    // 7. Child task tools register against the built driver (phase two).
    // The system prompt and tool-surface policy attach after assembly
    // completes (they need the full catalog); spawns only start once the
    // actor runs, so every child observes them.
    let children = Arc::new(ChildRuntime::new());
    let mut child_service = TurnChildService::new(
        driver.clone() as Arc<dyn runtime_runner::TurnDriver>,
        children.clone(),
        run_allowlist,
        run_interrupts,
    );
    // Subagent logs: each child task appends its own turns beside the
    // session's journal, so a long run can explain what a child actually did
    // instead of keeping only its returned summary.
    if let (Some(home), Some(parent)) = (home.clone(), session_id.clone()) {
        child_service = child_service.with_child_journal(home, parent);
    }
    let tasks = Arc::new(child_service);
    // Runtime plugins (service injection + middleware lifecycle): manifest
    // discovery warns-and-skips invalid plugins and never fails assembly.
    // The handle owns the started registry for the session's lifetime, so
    // plugins unload (reverse start order) when the session tears down.
    let runtime_plugins = runtime_plugin::load_and_start(home.as_deref(), &mut warnings);
    register_child_tools(&native, tasks.clone());

    // The `skill` model tool needs the child service, which only exists
    // after the driver is built: late registration on the shared registry
    // makes it visible to the executor, policy, and model adapters live.
    // `task_output` / `task_stop` / `task_continue` ride the same handle so
    // the model can observe, stop, and continue what task and skill forked.
    let task_service = tasks.clone() as Arc<dyn action_tasks::TaskService>;
    registry.register(Arc::new(SkillTool::new(skill_set, task_service.clone())));
    registry.register(Arc::new(TaskOutputTool::new(task_service.clone())));
    registry.register(Arc::new(TaskStopTool::new(task_service.clone())));
    registry.register(Arc::new(TaskContinueTool::new(task_service.clone())));
    // `ask_user` parks on the shared question gate: the sandbox routes valid
    // payloads to the question flow before mode policy, so this tool's body
    // only runs for schema errors or bypassed gates.
    registry.register(Arc::new(crate::ask_user_tool::AskUserTool));
    // `task` is the free-form delegation surface (named agent definitions
    // resolve per call from the tool's cwd, so registration needs only the
    // shared child handle).
    registry.register(Arc::new(crate::agent_task_tool::TaskTool::new(
        task_service.clone(),
    )));
    // LSP tools get one shared, registry-backed providers handle: pooled
    // servers survive across calls, and diagnostics pushed while any LSP
    // tool call is in flight land in the same store `lsp_diagnostics`
    // reads back. Re-registration replaces the builtin no-registry tools.
    let lsp_providers = Arc::new(wavecode_tools::LspProviders::new(cwd.clone()));
    registry.register(Arc::new(wavecode_tools::DocumentSymbols::with_providers(
        lsp_providers.clone(),
    )));
    registry.register(Arc::new(wavecode_tools::GotoDefinition::with_providers(
        lsp_providers.clone(),
    )));
    registry.register(Arc::new(wavecode_tools::Hover::with_providers(
        lsp_providers.clone(),
    )));
    registry.register(Arc::new(wavecode_tools::FindReferences::with_providers(
        lsp_providers.clone(),
    )));
    registry.register(Arc::new(wavecode_tools::LspDiagnostics::with_providers(
        lsp_providers,
    )));
    // Workflow engine (fan-out DAG runs plus Ralph loops) and durable
    // schedules ride the same child handle: schedules persist under
    // `<home>/.wavecode/schedule.json` so cron entries survive restarts.
    // Restored entries each owe one immediate fire covering the missed
    // window before the cadence resumes; running jobs are never persisted,
    // so a previous process's in-flight work reads as interrupted.
    let (scheduler_state, schedule_catchup) = match home.as_deref() {
        Some(home_dir) => runtime_scheduler::Scheduler::load_or_default(home_dir),
        None => (runtime_scheduler::Scheduler::default(), 0),
    };
    if schedule_catchup > 0 {
        warnings.push(format!(
            "schedule missed {schedule_catchup} fire(s) while away; each restored entry fires once, then the cadence resumes"
        ));
    }
    let scheduler = Arc::new(Mutex::new(scheduler_state));
    registry.register(Arc::new(crate::workflow_tools::WorkflowRunTool::new(
        task_service.clone(),
    )));
    registry.register(Arc::new(crate::workflow_tools::RalphRunTool::new(
        task_service.clone(),
    )));
    registry.register(Arc::new(crate::workflow_tools::ScheduleTool::new(
        scheduler,
    )));
    // Background shell jobs for long work that must not block the turn.
    // They file completion notices on the same child runtime the actor
    // drains, so results re-enter turns exactly like child tasks.
    // `job_spawn` writes, `job_wait`/`job_output` only observe, and
    // `job_cancel` is destructive (approval-gated).
    let jobs = Arc::new(action_jobs::JobService::new(children.clone()));
    registry.register(Arc::new(crate::job_tools::JobSpawnTool::new(jobs.clone())));
    registry.register(Arc::new(crate::job_tools::JobWaitTool::new(jobs.clone())));
    registry.register(Arc::new(crate::job_tools::JobCancelTool::new(jobs.clone())));
    registry.register(Arc::new(crate::job_tools::JobOutputTool::new(jobs)));
    // File-content snapshots ride the same late handle: the store root
    // derives from the session memory root (`<home>/.wavecode/memories`
    // -> `<home>/.wavecode/snapshots`) so injected roots stay hermetic.
    // `snapshot` is read-only; `restore` is destructive (approval-gated).
    let snapshot_memory_root = home
        .as_deref()
        .map(wavecode_memory::MemoryStore::default_root);
    let snapshot_root =
        wavecode_tools::snapshot::snapshot_store_root_for_session(snapshot_memory_root.as_deref());
    // The frontend status views read the same root the tools write, so
    // /snapshots and /rewind never drift from what `snapshot` produced.
    let status_snapshot_root = snapshot_root.clone();
    registry.register(Arc::new(wavecode_tools::snapshot::SnapshotTool::new(
        snapshot_root.clone(),
    )));
    registry.register(Arc::new(wavecode_tools::snapshot::RestoreTool::new(
        snapshot_root,
    )));
    // Reviewed plan mode (explore -> present -> approve -> execute) rides
    // the same late handle: the store path derives from home
    // (`<home>/.wavecode/plans/<session>.json`) so resume in the same home
    // reopens the same plan. Corrupt content warns and starts empty, never
    // a hard stop. The tool mutates plan state, not the repo, and assembly
    // classifies it as in-session state (no approval gate in any mode).
    let (plan_state, plan_warning) = crate::plan_tools::load_for_session(
        home.as_deref(),
        crate::plan_tools::DEFAULT_PLAN_SESSION_ID,
    );
    if let Some(warning) = plan_warning {
        warnings.push(warning);
    }
    let plan_store = Arc::new(crate::plan_tools::PlanStore::new(
        plan_state,
        home.as_deref(),
        crate::plan_tools::DEFAULT_PLAN_SESSION_ID,
    ));
    registry.register(Arc::new(crate::plan_tools::PlanTool::new(plan_store)));

    // Durable goal service (persisted per-session objective with CAS and a
    // tools-only round driver): the store path derives from home
    // (`<home>/.wavecode/goals/<session>.json`) so resume in the same home
    // reopens the same goal. Corrupt content warns and starts empty, never
    // a hard stop. The tool mutates goal state, not the repo, and assembly
    // classifies it as in-session state (no approval gate in any mode).
    // The driver has no loop hook yet: the model calls the tick action
    // once per round.
    let (goal_state, goal_warning) = crate::goal_tools::load_for_session(
        home.as_deref(),
        crate::goal_tools::DEFAULT_GOAL_SESSION_ID,
    );
    if let Some(warning) = goal_warning {
        warnings.push(warning);
    }
    let goal_store = Arc::new(crate::goal_tools::GoalStore::new(
        goal_state,
        home.as_deref(),
        crate::goal_tools::DEFAULT_GOAL_SESSION_ID,
    ));
    registry.register(Arc::new(crate::goal_tools::GoalTool::new(goal_store)));

    // 8. System prompt from assembled slots plus the live tool catalog.
    // Plan mode adds a soft guidance paragraph: read-only exploration,
    // prefer proposing via the plan tool, answering directly is fine.
    let tool_names: Vec<String> = registry
        .specs()
        .into_iter()
        .map(|spec| spec.name.clone())
        .collect();
    let mut instructions = instruction_memory;
    if permission_mode == wavecode_protocol::PermissionMode::Plan {
        instructions.push_str(
            "\nYou are in plan mode: only read-only tools are available. \
             Explore first, then propose your approach with the plan tool \
             so the user can approve it; when the request only needs an \
             answer, simply answer. Do not attempt to modify anything.",
        );
    }
    let system = build_system(&PromptSlots {
        identity,
        instructions,
        memory_index: memory_index.clone(),
        skill_catalog,
        tool_note: format!("Available tools: {}", tool_names.join(", ")),
        environment: crate::environment::describe(&cwd, session_now),
        summary: String::new(),
    });

    // Child turns ride the fully assembled registry: attach the system
    // prompt and the tool-surface policy now that both exist. Children
    // spawn through the same service, so without the forbidden
    // subtraction a child could re-fork `task`/`skill`/workflow spawns
    // and the runtime depth cap would never see the nested generations;
    // read-only profiles additionally narrow to the read-only subset.
    // The forbidden list is a hand-maintained copy of the spawn-tool
    // names (they live in their tool types), so drift fails loudly here
    // instead of silently re-opening the recursion hole. The check runs
    // against registry plus native names — snapshotted here, after every
    // late registration, so the native-only `child_spawn` is visible —
    // and every listed name must exist somewhere in the assembled
    // catalog.
    let native_names: Vec<String> = native
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .available_tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    for name in CHILD_FORBIDDEN_TOOLS {
        let in_registry = tool_names.iter().any(|offered| offered == name);
        let in_native = native_names.iter().any(|offered| offered == name);
        if !in_registry && !in_native {
            warnings.push(format!(
                "child-forbidden tool '{name}' is not in the tool catalog; \
                 update CHILD_FORBIDDEN_TOOLS"
            ));
        }
    }
    tasks.set_system(system.clone());
    tasks.set_surface(crate::child_service::ChildSurface {
        all: tool_names.iter().cloned().collect(),
        read_only: registry
            .read_only_subset()
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect(),
        forbidden: CHILD_FORBIDDEN_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    });

    // 9. Actor task and client handle, sharing the child runtime with
    // the task tools so completions re-enter turns. Imported history
    // seeds the conversation before the first turn snapshot.
    let system_for_actor = system.clone();
    let mut conv = Conversation::new();
    for (from_model, text) in &initial_history {
        conv.push(
            if *from_model {
                state_store::Role::Assistant
            } else {
                state_store::Role::User
            },
            text.clone(),
        );
    }
    let client = SessionActor::spawn(
        driver,
        conv,
        children,
        approvals.clone(),
        questions.clone(),
        interrupt.clone(),
        system_for_actor,
    );
    // A session that journals also measures: every finished turn appends one
    // ledger sample keyed by the same id `resume` takes. Without a journal id
    // (the in-memory REPL) there is nothing to attribute samples to.
    let client = match (session_id.as_deref(), home.as_ref()) {
        (Some(id), Some(home)) => client.with_tap(crate::metrics_tap::metrics_tap(home, id)),
        _ => client,
    };

    let mut mcp_pending: Vec<(String, wavecode_config::McpServerRaw)> =
        config.mcp_servers.clone().into_iter().collect();
    mcp_pending.sort_by(|a, b| a.0.cmp(&b.0));

    // Frontend status views share the tool-side snapshot root so /plan,
    // /goal, /snapshots, and /rewind read exactly what the tools wrote.
    let status =
        crate::status_queries::SessionStatus::new(home.as_deref(), status_snapshot_root).shared();

    SessionHandle {
        client,
        approvals,
        questions,
        interrupt,
        system,
        model_name,
        provider_id,
        thinking_effort,
        permission_mode: permission_mode_raw,
        skill_names,
        memory_index,
        mcp_servers,
        warnings,
        tools_registry: registry.clone(),
        plugins: runtime_plugins,
        status,
        mcp_pending,
    }
}

/// Child-spawning tool surfaces a child run may never invoke. Children
/// share the session registry, so the child service subtracts these from
/// every spawn's surface — the runtime depth cap only stays meaningful
/// if children cannot re-fork.
const CHILD_FORBIDDEN_TOOLS: [&str; 6] = [
    "task",
    "skill",
    "task_continue",
    "child_spawn",
    "workflow_run",
    "ralph_run",
];

/// Register child task tools against a task service.
fn register_child_tools(native: &Arc<Mutex<NativeExecutor>>, tasks: Arc<TurnChildService>) {
    let spawn_service = tasks.clone();
    native
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(NativeTool {
            name: "child_spawn".to_string(),
            description: "Spawn a background child task working on the given input.".to_string(),
            read_only: false,
            destructive: false,
            handler: Arc::new(move |input| {
                // Native handlers see no run context, so parent_run_id
                // stays empty here (correlation via lineage only).
                let kind = match input.get("kind").and_then(|v| v.as_str()) {
                    Some("readonly") => action_tasks::TaskKind::ReadOnly,
                    _ => action_tasks::TaskKind::Standard,
                };
                match input.get("input").and_then(|v| v.as_str()) {
                    Some(text) => {
                        let id = spawn_service.spawn(action_tasks::TaskRequest {
                            kind,
                            input: text.to_string(),
                            parent_run_id: String::new(),
                            allowed_tools: Vec::new(),
                            depth: 0,
                            parent: None,
                        });
                        (id, false)
                    }
                    None => ("missing required field: input".to_string(), true),
                }
            }),
        });
    let query_service = tasks.clone();
    native
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(NativeTool {
            name: "child_query".to_string(),
            description: "Query a child task by id for its state and outcome.".to_string(),
            read_only: true,
            destructive: false,
            handler: Arc::new(
                move |input| match input.get("id").and_then(|v| v.as_str()) {
                    Some(id) => match query_service.query(id) {
                        Some(info) => (format!("{info:?}"), false),
                        None => (format!("unknown child task: {id}"), true),
                    },
                    None => ("missing required field: id".to_string(), true),
                },
            ),
        });
    native
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(NativeTool {
            name: "child_stop".to_string(),
            description: "Request a child task stop by id.".to_string(),
            read_only: false,
            destructive: false,
            handler: Arc::new(
                move |input| match input.get("id").and_then(|v| v.as_str()) {
                    Some(id) => (format!("stopped: {}", tasks.stop(id)), false),
                    None => ("missing required field: id".to_string(), true),
                },
            ),
        });
}

/// Assemble instruction memory and the persistent index with degradation.
fn assemble_memory(
    home: Option<&Path>,
    cwd: &Path,
    warnings: &mut Vec<String>,
) -> (String, String) {
    let Some(home) = home else {
        warnings.push("home directory unavailable; memory disabled".to_string());
        return (String::new(), String::new());
    };
    let instruction = wavecode_memory::collect(Some(home), cwd);
    let store_root = wavecode_memory::MemoryStore::default_root(home);
    let index = wavecode_memory::MemoryStore::new(store_root)
        .read_index()
        .unwrap_or_else(|e| {
            warnings.push(format!(
                "memory index unreadable, continuing without it: {e}"
            ));
            String::new()
        });
    (instruction.combined, index)
}

/// Assemble the skill catalog text with discovery warnings preserved,
/// returning catalog text, directly invokable names for UIs, and the set
/// itself for the `skill` tool backend.
fn assemble_skills(
    home: Option<&Path>,
    cwd: &Path,
    warnings: &mut Vec<String>,
) -> (String, Vec<String>, Arc<wavecode_skills::SkillSet>) {
    let roots = wavecode_skills::standard_roots(None, home, cwd);
    let discovery = wavecode_skills::discover(&roots);
    warnings.extend(discovery.warnings.iter().cloned());
    let names: Vec<String> = discovery
        .set
        .iter()
        .filter(|skill| skill.meta.user_invocable)
        .map(|skill| skill.name.clone())
        .collect();
    (
        discovery.set.catalog(DEFAULT_CATALOG_BUDGET),
        names,
        Arc::new(discovery.set),
    )
}

/// Convert raw config hooks into an engine, warning past bad entries.
fn assemble_hooks(
    config: &wavecode_config::Config,
    warnings: &mut Vec<String>,
) -> Arc<wavecode_hooks::HookEngine> {
    let mut defs = HashMap::new();
    for (point_name, ruleset) in &config.hooks {
        match wavecode_hooks::HookEventPoint::parse(point_name) {
            None => warnings.push(format!("unknown hook point {point_name:?}; skipped")),
            Some(point) => {
                defs.insert(
                    point,
                    ruleset
                        .rules()
                        .iter()
                        .map(|rule| wavecode_hooks::HookDef {
                            matcher: rule.matcher.clone(),
                            command: rule.command.clone(),
                            timeout_ms: rule
                                .timeout_ms
                                .unwrap_or(wavecode_hooks::DEFAULT_TIMEOUT_MS),
                            once: rule.once.unwrap_or(false),
                        })
                        .collect(),
                );
            }
        }
    }
    Arc::new(wavecode_hooks::HookEngine::new(defs))
}

/// Describe configured MCP servers as one display line each.
///
/// stdio entries show their command, HTTP entries their URL; entries
/// with neither are reported, never silently dropped.
fn describe_mcp_servers(config: &wavecode_config::Config) -> Vec<String> {
    let mut lines: Vec<String> = config
        .mcp_servers
        .iter()
        .map(|(name, server)| {
            if let Some(command) = &server.command {
                format!("{name} (stdio: {command})")
            } else if let Some(url) = &server.url {
                format!("{name} (http: {url})")
            } else {
                format!("{name} (unconfigured)")
            }
        })
        .collect();
    lines.sort();
    lines
}

/// True for http URLs outside loopback hosts (credentials at risk).
fn is_insecure_http_url(base_url: &str) -> bool {
    // URL schemes are case-insensitive (RFC 3986); match `http://` in any
    // casing so `HTTP://evil.example.com` cannot bypass the warning.
    let rest = match base_url.get(.."http://".len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case("http://") => &base_url["http://".len()..],
        _ => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority
        .strip_prefix('[')
        .and_then(|rest| rest.split(']').next())
    {
        Some(v6) => v6,
        None => authority.split(':').next().unwrap_or_default(),
    };
    !matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HeadlessDeny;
    use wavecode_llm::{ChatModel, ChatRequest, EventStream, StreamEvent, Usage};
    use wavecode_wire::{Op, Submission};

    const CONFIG: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
api_key = "k-inline"
"#;

    /// Load a config built on the default fixture text.
    fn config_with(tail: &str) -> wavecode_config::Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, format!("{CONFIG}{tail}")).unwrap();
        wavecode_config::Config::load_from(&path).unwrap()
    }

    fn displayed(rules: &[wavecode_sandbox::Rule]) -> Vec<String> {
        rules.iter().map(|rule| rule.to_string()).collect()
    }

    /// The one-line status copy must not claim more (or less) than the
    /// backend's own documented limitation.
    #[test]
    fn job_gap_summary_agrees_with_the_backend_reason() {
        for phrase in [
            "process-tree lifetime control",
            "no filesystem write boundary",
            "no network policy",
        ] {
            assert!(JOB_GAP.contains(phrase), "summary omits {phrase:?}");
            assert!(
                wavecode_sandbox::WINDOWS_UNAVAILABLE_REASON.contains(phrase),
                "the backend no longer documents {phrase:?}: update the summary"
            );
        }
    }

    /// `[permissions]` reaches the tables, and bare denylist entries keep
    /// their historical Bash scoping.
    #[test]
    fn authored_rules_load_into_both_tables() {
        let config = config_with(
            r#"
[permissions]
allow = ["Bash(git *)"]
deny = ["File(.env)"]
"#,
        );
        let perms = load_permissions(&config, None, &["rm -rf".to_string()]);
        assert_eq!(displayed(&perms.allow), ["Bash(git *)"]);
        assert_eq!(displayed(&perms.deny), ["File(.env)", "Bash(rm -rf)"]);
        assert_eq!((perms.authored_allow, perms.persisted_grants), (1, 0));
        assert!(perms.findings.is_empty(), "{:?}", perms.findings);
    }

    /// Grants a previous session persisted are startup allow rules, and the
    /// report tells authored entries apart from clicked ones.
    #[test]
    fn persisted_grants_join_the_allow_table() {
        let dir = tempfile::tempdir().unwrap();
        let grant = state_persistence::grants::Grant {
            rule: "Bash(cargo test --locked)".to_string(),
            tool: "shell".to_string(),
            granted_at_secs: 1,
            session: "older".to_string(),
        };
        state_persistence::grants::add_grant(dir.path(), &grant).unwrap();
        let config = config_with(
            r#"
[permissions]
allow = ["Bash(git status)"]
"#,
        );
        let perms = load_permissions(&config, Some(dir.path()), &[]);
        assert_eq!(
            displayed(&perms.allow),
            ["Bash(git status)", "Bash(cargo test --locked)"]
        );
        assert_eq!((perms.authored_allow, perms.persisted_grants), (1, 1));
        assert!(perms.findings.is_empty(), "{:?}", perms.findings);
    }

    /// The whole point of per-entry validation: one typo must never cost the
    /// other table, and never fail silently. A bare config entry is a typo
    /// too — only the denylist gets Bash scoping.
    #[test]
    fn an_invalid_entry_costs_only_itself() {
        let config = config_with(
            r#"
[permissions]
allow = ["Bash(git *)", "not a rule"]
deny = ["also not a rule", "File(.env)"]
"#,
        );
        let perms = load_permissions(&config, None, &[]);
        assert_eq!(displayed(&perms.allow), ["Bash(git *)"]);
        assert_eq!(displayed(&perms.deny), ["File(.env)"]);
        assert_eq!(perms.findings.len(), 2, "{:?}", perms.findings);
        assert!(perms.findings.iter().all(|f| f.starts_with("invalid")));
    }

    /// Deny-first makes a fully shadowed allow dead; doctor says so instead
    /// of letting the human wonder why the prompt never went away.
    #[test]
    fn shadowed_allow_rules_are_reported_as_dead() {
        let config = config_with(
            r#"
[permissions]
allow = ["Bash(git commit *)", "Bash(git status)"]
deny = ["Bash(git *)"]
"#,
        );
        let perms = load_permissions(&config, None, &[]);
        assert_eq!(perms.allow.len(), 2, "dead allows stay loaded");
        assert_eq!(
            perms
                .findings
                .iter()
                .filter(|finding| finding.contains("can never apply"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn assembly_builds_a_live_client_offline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let mut handle = assemble_session(AssembleOptions {
            config_path: Some(path),
            model_override: None,
            provider_override: None,
            permission_override: None,
            thinking_override: None,
            wave_denylist: Vec::new(),
            cwd: dir.path().to_path_buf(),
            home: None,
            identity: DEFAULT_IDENTITY.to_string(),
            headless: true,
            initial_history: Vec::new(),
            session_id: None,
        })
        .unwrap();
        // Memory degrades with warnings instead of failing assembly.
        assert!(
            handle
                .warnings
                .iter()
                .any(|w| w.contains("memory disabled"))
        );
        assert!(handle.system.contains("WaveCode"));
        assert!(handle.system.contains("Available tools:"));
        assert!(handle.memory_index.is_empty());
        assert!(handle.mcp_servers.is_empty());
        // The forbidden-spawn-tool list matches the assembled catalog: no
        // drift warning on the happy path (this assembles a full session
        // with the child surface attached).
        assert!(
            !handle
                .warnings
                .iter()
                .any(|w| w.contains("child-forbidden")),
            "unexpected drift warning: {:?}",
            handle.warnings
        );
        // The client submits without network access; shutdown closes cleanly.
        handle
            .client
            .submit(Submission {
                id: "s1".to_string(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        assert!(handle.client.next_event().await.is_none());
    }

    /// Context budget gate: the advertised tool catalog is paid for on every
    /// single request, so its size has to be a number someone chose. Run
    /// `cargo test -p operations-bootstrap catalog -- --nocapture` to print
    /// the per-tool breakdown; raise the ceiling deliberately, never silently.
    #[tokio::test]
    async fn advertised_tool_catalog_stays_within_its_budget() {
        /// Ceiling in tokens for the always-on catalog, measured with the
        /// same CJK-aware estimator the context budget uses.
        const CATALOG_TOKEN_BUDGET: u64 = 9_000;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let handle = assemble_session(AssembleOptions {
            config_path: Some(path),
            model_override: None,
            provider_override: None,
            permission_override: None,
            thinking_override: None,
            wave_denylist: Vec::new(),
            cwd: dir.path().to_path_buf(),
            home: Some(dir.path().to_path_buf()),
            identity: DEFAULT_IDENTITY.to_string(),
            headless: true,
            initial_history: Vec::new(),
            session_id: None,
        })
        .unwrap();

        // What the model is actually offered: `ModelAdapter::tools` resolves
        // every advertised name through this registry, so the registry's
        // specs are the shipped catalog (no MCP servers configured here).
        let specs = handle.tools_registry.specs();
        let catalog_json = serde_json::to_string(&specs).unwrap();
        let tokens = state_store::estimate_tokens(&catalog_json);
        let json_chars = catalog_json.len() as u64;
        let system_tokens = state_store::estimate_tokens(&handle.system);

        let mut rows: Vec<(usize, String)> = specs
            .iter()
            .map(|spec| {
                (
                    serde_json::to_string(spec).unwrap().len(),
                    spec.name.clone(),
                )
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.0));
        println!(
            "CATALOG tools={} json_chars={} est_tokens={} budget={CATALOG_TOKEN_BUDGET} system_tokens={}",
            specs.len(),
            json_chars,
            tokens,
            system_tokens
        );
        for (chars, name) in &rows {
            println!("  {name}	{chars}");
        }

        assert_eq!(
            specs.len(),
            rows.len(),
            "tool names must be unique: a duplicate would advertise twice"
        );
        assert!(
            tokens <= CATALOG_TOKEN_BUDGET,
            "advertised catalog costs {tokens} tokens, over the {CATALOG_TOKEN_BUDGET} budget"
        );
    }

    /// The actor's driver reaches the run loop through an `Arc<T>`
    /// blanket impl; that impl must forward the live-switch seams or
    /// `/model` and `/effort` silently reject in every production
    /// session (regression guard: both forwards once went missing).
    #[tokio::test]
    async fn live_model_and_thinking_switches_survive_the_arc_wrapper() {
        // OpenAI-compatible provider so `set_thinking` has a mutable
        // effort behind it (Anthropic budgets stay config-driven and
        // legitimately reject).
        const OPENAI_CONFIG: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "open-ai-compatible"
base_url = "https://api.example.com/v1"
api_key = "k-inline"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, OPENAI_CONFIG).unwrap();
        let mut handle = assemble_session(AssembleOptions {
            config_path: Some(path),
            model_override: None,
            provider_override: None,
            permission_override: None,
            thinking_override: None,
            wave_denylist: Vec::new(),
            cwd: dir.path().to_path_buf(),
            home: None,
            identity: DEFAULT_IDENTITY.to_string(),
            headless: true,
            initial_history: Vec::new(),
            session_id: None,
        })
        .unwrap();
        handle
            .client
            .submit(Submission {
                id: "s-model".to_string(),
                op: Op::SetModel {
                    name: "m2".to_string(),
                },
            })
            .await
            .unwrap();
        handle
            .client
            .submit(Submission {
                id: "s-think".to_string(),
                op: Op::SetThinking {
                    effort: "low".to_string(),
                },
            })
            .await
            .unwrap();
        handle
            .client
            .submit(Submission {
                id: "s-end".to_string(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        let mut rejections = Vec::new();
        while let Some(event) = handle.client.next_event().await {
            if let wavecode_wire::EventMsg::Warning { message } = event.msg {
                rejections.push(message);
            }
        }
        assert!(
            rejections.is_empty(),
            "live switches rejected through the Arc driver: {rejections:?}"
        );
    }

    /// A stale provider override degrades to the configured provider with
    /// a warning, and the reported provider id must name the provider
    /// actually resolved to — not the dead override name.
    #[tokio::test]
    async fn stale_provider_override_falls_back_and_reports_configured_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let handle = assemble_session(AssembleOptions {
            config_path: Some(path),
            model_override: None,
            provider_override: Some("ghost".to_string()),
            permission_override: None,
            thinking_override: None,
            wave_denylist: Vec::new(),
            cwd: dir.path().to_path_buf(),
            home: None,
            identity: DEFAULT_IDENTITY.to_string(),
            headless: true,
            initial_history: Vec::new(),
            session_id: None,
        })
        .unwrap();
        assert!(
            handle
                .warnings
                .iter()
                .any(|w| w.contains("provider override")),
            "fallback must warn: {:?}",
            handle.warnings
        );
        assert_eq!(handle.provider_id, "p1");
        handle
            .client
            .submit(Submission {
                id: "s-end".to_string(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
    }

    #[test]
    fn cli_permission_override_wins_over_config() {
        let mut warnings = Vec::new();
        let mode = resolve_permission_mode(Some("plan"), Some("auto"), &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_permission_values_warn_and_fall_back() {
        let mut warnings = Vec::new();
        let mode = resolve_permission_mode(Some("plan"), Some("nope"), &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
        assert!(warnings.iter().any(|w| w.contains("--permission-mode")));
        warnings.clear();
        let mode = resolve_permission_mode(Some("nope"), None, &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
        assert!(warnings.iter().any(|w| w.contains("permission_mode")));
    }

    #[test]
    fn config_permission_used_without_override() {
        let mut warnings = Vec::new();
        let mode = resolve_permission_mode(Some("guarded"), None, &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
        assert!(warnings.is_empty());
        let mode = resolve_permission_mode(None, None, &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
        assert!(warnings.is_empty());
    }

    #[test]
    fn legacy_mode_names_migrate_with_a_visible_warning() {
        let mut warnings = Vec::new();
        let mode = resolve_permission_mode(Some("acceptEdits"), None, &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
        assert!(warnings.iter().any(|w| w.contains("legacy name")));
        warnings.clear();
        let mode = resolve_permission_mode(Some("bypassPermissions"), None, &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Wave);
        assert!(warnings.iter().any(|w| w.contains("legacy name")));
    }

    #[test]
    fn insecure_urls_detected_without_false_loopback_positives() {
        assert!(is_insecure_http_url("http://api.example.com"));
        assert!(!is_insecure_http_url("https://api.example.com"));
        assert!(!is_insecure_http_url("http://127.0.0.1:8080"));
        assert!(!is_insecure_http_url("http://localhost:3000/v1"));
        assert!(!is_insecure_http_url("http://[::1]:9000"));
        assert!(is_insecure_http_url("http://127.0.0.1.evil.example.com"));
        // URL schemes are case-insensitive: uppercase HTTP must warn too.
        assert!(is_insecure_http_url("HTTP://api.example.com"));
        assert!(is_insecure_http_url("Http://api.example.com"));
        assert!(!is_insecure_http_url("HTTP://localhost:3000/v1"));
    }

    /// Scripted model replaying canned streams in order; exhausted scripts
    /// degrade to an empty completion so the loop always terminates.
    struct ScriptedModel {
        scripts: std::sync::Mutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for ScriptedModel {
        async fn stream(&self, _req: ChatRequest) -> wavecode_llm::Result<EventStream> {
            let script = self
                .scripts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_else(|| {
                    vec![StreamEvent::MessageComplete {
                        stop_reason: "end_turn".to_string(),
                        usage: Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                            ..Usage::default()
                        },
                    }]
                });
            Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
        }
    }

    fn write_turn_script() -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta {
                text: "working".to_string(),
            },
            StreamEvent::ToolUseBegin {
                id: "c1".to_string(),
                name: "write".to_string(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"hello.txt","content":"wavecode-smoke-ok"}"#.to_string(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Usage::default()
                },
            },
        ]
    }

    fn done_script() -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta {
                text: "done".to_string(),
            },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Usage::default()
                },
            },
        ]
    }

    /// End-to-end ReAct proof without network: a scripted model drives two
    /// rounds (write, then report) through the real policy, real approval
    /// bypass, and real filesystem tools in an isolated directory.
    ///
    /// This is the closest offline stand-in for a live coding task: the
    /// only substitution is the model itself. Live runs additionally need
    /// a reachable provider; everything past sampling is identical.
    #[tokio::test]
    async fn scripted_loop_writes_a_real_file_across_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let (registry, todos) = wavecode_tools::Registry::builtin_with_todos();
        let registry = Arc::new(registry);
        let tools = ToolAdapter::new(
            registry.clone(),
            wavecode_tools::ToolCtx {
                cwd: cwd.clone(),
                deny_env: Vec::new(),
            },
        );
        let policy = PolicyAdapter::new(
            wavecode_sandbox::Sandbox::without_rules(wavecode_protocol::PermissionMode::Auto),
            registry.clone(),
        );
        let hooks = HookAdapter::new(
            Arc::new(wavecode_hooks::HookEngine::new(
                std::collections::HashMap::new(),
            )),
            cwd.clone(),
        );
        let model = Arc::new(ScriptedModel {
            scripts: std::sync::Mutex::new(
                [write_turn_script(), done_script()].into_iter().collect(),
            ),
        });
        let adapter =
            ModelAdapter::new(model.clone(), "scripted".to_string(), 100, registry.clone());
        let plans = TodoPlanTracker::new(todos);
        let compactor = ContextCompactor::new(model.clone(), "scripted".to_string());
        let interrupt = infrastructure_base::InterruptHandle::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_events = seen.clone();
        let outcome = RunLoop::new(
            tools,
            policy,
            hooks,
            adapter,
            HeadlessDeny,
            plans,
            compactor,
            RunConfig {
                model_name: "scripted".to_string(),
                context_window: 200_000,
                max_output_tokens: 100,
                max_tool_rounds: 8,
                max_continuations: runtime_runner::MAX_CONTINUATIONS,
                max_plan_nudges: runtime_runner::MAX_PLAN_NUDGES,
                max_stop_blocks: runtime_runner::MAX_STOP_BLOCKS,
                max_reactive_compacts: runtime_runner::MAX_REACTIVE_COMPACTS,
                max_repeat_streak: runtime_runner::MAX_REPEAT_STREAK,
                max_wire_images: runtime_runner::MAX_WIRE_IMAGES,
                session_date: None,
            },
            interrupt,
        )
        .run_turn(
            &runtime_runner::RunContext {
                run_id: "proof".to_string(),
                submission_id: "proof".to_string(),
                input: "write and report".to_string(),
                images: Vec::new(),
            },
            &mut Conversation::new(),
            runtime_runner::TurnInput::text("write and report"),
            "sys",
            &|event| {
                seen_events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event);
            },
        )
        .await;
        assert_eq!(outcome, runtime_runner::StopReason::Completed);
        // The write really landed on disk with exact content.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "wavecode-smoke-ok"
        );
        // Both rounds ran: two assistant messages, one paired tool call.
        let kinds: Vec<String> = seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|event| {
                serde_json::to_value(&event.msg)
                    .unwrap()
                    .get("type")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|k| *k == "agent_message_complete")
                .count(),
            2
        );
        assert_eq!(kinds.iter().filter(|k| *k == "tool_call_begin").count(), 1);
        assert_eq!(kinds.iter().filter(|k| *k == "tool_call_end").count(), 1);
    }
}
