//! The model-independent half of session assembly: from a resolved
//! model client to a live session handle. Pure movement of the phase
//! machinery (shared stores, policy, context sources, wiring, late
//! registrations) out of `session.rs`; the public entry points stay
//! re-exported from `session.rs` so every path keeps resolving.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use action_tasks::TaskService;
use operations_actor::SessionActor;
use runtime_child::ChildRuntime;
use runtime_prompt::{Budget, DEFAULT_CATALOG_BUDGET, PromptSlots, assemble_budgeted};
use runtime_runner::{RunInterrupts, RunLoop, ToolExecutor};
use safety_gate::{ApprovalGate, QuestionGate};
use state_store::Conversation;

use super::{
    APPROVAL_TIMEOUT, DEFAULT_MAX_TOOL_ROUNDS, SessionHandle, WithModel, load_permissions,
    resolve_permission_mode,
};
use crate::child_service::TurnChildService;
use crate::compactor::ContextCompactor;
use crate::composite::CompositeExecutor;
use crate::gate_adapter::GateApprovalSource;
use crate::hook_adapter::HookAdapter;
use crate::memory_finish::{MemoryFinisher, SessionMemory};
use crate::model_adapter::ModelAdapter;
use crate::native::{NativeExecutor, NativeTool};
use crate::plan_adapter::TodoPlanTracker;
use crate::policy_adapter::PolicyAdapter;
use crate::prune_adapter::PruningExecutor;
use crate::tool_adapter::ToolAdapter;
use wavecode_memory::tool::MemoryWrite;
use wavecode_skills::tool::SkillTool;
use wavecode_tools::{TaskContinueTool, TaskOutputTool, TaskStopTool};

/// Slot inputs for the system prompt: what context assembly produced
/// plus the session identity and the resolved permission mode, consumed
/// only when the prompt is built.
struct PromptMaterials {
    identity: String,
    instruction_memory: String,
    skill_catalog: String,
    permission_mode: wavecode_protocol::PermissionMode,
}

/// Phase 8 of assembly: build the system prompt from the slots plus the
/// live tool catalog, then attach it and the tool-surface policy to the
/// child service.
///
/// Plan mode adds a soft guidance paragraph: read-only exploration,
/// prefer proposing via the plan tool, answering directly is fine.
///
/// Children spawn through the same service, so without the forbidden
/// subtraction a child could re-fork `task`/`skill`/workflow spawns and
/// the runtime depth cap would never see the nested generations;
/// read-only profiles additionally narrow to the read-only subset. The
/// forbidden list is a hand-maintained copy of the spawn-tool names (they
/// live in their tool types), so drift fails loudly as an assembly
/// warning instead of silently re-opening the recursion hole. The check
/// runs against registry plus native names — snapshotted after every
/// late registration, so the native-only `child_spawn` is visible — and
/// every listed name must exist somewhere in the assembled catalog.
///
/// Returns `(system prompt, tool names)`; the caller keeps both for the
/// actor spawn and the status surface.
fn assemble_system_prompt_and_child_surface(
    registry: &wavecode_tools::Registry,
    native: &Arc<Mutex<NativeExecutor>>,
    tasks: &Arc<TurnChildService>,
    materials: PromptMaterials,
    memory_index: &str,
    cwd: &Path,
    warnings: &mut Vec<String>,
) -> (String, Vec<String>) {
    let tool_names: Vec<String> = registry
        .specs()
        .into_iter()
        .map(|spec| spec.name.clone())
        .collect();
    let PromptMaterials {
        identity,
        mut instruction_memory,
        skill_catalog,
        permission_mode,
    } = materials;
    if permission_mode == wavecode_protocol::PermissionMode::Plan {
        instruction_memory.push_str(
            "\nYou are in plan mode: only read-only tools are available. \
             Explore first, then propose your approach with the plan tool \
             so the user can approve it; when the request only needs an \
             answer, simply answer.",
        );
    }
    let assembled = assemble_budgeted(
        &PromptSlots {
            identity,
            instructions: instruction_memory,
            memory_index: memory_index.to_string(),
            skill_catalog,
            tool_note: format!("Available tools: {}", tool_names.join(", ")),
            environment: crate::environment::describe(cwd),
            summary: String::new(),
        },
        &Budget::default(),
    );
    // The budget is a backstop, so a drop is an anomaly the user hears
    // about — never a silent loss of instructions or identity.
    for title in &assembled.dropped {
        warnings.push(format!(
            "system prompt exceeded its character budget; the '{title}' section was dropped"
        ));
    }
    if assembled.catalog_truncated {
        warnings
            .push("the skill catalog was truncated to fit the system prompt budget".to_string());
    }
    let system = assembled.system;

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
    (system, tool_names)
}

/// Durable goal service (persisted objective with CAS) plus the
/// model-driven compaction channel, registered on the shared registry,
/// with the compactor configured from the journal footer and the live
/// plan tracker. Store paths derive from home so resume in the same
/// home reopens the same data; corrupt content warns and starts empty,
/// never a hard stop. Assembled before the loop because the loop reads
/// both: an open objective keeps a long task running after the model
/// stops (the round counter stays model-driven; only continuation is
/// loop-driven), and a queued compaction is reviewed at the next loop
/// head — never inside tool execution, where a grant would rewrite the
/// history the tool result still rides.
fn build_goal_and_compaction_services(
    registry: &wavecode_tools::Registry,
    model: &Arc<dyn wavecode_llm::ChatModel>,
    model_name: &str,
    todos: wavecode_tools::TodoStore,
    home: Option<&Path>,
    session_id: Option<&str>,
    warnings: &mut Vec<String>,
) -> (
    Arc<state_goal::tool::GoalStore>,
    Arc<runtime_runner::CompactionRequests>,
    ContextCompactor,
) {
    let (goal_state, goal_warning) =
        state_goal::tool::load_for_session(home, state_goal::tool::DEFAULT_GOAL_SESSION_ID);
    if let Some(warning) = goal_warning {
        warnings.push(warning);
    }
    let goal_store = Arc::new(state_goal::tool::GoalStore::new(
        goal_state,
        home,
        state_goal::tool::DEFAULT_GOAL_SESSION_ID,
    ));
    registry.register(Arc::new(state_goal::tool::GoalTool::new(
        goal_store.clone(),
    )));
    let compaction_requests = Arc::new(runtime_runner::CompactionRequests::new());
    registry.register(Arc::new(crate::compaction_tool::CompactContextTool::new(
        compaction_requests.clone(),
    )));
    // Compaction footers: the journal pointer needs a home plus the
    // session id; the live plan shares the todo store the todo tool
    // writes.
    let journal = home.and_then(|home| {
        let id = session_id?;
        state_persistence::sessions::session_journal_file(home, id)
    });
    let compactor = ContextCompactor::new(model.clone(), model_name.to_string())
        .with_footers(crate::compactor::CompactionFooters { journal })
        .with_plans(todos);
    (goal_store, compaction_requests, compactor)
}

/// Loop budget configuration: model identity plus the configured tool
/// round cap and the runner's fixed continuation ceilings.
fn run_config(
    config: &wavecode_config::Config,
    model_name: &str,
    context_window: u64,
    max_output_tokens: u32,
) -> runtime_runner::RunConfig {
    runtime_runner::RunConfig {
        model_name: model_name.to_string(),
        context_window,
        max_output_tokens,
        max_tool_rounds: config.max_tool_rounds.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS),
        max_continuations: runtime_runner::MAX_CONTINUATIONS,
        max_plan_nudges: runtime_runner::MAX_PLAN_NUDGES,
        max_goal_continuations: runtime_runner::MAX_GOAL_CONTINUATIONS,
        max_goal_rearms: runtime_runner::MAX_GOAL_REARMS,
        max_stop_blocks: runtime_runner::MAX_STOP_BLOCKS,
        max_reactive_compacts: runtime_runner::MAX_REACTIVE_COMPACTS,
        max_repeat_streak: runtime_runner::MAX_REPEAT_STREAK,
        max_wire_images: runtime_runner::MAX_WIRE_IMAGES,
    }
}

/// Build the child turn service: journals per child beside the session
/// journal (so a long run can explain what a child did), riding the same
/// credential mask as the session journal — no provider key survives
/// into readable history. One secrets store is shared by the child gate
/// and the handle (frontends fetch it for their own writers); it is
/// returned alongside so the handle and the gate see the same masks.
fn build_child_service<D>(
    driver: &Arc<SessionMemory<D>>,
    run_allowlist: runtime_runner::RunAllowlist,
    run_interrupts: runtime_runner::RunInterrupts,
    home: Option<std::path::PathBuf>,
    session_id: Option<String>,
    config: &wavecode_config::Config,
) -> (
    Arc<ChildRuntime>,
    Arc<TurnChildService>,
    std::sync::Arc<safety_secrets::SecretsStore>,
)
where
    SessionMemory<D>: runtime_runner::TurnDriver,
    D: 'static,
{
    let children = Arc::new(ChildRuntime::new());
    let mut child_service = TurnChildService::new(
        driver.clone() as Arc<dyn runtime_runner::TurnDriver>,
        children.clone(),
        run_allowlist,
        run_interrupts,
    );
    // Subagent logs: each child task appends its own turns beside the
    // session's journal, so a long run can explain what a child actually
    // did instead of keeping only its returned summary.
    if let (Some(home), Some(parent)) = (home.clone(), session_id.clone()) {
        child_service = child_service.with_child_journal(home, parent);
    }
    let secrets = std::sync::Arc::new(build_secret_store(config));
    let redactor = secrets.clone();
    child_service = child_service
        .with_journal_redaction(std::sync::Arc::new(move |text: &str| redactor.redact(text)));
    (children, Arc::new(child_service), secrets)
}

/// Assemble the model-independent remainder: registries to live client.
///
/// Must be called inside a tokio runtime (the actor task spawns here).
/// Shares its body with [`assemble_session`]; the split is purely a
/// seam for hermetic tests, never a behavior fork. Public only so the
/// gateway's server tests can assemble sessions the production way.
pub fn assemble_session_after_model(parts: WithModel) -> SessionHandle {
    let WithModel {
        config,
        model,
        model_name,
        provider_id,
        thinking_effort,
        deny_env,
        context_window,
        max_output_tokens,
        per_model_window,
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
    // Phases 1-4: shared stores, the policy stack, and the context
    // sources. All soft degradation lands in `warnings` in the original
    // assembly order.
    let ctx = build_assembled_context(
        &config,
        home.as_deref(),
        permission_override.as_deref(),
        &cwd,
        &wave_denylist,
        &mut warnings,
    );
    // Phases 5-7: seam adapters, gates, the run loop and its driver, the
    // child runtime, and the late tool registrations.
    let wiring = build_runtime_wiring(
        &ctx,
        RuntimeInputs {
            config: &config,
            model: &model,
            model_name: &model_name,
            deny_env: &deny_env,
            cwd: &cwd,
            home: &home,
            session_id: &session_id,
            context_window,
            max_output_tokens,
            per_model_window,
            headless,
        },
        &mut warnings,
    );
    let AssembledContext {
        registry,
        permission_mode,
        permission_mode_raw,
        instruction_memory,
        memory_index,
        instruction_sources,
        skill_catalog,
        skill_names,
        mcp_servers,
        ..
    } = ctx;

    // 8. System prompt from assembled slots plus the live tool catalog,
    // then the child surface attach (see the helper for the guarantees).
    // The tool-catalog list is the helper's return half and stays there;
    // the handle only carries the assembled system text.
    let (system, _tool_names) = assemble_system_prompt_and_child_surface(
        &registry,
        &wiring.native,
        &wiring.tasks,
        PromptMaterials {
            identity,
            instruction_memory,
            skill_catalog,
            permission_mode,
        },
        &memory_index,
        &cwd,
        &mut warnings,
    );

    // 9. Actor task and client handle, sharing the child runtime with
    // the task tools so completions re-enter turns. History comes from the
    // session's own block journal when it has one — that is the only source
    // carrying tool calls, results, thinking and images.
    let system_for_actor = system.clone();
    let journal = match (home.as_deref(), session_id.as_deref()) {
        (Some(home), Some(id)) => state_persistence::sessions::session_history_file(home, id)
            .map(state_persistence::history::HistoryJournal::new),
        // No journal id (the in-memory REPL, a child run): history lives in
        // memory only, exactly as before.
        _ => None,
    };
    let conv = seed_conversation(journal, &initial_history, &mut warnings);
    let client = SessionActor::spawn(
        wiring.driver,
        conv,
        wiring.children,
        wiring.approvals.clone(),
        wiring.questions.clone(),
        wiring.interrupt.clone(),
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
    let status = crate::status_queries::SessionStatus::new(
        home.as_deref(),
        wiring.snapshot_root,
        Some(wiring.jobs.clone()),
    )
    .shared();

    SessionHandle {
        client,
        approvals: wiring.approvals,
        questions: wiring.questions,
        interrupt: wiring.interrupt,
        system,
        model_name,
        provider_id,
        thinking_effort,
        permission_mode: permission_mode_raw,
        skill_names,
        memory_index,
        instruction_sources,
        mcp_servers,
        warnings,
        tools_registry: registry.clone(),
        plugins: wiring.runtime_plugins,
        status,
        mcp_pending,
        secrets: Some(wiring.secrets),
    }
}

/// Phases 1-4 of assembly: the shared stores, the policy stack, and the
/// warn-and-continue context sources. Everything later phases read from
/// these four phases rides in here; fields still consumed by value
/// during wiring are moved out at the call site.
struct AssembledContext {
    /// One spill store for the whole session, shared by the shell tool,
    /// the `spill` tool, the pruning executor, and the job tools. A
    /// single shared instance (one manifest ledger) is load-bearing —
    /// separate instances over the same root would race their manifest
    /// read-modify-write cycles under concurrent spills and drop entries
    /// past the total cap.
    spill_store: Arc<wavecode_context::spill::SpillStore>,
    /// Shared tool registry with its builtin tools plus `memory_write`
    /// already registered; every late registration below attaches
    /// through the same `Arc`.
    registry: Arc<wavecode_tools::Registry>,
    /// Live plan list shared by the todo tool, the plan tracker, and the
    /// compactor footers (cloning shares the same store).
    todos: wavecode_tools::TodoStore,
    /// Effective permission mode after the override/config fallback.
    permission_mode: wavecode_protocol::PermissionMode,
    /// Wire name of [`AssembledContext::permission_mode`] for status
    /// displays (post-fallback, so the UI never shows a mode the policy
    /// rejected).
    permission_mode_raw: String,
    /// Sandbox built from the resolved mode plus the validated allow /
    /// deny rules; cloning shares the session-level "always allow" state.
    sandbox: wavecode_sandbox::Sandbox,
    /// Combined instruction text injected into the system prompt.
    instruction_memory: String,
    /// Persistent memory index text (empty when memory degraded).
    memory_index: String,
    /// Instruction files actually loaded into context (`AGENTS.md`,
    /// `AGENTS.local.md`, `.wavecode/rules/*.md` tiers), in concat order.
    instruction_sources: Vec<PathBuf>,
    /// Skill catalog text for the system prompt.
    skill_catalog: String,
    /// Directly invokable skill names for completion sources.
    skill_names: Vec<String>,
    /// Shared skill set the `skill` tool executes against.
    skill_set: Arc<wavecode_skills::SkillSet>,
    /// Hook engine converted from the configured hook points.
    hooks: Arc<wavecode_hooks::HookEngine>,
    /// Configured MCP servers as one display line each.
    mcp_servers: Vec<String>,
}

/// Build [`AssembledContext`] (phases 1-4): shared stores, the policy
/// stack, and the context sources. Warning pushes keep the original
/// assembly order: permission-mode fallback first, then permission
/// findings, then memory, skills, and hooks.
fn build_assembled_context(
    config: &wavecode_config::Config,
    home: Option<&Path>,
    permission_override: Option<&str>,
    cwd: &Path,
    wave_denylist: &[String],
    warnings: &mut Vec<String>,
) -> AssembledContext {
    // One spill store for the whole session, built here at the composition
    // root and injected everywhere: the shell tool spills truncated output
    // into it, the `spill` tool reads it back, and the pruning executor
    // prunes oversized results through it.
    let spill_store = Arc::new(wavecode_context::spill::SpillStore::new(
        wavecode_context::spill::default_spill_store_root(),
    ));
    let (registry, todos) =
        wavecode_tools::Registry::builtin_with_spill_store_and_todos(spill_store.clone());
    // `memory_write` shares the prompt index root so model-written entries
    // surface in the next session without a restart. No home means no
    // memory at all (same gate as `assemble_memory` below).
    if let Some(home) = home {
        registry.register(Arc::new(MemoryWrite::new(
            wavecode_memory::MemoryStore::new(wavecode_memory::MemoryStore::default_root(home)),
        )));
    }
    let registry = Arc::new(registry);

    // 3. Policy: permission mode with explicit fallback, rules unconfigured.
    let permission_mode = resolve_permission_mode(
        config.permission_mode.as_deref(),
        permission_override,
        warnings,
    );
    // Effective wire name for status displays (post-fallback, so the UI
    // never shows a mode the policy rejected). `as_str` is an exhaustive
    // match, so a future PermissionMode variant fails compilation here
    // instead of silently displaying under the wrong mode name.
    let permission_mode_raw = permission_mode.as_str().to_string();
    let permissions = load_permissions(config, home, wave_denylist);
    warnings.extend(permissions.findings);
    let sandbox =
        wavecode_sandbox::Sandbox::from_rules(permission_mode, permissions.allow, permissions.deny);

    // 4. Context sources with warn-and-continue degradation.
    let (instruction_memory, memory_index, instruction_sources) =
        assemble_memory(home, cwd, warnings);
    let (skill_catalog, skill_names, skill_set) = assemble_skills(home, cwd, warnings);
    let hooks = assemble_hooks(config, warnings);
    let mcp_servers = describe_mcp_servers(config);

    AssembledContext {
        spill_store,
        registry,
        todos,
        permission_mode,
        permission_mode_raw,
        sandbox,
        instruction_memory,
        memory_index,
        instruction_sources,
        skill_catalog,
        skill_names,
        skill_set,
        hooks,
        mcp_servers,
    }
}

/// The concrete turn loop a live session runs: the full executor chain
/// (registry + native tools + spill pruning + nested-instruction
/// discovery) behind the loop's seven seams. A type alias keeps the
/// wiring stage's driver field nameable without repeating the chain;
/// the loop builder methods all return `Self`, so this is the exact
/// type [`RunLoop::new`] plus its `with_*` builders produce here.
type SessionLoop = runtime_runner::RunLoop<
    crate::agents_instructions::AgentsInstructionsExecutor<
        crate::prune_adapter::PruningExecutor<crate::composite::CompositeExecutor>,
    >,
    crate::policy_adapter::PolicyAdapter,
    crate::hook_adapter::HookAdapter,
    crate::evicting_gateway::EvictingGateway<crate::model_adapter::ModelAdapter>,
    crate::gate_adapter::Approvals,
    crate::plan_adapter::TodoPlanTracker,
    crate::compactor::ContextCompactor,
>;

/// Model and runtime inputs the wiring stage reads but never owns:
/// borrowed views over the [`WithModel`] fields plus the resolved
/// window numbers (primitives copied). Borrowing keeps the originals
/// usable by the finalize phase and the handle.
struct RuntimeInputs<'a> {
    /// Full config for run budgets and the child service.
    config: &'a wavecode_config::Config,
    /// Chat model backing the sampling adapter, the compactor, and the
    /// session-end memory finisher.
    model: &'a Arc<dyn wavecode_llm::ChatModel>,
    /// Effective model name for displays, sampling, and the loop config.
    model_name: &'a str,
    /// Env names hidden from tools (also reaching the LSP registry).
    deny_env: &'a [String],
    /// Working directory for tools and relative paths.
    cwd: &'a Path,
    /// Home directory; `None` degrades grants, memory, and journals.
    home: &'a Option<PathBuf>,
    /// Session id for grants, journals, and goals; `None` in memory-only
    /// runs.
    session_id: &'a Option<String>,
    /// Effective context window (fixed by provider config).
    context_window: u64,
    /// Effective output cap.
    max_output_tokens: u32,
    /// Capability-table fallback for per-name window resolution; `Some`
    /// when a `/model` switch moves the loop's budget gate onto the new
    /// window.
    per_model_window: Option<u64>,
    /// True for non-interactive drivers: approvals deny openly.
    headless: bool,
}

/// Phases 5-7 of assembly: everything the finalize phase (system prompt,
/// actor spawn, handle construction) needs from the wiring half.
struct RuntimeWiring {
    /// Shared approval gate behind parked decisions; handed to the gate
    /// source and the actor, and carried on the handle.
    approvals: Arc<ApprovalGate>,
    /// Shared question gate behind parked interactive questions; same
    /// three-way sharing as the approval gate.
    questions: Arc<QuestionGate>,
    /// Session interrupt handle feeding the gate source, the run loop,
    /// and the actor.
    interrupt: infrastructure_base::InterruptHandle,
    /// Native tool executor; the `child_*` tools register into it and
    /// the prompt helper snapshots its names.
    native: Arc<Mutex<NativeExecutor>>,
    /// Turn driver behind the shared pointer; the child service holds a
    /// clone and the actor moves this one in.
    driver: Arc<SessionMemory<Arc<SessionLoop>>>,
    /// Child runtime shared by the child service, the job tools, and the
    /// actor.
    children: Arc<ChildRuntime>,
    /// Child turn service; the task tools and the prompt's child surface
    /// ride it.
    tasks: Arc<TurnChildService>,
    /// Credential store shared by the child journal gate; the handle
    /// carries it so frontends mask with the same redaction.
    secrets: Arc<safety_secrets::SecretsStore>,
    /// Job service the job tools write; status views observe the same
    /// jobs.
    jobs: Arc<action_jobs::JobService>,
    /// Snapshot store root the snapshot/plan tools wrote; status views
    /// read exactly that root.
    snapshot_root: PathBuf,
    /// Started runtime plugins owned for the session's lifetime; they
    /// unload (reverse start order) on teardown.
    runtime_plugins: runtime_plugin::Registry,
}

/// Build [`RuntimeWiring`] (phases 5-7): seam adapters, the gates, the
/// run loop and its driver, the child service, and the late tool
/// registrations. Registration order is preserved from the original
/// single body — the composite executor enters the loop first, then the
/// child and task tools register against the built driver.
fn build_runtime_wiring(
    ctx: &AssembledContext,
    inputs: RuntimeInputs,
    warnings: &mut Vec<String>,
) -> RuntimeWiring {
    let RuntimeInputs {
        config,
        model,
        model_name,
        deny_env,
        cwd,
        home,
        session_id,
        context_window,
        max_output_tokens,
        per_model_window,
        headless,
    } = inputs;
    // 5. Seam adapters (pure wiring, no policy inside).
    let tools = ToolAdapter::new(
        ctx.registry.clone(),
        wavecode_tools::ToolCtx {
            cwd: cwd.to_path_buf(),
            // Cloned so the same list also reaches the LSP provider registry
            // below: every model-facing spawn path needs the provider's
            // deny names, and assembly order consumes this one first.
            deny_env: deny_env.to_vec(),
        },
    );
    let policy = match (home.as_deref(), session_id.as_deref()) {
        (Some(home), Some(session_id)) => {
            PolicyAdapter::new(ctx.sandbox.clone(), ctx.registry.clone())
                .with_grants(crate::grants_sink::GrantSink::new(home, session_id))
        }
        // No home or no session identity: "always allow" stays in-session,
        // because a grant needs somewhere to live and a writer to attribute.
        _ => PolicyAdapter::new(ctx.sandbox.clone(), ctx.registry.clone()),
    };
    // Tool-result eviction rides the gateway seam: every sample crosses
    // the pass, stored history keeps original payloads. When the window
    // resolves per model name (`per_model_window` carries the fallback),
    // the adapter resolves it live so a `/model` switch moves the loop's
    // budget gate onto the new window.
    let model_adapter = crate::evicting_gateway::EvictingGateway::new({
        let adapter = ModelAdapter::new(
            model.clone(),
            model_name.to_string(),
            max_output_tokens,
            ctx.registry.clone(),
        );
        match per_model_window {
            Some(fallback) => adapter.with_per_model_window(fallback),
            None => adapter,
        }
    });
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
    let plans = TodoPlanTracker::new(ctx.todos.clone());
    let (goal_store, compaction_requests, compactor) = build_goal_and_compaction_services(
        &ctx.registry,
        model,
        model_name,
        ctx.todos.clone(),
        home.as_deref(),
        session_id.as_deref(),
        warnings,
    );

    // 6. Run loop behind the shared driver pointer.
    let native = Arc::new(Mutex::new(NativeExecutor::new()));
    // Oversized tool outputs spill to the shared side-store (the same
    // instance the shell tool and the `spill` tool hold) instead of
    // flowing into history unbounded.
    let executor = PruningExecutor::new(
        CompositeExecutor::new(tools, native.clone(), ctx.registry.clone()),
        ctx.spill_store.clone(),
    );
    // Nested-instruction discovery: session assembly loaded the global,
    // project-root, and cwd tiers; this layer offers each deeper
    // directory's AGENTS.md the first time a file tool touches it. The
    // project root bounds the upward walk — its own tier is already in.
    let instruction_requests = Arc::new(runtime_runner::DirectoryInstructions::new());
    let executor = crate::agents_instructions::AgentsInstructionsExecutor::new(
        executor,
        wavecode_memory::find_project_root(cwd),
        instruction_requests.clone(),
    );
    let worker = Arc::new(
        RunLoop::new(
            executor,
            policy,
            HookAdapter::new(ctx.hooks.clone(), cwd.to_path_buf()),
            model_adapter,
            gate_source,
            plans,
            compactor,
            run_config(config, model_name, context_window, max_output_tokens),
            interrupt.clone(),
        )
        .with_goals(crate::goal_adapter::GoalTrackerAdapter::new(goal_store).shared())
        .with_compaction_requests(compaction_requests)
        .with_instruction_requests(instruction_requests),
    );
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
                model_name.to_string(),
                wavecode_memory::MemoryStore::new(wavecode_memory::MemoryStore::default_root(root)),
            )
        });
        Arc::new(SessionMemory::new(worker, finisher))
    };

    // 7. Child task tools register against the built driver (phase two).
    // The system prompt and tool-surface policy attach after assembly
    // completes (they need the full catalog); spawns only start once the
    // actor runs, so every child observes them.
    let (children, tasks, secrets) = build_child_service(
        &driver,
        run_allowlist,
        run_interrupts,
        home.clone(),
        session_id.clone(),
        config,
    );
    // Runtime plugins (service injection + middleware lifecycle): manifest
    // discovery warns-and-skips invalid plugins and never fails assembly.
    // The handle owns the started registry for the session's lifetime, so
    // plugins unload (reverse start order) when the session tears down.
    let runtime_plugins = runtime_plugin::load_and_start(home.as_deref(), warnings);
    register_child_tools(&native, tasks.clone());

    // The `skill` model tool needs the child service, which only exists
    // after the driver is built: late registration on the shared registry
    // makes it visible to the executor, policy, and model adapters live.
    // `task_output` / `task_stop` / `task_continue` ride the same handle so
    // the model can observe, stop, and continue what task and skill forked.
    let task_service = tasks.clone() as Arc<dyn action_tasks::TaskService>;
    register_task_tools(&ctx.registry, ctx.skill_set.clone(), task_service.clone());
    register_lsp_tools(&ctx.registry, cwd, deny_env);
    register_workflow_schedule_tools(
        &ctx.registry,
        task_service.clone(),
        home.as_deref(),
        warnings,
    );
    let jobs = register_job_tools(&ctx.registry, children.clone(), ctx.spill_store.clone());
    let snapshot_root = register_snapshot_and_plan_tools(&ctx.registry, home.as_deref(), warnings);

    RuntimeWiring {
        approvals,
        questions,
        interrupt,
        native,
        driver,
        children,
        tasks,
        secrets,
        jobs,
        snapshot_root,
        runtime_plugins,
    }
}

/// Build the conversation a session starts with.
///
/// The block journal wins whenever it holds anything: it is the only source
/// that preserves tool calls, tool results, thinking and images across a
/// restart. The caller's text seed stays the fallback for sessions written
/// before the journal existed, and in that case the seed is journaled too —
/// otherwise the journal would describe only the tail of the session, and a
/// later crash would recover a history silently missing its head.
pub(super) fn seed_conversation(
    journal: Option<state_persistence::history::HistoryJournal>,
    initial_history: &[(bool, String)],
    warnings: &mut Vec<String>,
) -> Conversation {
    let Some(journal) = journal else {
        let mut conv = Conversation::new();
        push_text_seed(&mut conv, initial_history);
        return conv;
    };
    let replayed = crate::history_journal::replay_history(&journal);
    // A torn tail or a hole hides every byte appended after it. The repaired
    // snapshot replaces the file in one rename: truncating to a header first
    // would drop the verified prefix if the process died before the append.
    let mut next_seq = replayed.next_seq;
    let mut snapshot_durable = false;
    if replayed.gapped || replayed.torn_tail {
        let records = if replayed.entries.is_empty() {
            Vec::new()
        } else {
            vec![serde_json::json!({
                "k": "replace",
                "seq": 0,
                "entries": &replayed.entries,
            })]
        };
        match journal.rewrite(&records) {
            Ok(()) => {
                next_seq = if records.is_empty() { 0 } else { 1 };
                snapshot_durable = !records.is_empty();
            }
            Err(error) => warnings.push(format!(
                "history journal could not be rewritten after damage ({error}); new records may stay unreachable on the next resume"
            )),
        }
    }
    if replayed.torn_tail {
        warnings.push(
            "history journal ended inside a record; the unfinished step was dropped".to_string(),
        );
    }
    if replayed.gapped {
        warnings.push(
            "history journal is missing a record; resumed from the prefix it could verify"
                .to_string(),
        );
    }
    if !replayed.lost_calls.is_empty() {
        warnings.push(format!(
            "{} tool call(s) lost their outcome when the session died and are marked unresolved: {}",
            replayed.lost_calls.len(),
            replayed.lost_calls.join(", ")
        ));
    }
    let mut conv = Conversation::with_sink(std::sync::Arc::new(
        crate::history_journal::JournalSink::new(journal, next_seq),
    ));
    if replayed.entries.is_empty() {
        push_text_seed(&mut conv, initial_history);
    } else if snapshot_durable {
        // The rewrite already stored this snapshot at seq 0.
        conv.install_unjournaled(replayed.entries);
    } else {
        // One replace record rather than re-appending every entry: replaying
        // a journal must not lengthen it.
        conv.replace(replayed.entries);
    }
    conv
}

/// Import the caller's flattened history (the text-level resume shape).
fn push_text_seed(conv: &mut Conversation, initial_history: &[(bool, String)]) {
    for (from_model, text) in initial_history {
        conv.push(
            if *from_model {
                state_store::Role::Assistant
            } else {
                state_store::Role::User
            },
            text.clone(),
        );
    }
}

/// Tools a child run may never invoke. Spawn tools would recurse past the
/// depth cap. `compact_context` writes the session compaction slot, and
/// only the parent turn drains that slot — a child request would compact
/// the parent's history.
const CHILD_FORBIDDEN_TOOLS: [&str; 7] = [
    "task",
    "skill",
    "task_continue",
    "child_spawn",
    "workflow_run",
    "ralph_run",
    "compact_context",
];

/// The credential values the journal redaction gate masks: every
/// provider's env-var key (read now) and inline key. Best-effort
/// hygiene — only values known to config/env are masked — but it keeps
/// provider credentials from surviving into readable session history.
pub fn build_secret_store(config: &wavecode_config::Config) -> safety_secrets::SecretsStore {
    let providers = config.model_providers.values().collect::<Vec<_>>();
    let env_names: Vec<&str> = providers
        .iter()
        .filter_map(|provider| provider.env_key.as_deref())
        .collect();
    let mut store = safety_secrets::SecretsStore::from_env(&env_names);
    for (index, provider) in providers.iter().enumerate() {
        if let Some(key) = &provider.api_key {
            store.insert(format!("inline-{index}"), key.clone());
        }
    }
    store
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

/// Late registrations that need the finished child service: the skill
/// surface, task observability, the shared question gate tool, and the
/// free-form delegation surface.
fn register_task_tools(
    registry: &wavecode_tools::Registry,
    skill_set: Arc<wavecode_skills::SkillSet>,
    task_service: Arc<dyn action_tasks::TaskService>,
) {
    registry.register(Arc::new(SkillTool::new(skill_set, task_service.clone())));
    registry.register(Arc::new(TaskOutputTool::new(task_service.clone())));
    registry.register(Arc::new(TaskStopTool::new(task_service.clone())));
    registry.register(Arc::new(TaskContinueTool::new(task_service.clone())));
    // `ask_user` parks on the shared question gate: the sandbox routes valid
    // payloads to the question flow before mode policy, so this tool's body
    // only runs for schema errors or bypassed gates.
    registry.register(Arc::new(wavecode_tools::AskUserTool));
    // `task` is the free-form delegation surface (named agent definitions
    // resolve per call from the tool's cwd, so registration needs only the
    // shared child handle).
    registry.register(Arc::new(wavecode_tools::TaskTool::new(task_service)));
}

/// LSP tools get one shared, registry-backed providers handle: pooled
/// servers survive across calls, and diagnostics pushed while any LSP
/// tool call is in flight land in the same store `lsp_diagnostics`
/// reads back. Re-registration replaces the builtin no-registry tools.
fn register_lsp_tools(registry: &wavecode_tools::Registry, cwd: &Path, deny_env: &[String]) {
    let lsp_providers = Arc::new(wavecode_tools::LspProviders::new(
        cwd.to_path_buf(),
        deny_env.to_vec(),
    ));
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
}

/// Workflow engine (fan-out DAG runs plus Ralph loops) and durable
/// schedules ride the same child handle: schedules persist under
/// `<home>/.wavecode/schedule.json` so cron entries survive restarts.
/// Missed fires during downtime are not replayed — each entry fires at
/// its next cron occurrence; running jobs are never persisted, so a
/// previous process's in-flight work reads as interrupted.
fn register_workflow_schedule_tools(
    registry: &wavecode_tools::Registry,
    task_service: Arc<dyn action_tasks::TaskService>,
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) {
    let scheduler_state = match home {
        Some(home_dir) => runtime_scheduler::Scheduler::load_or_default(home_dir),
        None => runtime_scheduler::Scheduler::default(),
    };
    if scheduler_state.degraded() {
        warnings.push(
            "schedule store is unreadable; schedule changes are disabled until the file is removed"
                .to_string(),
        );
    }
    let scheduler = Arc::new(Mutex::new(scheduler_state));
    registry.register(Arc::new(action_workflow::tools::WorkflowRunTool::new(
        task_service.clone(),
    )));
    registry.register(Arc::new(action_workflow::tools::RalphRunTool::new(
        task_service.clone(),
    )));
    registry.register(Arc::new(action_workflow::tools::ScheduleTool::new(
        scheduler,
    )));
}

/// Background shell jobs for long work that must not block the turn.
/// They file completion notices on the same child runtime the actor
/// drains, so results re-enter turns exactly like child tasks.
/// `job_spawn` writes, `job_wait`/`job_output` only observe, and
/// `job_cancel` is destructive (approval-gated). With the job service
/// live, the shell tool promotes a foreground command that outlives its
/// timeout into a background job instead of killing it (the builtin
/// registration carries no handoff); the re-registration keeps the
/// session's shared spill store so promoted runs spill into the same
/// ledger. Returns the service so status views can observe the same
/// jobs the tools write.
fn register_job_tools(
    registry: &wavecode_tools::Registry,
    children: Arc<ChildRuntime>,
    spill_store: Arc<wavecode_context::spill::SpillStore>,
) -> Arc<action_jobs::JobService> {
    let jobs = Arc::new(action_jobs::JobService::new(children));
    registry.register(Arc::new(action_jobs::tools::JobSpawnTool::new(
        jobs.clone(),
    )));
    registry.register(Arc::new(action_jobs::tools::JobWaitTool::new(jobs.clone())));
    registry.register(Arc::new(action_jobs::tools::JobCancelTool::new(
        jobs.clone(),
    )));
    registry.register(Arc::new(action_jobs::tools::JobOutputTool::new(
        jobs.clone(),
    )));
    registry.register(wavecode_tools::shell_with_handoff(
        spill_store,
        Arc::new(action_jobs::tools::ForegroundRuns::new(jobs.clone())),
    ));
    jobs
}

/// File-content snapshots and reviewed plan mode (explore -> present ->
/// approve -> execute) ride the same late handle: both store roots derive
/// from home so resume in the same home reopens the same data. Corrupt
/// content warns and starts empty, never a hard stop. `snapshot` is
/// read-only; `restore` is destructive (approval-gated); the plan tool
/// mutates plan state, not the repo (in-session state, no approval gate
/// in any mode). Returns the snapshot store root so frontend status views
/// read exactly what the tools wrote.
fn register_snapshot_and_plan_tools(
    registry: &wavecode_tools::Registry,
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) -> PathBuf {
    // The store root derives from the session memory root
    // (`<home>/.wavecode/memories` -> `<home>/.wavecode/snapshots`) so
    // injected roots stay hermetic.
    let snapshot_memory_root = home.map(wavecode_memory::MemoryStore::default_root);
    let snapshot_root =
        state_checkpoint::snapshot_store_root_for_session(snapshot_memory_root.as_deref());
    registry.register(Arc::new(crate::snapshot_tools::SnapshotTool::new(
        snapshot_root.clone(),
    )));
    registry.register(Arc::new(crate::snapshot_tools::RestoreTool::new(
        snapshot_root.clone(),
    )));
    let (plan_state, plan_warning) =
        state_plan::tool::load_for_session(home, state_plan::tool::DEFAULT_PLAN_SESSION_ID);
    if let Some(warning) = plan_warning {
        warnings.push(warning);
    }
    let plan_store = Arc::new(state_plan::tool::PlanStore::new(
        plan_state,
        home,
        state_plan::tool::DEFAULT_PLAN_SESSION_ID,
    ));
    registry.register(Arc::new(state_plan::tool::PlanTool::new(plan_store)));
    snapshot_root
}

/// Assemble instruction memory and the persistent index with
/// degradation, returning the combined text, the index text, and the
/// files that went into the concatenation.
fn assemble_memory(
    home: Option<&Path>,
    cwd: &Path,
    warnings: &mut Vec<String>,
) -> (String, String, Vec<PathBuf>) {
    let Some(home) = home else {
        warnings.push("home directory unavailable; memory disabled".to_string());
        return (String::new(), String::new(), Vec::new());
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
    (instruction.combined, index, instruction.sources)
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
