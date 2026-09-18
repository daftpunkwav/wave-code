/*!
 * @file HarnessCli
 * @description Headless exec and interactive REPL over the new stack.
 *
 * Responsibilities:
 * - Assemble sessions from config and arguments.
 * - Stream turns to stdout with progress on stderr.
 * - Prompt for parked approvals interactively in the REPL.
 * - Map terminal outcomes to process exit codes.
 *
 * This binary is the first frontend on the new stack. Interactive
 * frontends (REPL, TUI) port separately; nothing here is shared with
 * the legacy CLI.
 */

//! Headless `wavecode exec`: one prompt in, streamed answer out.
//!
//! Approvals deny openly (nothing can answer), interrupts arrive via
//! Ctrl-C, and every event lands either on stdout (answer text) or
//! stderr (progress, usage, diagnostics).

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use clap::Parser;
use operations_actor::ActorClient;
use operations_bootstrap::{AssembleOptions, DEFAULT_IDENTITY, assemble_session};
use uuid::Uuid;
use wavecode_wire::{EventMsg, Op, Submission};

/// Single-turn headless execution and interactive REPL over the new stack.
#[derive(Debug, Parser)]
#[command(name = "wavecode", version)]
struct Args {
    /// Config file path; defaults to the user-level config.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Model override winning over the configured model.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Permission mode override winning over the configured mode
    /// (`plan`, `auto`, `wave`; legacy names still parse).
    #[arg(long, global = true)]
    permission_mode: Option<String>,

    /// Resume a recorded session by id (fullscreen TUI only).
    #[arg(long, global = true)]
    session: Option<String>,

    /// Continue the most recent session recorded for this directory
    /// (fullscreen TUI only; an explicit `--session` wins).
    #[arg(long, short = 'c', global = true)]
    continue_last: bool,

    /// Subcommand selecting the frontend surface; empty runs the TUI on a
    /// TTY and the REPL otherwise.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Frontend surfaces on the new stack.
#[derive(Debug, Parser)]
enum Command {
    /// Run one prompt and exit with the turn outcome as exit code.
    Exec {
        /// Prompt text for the single turn.
        prompt: String,
        /// Emit JSONL events on stdout, human rendering on stderr.
        #[arg(long)]
        json: bool,
    },
    /// Interactive multi-turn session sharing one conversation.
    Repl,
    /// Resume a legacy session: no id lists recent threads, an id
    /// imports its history and continues interactively.
    Resume {
        /// Legacy thread id (`resume` without id lists threads).
        thread_id: Option<String>,
    },
    /// Expose harness tools over MCP stdio.
    Mcp {
        /// MCP surface to run.
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Inspect installed plugin packs.
    Plugin {
        /// Plugin surface to run.
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Drive sessions over ACP stdio (JSON-RPC).
    ///
    /// Each `session/new` assembles a headless session from config, so
    /// this surface needs provider credentials like `exec`.
    Acp,
}

/// MCP surfaces on the new stack.
#[derive(Debug, Parser)]
enum McpCommand {
    /// Serve the builtin tools over stdio as an MCP server.
    Serve,
}

/// Plugin surfaces on the new stack.
#[derive(Debug, Parser)]
enum PluginCommand {
    /// List installed plugins with skill/MCP/hook counts.
    List,
}

/// Terminal outcome driving the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Turn completed (even with model-level error results inside).
    Completed,
    /// Turn interrupted by Ctrl-C.
    Interrupted,
    /// Turn failed at the harness level.
    Failed,
}

impl Outcome {
    /// Process exit code per outcome.
    fn exit_code(&self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Interrupted => 130,
            Self::Failed => 1,
        }
    }
}

/// Render one event to the text streams.
///
/// Every model- or tool-sourced string (deltas, completions, plan/goal
/// bodies, warnings, errors, tool names, call ids) passes
/// `sanitize_terminal` before reaching the streams, matching the TUI's
/// threat model: ANSI / OSC sequences must not reach the terminal.
///
/// Returns a terminal outcome when the event ends the turn.
fn render_event(msg: &EventMsg, stdout: &mut String, stderr: &mut String) -> Option<Outcome> {
    /// Sanitize into an owned string for `format!`/`push_str` use.
    fn clean(text: &str) -> String {
        console_ui::sanitize_terminal(text).into_owned()
    }
    match msg {
        EventMsg::TurnStarted => {
            stderr.push_str("[turn started]\n");
            None
        }
        EventMsg::AgentMessageDelta { text } => {
            stdout.push_str(&clean(text));
            None
        }
        EventMsg::AgentThinkingDelta { text } => {
            // Thinking is reasoning trace, not answer text: it goes to the
            // human side channel (stderr) and never onto programmatic stdout.
            stderr.push_str(&format!("[think] {}\n", clean(text)));
            None
        }
        EventMsg::AgentMessageComplete { text } => {
            // Deltas already streamed this text; the completion repeats
            // the full message for transcript use. Skip text already on
            // stdout so answers are not printed twice, while still
            // covering senders that emit a completion without deltas.
            // Both sides are sanitized, so the suffix check compares the
            // same bytes the deltas wrote.
            let text = clean(text);
            if !text.is_empty() && !stdout.ends_with(text.as_str()) {
                stdout.push_str(&text);
            }
            if !stdout.is_empty() && !stdout.ends_with('\n') {
                stdout.push('\n');
            }
            None
        }
        EventMsg::ToolCallBegin { call_id, name, .. } => {
            stderr.push_str(&format!("[tool] {} ({})\n", clean(name), clean(call_id)));
            None
        }
        EventMsg::ToolCallEnd {
            call_id, is_error, ..
        } => {
            if *is_error {
                stderr.push_str(&format!("[tool] {} reported an error\n", clean(call_id)));
            }
            None
        }
        EventMsg::ApprovalRequested { call_id, kind, .. } => {
            // Neutral line: exec leaves the denial to the headless gate
            // while the REPL answers below via an inline prompt.
            stderr.push_str(&format!(
                "[approval] {} wants to {}\n",
                clean(call_id),
                approval_what(kind)
            ));
            None
        }
        EventMsg::QuestionRequested { .. } => {
            // Nothing here: the REPL prompt (and the exec headless gate)
            // handle the parked question; the prompt prints the payload.
            None
        }
        EventMsg::TokenCount {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            context_window,
            context_used,
        } => {
            let cache_part = if *cache_read_tokens > 0 || *cache_creation_tokens > 0 {
                format!(" cache_r={cache_read_tokens} cache_w={cache_creation_tokens}")
            } else {
                String::new()
            };
            let context_part = match (context_used, context_window) {
                (Some(used), Some(window)) => format!(" ctx={used}/{window}"),
                _ => String::new(),
            };
            stderr.push_str(&format!(
                "[usage] in={input_tokens} out={output_tokens}{cache_part}{context_part}\n"
            ));
            None
        }
        EventMsg::CompactStarted { trigger } => {
            stderr.push_str(&format!("[compact started: {trigger}]\n"));
            None
        }
        EventMsg::CompactCompleted { summary_tokens } => {
            stderr.push_str(&format!("[compact completed: {summary_tokens} tokens]\n"));
            None
        }
        EventMsg::HistoryRewound { turns } => {
            stderr.push_str(&format!("[history rewound: {turns} turns]\n"));
            None
        }
        EventMsg::PlanProposed { text } => {
            stderr.push_str(&format!("[plan proposed]\n{}\n", clean(text)));
            None
        }
        EventMsg::PlanApproved => {
            stderr.push_str("[plan approved]\n");
            None
        }
        EventMsg::GoalSet { objective } => {
            stderr.push_str(&format!("[goal set]\n{}\n", clean(objective)));
            None
        }
        EventMsg::GoalCompleted => {
            stderr.push_str("[goal completed]\n");
            None
        }
        EventMsg::Warning { message } => {
            stderr.push_str(&format!("[warn] {}\n", clean(message)));
            None
        }
        EventMsg::Error {
            message,
            recoverable,
        } => {
            stderr.push_str(&format!("[error] {}\n", clean(message)));
            if *recoverable {
                None
            } else {
                Some(Outcome::Failed)
            }
        }
        EventMsg::TurnCompleted { interrupted } => Some(if *interrupted {
            Outcome::Interrupted
        } else {
            Outcome::Completed
        }),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir()?;
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    // `--session` / `--continue` seed only the fullscreen TUI; every
    // other surface says so instead of silently dropping the request.
    if (args.session.is_some() || args.continue_last)
        && !(args.command.is_none() && std::io::stdout().is_terminal())
    {
        eprintln!("[warn] --session/--continue resume the fullscreen TUI only; ignored here");
    }
    // Bare invocation: fullscreen TUI on a TTY, line REPL otherwise.
    if args.command.is_none() {
        if std::io::stdout().is_terminal() {
            run_tui_new(
                args.config,
                args.model,
                args.permission_mode,
                args.session,
                args.continue_last,
                cwd,
                home,
            )
            .await?;
            std::process::exit(Outcome::Completed.exit_code())
        }
        let settings = console_ui::settings::UiSettings::load();
        let (model_override, provider_override) =
            model_provider_overrides(args.model.as_deref(), &settings);
        let mut handle = assemble_session(AssembleOptions {
            config_path: args.config,
            model_override,
            provider_override,
            permission_override: args.permission_mode.clone(),
            thinking_override: settings.default_effort.clone(),
            cwd,
            home: home.clone(),
            identity: DEFAULT_IDENTITY.to_string(),
            wave_denylist: settings.wave_denylist,
            headless: false,
            initial_history: Vec::new(),
        })
        .map_err(|e| anyhow::anyhow!("session assembly failed: {e}"))?;
        handle.connect_mcp_servers().await;
        for warning in &handle.warnings {
            eprintln!("[warn] {warning}");
        }
        run_repl(
            &mut handle.client,
            &handle.memory_index,
            &handle.mcp_servers,
            &handle.skill_names,
            handle.status.as_ref(),
            &handle.permission_mode,
        )
        .await?;
        std::process::exit(Outcome::Completed.exit_code())
    }
    // Credential-free surfaces return before assembly: `mcp serve` and
    // `plugin list` expose local tools and metadata, so they must work
    // with no provider configured.
    if matches!(
        args.command,
        Some(Command::Mcp {
            command: McpCommand::Serve
        })
    ) {
        run_mcp_serve(cwd).await?;
        std::process::exit(Outcome::Completed.exit_code())
    }
    if matches!(
        args.command,
        Some(Command::Plugin {
            command: PluginCommand::List
        })
    ) {
        run_plugin_list(home);
        std::process::exit(Outcome::Completed.exit_code())
    }
    // ACP serves headless sessions over stdio until EOF. Sessions
    // assemble lazily per `session/new`, so this returns before the
    // shared assembly below (which would build a throwaway session).
    // Like `exec`, it needs provider credentials from config.
    if matches!(args.command, Some(Command::Acp)) {
        run_acp(
            args.config.clone(),
            args.model.clone(),
            args.permission_mode.clone(),
            cwd.clone(),
            home.clone(),
        )
        .await?;
        std::process::exit(Outcome::Completed.exit_code())
    }
    // Only headless exec denies approvals openly: the REPL parks them on
    // the gate and answers inline, which needs parking enabled here.
    let headless = matches!(args.command, Some(Command::Exec { .. }));
    let settings = console_ui::settings::UiSettings::load();
    let (model_override, provider_override) =
        model_provider_overrides(args.model.as_deref(), &settings);
    let mut handle = assemble_session(AssembleOptions {
        config_path: args.config,
        model_override,
        provider_override,
        permission_override: args.permission_mode.clone(),
        thinking_override: settings.default_effort.clone(),
        cwd,
        home: home.clone(),
        wave_denylist: console_ui::settings::UiSettings::load().wave_denylist,
        identity: DEFAULT_IDENTITY.to_string(),
        headless,
        initial_history: Vec::new(),
    })
    .map_err(|e| anyhow::anyhow!("session assembly failed: {e}"))?;
    handle.connect_mcp_servers().await;
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }

    match args.command {
        Some(Command::Exec { prompt, json }) => {
            let outcome = run_exec(&mut handle.client, &prompt, json).await?;
            std::process::exit(outcome.exit_code())
        }
        Some(Command::Repl) => {
            run_repl(
                &mut handle.client,
                &handle.memory_index,
                &handle.mcp_servers,
                &handle.skill_names,
                handle.status.as_ref(),
                &handle.permission_mode,
            )
            .await?;
            std::process::exit(Outcome::Completed.exit_code())
        }
        Some(Command::Resume { thread_id }) => {
            run_resume(thread_id, args.permission_mode, home).await?;
            std::process::exit(Outcome::Completed.exit_code())
        }
        // Mcp/Plugin/Acp return before assembly above.
        Some(Command::Mcp { .. }) | Some(Command::Plugin { .. }) | Some(Command::Acp) => {
            unreachable!("early-return surfaces never reach assembly")
        }
        None => unreachable!("bare invocation returns above"),
    }
}

/// Session seed resolved from `--session` / `--continue`.
#[derive(Debug)]
struct TuiSeed {
    session_id: String,
    title: Option<String>,
    history: Vec<(bool, String)>,
}

/// Resolve the resume seed: an explicit `--session <id>` wins over
/// `--continue` (most recent session recorded for this directory).
/// Unknown ids and missing sessions surface as errors with guidance.
fn resolve_tui_seed(
    session: &Option<String>,
    continue_last: bool,
    cwd: &Path,
    home: &Option<PathBuf>,
) -> anyhow::Result<Option<TuiSeed>> {
    use state_persistence::sessions::{list_sessions, load_session_history};
    let Some(home) = home else {
        // A missing home cannot honor an explicit resume request; only
        // the flagless case stays a silent fresh session.
        if session.is_some() || continue_last {
            anyhow::bail!("home directory unavailable; cannot resume sessions");
        }
        return Ok(None);
    };
    if let Some(id) = session {
        if !state_persistence::sessions::is_valid_session_id(id) {
            anyhow::bail!("invalid session id {id:?}");
        }
        let history = load_session_history(home, id)
            .map_err(|e| anyhow::anyhow!("cannot load session {id}: {e}"))?;
        let title = list_sessions(home)
            .into_iter()
            .find(|meta| meta.id == *id)
            .and_then(|meta| (!meta.title.is_empty()).then_some(meta.title));
        return Ok(Some(TuiSeed {
            session_id: id.clone(),
            title,
            history,
        }));
    }
    if continue_last {
        let cwd_text = cwd.to_string_lossy().to_string();
        let target = list_sessions(home)
            .into_iter()
            .find(|meta| meta.cwd == cwd_text);
        let Some(meta) = target else {
            anyhow::bail!("no sessions to continue under {cwd_text}; starting a fresh session");
        };
        let history = load_session_history(home, &meta.id)
            .map_err(|e| anyhow::anyhow!("cannot load session {}: {e}", meta.id))?;
        return Ok(Some(TuiSeed {
            session_id: meta.id,
            title: (!meta.title.is_empty()).then_some(meta.title),
            history,
        }));
    }
    Ok(None)
}

/// Model catalog for the `/model` picker: the config `[models]` aliases
/// plus one entry for the configured default model. Pure conversion;
/// unknown provider ids pass through (the picker's live switch refuses
/// them via the same-provider check).
fn build_model_entries(
    config: &wavecode_config::Config,
    provider_id: &str,
    model_name: &str,
    effort: Option<&str>,
) -> Vec<console_ui::dialogs::ModelEntryView> {
    use console_ui::dialogs::ModelEntryView;
    let mut entries: Vec<ModelEntryView> = config
        .models
        .iter()
        .map(|(alias, entry)| ModelEntryView {
            label: alias.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            effort: entry.reasoning_effort.clone(),
        })
        .collect();
    let default_here = ModelEntryView {
        label: model_name.to_string(),
        provider: provider_id.to_string(),
        model: model_name.to_string(),
        effort: effort.map(str::to_string),
    };
    if !entries
        .iter()
        .any(|entry| entry.label == default_here.label && entry.provider == default_here.provider)
    {
        entries.push(default_here);
    }
    entries
}

/// Thinking levels the picker offers: OpenAI-compatible providers take a
/// string reasoning effort; budget-driven Anthropic thinking is
/// config-only, so the row hides there.
fn thinking_levels_for(config: &wavecode_config::Config, provider_id: &str) -> Vec<String> {
    match config.model_providers.get(provider_id) {
        Some(provider) if provider.kind == wavecode_config::ProviderKind::OpenAiCompatible => {
            vec![
                "off".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ]
        }
        _ => Vec::new(),
    }
}

/// Build the UI context from a live handle.
#[allow(clippy::too_many_arguments)]
fn ui_ctx_of(
    handle: &operations_bootstrap::SessionHandle,
    cwd: PathBuf,
    home: &Option<PathBuf>,
    session_id: String,
    title: Option<String>,
    model_entries: Vec<console_ui::dialogs::ModelEntryView>,
    thinking_levels: Vec<String>,
) -> console_ui::UiContext {
    console_ui::UiContext {
        model_name: handle.model_name.clone(),
        provider_id: handle.provider_id.clone(),
        thinking_effort: handle.thinking_effort.clone(),
        thinking_levels,
        cwd,
        permission_mode: handle.permission_mode.clone(),
        skill_names: handle.skill_names.clone(),
        mcp_servers: handle.mcp_servers.clone(),
        status: handle.status.clone(),
        session_id,
        session_title: title,
        model_entries,
        home: home.clone(),
    }
}

/// On-demand session factory for resume and `/new`: assembles a session
/// from the same inputs as the initial one and connects MCP servers.
/// `block_in_place` is safe here: the console run loop stays on the
/// multi-thread runtime worker and never blocks a worker while turns run.
#[allow(clippy::too_many_arguments)]
fn make_tui_factory(
    config_path: Option<PathBuf>,
    model: Option<String>,
    permission_mode: Option<String>,
    cwd: PathBuf,
    home: Option<PathBuf>,
    model_entries: Vec<console_ui::dialogs::ModelEntryView>,
    thinking_levels: Vec<String>,
) -> std::sync::Arc<console_ui::SessionFactory> {
    std::sync::Arc::new(move |spec: &console_ui::LaunchSpec| {
        let runtime = tokio::runtime::Handle::current();
        let config_path = config_path.clone();
        let model = model.clone();
        let permission_mode = permission_mode.clone();
        let cwd = cwd.clone();
        let home = home.clone();
        let model_entries = model_entries.clone();
        let thinking_levels = thinking_levels.clone();
        let history = spec.history.clone();
        let resume_id = spec.session_id.clone();
        let readonly = spec.readonly;
        let model_hint = spec.model_override.clone();
        tokio::task::block_in_place(|| {
            runtime.block_on(async move {
                let settings = console_ui::settings::UiSettings::load();
                // The provider pairing keys on the CLI model, not the
                // launch hint: the live /model choice never crosses
                // providers (same-provider check), so it samples through
                // whatever provider the original assembly resolved to.
                let (base_model, provider_override) =
                    model_provider_overrides(model.as_deref(), &settings);
                let mut handle = assemble_session(AssembleOptions {
                    config_path,
                    // A launch hint (the live /model choice) wins; a
                    // read-only side session runs in plan mode so
                    // approvals and destructive work are impossible by
                    // mode, never by trust.
                    model_override: model_hint.or(base_model),
                    provider_override,
                    permission_override: if readonly {
                        Some("plan".to_string())
                    } else {
                        permission_mode
                    },
                    thinking_override: settings.default_effort.clone(),
                    wave_denylist: settings.wave_denylist,
                    cwd: cwd.clone(),
                    home: home.clone(),
                    identity: DEFAULT_IDENTITY.to_string(),
                    headless: false,
                    initial_history: history.clone(),
                })
                .map_err(|e| format!("session assembly failed: {e}"))?;
                handle.connect_mcp_servers().await;
                let session_id = resume_id.unwrap_or_else(|| Uuid::new_v4().to_string());
                let title = home.as_deref().and_then(|home_root| {
                    state_persistence::sessions::list_sessions(home_root)
                        .into_iter()
                        .find(|meta| meta.id == session_id)
                        .and_then(|meta| (!meta.title.is_empty()).then_some(meta.title))
                });
                let ctx = ui_ctx_of(
                    &handle,
                    cwd.clone(),
                    &home,
                    session_id,
                    title,
                    model_entries,
                    thinking_levels,
                );
                Ok(console_ui::SessionLaunch {
                    link: Box::new(handle.client),
                    ctx,
                    history,
                })
            })
        })
    })
}

/// Fullscreen TUI over a live harness session.
///
/// Interactive approval parking works here (headless stays false);
/// config failures keep the exit-code-2-with-guidance contract.
/// `--session` / `--continue` seed the conversation from the recorded
/// session journal; every launch (initial or resumed) journals its
/// turns under `~/.wavecode/sessions/`.
async fn run_tui_new(
    config: Option<PathBuf>,
    model: Option<String>,
    permission_mode: Option<String>,
    session: Option<String>,
    continue_last: bool,
    cwd: PathBuf,
    home: Option<PathBuf>,
) -> anyhow::Result<()> {
    let settings = console_ui::settings::UiSettings::load();
    // Model precedence: CLI > saved picker default > config; the
    // provider rides the saved default (see `model_provider_overrides`).
    let (model_override, provider_override) = model_provider_overrides(model.as_deref(), &settings);
    // Derive the picker catalog and thinking levels from the config the
    // session will assemble from; a broken config degrades to an empty
    // catalog (assembly below reports the real error).
    let (model_entries, thinking_levels) = match load_config_opt(config.as_deref()) {
        Ok(cfg) => {
            let provider_id = provider_override
                .clone()
                .unwrap_or_else(|| cfg.model_provider.clone());
            let effort = settings.default_effort.clone().or_else(|| {
                cfg.model_providers
                    .get(&provider_id)
                    .and_then(|p| p.reasoning_effort.clone())
            });
            (
                build_model_entries(
                    &cfg,
                    &provider_id,
                    &effective_model(&model_override, &cfg),
                    effort.as_deref(),
                ),
                thinking_levels_for(&cfg, &provider_id),
            )
        }
        Err(_) => (Vec::new(), Vec::new()),
    };
    // Resume seed: an explicit --session errors hard; --continue
    // degrades to a fresh session with a warning (kimi semantics).
    let seed = match (&session, continue_last) {
        (Some(_), _) => resolve_tui_seed(&session, false, &cwd, &home)?,
        (None, true) => match resolve_tui_seed(&None, true, &cwd, &home) {
            Ok(seed) => seed,
            Err(error) => {
                eprintln!("[warn] {error}");
                None
            }
        },
        _ => None,
    };
    let (session_id, title, initial_history) = match &seed {
        Some(seed) => (
            seed.session_id.clone(),
            seed.title.clone(),
            seed.history.clone(),
        ),
        None => (Uuid::new_v4().to_string(), None, Vec::new()),
    };
    let mut handle = match assemble_session(AssembleOptions {
        config_path: config.clone(),
        model_override: model_override.clone(),
        provider_override,
        permission_override: permission_mode.clone(),
        thinking_override: settings.default_effort.clone(),
        cwd: cwd.clone(),
        home: home.clone(),
        identity: DEFAULT_IDENTITY.to_string(),
        headless: false,
        initial_history,
        wave_denylist: settings.wave_denylist,
    }) {
        Ok(handle) => handle,
        Err(operations_bootstrap::SessionError::Config(e)) => {
            print_config_error(&e);
            std::process::exit(2)
        }
    };
    handle.connect_mcp_servers().await;
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }
    let ctx = ui_ctx_of(
        &handle,
        cwd.clone(),
        &home,
        session_id,
        title,
        model_entries.clone(),
        thinking_levels.clone(),
    );
    let factory = make_tui_factory(
        config,
        model,
        permission_mode,
        cwd,
        home,
        model_entries,
        thinking_levels,
    );
    console_ui::run_with_factory(handle.client, ctx, Some(factory)).await
}

/// Model/provider override pairing shared by every assembly path.
///
/// Precedence is CLI model > saved picker default > config. A saved
/// default may live on another provider, so the saved provider rides
/// along only with the saved default model; an explicit CLI model keeps
/// the configured provider (it names a model on that provider).
fn model_provider_overrides(
    cli_model: Option<&str>,
    settings: &console_ui::settings::UiSettings,
) -> (Option<String>, Option<String>) {
    let model = cli_model
        .map(str::to_string)
        .or_else(|| settings.default_model.clone());
    let provider = if cli_model.is_some() {
        None
    } else {
        settings.default_provider.clone()
    };
    (model, provider)
}

/// Load the config for catalog derivation, from `path` or the default
/// location.
fn load_config_opt(
    path: Option<&std::path::Path>,
) -> Result<wavecode_config::Config, wavecode_config::ConfigError> {
    match path {
        Some(path) => wavecode_config::Config::load_from(path),
        None => wavecode_config::Config::load(),
    }
}

/// The effective model name: override or config default.
fn effective_model(model_override: &Option<String>, config: &wavecode_config::Config) -> String {
    model_override
        .clone()
        .unwrap_or_else(|| config.model.clone())
}

/// Serve the builtin tool registry over MCP stdio until EOF.
///
/// Needs no model credentials: it exposes local tools, so it returns
/// before session assembly.
async fn run_mcp_serve(cwd: PathBuf) -> anyhow::Result<()> {
    let (registry, _todos) = wavecode_tools::Registry::builtin_with_todos();
    operations_bootstrap::mcp_serve::run_stdio_server(
        std::sync::Arc::new(registry),
        wavecode_tools::ToolCtx {
            cwd,
            deny_env: Vec::new(),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("mcp server failed: {e}"))
}

/// Drive ACP sessions over stdio until EOF.
///
/// Like `exec`, sessions assemble headless from config, so provider
/// credentials are required; unlike `exec`, turns arrive as
/// `session/prompt` requests instead of CLI arguments.
async fn run_acp(
    config: Option<PathBuf>,
    model: Option<String>,
    permission_mode: Option<String>,
    cwd: PathBuf,
    home: Option<PathBuf>,
) -> anyhow::Result<()> {
    operations_bootstrap::acp::run_stdio_server(operations_bootstrap::acp::AcpServerOptions {
        config_path: config,
        model_override: model,
        permission_override: permission_mode,
        cwd,
        home,
    })
    .await
    .map_err(|e| anyhow::anyhow!("acp server failed: {e}"))
}

/// List installed plugin packs with skill/MCP/hook counts.
fn run_plugin_list(home: Option<PathBuf>) {
    let loader = wavecode_skills::plugin::PluginLoader::load(home.as_deref());
    for warning in loader.warnings() {
        eprintln!("[warn] {warning}");
    }
    if loader.plugins().is_empty() {
        println!("(no plugins installed)");
        return;
    }
    for plugin in loader.plugins() {
        println!(
            "{} {} (skills: {}, mcp servers: {}, hooks: {})",
            plugin.name,
            plugin.version,
            plugin.skill_count(),
            plugin.mcp_count(),
            plugin.hook_count()
        );
    }
}

/// Print a config error with creation guidance on missing files.
fn print_config_error(err: &wavecode_config::ConfigError) {
    eprintln!("Error: {err}");
    if let wavecode_config::ConfigError::NotFound(path) = err {
        eprintln!(
            r#"
Please create config file {}, example contents:

model = "claude-sonnet-4-5"
model_provider = "anthropic"

[model_providers.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com"
# api key, choose one (env_key wins):
# Option 1 (recommended): env_key names an env var, read at runtime
env_key = "ANTHROPIC_API_KEY"
# Option 2: inline api_key (keep secret, do not commit)
# api_key = "sk-ant-..."
"#,
            path.display()
        );
    }
}

/// Resume a legacy session from rollout journals.
///
/// Without an id, lists recent threads newest-first. With an id, imports
/// its history into a fresh assembly and continues in the REPL. History
/// import is plain text: tool payloads collapse to bracketed lines.
async fn run_resume(
    thread_id: Option<String>,
    permission_mode: Option<String>,
    home: Option<PathBuf>,
) -> anyhow::Result<()> {
    use state_persistence::legacy::{default_root, list_threads, load_history};

    let Some(home) = home else {
        return Err(anyhow::anyhow!(
            "home directory unavailable; resume needs ~/.wavecode/threads"
        ));
    };
    let root = default_root(&home);
    let Some(id) = thread_id else {
        let threads = list_threads(&root)?;
        if threads.is_empty() {
            println!("(no previous sessions; directory: {})", root.display());
            return Ok(());
        }
        println!("recent sessions, newest first:");
        for thread in &threads {
            let preview = thread
                .first_user_text
                .as_deref()
                .unwrap_or("(no user messages)");
            println!(
                "  {}  {} messages / {} compactions  {}",
                thread.thread_id, thread.message_count, thread.compaction_count, preview
            );
        }
        println!("\nresume with: `wavecode resume <thread-id>`");
        return Ok(());
    };
    let history = load_history(&root, &id)?;
    println!("resumed {id} ({} messages)", history.len());
    let cwd = std::env::current_dir()?;
    let mut handle = assemble_session(AssembleOptions {
        config_path: None,
        model_override: None,
        provider_override: None,
        permission_override: permission_mode,
        thinking_override: None,
        cwd,
        home: Some(home),
        identity: DEFAULT_IDENTITY.to_string(),
        // Resumed sessions continue interactively: park approvals for
        // inline answers instead of denying them openly.
        headless: false,
        initial_history: history,
        wave_denylist: console_ui::settings::UiSettings::load().wave_denylist,
    })
    .map_err(|e| anyhow::anyhow!("session assembly failed: {e}"))?;
    handle.connect_mcp_servers().await;
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }
    run_repl(
        &mut handle.client,
        &handle.memory_index,
        &handle.mcp_servers,
        &handle.skill_names,
        handle.status.as_ref(),
        &handle.permission_mode,
    )
    .await
}

/// One parsed REPL line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Slash {
    /// End the session.
    Quit,
    /// Compact now via an idle operation.
    Compact,
    /// Show the persistent memory index.
    Memory,
    /// List configured MCP servers.
    Mcp,
    /// Show reviewed-plan status (local display only, mirrors /mcp).
    Plan(String),
    /// Show durable-goal status (local display only, mirrors /plan).
    Goal(String),
    /// List file-content snapshot labels (local display).
    Snapshots,
    /// Show one file-content snapshot (local display; the rewind itself
    /// runs through the `restore` tool with approval).
    Rewind(String),
    /// Cycle the session permission mode.
    Permissions,
    /// Show help text.
    Help,
    /// Unknown slash command with its name and trailing arguments.
    Unknown { name: String, args: String },
    /// Plain user input for the next turn.
    Text(String),
}

/// Parse one REPL line into slash commands or plain text.
fn parse_slash(line: &str) -> Slash {
    let trimmed = line.trim();
    if let Some(command) = trimmed.strip_prefix('/') {
        let mut parts = command.split_whitespace();
        let name = parts.next().unwrap_or("");
        let args = parts.collect::<Vec<_>>().join(" ");
        return match name {
            "quit" | "exit" => Slash::Quit,
            "compact" => Slash::Compact,
            "memory" => Slash::Memory,
            "mcp" => Slash::Mcp,
            "plan" => Slash::Plan(args),
            "goal" => Slash::Goal(args),
            "snapshots" => Slash::Snapshots,
            "rewind" => Slash::Rewind(args),
            "permissions" => Slash::Permissions,
            "help" => Slash::Help,
            _ => Slash::Unknown {
                name: name.to_string(),
                args,
            },
        };
    }
    Slash::Text(trimmed.to_string())
}

/// REPL help text printed for `/help` and unknown commands.
const REPL_HELP: &str = "commands: /compact (compress context now), /memory (show memory index), /mcp (list servers), /plan (show reviewed-plan status), /goal (show durable-goal status), /snapshots (list snapshots), /rewind <label> (show snapshot), /permissions (cycle approval mode), /quit (end session), /help";

/// Guided turn text routing a slash-invoked skill through the model.
///
/// No wire change is needed: the skill catalog is already in context and
/// the `skill` tool executes with proper inline/fork routing, so naming
/// the skill plus its arguments deterministically triggers it.
fn skill_request(name: &str, args: &str) -> String {
    if args.trim().is_empty() {
        format!("Please use the '{name}' skill for this request.")
    } else {
        format!(
            "Please use the '{name}' skill for this request. Arguments: {}",
            args.trim()
        )
    }
}

/// Permission modes in `/permissions` cycle order (wire names; legacy
/// names such as `guarded` are input aliases, not cycle steps).
const PERMISSION_CYCLE: &[&str] = &["plan", "auto", "wave"];

/// Next mode in the cycle order, wrapping around.
fn next_mode(current: &str) -> &'static str {
    let pos = PERMISSION_CYCLE
        .iter()
        .position(|m| *m == current)
        .unwrap_or(0);
    PERMISSION_CYCLE[(pos + 1) % PERMISSION_CYCLE.len()]
}

/// Drive one turn to completion, streaming answer text to stdout.
///
/// With `json`, stdout carries one JSON event per line while the human
/// rendering falls back to stderr (legacy exec contract); otherwise
/// stdout carries the answer text.
async fn run_exec(client: &mut ActorClient, prompt: &str, json: bool) -> anyhow::Result<Outcome> {
    client
        .submit(Submission {
            id: "exec-1".to_string(),
            op: Op::UserInput {
                text: prompt.to_string(),
            },
        })
        .await
        .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;

    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    let mut json_lines = String::new();
    let mut failed = false;
    let mut interrupted = false;
    let outcome = loop {
        tokio::select! {
            event = client.next_event() => {
                let Some(event) = event else {
                    // Actor exited without TurnCompleted: treat as failure.
                    break Outcome::Failed;
                };
                if json {
                    json_lines.push_str(
                        &serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string()),
                    );
                    json_lines.push('\n');
                }
                if let Some(end) = render_event(&event.msg, &mut stdout_text, &mut stderr_text) {
                    if end == Outcome::Failed {
                        failed = true;
                    } else {
                        break if interrupted { Outcome::Interrupted } else { end };
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = client
                    .submit(Submission { id: "exec-interrupt".to_string(), op: Op::Interrupt })
                    .await;
                interrupted = true;
            }
        }
    };
    // Graceful shutdown: SessionEnd hooks speak during the bounded
    // drain instead of dying on client drop (Drop aborts the actor).
    // A hung hook cannot hold exit open past the deadline.
    let _ = client
        .submit(Submission {
            id: "exec-shutdown".to_string(),
            op: Op::Shutdown,
        })
        .await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while let Some(event) = tokio::time::timeout_at(deadline, client.next_event())
        .await
        .ok()
        .flatten()
    {
        if json {
            json_lines
                .push_str(&serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string()));
            json_lines.push('\n');
        } else {
            render_event(&event.msg, &mut stdout_text, &mut stderr_text);
        }
    }
    let end = if failed { Outcome::Failed } else { outcome };
    let broken = if json {
        flush_buffers(&json_lines, &stderr_text)?
    } else {
        flush_buffers(&stdout_text, &stderr_text)?
    };
    Ok(if broken {
        // Closed stdout pipe (e.g. `| head`): the user took what they
        // needed; a clean end, not an error.
        Outcome::Completed
    } else {
        end
    })
}

/// Flush buffered streams with locked handles.
///
/// Returns true on a closed pipe (stdout or stderr); panicking `print!`
/// would turn `| head` into a crash, while `writeln!` lets a broken pipe
/// read as a clean end.
fn flush_buffers(stdout_text: &str, stderr_text: &str) -> std::io::Result<bool> {
    let mut out = std::io::stdout().lock();
    match out
        .write_all(stdout_text.as_bytes())
        .and_then(|()| out.flush())
    {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(true),
        Err(e) => return Err(e),
    }
    let mut err = std::io::stderr().lock();
    match err
        .write_all(stderr_text.as_bytes())
        .and_then(|()| err.flush())
    {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(true),
        Err(e) => return Err(e),
    }
    Ok(false)
}

/// Interactive multi-turn session over one shared conversation.
///
/// Turns accumulate in the actor-side conversation; `/compact` runs an
/// idle compaction between turns. Ctrl-C at the prompt starts a fresh
/// line; Ctrl-C mid-turn interrupts the running turn.
async fn run_repl(
    client: &mut ActorClient,
    memory_index: &str,
    mcp_servers: &[String],
    skill_names: &[String],
    status: &dyn operations_actor::StatusQueries,
    initial_permission_mode: &str,
) -> anyhow::Result<()> {
    use rustyline::error::ReadlineError;

    let mut editor = rustyline::DefaultEditor::new()?;
    let mut turn: u64 = 0;
    // Start from the assembled session mode: a hardcoded default here
    // would desync the local copy from the policy, so the first
    // `/permissions` cycle could jump a plan session straight to auto.
    let mut permission_mode: &str = initial_permission_mode;
    println!("wavecode repl (permission: {permission_mode}): {REPL_HELP}");
    loop {
        let line = match editor.readline("> ") {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("input failed: {e}")),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(trimmed);
        match parse_slash(trimmed) {
            Slash::Quit => break,
            Slash::Help => println!("{REPL_HELP}"),
            Slash::Unknown { name, args } => {
                if skill_names.iter().any(|n| n == &name) {
                    turn += 1;
                    client
                        .submit(Submission {
                            id: format!("repl-{turn}-skill"),
                            op: Op::UserInput {
                                text: skill_request(&name, &args),
                            },
                        })
                        .await
                        .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                    drain_turn(client, &mut editor).await?;
                } else {
                    println!("unknown command /{name}; {REPL_HELP}");
                }
            }
            Slash::Memory => {
                if memory_index.trim().is_empty() {
                    println!("(memory unavailable: no index assembled)");
                } else {
                    println!("{memory_index}");
                }
            }
            Slash::Mcp => {
                if mcp_servers.is_empty() {
                    println!("(no MCP servers configured)");
                } else {
                    for server in mcp_servers {
                        println!("- {server}");
                    }
                }
            }
            // Reviewed plan mode is local display only (mirrors /mcp):
            // state comes from the assembly-side queries, so the REPL
            // renders current status without knowing the storage layout;
            // the model mutates it through the plan tool.
            Slash::Plan(_) => match status.plan_status() {
                Some(text) => println!("{text}"),
                None => println!(
                    "(no reviewed plan yet; ask the agent to propose one with the plan tool)"
                ),
            },
            // Durable goal mode is local display only (mirrors /plan):
            // state comes from the assembly-side queries; the model
            // mutates it through the goal tool.
            Slash::Goal(_) => match status.goal_status() {
                Some(text) => println!("{text}"),
                None => {
                    println!("(no durable goal yet; ask the agent to set one with the goal tool)")
                }
            },
            // File-content snapshots (no git dependence): `/snapshots`
            // lists labels, `/rewind <label>` shows one snapshot.
            // Both are local display only; the actual rewind runs
            // through the `restore` tool (approval-gated).
            Slash::Snapshots => {
                let labels = status.snapshot_labels();
                if labels.is_empty() {
                    println!(
                        "(no snapshots yet; ask the agent to capture one with the snapshot tool)"
                    );
                } else {
                    for label in &labels {
                        println!("{label}");
                    }
                }
            }
            Slash::Rewind(label) => {
                let label = label.trim();
                if label.is_empty() {
                    println!("usage: /rewind <label> (see /snapshots for labels)");
                } else {
                    match status.snapshot_summary(label) {
                        Some(summary) => println!("{summary}"),
                        None => println!("unknown snapshot {label:?} (see /snapshots for labels)"),
                    }
                    println!("(rewind itself runs through the restore tool and needs approval)");
                }
            }
            Slash::Permissions => {
                permission_mode = next_mode(permission_mode);
                turn += 1;
                client
                    .submit(Submission {
                        id: format!("repl-{turn}-mode"),
                        op: Op::SetPermissionMode {
                            mode: permission_mode.to_string(),
                        },
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                println!("(permission mode: {permission_mode})");
            }
            Slash::Compact => {
                turn += 1;
                client
                    .submit(Submission {
                        id: format!("repl-{turn}-compact"),
                        op: Op::Compact,
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                drain_turn(client, &mut editor).await?;
            }
            Slash::Text(text) => {
                turn += 1;
                client
                    .submit(Submission {
                        id: format!("repl-{turn}"),
                        op: Op::UserInput { text },
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                drain_turn(client, &mut editor).await?;
            }
        }
    }
    Ok(())
}

/// Human phrase for an approval kind, shared by progress lines and prompts.
fn approval_what(kind: &wavecode_wire::ApprovalKind) -> &'static str {
    match kind {
        wavecode_wire::ApprovalKind::Exec => "execute a command",
        wavecode_wire::ApprovalKind::Write => "modify files",
    }
}

/// Map one approval answer line to a wire decision: y/yes approves once,
/// a/always approves with a session rule, anything else (including an
/// empty line) denies without a reason.
fn decide_approval(line: &str) -> wavecode_wire::WireDecision {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => wavecode_wire::WireDecision::AllowOnce,
        "a" | "always" => wavecode_wire::WireDecision::AllowAlways,
        _ => wavecode_wire::WireDecision::Deny {
            reason: String::new(),
        },
    }
}

/// Ask the user about one parked approval on the REPL editor.
///
/// Ctrl-C / Ctrl-D deny without a reason so no wait is left parked; other
/// input errors deny the same way after noting them. Blocking here is
/// safe: the actor parks on the gate with no polling of its own.
fn prompt_approval(
    editor: &mut rustyline::DefaultEditor,
    call_id: &str,
    what: &str,
) -> anyhow::Result<wavecode_wire::WireDecision> {
    use rustyline::error::ReadlineError;
    println!("allow {what} ({call_id})? [y/a/N] ");
    match editor.readline("> ") {
        Ok(line) => Ok(decide_approval(&line)),
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            Ok(wavecode_wire::WireDecision::Deny {
                reason: String::new(),
            })
        }
        Err(e) => {
            eprintln!("[approval] input error, denying: {e}");
            Ok(wavecode_wire::WireDecision::Deny {
                reason: String::new(),
            })
        }
    }
}

/// Ask the user one parked question on the REPL editor.
///
/// A typed number within range selects that option; any other text goes
/// through as a free-form answer; Ctrl-C / Ctrl-D and empty input dismiss
/// the question (surfaced to the model as "not answered", never as a
/// crash). Blocking here is safe: the actor parks on the gate.
fn prompt_question(
    editor: &mut rustyline::DefaultEditor,
    _call_id: &str,
    question: &str,
    options: &[String],
) -> anyhow::Result<String> {
    use rustyline::error::ReadlineError;
    println!("{question}");
    for (index, option) in options.iter().enumerate() {
        println!("  {}. {option}", index + 1);
    }
    if !options.is_empty() {
        println!(
            "answer with an option number (1-{}) or your own text",
            options.len()
        );
    }
    match editor.readline("> ") {
        Ok(line) => {
            let trimmed = line.trim();
            if let Ok(number) = trimmed.parse::<usize>()
                && (1..=options.len()).contains(&number)
            {
                return Ok(options[number - 1].clone());
            }
            Ok(trimmed.to_string())
        }
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => Ok(String::new()),
        Err(e) => {
            eprintln!("[question] input error, dismissing: {e}");
            Ok(String::new())
        }
    }
}

/// Drain events until the turn ends, rendering as they arrive.
///
/// Parked approvals and questions are answered inline on the REPL editor:
/// the gate holds the turn until this submits a decision or answer, so
/// REPL sessions must assemble with parking enabled (exec keeps the
/// headless gate and never calls this with a live turn expecting answers).
async fn drain_turn(
    client: &mut ActorClient,
    editor: &mut rustyline::DefaultEditor,
) -> anyhow::Result<()> {
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    loop {
        tokio::select! {
            event = client.next_event() => {
                let Some(event) = event else { break };
                if render_event(&event.msg, &mut stdout_text, &mut stderr_text).is_some() {
                    break;
                }
                if let EventMsg::ApprovalRequested { call_id, kind, .. } = &event.msg {
                    let decision =
                        prompt_approval(editor, call_id, approval_what(kind))?;
                    client
                        .submit(Submission {
                            id: format!("approval-{call_id}"),
                            op: Op::ExecApproval {
                                call_id: call_id.clone(),
                                decision,
                            },
                        })
                        .await
                        .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                }
                if let EventMsg::QuestionRequested {
                    call_id,
                    question,
                    options,
                } = &event.msg
                {
                    let answer =
                        prompt_question(editor, call_id, question, options)?;
                    client
                        .submit(Submission {
                            id: format!("question-{call_id}"),
                            op: Op::QuestionAnswer {
                                call_id: call_id.clone(),
                                answer,
                            },
                        })
                        .await
                        .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = client
                    .submit(Submission { id: "repl-interrupt".to_string(), op: Op::Interrupt })
                    .await;
            }
        }
    }
    print!("{stdout_text}");
    eprint!("{stderr_text}");
    std::io::stdout().flush()?;
    std::io::stderr().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_permission_mode_flag_parses() {
        let args =
            Args::try_parse_from(["wavecode", "--permission-mode", "plan", "exec", "hi"]).unwrap();
        assert_eq!(args.permission_mode.as_deref(), Some("plan"));
        let args = Args::try_parse_from(["wavecode", "repl"]).unwrap();
        assert_eq!(args.permission_mode, None);
    }

    /// Crate boundary: the binary's workspace edges stay exactly the
    /// composition it was assembled against (actor, bootstrap, wire,
    /// persistence, config, tui). New internal deps need a deliberate
    /// matrix update, not a silent Cargo.toml line.
    #[test]
    fn dependency_matrix_locked() {
        let mut in_deps = false;
        let mut names = Vec::new();
        for line in include_str!("../Cargo.toml").lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_deps = trimmed == "[dependencies]";
                continue;
            }
            if in_deps && trimmed.contains("path =") {
                names.extend(trimmed.split('=').next().map(str::trim).map(str::to_string));
            }
        }
        names.sort();
        assert_eq!(
            names,
            [
                "console-ui",
                "operations-actor",
                "operations-bootstrap",
                "state-persistence",
                "wavecode-config",
                "wavecode-skills",
                "wavecode-tools",
                "wavecode-wire",
            ],
            "harness internal deps changed; update the matrix deliberately",
        );
    }

    #[test]
    fn cli_routes_mcp_serve_and_plugin_list() {
        let args = Args::try_parse_from(["wavecode", "mcp", "serve"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Mcp {
                command: McpCommand::Serve
            })
        ));
        let args = Args::try_parse_from(["wavecode", "plugin", "list"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Plugin {
                command: PluginCommand::List
            })
        ));
        // Existing surfaces keep parsing.
        let args = Args::try_parse_from(["wavecode", "repl"]).unwrap();
        assert!(matches!(args.command, Some(Command::Repl)));
    }

    #[test]
    fn cli_routes_acp() {
        let args = Args::try_parse_from(["wavecode", "acp"]).unwrap();
        assert!(matches!(args.command, Some(Command::Acp)));
    }

    #[test]
    fn outcomes_map_to_exit_codes() {
        assert_eq!(Outcome::Completed.exit_code(), 0);
        assert_eq!(Outcome::Interrupted.exit_code(), 130);
        assert_eq!(Outcome::Failed.exit_code(), 1);
    }

    #[test]
    fn permission_modes_cycle_in_wire_order() {
        assert_eq!(next_mode("plan"), "auto");
        assert_eq!(next_mode("auto"), "wave");
        assert_eq!(next_mode("wave"), "plan");
        // Unknown names re-enter the cycle at the front (plan) and get
        // its successor.
        assert_eq!(next_mode("garbage"), "auto");
    }

    #[test]
    fn slash_lines_route_to_commands() {
        assert_eq!(parse_slash("/quit"), Slash::Quit);
        assert_eq!(parse_slash("/exit"), Slash::Quit);
        assert_eq!(parse_slash("  /compact  "), Slash::Compact);
        assert_eq!(parse_slash("/memory"), Slash::Memory);
        assert_eq!(parse_slash("/mcp"), Slash::Mcp);
        assert_eq!(parse_slash("/plan"), Slash::Plan(String::new()));
        assert_eq!(parse_slash("  /plan  "), Slash::Plan(String::new()));
        assert_eq!(parse_slash("/goal"), Slash::Goal(String::new()));
        assert_eq!(parse_slash("  /goal  "), Slash::Goal(String::new()));
        assert_eq!(parse_slash("/snapshots"), Slash::Snapshots);
        assert_eq!(
            parse_slash("/rewind before-refactor"),
            Slash::Rewind("before-refactor".to_string())
        );
        assert_eq!(parse_slash("  /rewind  "), Slash::Rewind(String::new()));
        assert_eq!(parse_slash("/permissions"), Slash::Permissions);
        assert_eq!(parse_slash("/help"), Slash::Help);
        assert_eq!(
            parse_slash("/nope"),
            Slash::Unknown {
                name: "nope".to_string(),
                args: String::new(),
            }
        );
        assert_eq!(
            parse_slash("/review the diff"),
            Slash::Unknown {
                name: "review".to_string(),
                args: "the diff".to_string(),
            }
        );
        assert_eq!(
            parse_slash("hello there"),
            Slash::Text("hello there".to_string())
        );
        // A leading slash with no name is unknown, not text.
        assert_eq!(
            parse_slash("/"),
            Slash::Unknown {
                name: String::new(),
                args: String::new(),
            }
        );
    }

    #[test]
    fn skill_requests_name_the_skill_and_args() {
        assert_eq!(
            skill_request("review", ""),
            "Please use the 'review' skill for this request."
        );
        assert_eq!(
            skill_request("review", "  the diff  "),
            "Please use the 'review' skill for this request. Arguments: the diff"
        );
    }

    #[test]
    fn streaming_text_lands_on_stdout() {
        let (mut out, mut err) = (String::new(), String::new());
        assert!(
            render_event(
                &EventMsg::AgentMessageDelta {
                    text: "hi".to_string()
                },
                &mut out,
                &mut err
            )
            .is_none()
        );
        assert!(
            render_event(
                &EventMsg::AgentMessageComplete {
                    text: String::new()
                },
                &mut out,
                &mut err
            )
            .is_none()
        );
        assert_eq!(out, "hi\n");
        assert!(err.is_empty());
    }

    /// Model / tool-sourced text must not carry ANSI / OSC sequences to
    /// the terminal (same threat model as the TUI's sanitize_terminal
    /// gate). This locks the render path against silently dropping the
    /// sanitizer again.
    #[test]
    fn model_and_tool_text_is_sanitized_before_output() {
        let (mut out, mut err) = (String::new(), String::new());
        let attack = "\x1b]52;;x\x07wipe \x1b[2J";
        render_event(
            &EventMsg::AgentMessageDelta {
                text: attack.to_string(),
            },
            &mut out,
            &mut err,
        );
        assert!(!out.contains('\x1b'), "delta leaked ANSI: {out:?}");
        render_event(
            &EventMsg::Warning {
                message: attack.to_string(),
            },
            &mut out,
            &mut err,
        );
        render_event(
            &EventMsg::ToolCallBegin {
                call_id: attack.to_string(),
                name: attack.to_string(),
                input: serde_json::Value::Null,
            },
            &mut out,
            &mut err,
        );
        assert!(
            !err.contains('\x1b'),
            "warning / tool lines leaked ANSI: {err:?}"
        );
    }

    #[test]
    fn fatal_errors_end_failed_but_recoverable_continues() {
        let (mut out, mut err) = (String::new(), String::new());
        assert!(
            render_event(
                &EventMsg::Error {
                    message: "x".to_string(),
                    recoverable: true
                },
                &mut out,
                &mut err
            )
            .is_none()
        );
        assert_eq!(
            render_event(
                &EventMsg::Error {
                    message: "x".to_string(),
                    recoverable: false
                },
                &mut out,
                &mut err
            ),
            Some(Outcome::Failed)
        );
        assert_eq!(
            render_event(
                &EventMsg::TurnCompleted { interrupted: false },
                &mut out,
                &mut err
            ),
            Some(Outcome::Completed)
        );
    }

    #[test]
    fn completed_text_is_not_printed_twice() {
        // Deltas stream "hello", then the completion repeats the full
        // text for transcript use: stdout must hold it exactly once.
        let (mut out, mut err) = (String::new(), String::new());
        for chunk in ["he", "llo"] {
            assert!(
                render_event(
                    &EventMsg::AgentMessageDelta {
                        text: chunk.to_string()
                    },
                    &mut out,
                    &mut err
                )
                .is_none()
            );
        }
        assert!(
            render_event(
                &EventMsg::AgentMessageComplete {
                    text: "hello".to_string()
                },
                &mut out,
                &mut err
            )
            .is_none()
        );
        assert_eq!(out, "hello\n");
        assert!(err.is_empty());
    }

    #[test]
    fn completion_without_deltas_still_prints() {
        // Senders that skip deltas must not lose the message text.
        let (mut out, mut err) = (String::new(), String::new());
        assert!(
            render_event(
                &EventMsg::AgentMessageComplete {
                    text: "hello".to_string()
                },
                &mut out,
                &mut err
            )
            .is_none()
        );
        assert_eq!(out, "hello\n");
        assert!(err.is_empty());
    }

    #[test]
    fn interrupted_turn_maps_to_interrupted_outcome() {
        let (mut out, mut err) = (String::new(), String::new());
        assert_eq!(
            render_event(
                &EventMsg::TurnCompleted { interrupted: true },
                &mut out,
                &mut err
            ),
            Some(Outcome::Interrupted)
        );
        assert_eq!(Outcome::Interrupted.exit_code(), 130);
    }

    #[test]
    fn approval_answers_map_to_wire_decisions() {
        use wavecode_wire::WireDecision;
        assert_eq!(decide_approval("y"), WireDecision::AllowOnce);
        assert_eq!(decide_approval("YES"), WireDecision::AllowOnce);
        assert_eq!(decide_approval("a"), WireDecision::AllowAlways);
        assert_eq!(decide_approval("Always"), WireDecision::AllowAlways);
        assert_eq!(
            decide_approval(""),
            WireDecision::Deny {
                reason: String::new()
            }
        );
        assert_eq!(
            decide_approval("no"),
            WireDecision::Deny {
                reason: String::new()
            }
        );
        assert_eq!(
            approval_what(&wavecode_wire::ApprovalKind::Exec),
            "execute a command"
        );
        assert_eq!(
            approval_what(&wavecode_wire::ApprovalKind::Write),
            "modify files"
        );
    }

    #[test]
    fn permission_mode_labels_follow_wire_names() {
        use console_ui::ui::permission_mode_label;
        assert_eq!(permission_mode_label("plan"), "Plan Mode");
        assert_eq!(permission_mode_label("auto"), "Auto Mode");
        assert_eq!(permission_mode_label("wave"), "Wave Mode");
        // Unknown names degrade to the auto label.
        assert_eq!(permission_mode_label("typo"), "Auto Mode");
    }

    /// A two-provider config (one OpenAI-compatible, one Anthropic) with
    /// one `[models]` alias, for the picker-catalog helpers below.
    fn picker_config() -> wavecode_config::Config {
        let provider = |kind| wavecode_config::ProviderConfig {
            kind,
            base_url: "https://api.example.com".to_string(),
            env_key: None,
            api_key: Some("k".to_string()),
            context_window: None,
            max_output_tokens: None,
            fallback_providers: Vec::new(),
            reasoning_effort: None,
            thinking_budget_tokens: None,
            prompt_caching: None,
        };
        let mut model_providers = std::collections::HashMap::new();
        model_providers.insert(
            "fastp".to_string(),
            provider(wavecode_config::ProviderKind::OpenAiCompatible),
        );
        model_providers.insert(
            "anthropic".to_string(),
            provider(wavecode_config::ProviderKind::Anthropic),
        );
        let mut models = std::collections::HashMap::new();
        models.insert(
            "fast".to_string(),
            wavecode_config::ModelEntry {
                provider: "fastp".to_string(),
                model: "fast-model".to_string(),
                reasoning_effort: Some("low".to_string()),
            },
        );
        wavecode_config::Config {
            model: "default-model".to_string(),
            model_provider: "anthropic".to_string(),
            model_providers,
            permission_mode: None,
            hooks: std::collections::HashMap::new(),
            mcp_servers: std::collections::HashMap::new(),
            models,
        }
    }

    /// Regression guard: an explicit CLI model keeps the configured
    /// provider, while a saved picker default carries its saved provider.
    /// Forwarding the saved provider unconditionally used to pair a CLI
    /// model with a foreign provider at assembly time.
    #[test]
    fn model_provider_overrides_pair_model_with_the_right_provider() {
        let saved = console_ui::settings::UiSettings {
            default_model: Some("saved-model".to_string()),
            default_provider: Some("saved-provider".to_string()),
            ..Default::default()
        };
        let (model, provider) = model_provider_overrides(Some("cli-model"), &saved);
        assert_eq!(model.as_deref(), Some("cli-model"));
        assert_eq!(provider, None);
        // Without a CLI model the saved pair rides together.
        let (model, provider) = model_provider_overrides(None, &saved);
        assert_eq!(model.as_deref(), Some("saved-model"));
        assert_eq!(provider.as_deref(), Some("saved-provider"));
        // Neither CLI model nor saved default: the config decides (Nones).
        let (model, provider) = model_provider_overrides(None, &Default::default());
        assert_eq!(model, None);
        assert_eq!(provider, None);
    }

    /// The thinking-level row exists only where a string effort applies;
    /// Anthropic budgets and unknown providers hide it.
    #[test]
    fn thinking_levels_follow_provider_kind() {
        let cfg = picker_config();
        assert_eq!(
            thinking_levels_for(&cfg, "fastp"),
            ["off", "low", "medium", "high"]
        );
        assert!(thinking_levels_for(&cfg, "anthropic").is_empty());
        assert!(thinking_levels_for(&cfg, "ghost").is_empty());
    }

    /// The picker catalog carries the `[models]` aliases plus exactly one
    /// entry for the live default model.
    #[test]
    fn model_catalog_merges_aliases_and_default_entry() {
        let cfg = picker_config();
        let entries = build_model_entries(&cfg, "anthropic", "default-model", None);
        let fast = entries
            .iter()
            .find(|e| e.label == "fast")
            .expect("alias entry present");
        assert_eq!(fast.provider, "fastp");
        assert_eq!(fast.model, "fast-model");
        assert_eq!(fast.effort.as_deref(), Some("low"));
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.label == "default-model" && e.provider == "anthropic")
                .count(),
            1
        );
    }

    /// `--session` resolves the journal snapshot and index title; a path
    /// escape is rejected before any filesystem access.
    #[test]
    fn resume_seed_loads_journal_and_rejects_bad_ids() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let history = vec![(false, "hello".to_string()), (true, "hi".to_string())];
        state_persistence::sessions::record_turn(
            &home,
            "s-1",
            "/tmp",
            "hello",
            &history,
            "Completed",
        )
        .unwrap();
        let seed = resolve_tui_seed(
            &Some("s-1".to_string()),
            false,
            Path::new("/tmp"),
            &Some(home.clone()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(seed.session_id, "s-1");
        assert_eq!(seed.history, history);
        assert!(
            resolve_tui_seed(
                &Some("../evil".to_string()),
                false,
                Path::new("/tmp"),
                &Some(home),
            )
            .is_err()
        );
    }

    /// `--continue` with nothing recorded errors with guidance; a missing
    /// home cannot honor explicit resume requests, while the flagless
    /// case stays a silent fresh session.
    #[test]
    fn continue_without_recorded_sessions_errors_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_tui_seed(
            &None,
            true,
            Path::new("/somewhere"),
            &Some(dir.path().to_path_buf()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no sessions"));
        assert!(resolve_tui_seed(&None, true, Path::new("/tmp"), &None).is_err());
        assert!(
            resolve_tui_seed(&Some("s-1".to_string()), false, Path::new("/tmp"), &None).is_err()
        );
        assert!(
            resolve_tui_seed(&None, false, Path::new("/tmp"), &None)
                .unwrap()
                .is_none()
        );
    }
}
