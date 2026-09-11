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
use runtime_runner::{RunConfig, RunLoop};
use safety_gate::ApprovalGate;
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
use crate::skill_tool::SkillTool;
use crate::task_tools::{TaskOutputTool, TaskStopTool};
use crate::tool_adapter::ToolAdapter;

/// Approval wait timeout applied to parked decisions.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Tool dispatch rounds per turn.
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 32;
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
    /// Shared interrupt handle for stops and drops.
    pub interrupt: infrastructure_base::InterruptHandle,
    /// Assembled system prompt (also injected into every turn).
    pub system: String,
    /// Resolved model name for status displays.
    pub model_name: String,
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
    pub async fn connect_mcp_servers(&mut self) {
        if self.mcp_pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.mcp_pending);
        let report = crate::mcp_bridge::connect_all(&pending, &self.tools_registry).await;
        self.mcp_servers = report.lines;
        self.warnings.extend(report.warnings);
    }
}

/// Assembly inputs, all caller-owned.
pub struct AssembleOptions {
    /// Config file path; `None` loads the user-level config.
    pub config_path: Option<PathBuf>,
    /// `--model` override winning over the configured model.
    pub model_override: Option<String>,
    /// `--permission-mode` override winning over the configured mode.
    pub permission_override: Option<String>,
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
}

/// Resolve the effective permission mode: CLI override wins over config.
///
/// Unknown values warn and fall back to `Default` so a typo never locks
/// the session into a mode the policy rejected.
pub fn resolve_permission_mode(
    config_value: Option<&str>,
    cli_override: Option<&str>,
    warnings: &mut Vec<String>,
) -> wavecode_protocol::PermissionMode {
    if let Some(raw) = cli_override {
        return wavecode_protocol::PermissionMode::parse(raw).unwrap_or_else(|| {
            warnings.push(format!(
                "unrecognized --permission-mode {raw:?}; falling back to default"
            ));
            wavecode_protocol::PermissionMode::Default
        });
    }
    config_value
        .and_then(|raw| {
            wavecode_protocol::PermissionMode::parse(raw).or_else(|| {
                warnings.push(format!(
                    "unrecognized permission_mode {raw:?}; falling back to default"
                ));
                None
            })
        })
        .unwrap_or(wavecode_protocol::PermissionMode::Default)
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
        permission_override,
        cwd,
        home,
        identity,
        headless,
        initial_history,
    } = options;
    let mut warnings = Vec::new();

    // 1. Configuration and provider resolution (hard failure surface).
    let config = match config_path {
        Some(path) => wavecode_config::Config::load_from(&path)?,
        None => wavecode_config::Config::load()?,
    };
    let (provider, api_key) = config.resolve_provider()?;
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
    let model_name = model_override.unwrap_or_else(|| config.model.clone());
    let model: Arc<dyn wavecode_llm::ChatModel> = match provider.kind {
        wavecode_config::ProviderKind::OpenAiCompatible => {
            Arc::new(wavecode_llm::OpenAIClient::new(
                provider.base_url.clone(),
                api_key,
                model_name.clone(),
            ))
        }
        wavecode_config::ProviderKind::Anthropic => Arc::new(wavecode_llm::AnthropicClient::new(
            provider.base_url.clone(),
            api_key,
        )),
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
    let (context_window, max_output_tokens) = match provider.kind {
        wavecode_config::ProviderKind::OpenAiCompatible
            if provider.context_window.is_none() && provider.max_output_tokens.is_none() =>
        {
            let caps = wavecode_llm::ModelCapabilities::resolve_or(
                &model_name,
                provider.context_window(),
                provider.max_output_tokens(),
            );
            (caps.context_window, caps.max_output_tokens)
        }
        _ => (provider.context_window(), provider.max_output_tokens()),
    };
    Ok(assemble_session_with_model(WithModel {
        config,
        model,
        model_name,
        deny_env,
        context_window,
        max_output_tokens,
        permission_override,
        cwd,
        home,
        identity,
        headless,
        initial_history,
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
        deny_env,
        context_window,
        max_output_tokens,
        permission_override,
        cwd,
        home,
        identity,
        headless,
        initial_history,
        mut warnings,
    } = parts;
    let (registry, todos) = wavecode_tools::Registry::builtin_with_todos();
    // `memory_write` shares the prompt index root so model-written entries
    // surface in the next session without a restart. No home means no
    // memory at all (same gate as `assemble_memory` below).
    if let Some(home) = home.as_deref() {
        registry.register(Arc::new(MemoryWrite::new(
            wavecode_memory::MemoryStore::new(wavecode_memory::MemoryStore::default_root(
                home,
            )),
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
    // never shows a mode the policy rejected).
    let permission_mode_raw = match permission_mode {
        wavecode_protocol::PermissionMode::Default => "default".to_string(),
        wavecode_protocol::PermissionMode::Plan => "plan".to_string(),
        wavecode_protocol::PermissionMode::AcceptEdits => "acceptEdits".to_string(),
        wavecode_protocol::PermissionMode::BypassPermissions => "bypassPermissions".to_string(),
        _ => "default".to_string(),
    };
    let sandbox = wavecode_sandbox::Sandbox::without_rules(permission_mode);

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
    let policy = PolicyAdapter::new(sandbox, registry.clone());
    let model_adapter = ModelAdapter::new(
        model.clone(),
        model_name.clone(),
        max_output_tokens,
        registry.clone(),
    );
    let approvals = Arc::new(ApprovalGate::new());
    let gate_source = if headless {
        crate::gate_adapter::Approvals::Headless(crate::gate_adapter::HeadlessDeny)
    } else {
        crate::gate_adapter::Approvals::Gate(GateApprovalSource::new(
            approvals.clone(),
            APPROVAL_TIMEOUT,
        ))
    };
    let plans = TodoPlanTracker::new(todos);
    let compactor = ContextCompactor::new(model.clone(), model_name.clone());

    // 6. Run loop behind the shared driver pointer.
    let native = Arc::new(Mutex::new(NativeExecutor::new()));
    let executor = CompositeExecutor::new(tools, native.clone(), registry.clone());
    let interrupt = infrastructure_base::InterruptHandle::new();
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
        },
        interrupt.clone(),
    ));
    // Per-run tool allowlist for fork-scoped skill surfaces; the handle
    // is Arc-backed, so grabbing it before `worker` moves is enough.
    let run_allowlist = worker.run_allowlist();
    let driver = {
        // Session-end memory extraction rides the driver seam: the actor
        // calls `end_session` with the final transcript on teardown. No
        // home means no memory at all (same gate as the write tool above
        // and `assemble_memory` below).
        let finisher = home.as_deref().map(|root| {
            MemoryFinisher::new(
                model.clone(),
                model_name.clone(),
                wavecode_memory::MemoryStore::new(
                    wavecode_memory::MemoryStore::default_root(root),
                ),
            )
        });
        Arc::new(SessionMemory::new(worker, finisher))
    };

    // 7. Child task tools register against the built driver (phase two).
    let children = Arc::new(ChildRuntime::new());
    let tasks = Arc::new(TurnChildService::new(
        driver.clone() as Arc<dyn runtime_runner::TurnDriver>,
        children.clone(),
        String::new(),
        run_allowlist,
    ));
    register_child_tools(&native, tasks.clone());

    // The `skill` model tool needs the child service, which only exists
    // after the driver is built: late registration on the shared registry
    // makes it visible to the executor, policy, and model adapters live.
    // `task_output` / `task_stop` ride the same handle so the model can
    // observe and stop what skills forked.
    let task_service = tasks.clone() as Arc<dyn action_tasks::TaskService>;
    registry.register(Arc::new(SkillTool::new(
        skill_set,
        task_service.clone(),
    )));
    registry.register(Arc::new(TaskOutputTool::new(task_service.clone())));
    registry.register(Arc::new(TaskStopTool::new(task_service)));
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
    let snapshot_memory_root = home.as_deref().map(wavecode_memory::MemoryStore::default_root);
    let snapshot_root =
        wavecode_tools::snapshot::snapshot_store_root_for_session(snapshot_memory_root.as_deref());
    registry.register(Arc::new(wavecode_tools::snapshot::SnapshotTool::new(
        snapshot_root.clone(),
    )));
    registry.register(Arc::new(
        wavecode_tools::snapshot::RestoreTool::new(snapshot_root),
    ));
    // Reviewed plan mode (explore -> present -> approve -> execute) rides
    // the same late handle: the store path derives from home
    // (`<home>/.wavecode/plans/<session>.json`) so resume in the same home
    // reopens the same plan. Corrupt content warns and starts empty, never
    // a hard stop. The mutating tools are not read-only, but they are not
    // destructive either (they touch plan state, not the repo), so they
    // never park on an approval gate.
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
    registry.register(Arc::new(crate::plan_tools::PlanProposeTool::new(
        plan_store.clone(),
    )));
    registry.register(Arc::new(crate::plan_tools::PlanApproveTool::new(
        plan_store.clone(),
    )));
    registry.register(Arc::new(crate::plan_tools::PlanFeedbackTool::new(
        plan_store.clone(),
    )));
    registry.register(Arc::new(crate::plan_tools::PlanStatusTool::new(
        plan_store,
    )));

    // 8. System prompt from assembled slots plus the live tool catalog.
    let tool_names: Vec<String> = registry
        .specs()
        .into_iter()
        .map(|spec| spec.name.clone())
        .collect();
    let system = build_system(&PromptSlots {
        identity,
        instructions: instruction_memory,
        memory_index: memory_index.clone(),
        skill_catalog,
        tool_note: format!("Available tools: {}", tool_names.join(", ")),
        summary: String::new(),
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
        interrupt.clone(),
        system_for_actor,
    );

    let mut mcp_pending: Vec<(String, wavecode_config::McpServerRaw)> = config
        .mcp_servers
        .clone()
        .into_iter()
        .collect();
    mcp_pending.sort_by(|a, b| a.0.cmp(&b.0));

    SessionHandle {
        client,
        approvals,
        interrupt,
        system,
        model_name,
        permission_mode: permission_mode_raw,
        skill_names,
        memory_index,
        mcp_servers,
        warnings,
        tools_registry: registry.clone(),
        mcp_pending,
    }
}

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
    use operations_wire::{Op, Submission};
    use wavecode_llm::{ChatModel, ChatRequest, EventStream, StreamEvent, Usage};

    const CONFIG: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
api_key = "k-inline"
"#;

    #[tokio::test]
    async fn assembly_builds_a_live_client_offline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let mut handle = assemble_session(AssembleOptions {
            config_path: Some(path),
            model_override: None,
            permission_override: None,
            cwd: dir.path().to_path_buf(),
            home: None,
            identity: DEFAULT_IDENTITY.to_string(),
            headless: true,
            initial_history: Vec::new(),
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

    #[test]
    fn cli_permission_override_wins_over_config() {
        let mut warnings = Vec::new();
        let mode = resolve_permission_mode(Some("plan"), Some("bypassPermissions"), &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::BypassPermissions);
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_permission_values_warn_and_fall_back() {
        let mut warnings = Vec::new();
        let mode = resolve_permission_mode(Some("plan"), Some("nope"), &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Default);
        assert!(warnings.iter().any(|w| w.contains("--permission-mode")));
        warnings.clear();
        let mode = resolve_permission_mode(Some("nope"), None, &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Default);
        assert!(warnings.iter().any(|w| w.contains("permission_mode")));
    }

    #[test]
    fn config_permission_used_without_override() {
        let mut warnings = Vec::new();
        let mode = resolve_permission_mode(Some("acceptEdits"), None, &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::AcceptEdits);
        assert!(warnings.is_empty());
        let mode = resolve_permission_mode(None, None, &mut warnings);
        assert_eq!(mode, wavecode_protocol::PermissionMode::Default);
        assert!(warnings.is_empty());
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
                        },
                    }]
                });
            Ok(Box::pin(futures::stream::iter(
                script.into_iter().map(Ok),
            )))
        }
    }

    fn write_turn_script() -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta {
                text: "working".to_string(),
            },
            StreamEvent::ToolUseBegin {
                id: "c1".to_string(),
                name: "write_file".to_string(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"hello.txt","content":"wavecode-smoke-ok"}"#
                    .to_string(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
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
            wavecode_sandbox::Sandbox::without_rules(
                wavecode_protocol::PermissionMode::BypassPermissions,
            ),
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
        let adapter = ModelAdapter::new(
            model.clone(),
            "scripted".to_string(),
            100,
            registry.clone(),
        );
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
            },
            interrupt,
        )
        .run_turn(
            &runtime_runner::RunContext {
                run_id: "proof".to_string(),
                submission_id: "proof".to_string(),
                input: "write and report".to_string(),
            },
            &mut Conversation::new(),
            "write and report",
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
        assert_eq!(
            kinds.iter().filter(|k| *k == "tool_call_begin").count(),
            1
        );
        assert_eq!(kinds.iter().filter(|k| *k == "tool_call_end").count(), 1);
    }
}
