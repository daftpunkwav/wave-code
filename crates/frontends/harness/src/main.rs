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
use std::sync::Arc;

use clap::Parser;
use operations_actor::ActorClient;
use operations_bootstrap::{AssembleOptions, DEFAULT_IDENTITY, assemble_session};
use uuid::Uuid;
use wavecode_wire::{EventMsg, Op, Submission, WireDecision};

mod logging;
mod task_eval;
mod update;

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
    #[arg(long, global = true, conflicts_with_all = ["plan", "yolo"])]
    permission_mode: Option<String>,

    /// Start in plan mode (shortcut for `--permission-mode plan`).
    #[arg(long, global = true, conflicts_with = "yolo")]
    plan: bool,

    /// Start in auto mode (shortcut for `--permission-mode auto`).
    #[arg(long, short = 'y', global = true, conflicts_with = "plan")]
    yolo: bool,

    /// Resume a recorded session by id (fullscreen TUI only).
    #[arg(long, global = true)]
    session: Option<String>,

    /// Continue the most recent session recorded for this directory
    /// (fullscreen TUI only; an explicit `--session` wins).
    #[arg(long, short = 'c', global = true)]
    continue_last: bool,

    /// Log at debug level to `~/.wavecode/logs/wavecode.log.<date>`;
    /// overrides `WAVECODE_LOG` (default warn).
    #[arg(long, global = true)]
    debug: bool,

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
        /// Answer parked approvals from stdin (opt-in; off by default so
        /// unattended runs keep the fail-closed deny). JSON dialect:
        /// `<call_id> <allow|always|deny[:reason]>`. Text dialect: a
        /// `y`/`a`/`n` line per prompt. stdin EOF denies everything
        /// still parked.
        #[arg(long)]
        approvals: bool,
        /// Attach an image (PNG/JPEG/WebP/GIF, up to 5 MB) to the prompt.
        /// Repeatable; requires a vision-capable model.
        #[arg(long = "image")]
        image_paths: Vec<std::path::PathBuf>,
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
    /// Validate local configuration, permission rules, settings, themes,
    /// session records, and the OS confinement backend, without contacting
    /// any provider.
    Doctor,
    /// Aggregate the metrics ledger into per-model, per-tool quality
    /// numbers (offline: reads `~/.wavecode/metrics` only).
    Metrics {
        /// Restrict the report to one session id.
        #[arg(long)]
        session: Option<String>,
        /// Emit the merged totals as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Compare the running version against the newest published GitHub
    /// release, or with `--install`, download and replace this binary
    /// with it (checksum-verified, `.bak` kept for rollback; source
    /// builds refuse).
    Update {
        /// Download the release and replace the running binary.
        #[arg(long)]
        install: bool,
    },
    /// List and revoke the "always allow" grants sessions persisted
    /// (offline: reads `~/.wavecode/grants.jsonl` only).
    Grants {
        /// Grant surface to run.
        #[command(subcommand)]
        command: GrantsCommand,
    },
    /// Task-level benchmark suites: each task runs a real turn over a
    /// throwaway copy of a fixture and is judged by its assertions.
    Eval {
        /// Eval surface to run.
        #[command(subcommand)]
        command: EvalCommand,
    },
    /// Serve live sessions over local HTTP (REST + SSE). Binds the
    /// loopback interface only; the bearer token prints on startup.
    Serve {
        /// Bind port (0 picks an ephemeral port).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Bearer token (default: a fresh random token per run).
        #[arg(long)]
        token: Option<String>,
    },
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

/// Grant surfaces on the new stack.
#[derive(Debug, Parser)]
enum GrantsCommand {
    /// Show every stored grant with the index `remove` takes.
    List,
    /// Revoke the grant at a `list` index.
    Remove {
        /// Index shown by `grants list`.
        index: usize,
    },
    /// Revoke every stored grant.
    Clear,
}

/// Task-level benchmark surfaces.
#[derive(Debug, clap::Subcommand)]
enum EvalCommand {
    /// Run every task manifest in a directory against a real model.
    ///
    /// Each task copies its fixture into a throwaway work root, drives one
    /// `exec` turn there, then judges the workspace. Unattended runs need
    /// room to act: pass `--permission-mode wave` (or grant ahead of time),
    /// otherwise a parked approval denies and the task fails.
    Tasks {
        /// Directory holding `task.toml` manifests, searched recursively.
        #[arg(long, default_value = "benchmarks/tasks")]
        dir: PathBuf,
        /// Keep only tasks whose id contains this substring.
        #[arg(long)]
        filter: Option<String>,
        /// Keep only tasks carrying this exact tag.
        #[arg(long)]
        tag: Option<String>,
        /// Where per-task work roots are created (default: a temp directory).
        #[arg(long)]
        work_root: Option<PathBuf>,
        /// `wavecode` binary each task drives (default: this executable).
        #[arg(long)]
        agent_bin: Option<PathBuf>,
        /// Print the report as JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Also write the JSON report to this path.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// The permission mode the session should start in: an explicit
/// `--permission-mode` wins (clap already rejects combining it with
/// the shortcuts); `--plan`/`--yolo` map onto the wire names.
fn effective_permission_mode(args: &Args) -> Option<String> {
    if args.permission_mode.is_some() {
        return args.permission_mode.clone();
    }
    if args.plan {
        return Some("plan".to_string());
    }
    if args.yolo {
        return Some("auto".to_string());
    }
    None
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
        EventMsg::TurnStarted { .. } => {
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
            code,
        } => {
            // The machine class rides the line so scripts can filter
            // without parsing prose (`[error] (provider.timeout) ...`).
            let class = code
                .as_deref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            stderr.push_str(&format!("[error]{class} {}\n", clean(message)));
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
    // Diagnostics before anything else can fail: the log file first,
    // then the panic hook that restores the terminal when a later
    // panic hits a live UI.
    logging::init(args.debug);
    console_ui::install_panic_restore();
    // Resolved before any `args` field moves below.
    let permission_mode = effective_permission_mode(&args);
    let cwd = std::env::current_dir()?;
    // The shared definition point: config loading, sessions, and logging
    // must agree on the home root even when both env vars are set.
    let home = wavecode_config::home_dir();
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
                permission_mode.clone(),
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
            permission_override: permission_mode.clone(),
            thinking_override: settings.default_effort.clone(),
            cwd,
            home: home.clone(),
            identity: DEFAULT_IDENTITY.to_string(),
            wave_denylist: settings.wave_denylist,
            headless: false,
            initial_history: Vec::new(),
            // The REPL keeps its history in memory and records no journal.
            session_id: None,
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
    // Doctor reads local files only: like mcp/plugin, it runs before
    // any session assembly and never needs provider credentials.
    if matches!(args.command, Some(Command::Doctor)) {
        let checks = doctor_checks(args.config.as_deref(), home.as_deref());
        for check in &checks {
            println!(
                "{} {}",
                if check.ok { "[ok]" } else { "[fail]" },
                check.line
            );
        }
        let failed = checks.iter().filter(|check| !check.ok).count();
        if failed > 0 {
            eprintln!("\n{failed} check(s) failed");
            std::process::exit(Outcome::Failed.exit_code())
        }
        println!("\nall checks passed");
        std::process::exit(Outcome::Completed.exit_code())
    }
    // Metrics reads the local ledger only: like doctor, it needs no
    // provider credentials and assembles no session.
    if let Some(Command::Metrics { session, json }) = args.command {
        run_metrics(home.as_deref(), session.as_deref(), json);
        std::process::exit(Outcome::Completed.exit_code())
    }
    // Grants inspects and edits one local file: no session, no credentials.
    if let Some(Command::Grants { command }) = args.command {
        let ok = run_grants(home.as_deref(), command);
        std::process::exit(if ok {
            Outcome::Completed.exit_code()
        } else {
            Outcome::Failed.exit_code()
        })
    }
    // Eval assembles no session of its own: every task spawns its own
    // `exec` child, which reads credentials and config from the same place
    // an interactive run does. The flags that steer a turn are forwarded.
    if let Some(Command::Eval {
        command:
            EvalCommand::Tasks {
                dir,
                filter,
                tag,
                work_root,
                agent_bin,
                json,
                out,
            },
    }) = args.command
    {
        let mut forward = Vec::new();
        if let Some(config) = args.config.as_deref() {
            forward.extend([
                "--config".to_string(),
                config.to_string_lossy().into_owned(),
            ]);
        }
        if let Some(model) = args.model.as_deref() {
            forward.push("--model".to_string());
            forward.push(model.to_string());
        }
        if let Some(mode) = permission_mode.as_deref() {
            forward.extend(["--permission-mode".to_string(), mode.to_string()]);
        }
        let ok = task_eval::run_tasks(task_eval::TasksRequest {
            tasks_dir: dir,
            filter,
            tag,
            work_root,
            agent_bin,
            forward,
            json,
            out,
        })?;
        std::process::exit(if ok {
            Outcome::Completed.exit_code()
        } else {
            Outcome::Failed.exit_code()
        })
    }
    // Update check and self-install talk only to the GitHub releases
    // API: no session assembly, no provider credentials. A failed
    // probe exits 1 so scripts can tell "no update" from "could not
    // tell".
    if let Some(Command::Update { install }) = args.command {
        if install {
            run_update_install().await;
        } else {
            run_update_check().await;
        }
        std::process::exit(Outcome::Completed.exit_code())
    }
    // The app server assembles sessions lazily per POST /sessions, so it
    // returns before the shared assembly below and needs no provider
    // credential check of its own.
    if let Some(Command::Serve { port, token }) = args.command {
        run_serve(args.config, port, token, cwd, home).await?;
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
            permission_mode.clone(),
            cwd.clone(),
            home.clone(),
        )
        .await?;
        std::process::exit(Outcome::Completed.exit_code())
    }
    // Only headless exec denies approvals openly: the REPL parks them on
    // the gate and answers inline, which needs parking enabled here.
    // With `--approvals`, exec keeps the parking gate alive and answers
    // from stdin; without it the headless gate denies openly (fail-closed
    // for unattended runs).
    let headless = !matches!(
        args.command,
        Some(Command::Exec {
            approvals: true,
            ..
        })
    );
    let settings = console_ui::settings::UiSettings::load();
    let (model_override, provider_override) =
        model_provider_overrides(args.model.as_deref(), &settings);
    // A headless turn leaves a resumable session behind (journal + meta
    // line) when a home directory exists; the id is minted up front so the
    // compaction footers can point at that journal.
    let mut session = home.as_ref().map(|home| ExecSession {
        id: Uuid::new_v4().to_string(),
        home: home.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        redactor: None,
    });
    let mut handle = assemble_session(AssembleOptions {
        config_path: args.config,
        model_override,
        provider_override,
        permission_override: permission_mode.clone(),
        thinking_override: settings.default_effort.clone(),
        cwd: cwd.clone(),
        home: home.clone(),
        wave_denylist: settings.wave_denylist,
        identity: DEFAULT_IDENTITY.to_string(),
        headless,
        initial_history: Vec::new(),
        session_id: session.as_ref().map(|s| s.id.clone()),
    })
    .map_err(|e| anyhow::anyhow!("session assembly failed: {e}"))?;
    // The journal gate rides the assembled handle's provider secrets.
    if let Some(sess) = &mut session {
        sess.redactor = handle.secret_redactor();
    }
    handle.connect_mcp_servers().await;
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }

    match args.command {
        Some(Command::Exec {
            prompt,
            json,
            approvals,
            image_paths,
        }) => {
            let images = load_images(&image_paths)?;
            let outcome = run_exec(
                &mut handle.client,
                &prompt,
                json,
                approvals,
                images,
                session,
            )
            .await?;
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
            run_resume(thread_id, permission_mode, home).await?;
            std::process::exit(Outcome::Completed.exit_code())
        }
        // Mcp/Plugin/Acp/Doctor/Metrics/Grants/Eval/Update/Serve return
        // before assembly above.
        Some(Command::Mcp { .. })
        | Some(Command::Plugin { .. })
        | Some(Command::Acp)
        | Some(Command::Doctor)
        | Some(Command::Metrics { .. })
        | Some(Command::Grants { .. })
        | Some(Command::Eval { .. })
        | Some(Command::Update { .. })
        | Some(Command::Serve { .. }) => {
            unreachable!("early-return surfaces never reach assembly")
        }
        None => unreachable!("bare invocation returns above"),
    }
}

/// Model name for a sample that carries none (a driver predating model
/// attribution, or a stub turn in tests).
const METRICS_UNKNOWN_MODEL: &str = "(unknown)";

/// Merge ledger samples into per-model totals.
///
/// Grouping by model is what makes the table answer "is this tool weak, or
/// is this model bad at it" — the same tool ranked across two models.
fn metrics_totals(
    samples: &[operations_observe::TurnSample],
) -> std::collections::BTreeMap<String, operations_observe::Metrics> {
    let mut totals: std::collections::BTreeMap<String, operations_observe::Metrics> =
        std::collections::BTreeMap::new();
    for sample in samples {
        let model = if sample.model.is_empty() {
            METRICS_UNKNOWN_MODEL.to_string()
        } else {
            sample.model.clone()
        };
        totals.entry(model).or_default().merge(&sample.metrics);
    }
    totals
}

/// Render one model's tool table, ranked by how often the model reached for
/// the tool. Success rate covers executed calls only, so a tool that is
/// mostly refused never reads as broken.
fn metrics_table(metrics: &operations_observe::Metrics) -> String {
    use operations_observe::ToolStat;

    let mut rows: Vec<(&String, &ToolStat)> = metrics
        .tools
        .iter()
        .filter(|(_, stat)| stat.total() > 0)
        .collect();
    rows.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(b.0)));
    if rows.is_empty() {
        return "  (no tool calls recorded)
"
        .to_string();
    }
    let mut out = format!(
        "  {:<22}{:>7}{:>7}{:>7}{:>9}{:>8}{:>7}{:>10}
",
        "tool", "calls", "ok", "fail", "refused", "denied", "busy", "success"
    );
    for (name, stat) in rows {
        let success = match stat.success_rate() {
            Some(rate) => format!("{:.1}%", rate * 100.0),
            None => "-".to_string(),
        };
        out.push_str(&format!(
            "  {:<22}{:>7}{:>7}{:>7}{:>9}{:>8}{:>6.1}s{:>10}
",
            name,
            stat.total(),
            stat.executed_ok,
            stat.executed_failed,
            stat.refused,
            stat.denied + stat.blocked,
            stat.busy_ms as f64 / 1000.0,
            success
        ));
    }
    if let Some(share) = metrics.cache_read_share() {
        out.push_str(&format!(
            "  prompt cache read {:.1}% of input (in {} / out {} tokens, {} compactions)
",
            share * 100.0,
            metrics.tokens_in,
            metrics.tokens_out,
            metrics.compactions
        ));
    }
    let turns = metrics.turns_completed;
    out.push_str(&format!(
        "  {} turns ({} interrupted), {} approvals requested, {} warnings, {} errors
",
        turns,
        metrics.turns_interrupted,
        metrics.approvals_requested,
        metrics.warnings,
        metrics.errors
    ));
    out
}

/// Full report text for the merged totals.
fn metrics_report(
    totals: &std::collections::BTreeMap<String, operations_observe::Metrics>,
) -> String {
    if totals.is_empty() {
        return "no metric samples recorded yet".to_string();
    }
    let mut out = String::new();
    for (model, metrics) in totals {
        out.push_str(&format!("model: {model}\n"));
        out.push_str(&metrics_table(metrics));
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// `metrics`: aggregate the local ledger into a tool-quality report.
fn run_metrics(home: Option<&Path>, session: Option<&str>, json: bool) {
    let Some(home) = home else {
        eprintln!("metrics: no home directory available");
        return;
    };
    let ledger = operations_observe::Ledger::in_home(home);
    let read = ledger.read();
    let kept: Vec<operations_observe::TurnSample> = read
        .samples
        .iter()
        .filter(|sample| session.is_none_or(|id| sample.session == id))
        .cloned()
        .collect();
    if read.malformed > 0 {
        eprintln!(
            "[warn] {} malformed ledger line(s) skipped in {}",
            read.malformed,
            ledger.path().display()
        );
    }
    let totals = metrics_totals(&kept);
    if json {
        match serde_json::to_string_pretty(&totals) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("metrics: serialization failed: {e}"),
        }
        return;
    }
    println!(
        "ledger {} ({} turn samples, {} session(s))",
        ledger.path().display(),
        kept.len(),
        kept.iter()
            .map(|sample| sample.session.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    );
    println!();
    println!("{}", metrics_report(&totals));
}

/// `grants`: inspect and revoke the persisted always-allow table.
///
/// Every action here only tightens authority (a revoked grant goes back to
/// asking), so no confirmation gate is warranted. The exit code separates
/// "nothing to do" from "could not tell".
fn run_grants(home: Option<&Path>, command: GrantsCommand) -> bool {
    use state_persistence::grants;
    let Some(home) = home else {
        eprintln!("grants: no home directory available");
        return false;
    };
    let read = grants::load_grants(home);
    if read.malformed > 0 {
        eprintln!(
            "[warn] {} malformed grant line(s) skipped in {}",
            read.malformed,
            grants::grants_path(home).display()
        );
    }
    match command {
        GrantsCommand::List => {
            println!(
                "grants {} ({} stored)",
                grants::grants_path(home).display(),
                read.grants.len()
            );
            for (index, grant) in read.grants.iter().enumerate() {
                println!(
                    "  {index:<3} {:<48} {:<8} {}",
                    grant.rule, grant.tool, grant.session
                );
            }
            true
        }
        GrantsCommand::Remove { index } => match grants::remove_grant(home, index) {
            Ok(Some(grant)) => {
                println!("revoked {}", grant.rule);
                true
            }
            Ok(None) => {
                eprintln!(
                    "grants: no grant at index {index} ({} stored)",
                    read.grants.len()
                );
                false
            }
            Err(error) => {
                eprintln!("grants: {error}");
                false
            }
        },
        GrantsCommand::Clear => match grants::clear_grants(home) {
            Ok(count) => {
                println!("revoked {count} grant(s)");
                true
            }
            Err(error) => {
                eprintln!("grants: {error}");
                false
            }
        },
    }
}

#[cfg(test)]
mod metrics_tests {
    use super::{metrics_report, metrics_totals};
    use operations_observe::{Ledger, Metrics, TurnSample};
    use wavecode_wire::{Event, EventMsg, ToolCallPreview, ToolOutcome};

    /// Metrics built the way the tap builds them — by folding events — so
    /// the read side is tested against the real fold, not a hand-built
    /// struct that could drift from it.
    fn folded(tool: &str, outcomes: &[(ToolOutcome, bool, u64)]) -> Metrics {
        let mut metrics = Metrics::new();
        for (index, (outcome, is_error, ms)) in outcomes.iter().enumerate() {
            let id = format!("c{index}");
            metrics.record(&Event {
                id: id.clone(),
                msg: EventMsg::ToolCallBegin {
                    call_id: id.clone(),
                    name: tool.to_string(),
                    input: serde_json::Value::Null,
                },
            });
            metrics.record(&Event {
                id: id.clone(),
                msg: EventMsg::ToolCallEnd {
                    call_id: id,
                    is_error: *is_error,
                    output: Some(ToolCallPreview::head("out", 16)),
                    outcome: *outcome,
                    duration_ms: *ms,
                },
            });
        }
        metrics
    }

    fn sample(model: &str, session: &str, metrics: Metrics) -> TurnSample {
        TurnSample {
            ts_secs: 0,
            session: session.to_string(),
            model: model.to_string(),
            metrics,
        }
    }

    const OK: ToolOutcome = ToolOutcome::Executed;
    const REFUSED: ToolOutcome = ToolOutcome::Refused;

    /// The table is the decision surface: the same tool ranked per model,
    /// with a refusal-heavy tool scored on its executed calls only.
    #[test]
    fn report_ranks_tools_per_model_and_ignores_refusals_in_rate() {
        let samples = vec![
            sample(
                "opus",
                "s1",
                folded("edit", &[(OK, true, 3), (OK, true, 4), (OK, false, 1)]),
            ),
            sample(
                "opus",
                "s1",
                folded("shell", &[(OK, false, 1200), (REFUSED, true, 0)]),
            ),
            sample("sonnet", "s2", folded("edit", &[(OK, false, 1)])),
        ];
        let report = metrics_report(&metrics_totals(&samples));
        assert!(report.starts_with("model: opus"), "{report}");
        let opus = report.split("model: sonnet").next().unwrap();
        // One of three executed edit calls landed.
        assert!(opus.contains("33.3%"), "{opus}");
        let shell = opus
            .lines()
            .find(|line| line.starts_with("  shell"))
            .unwrap();
        // One executed call that succeeded: 100%, and the refusal shows.
        assert!(shell.contains("100.0%"), "{shell}");
        assert!(shell.contains("1"), "{shell}");
        assert!(shell.contains("1.2s"), "{shell}");
        let sonnet = report.split("model: sonnet").nth(1).unwrap();
        assert!(sonnet.contains("edit"), "{sonnet}");
    }

    #[test]
    fn report_says_empty_rather_than_printing_nothing() {
        assert_eq!(
            metrics_report(&std::collections::BTreeMap::new()),
            "no metric samples recorded yet"
        );
        let turned = sample("opus", "s1", folded("read", &[]));
        let report = metrics_report(&metrics_totals(&[turned]));
        assert!(report.contains("no tool calls recorded"), "{report}");
    }

    /// Aggregation is keyed by the model on the sample, so a session that
    /// switched models mid-flight contributes to both rows.
    #[test]
    fn samples_merge_across_sessions_by_model() {
        let totals = metrics_totals(&[
            sample("opus", "a", folded("edit", &[(OK, false, 1)])),
            sample("opus", "b", folded("edit", &[(OK, true, 1)])),
        ]);
        let edit = &totals["opus"].tools["edit"];
        assert_eq!((edit.executed_ok, edit.executed_failed), (1, 1));
        assert_eq!(edit.success_rate(), Some(0.5));
    }

    /// What the tap writes is what the CLI reads: one round trip through
    /// the on-disk format, no shared fixture.
    #[test]
    fn ledger_round_trips_into_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::in_home(dir.path());
        ledger
            .append(&sample("opus", "s1", folded("read", &[(OK, false, 2)])))
            .unwrap();
        let read = ledger.read();
        assert_eq!(read.malformed, 0);
        let report = metrics_report(&metrics_totals(&read.samples));
        assert!(report.contains("model: opus"), "{report}");
        assert!(report.contains("read"), "{report}");
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

/// Thinking levels the picker offers: the OpenAI-protocol providers (Chat
/// Completions and Responses) take a string reasoning effort; budget-driven
/// Anthropic thinking is config-only, so the row hides there.
fn thinking_levels_for(config: &wavecode_config::Config, provider_id: &str) -> Vec<String> {
    match config.model_providers.get(provider_id) {
        Some(provider) if provider.kind.carries_reasoning_effort() => {
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
    update_notice: Option<Arc<std::sync::Mutex<Option<String>>>>,
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
        update_notice,
        redactor: handle.secret_redactor(),
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
    update_notice: Option<Arc<std::sync::Mutex<Option<String>>>>,
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
        let update_notice = update_notice.clone();
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
                // A read-only side session (`/btw`) is routine work: when
                // the config names a `secondary_model` alias resolvable in
                // `[models]`, sample through it instead of the primary —
                // unless the caller pinned a live model hint (an explicit
                // choice outranks the cost steering). An unresolvable
                // alias degrades to the primary with a warning.
                let secondary = if readonly {
                    load_config_opt(config_path.as_deref())
                        .ok()
                        .and_then(|config| resolve_secondary(&config))
                } else {
                    None
                };
                let (model_override, provider_override, thinking_override) =
                    match (&secondary, &model_hint) {
                        (Some((model, provider, effort)), None) => (
                            Some(model.clone()),
                            provider.clone(),
                            effort.clone().or(settings.default_effort.clone()),
                        ),
                        _ => (
                            model_hint.clone().or(base_model),
                            provider_override,
                            settings.default_effort.clone(),
                        ),
                    };
                let session_id = resume_id.unwrap_or_else(|| Uuid::new_v4().to_string());
                let mut handle = assemble_session(AssembleOptions {
                    session_id: Some(session_id.clone()),
                    config_path,
                    // A launch hint (the live /model choice) wins; a
                    // read-only side session runs in plan mode so
                    // approvals and destructive work are impossible by
                    // mode, never by trust.
                    model_override,
                    provider_override,
                    permission_override: if readonly {
                        Some("plan".to_string())
                    } else {
                        permission_mode
                    },
                    thinking_override,
                    wave_denylist: settings.wave_denylist,
                    cwd: cwd.clone(),
                    home: home.clone(),
                    identity: DEFAULT_IDENTITY.to_string(),
                    headless: false,
                    initial_history: history.clone(),
                })
                .map_err(|e| format!("session assembly failed: {e}"))?;
                handle.connect_mcp_servers().await;
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
                    update_notice,
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
    // Resume seed: an explicit --session errors hard on a missing
    // record; --continue degrades to a fresh session with a warning.
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
        session_id: Some(session_id.clone()),
    }) {
        Ok(handle) => handle,
        Err(operations_bootstrap::SessionError::Config(e)) => {
            print_config_error(&e);
            std::process::exit(2)
        }
        Err(operations_bootstrap::SessionError::Model(message)) => {
            eprintln!("[fail] {message}");
            std::process::exit(2)
        }
    };
    handle.connect_mcp_servers().await;
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }
    // Release check runs beside the session: the footer picks the
    // notice up on a later tick, never delaying the first frame.
    let update_slot = Arc::new(std::sync::Mutex::new(None));
    let slot = update_slot.clone();
    tokio::spawn(async move {
        let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        else {
            return;
        };
        let Ok(Some((tag, url))) = update::fetch_latest(&client).await else {
            return;
        };
        if matches!(
            update::classify(&tag, &url),
            update::UpdateStatus::Available { .. }
        ) {
            // Kept short: the footer's right slot drops content that
            // does not fit an 80-column terminal; `wavecode update`
            // carries the details.
            *slot.lock().expect("update slot lock") = Some(format!("update available: {tag}"));
        }
    });
    let ctx = ui_ctx_of(
        &handle,
        cwd.clone(),
        &home,
        session_id,
        title,
        model_entries.clone(),
        thinking_levels.clone(),
        Some(update_slot.clone()),
    );
    let factory = make_tui_factory(
        config,
        model,
        permission_mode,
        cwd,
        home,
        model_entries,
        thinking_levels,
        Some(update_slot),
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

/// Read one image file into a wire `UserImage`: mime sniffed from the
/// extension, size capped like the provider validators (5 MB decoded is
/// what they enforce; here we cap the raw file at the same bound).
fn load_image(path: &std::path::Path) -> anyhow::Result<wavecode_wire::UserImage> {
    const IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        other => {
            return Err(anyhow::anyhow!(
                "unsupported image extension {:?} (png/jpg/jpeg/webp/gif)",
                other
            ));
        }
    };
    // Reject oversized files before reading: the cap exists to bound
    // memory, so a multi-gigabyte file must fail on its metadata instead
    // of being fully loaded first. The post-read check stays as a
    // backstop for a file that grows between the two calls.
    let size = std::fs::metadata(path)?.len();
    if size > IMAGE_MAX_BYTES as u64 {
        return Err(anyhow::anyhow!(
            "image {} is {} bytes; the limit is {} bytes",
            path.display(),
            size,
            IMAGE_MAX_BYTES
        ));
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > IMAGE_MAX_BYTES {
        return Err(anyhow::anyhow!(
            "image {} is {} bytes; the limit is {} bytes",
            path.display(),
            bytes.len(),
            IMAGE_MAX_BYTES
        ));
    }
    use base64::Engine as _;
    Ok(wavecode_wire::UserImage {
        id: None,
        mime: mime.to_string(),
        base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

/// Load every `--image` path, failing the run on the first problem.
fn load_images(paths: &[std::path::PathBuf]) -> anyhow::Result<Vec<wavecode_wire::UserImage>> {
    paths.iter().map(|p| load_image(p)).collect()
}

/// Load the config for catalog derivation, from `path` or the default
/// location.
fn load_config_opt(
    path: Option<&std::path::Path>,
) -> Result<wavecode_config::Config, wavecode_config::ConfigError> {
    let mut config = match path {
        Some(path) => wavecode_config::Config::load_from(path),
        None => wavecode_config::Config::load(),
    }?;
    // The model catalog (`~/.wavecode/models.json`) merges on top: its
    // models become `[models]` entries and synthesized
    // `catalog:<provider>` providers (config.toml providers win on id
    // collisions). Catalog load failures degrade to an empty catalog —
    // config.toml models keep the session usable.
    if let Some(home) = wavecode_config::home_dir() {
        match wavecode_config::ModelCatalog::load(&home) {
            Ok(catalog) => catalog.merge_into(&mut config),
            Err(e) => eprintln!("model catalog ignored: {e}"),
        }
    }
    Ok(config)
}

/// Resolve the config's `secondary_model` alias into the
/// `(model, provider, effort)` triple side sessions sample through.
/// `None` when unset — or when the alias dangles, which warns here and
/// degrades to the primary model (doctor reports the same finding).
fn resolve_secondary(
    config: &wavecode_config::Config,
) -> Option<(String, Option<String>, Option<String>)> {
    let alias = config.secondary_model.as_deref()?;
    match config.models.get(alias) {
        Some(entry) => Some((
            entry.model.clone(),
            Some(entry.provider.clone()),
            entry.reasoning_effort.clone(),
        )),
        None => {
            eprintln!(
                "[warn] secondary_model {alias:?} is not in [models]; side sessions use the primary model"
            );
            None
        }
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
    let (registry, executor) = operations_bootstrap::ToolAdapter::mcp_serve_tools(cwd);
    operations_gateway::mcp_serve::run_stdio_server(registry, executor)
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
    operations_gateway::acp::run_stdio_server(
        operations_gateway::acp::AcpServerOptions {
            config_path: config,
            model_override: model,
            permission_override: permission_mode,
            cwd,
            home,
        },
        operations_bootstrap::assemble_session,
    )
    .await
    .map_err(|e| anyhow::anyhow!("acp server failed: {e}"))
}

/// List installed plugin packs with skill/MCP/hook counts.
fn run_plugin_list(home: Option<PathBuf>) {
    let (plugins, warnings) =
        operations_bootstrap::plugin_inventory::discover_plugin_summaries(home.as_deref());
    for warning in &warnings {
        eprintln!("[warn] {warning}");
    }
    if plugins.is_empty() {
        println!("(no plugins installed)");
        return;
    }
    for plugin in &plugins {
        println!(
            "{} {} (skills: {}, mcp servers: {}, hooks: {})",
            plugin.name, plugin.version, plugin.skills, plugin.mcp_servers, plugin.hooks
        );
    }
}

/// Serve live sessions over local HTTP until Ctrl-C or `POST /shutdown`.
///
/// The bearer token prints once; every route except `/healthz` requires
/// it, and the server binds the loopback interface only.
async fn run_serve(
    config: Option<PathBuf>,
    port: u16,
    token: Option<String>,
    cwd: PathBuf,
    home: Option<PathBuf>,
) -> anyhow::Result<()> {
    let token = token.unwrap_or_else(|| Uuid::new_v4().to_string());
    let handle = operations_gateway::app_server::serve(
        operations_gateway::app_server::ServeOptions {
            config_path: config,
            model_override: None,
            cwd,
            home,
            token: token.clone(),
            port,
        },
        operations_bootstrap::assemble_session,
    )
    .await?;
    // stderr: unbuffered even when the process is piped (stdout would
    // block-buffer and a spawning parent would never see the token).
    eprintln!(
        "wavecode serve listening on http://127.0.0.1:{} (Ctrl-C stops)",
        handle.port
    );
    eprintln!("bearer token: {token}");
    eprintln!(
        "try: curl -H 'Authorization: Bearer {token}' http://127.0.0.1:{}/healthz",
        handle.port
    );
    tokio::signal::ctrl_c().await?;
    handle.shutdown();
    Ok(())
}

/// Probe the newest published release and report the comparison.
///
/// `--debug` aside, this surface has no session; a network failure
/// prints and exits 1 so callers never read a failed probe as "no
/// update available".
async fn run_update_check() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let client = match client {
        Ok(client) => client,
        Err(cause) => {
            eprintln!("[fail] update check failed: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    match update::fetch_latest(&client).await {
        Ok(None) => println!("no published release yet"),
        Ok(Some((tag, url))) => match update::classify(&tag, &url) {
            update::UpdateStatus::UpToDate { latest } => {
                println!(
                    "wavecode {} is up to date (latest {latest})",
                    env!("CARGO_PKG_VERSION")
                );
            }
            update::UpdateStatus::Available { latest, url } => {
                println!(
                    "update available: {} -> {latest}",
                    env!("CARGO_PKG_VERSION")
                );
                println!("{url}");
            }
        },
        Err(cause) => {
            eprintln!("[fail] update check failed: {cause:#}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    }
}

/// Download the newest release and replace this binary with it.
///
/// Refuses source builds (nothing installed to replace) and
/// unpublished targets; verifies the download against the published
/// sha256 before touching the running file, and keeps the replaced
/// binary as `.bak` next to it for manual rollback.
async fn run_update_install() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let client = match client {
        Ok(client) => client,
        Err(cause) => {
            eprintln!("[fail] update failed: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(cause) => {
            eprintln!("[fail] cannot locate the running binary: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    if update::running_from_source(&exe) {
        eprintln!(
            "[fail] this is a source build (target/); self-update replaces
                      installed binaries only — use cargo build --release instead"
        );
        std::process::exit(Outcome::Failed.exit_code())
    }
    let Some(target) = update::current_target() else {
        eprintln!(
            "[fail] no prebuilt release for this platform ({}, {}); build
                      from source instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        std::process::exit(Outcome::Failed.exit_code())
    };

    let release = match update::fetch_release(&client).await {
        Ok(Some(release)) => release,
        Ok(None) => {
            eprintln!("[fail] no published release yet");
            std::process::exit(Outcome::Failed.exit_code())
        }
        Err(cause) => {
            eprintln!("[fail] update failed: {cause:#}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    let Some((bin, sha)) = update::pick_assets(&release.tag, &release.assets, target) else {
        eprintln!(
            "[fail] release {tag} carries no binary for {target}",
            tag = release.tag
        );
        std::process::exit(Outcome::Failed.exit_code())
    };

    println!("downloading {} ...", release.tag);
    let bytes = match client.get(&bin.url).send().await {
        Ok(response) => match response.bytes().await {
            Ok(bytes) => bytes,
            Err(cause) => {
                eprintln!("[fail] download failed: {cause}");
                std::process::exit(Outcome::Failed.exit_code())
            }
        },
        Err(cause) => {
            eprintln!("[fail] download failed: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    let checksum = match client.get(&sha.url).send().await {
        Ok(response) => match response.text().await {
            Ok(text) => text,
            Err(cause) => {
                eprintln!("[fail] checksum download failed: {cause}");
                std::process::exit(Outcome::Failed.exit_code())
            }
        },
        Err(cause) => {
            eprintln!("[fail] checksum download failed: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    if let Err(cause) = update::verify_sha256(&bytes, &checksum) {
        eprintln!("[fail] {cause:#}");
        std::process::exit(Outcome::Failed.exit_code())
    }

    match update::apply_swap(&exe, &bytes) {
        Ok(()) => {
            println!("installed {} -> {}", release.tag, exe.display());
            #[cfg(windows)]
            println!(
                "the previous binary was kept as {}",
                exe.with_file_name(format!(
                    "{}.bak",
                    exe.file_name().unwrap_or_default().to_string_lossy()
                ))
                .display()
            );
        }
        Err(cause) => {
            eprintln!("[fail] install failed: {cause:#}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    }
}

/// One `doctor` check outcome.
struct DoctorCheck {
    ok: bool,
    line: String,
}
fn ok(line: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        ok: true,
        line: line.into(),
    }
}

fn fail(line: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        ok: false,
        line: line.into(),
    }
}

/// The `wave` denylist stored in the console settings under `home`.
///
/// Missing or unreadable settings yield no entries: the settings check in
/// [`doctor_checks`] is what reports why, rather than every consumer
/// repeating the warning.
fn settings_denylist(home: &Path) -> Vec<String> {
    let path = home.join(".wavecode").join("console-settings.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<console_ui::settings::UiSettings>(&text).ok())
        .map(|settings| settings.wave_denylist)
        .unwrap_or_default()
}

/// `doctor`: validate every local file a session depends on — config,
/// provider credentials, UI settings, custom themes, and session
/// records — without contacting any provider. Secrets are never
/// printed, only where a key was found.
fn doctor_checks(config_path: Option<&std::path::Path>, home: Option<&Path>) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    let Some(home) = home else {
        checks.push(fail(
            "home: no HOME/USERPROFILE — sessions, settings, and config cannot be located",
        ));
        return checks;
    };

    // Config + provider credentials: an explicit --config path wins,
    // otherwise the file sits under the (already resolved) home.
    let path = match config_path {
        Some(path) => path.to_path_buf(),
        None => home.join(".wavecode").join("config.toml"),
    };
    match wavecode_config::Config::load_from(&path) {
        Err(wavecode_config::ConfigError::NotFound(path)) => checks.push(fail(format!(
            "config: not found at {} — see the example block in the startup error or docs/",
            path.display()
        ))),
        Err(error) => checks.push(fail(format!("config: {error}"))),
        Ok(config) => {
            checks.push(ok(format!(
                "config: {} (model {} via {})",
                path.display(),
                config.model,
                config.model_provider
            )));
            match config.resolve_provider() {
                Err(wavecode_config::ConfigError::MissingApiKey(name)) => checks.push(fail(
                    format!("provider {name}: no api key (set env_key or inline api_key)"),
                )),
                Err(error) => checks.push(fail(format!("provider: {error}"))),
                Ok((provider, _key)) => {
                    let source = provider
                        .env_key
                        .as_deref()
                        .filter(|name| std::env::var(name).is_ok_and(|v| !v.trim().is_empty()))
                        .map(|name| format!("env {name}"))
                        .unwrap_or_else(|| "inline api_key".to_string());
                    checks.push(ok(format!(
                        "provider {}: key from {source}",
                        config.model_provider
                    )));
                }
            }
            for (alias, entry) in &config.models {
                if config.model_providers.contains_key(&entry.provider) {
                    checks.push(ok(format!(
                        "models.{alias}: {} via {}",
                        entry.model, entry.provider
                    )));
                } else {
                    checks.push(fail(format!(
                        "models.{alias}: unknown provider {:?} (not in [model_providers])",
                        entry.provider
                    )));
                }
            }
            // The secondary alias steers side-session cost: a dangling
            // pointer degrades silently at runtime, so doctor surfaces it.
            if let Some(alias) = &config.secondary_model {
                match config.models.get(alias) {
                    Some(entry) => checks.push(ok(format!(
                        "secondary_model.{alias}: {} via {}",
                        entry.model, entry.provider
                    ))),
                    None => checks.push(fail(format!(
                        "secondary_model: alias {alias:?} is not in [models]"
                    ))),
                }
            }
            // Permission rules: built by the same function session
            // assembly uses, so this report cannot drift from what a
            // session actually loads. A rule that never applied is not a
            // crash — it is the one thing the user will otherwise hunt for.
            let permissions = operations_bootstrap::load_permissions(
                &config,
                Some(home),
                &settings_denylist(home),
            );
            if permissions.findings.is_empty() {
                checks.push(ok(format!(
                    "permissions: {} allow, {} grant(s), {} deny",
                    permissions.authored_allow,
                    permissions.persisted_grants,
                    permissions.deny.len()
                )));
            } else {
                for finding in &permissions.findings {
                    checks.push(fail(format!("permissions: {finding}")));
                }
            }
        }
    }

    // Console settings (a broken file silently degrades at runtime, so
    // doctor is the place where the breakage becomes visible).
    let settings_path = home.join(".wavecode").join("console-settings.json");
    if !settings_path.exists() {
        checks.push(ok("settings: defaults (no console-settings.json yet)"));
    } else {
        match std::fs::read_to_string(&settings_path)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                serde_json::from_str::<console_ui::settings::UiSettings>(&text)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }) {
            Ok(()) => checks.push(ok(format!("settings: {}", settings_path.display()))),
            Err(error) => checks.push(fail(format!(
                "settings: {} parses as defaults; fix or delete the file ({error})",
                settings_path.display()
            ))),
        }
    }

    // Custom themes: every file must resolve.
    let themes = console_ui::theme::file::list(home);
    if themes.is_empty() {
        checks.push(ok("themes: none custom"));
    } else {
        for name in &themes {
            match console_ui::theme::file::load(home, name) {
                Ok(_) => checks.push(ok(format!("theme {name}: parses"))),
                Err(error) => checks.push(fail(format!("theme {name}: {error}"))),
            }
        }
    }

    // Session records: the index must parse and every journal file
    // named by the index should still exist.
    let index = state_persistence::sessions::index_path(home);
    if !index.exists() {
        checks.push(ok("sessions: none recorded yet"));
    } else {
        let metas = state_persistence::sessions::list_sessions(home);
        if metas.is_empty() {
            // `list_sessions` reads a broken index as empty; tell a
            // valid empty index apart from one that fails to parse.
            let parses = std::fs::read_to_string(&index)
                .map(|text| {
                    serde_json::from_str::<Vec<state_persistence::sessions::SessionMeta>>(&text)
                        .is_ok()
                })
                .unwrap_or(false);
            if parses {
                checks.push(ok("sessions: index present, none recorded"));
            } else {
                checks.push(fail(format!(
                    "sessions: {} does not parse (read as empty; resumable sessions are lost until it is fixed or removed)",
                    index.display()
                )));
            }
        } else {
            let missing: Vec<&str> = metas
                .iter()
                .filter(|meta| {
                    !state_persistence::sessions::sessions_dir(home)
                        .join(format!("{}.jsonl", meta.id))
                        .exists()
                })
                .map(|meta| meta.id.as_str())
                .collect();
            if missing.is_empty() {
                checks.push(ok(format!(
                    "sessions: {} recorded, all journals present",
                    metas.len()
                )));
            } else {
                checks.push(fail(format!(
                    "sessions: {} recorded, missing journals for {}",
                    metas.len(),
                    missing.join(", ")
                )));
            }
        }
    }
    // OS confinement: policy rules on intent, this is the machine boundary
    // behind it. Report what actually holds rather than letting an
    // "available" backend read like a jail (the Windows job backend controls
    // process trees only — no filesystem or network boundary).
    checks.push(ok(format!(
        "sandbox: {}",
        operations_bootstrap::confinement_status()
    )));
    checks
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

# Other wire dialects: type = "openai-compatible" (Chat Completions, what
# most third-party gateways speak) or type = "openai-responses" (OpenAI
# Responses; needed for o1-pro / gpt-5-codex). base_url points at the API
# root, e.g. "https://api.openai.com/v1".
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
        // Legacy thread import; the new session records under its own id.
        session_id: None,
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

/// Write everything appended to `buffered` since the last call, then
/// flush. A broken pipe (e.g. `| head`) latches `broken` instead of
/// erroring: the consumer took what it needed and further writes are
/// skipped while the turn still drains for a clean shutdown.
fn stream_out(
    stream: &mut impl std::io::Write,
    buffered: &str,
    printed: &mut usize,
    broken: &mut bool,
) -> anyhow::Result<()> {
    if *broken || buffered.len() == *printed {
        return Ok(());
    }
    match stream
        .write_all(&buffered.as_bytes()[*printed..])
        .and_then(|()| stream.flush())
    {
        Ok(()) => *printed = buffered.len(),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => *broken = true,
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Write one JSONL line for a serializable value immediately. Returns
/// false on a broken pipe (latched by the caller).
fn stream_json_line<T: serde::Serialize>(
    out: &mut impl std::io::Write,
    value: &T,
    broken: &mut bool,
) -> anyhow::Result<bool> {
    if *broken {
        return Ok(true);
    }
    let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    match out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
    {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            *broken = true;
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// Session identity for a headless turn: when present, `exec` emits a
/// leading session meta line (JSON mode), journals the finished turn,
/// and leaves the session resumable via `wavecode --session <id>`.
struct ExecSession {
    id: String,
    home: PathBuf,
    cwd: String,
    /// Credential mask for the journal; filled once the session handle
    /// exists (constructed before assembly, gate arrives after).
    redactor: Option<console_ui::Redactor>,
}

impl ExecSession {
    /// The leading JSON-mode control line: it identifies the session
    /// session before any event so consumers can persist the handle.
    fn meta_line(&self) -> serde_json::Value {
        serde_json::json!({
            "meta": "session",
            "session_id": self.id,
            "version": env!("CARGO_PKG_VERSION"),
            "resume": format!("wavecode --session {}", self.id),
        })
    }

    /// Journal the finished turn (text-level snapshot: prompt plus the
    /// final answer; tool blocks are not replayed, matching the
    /// documented resume scope). Failures warn, never fail the turn.
    fn journal(&self, prompt: &str, answer: &str, outcome: &Outcome) {
        let name = match outcome {
            Outcome::Completed => "Completed",
            Outcome::Interrupted => "Interrupted",
            Outcome::Failed => "Failed",
        };
        let fallback = |text: &str| text.to_string();
        let redact: &dyn Fn(&str) -> String = match &self.redactor {
            Some(gate) => gate.as_ref(),
            None => &fallback,
        };
        if let Err(e) = state_persistence::sessions::record_turn(
            &self.home,
            &self.id,
            &self.cwd,
            prompt,
            &[(false, prompt.to_string()), (true, answer.to_string())],
            name,
            redact,
        ) {
            eprintln!("[warn] session journal update failed: {e}");
        }
    }
}

/// Parse the machine approval line: `<call_id> <allow|always|deny[:reason]>`.
/// Returns `None` for unrecognized decision tokens so a malformed line can
/// be re-sent instead of misread as a denial. The verb matches
/// case-insensitively; the deny reason is user content and keeps its
/// original casing (ASCII lowercasing preserves byte offsets, so the
/// reason is sliced from the untouched token).
fn parse_approval_line(line: &str) -> Option<(String, WireDecision)> {
    let trimmed = line.trim();
    let (call_id, token) = trimmed.split_once(char::is_whitespace)?;
    let call_id = call_id.trim();
    if call_id.is_empty() {
        return None;
    }
    let token = token.trim();
    let lower = token.to_ascii_lowercase();
    let decision = match lower.as_str() {
        "allow" => WireDecision::AllowOnce,
        "always" => WireDecision::AllowAlways,
        "deny" => WireDecision::Deny {
            reason: String::new(),
        },
        other => {
            let rest = other.strip_prefix("deny")?;
            let reason = token[token.len() - rest.len()..]
                .strip_prefix([':', ' '])?
                .to_string();
            WireDecision::Deny { reason }
        }
    };
    Some((call_id.to_string(), decision))
}

/// A stdin line reader for the approval dialect (lazy: only built when
/// `--approvals` is on, so plain exec never touches stdin).
fn exec_stdin_lines() -> tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>> {
    use tokio::io::AsyncBufReadExt as _;
    tokio::io::BufReader::new(tokio::io::stdin()).lines()
}

/// Drive the single exec turn to completion, streaming answer text to
/// stdout.
///
/// With `json`, stdout carries one JSON event per line while the human
/// rendering falls back to stderr (a leading `{"meta":"session",…}` control
/// line carries the resume handle; see `ExecSession::meta_line`); otherwise
/// stdout carries the answer text.
async fn run_exec(
    client: &mut ActorClient,
    prompt: &str,
    json: bool,
    approvals: bool,
    images: Vec<wavecode_wire::UserImage>,
    session: Option<ExecSession>,
) -> anyhow::Result<Outcome> {
    client
        .submit(Submission {
            id: "exec-1".to_string(),
            op: Op::UserInput {
                text: prompt.to_string(),
                images,
            },
        })
        .await
        .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;

    // Text mode renders into the two buffers and streams every append
    // as it lands; JSON mode writes each event line straight through.
    // Either way consumers see events in real time instead of after exit.
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    let mut printed_out = 0usize;
    let mut printed_err = 0usize;
    let mut broken = false;
    let mut failed = false;
    let mut interrupted = false;
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    // The session handle ships before any event so a consumer that only
    // keeps the first line still knows where the session lives.
    if json && let Some(sess) = &session {
        stream_json_line(&mut out, &sess.meta_line(), &mut broken)?;
    }
    // Approval answering: call ids parked for a decision, in request
    // order; stdin exists only with `--approvals`, and EOF (closed pipe)
    // flips to fail-closed auto-deny for everything still parked.
    let mut pending_approvals: std::collections::VecDeque<String> = Default::default();
    let mut stdin_lines = approvals.then(exec_stdin_lines);
    let mut input_closed = !approvals;
    let outcome = loop {
        tokio::select! {
            event = client.next_event() => {
                let Some(event) = event else {
                    // Actor exited without TurnCompleted: treat as failure.
                    break Outcome::Failed;
                };
                if json {
                    stream_json_line(&mut out, &event, &mut broken)?;
                }
                if let EventMsg::ApprovalRequested { call_id, kind, .. } = &event.msg {
                    if approvals && input_closed {
                        // --approvals ran but stdin closed: deny now,
                        // instead of parking until the gate timeout.
                        client
                            .submit(Submission {
                                id: format!("approval-{call_id}"),
                                op: Op::ExecApproval {
                                    call_id: call_id.clone(),
                                    decision: WireDecision::Deny {
                                        reason: "stdin closed".to_string(),
                                    },
                                },
                            })
                            .await
                            .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                    } else if approvals {
                        pending_approvals.push_back(call_id.clone());
                        if !json {
                            eprintln!(
                                "[approval] {} wants to {} — reply 'y', 'a' or 'n':",
                                call_id,
                                approval_what(kind)
                            );
                        }
                    }
                    // Without --approvals the headless gate denied at the
                    // gate itself; render_event's neutral line suffices.
                }
                if let Some(end) = render_event(&event.msg, &mut stdout_text, &mut stderr_text) {
                    if end == Outcome::Failed {
                        failed = true;
                    } else {
                        break if interrupted { Outcome::Interrupted } else { end };
                    }
                }
                // JSON stdout carries the protocol lines only; the text
                // rendering stays buffered as the human side channel.
                if !json {
                    stream_out(&mut out, &stdout_text, &mut printed_out, &mut broken)?;
                }
                stream_out(&mut err, &stderr_text, &mut printed_err, &mut broken)?;
            }
            line = async {
                match stdin_lines.as_mut() {
                    Some(reader) => reader.next_line().await,
                    // No reader (no --approvals): never wakes.
                    None => std::future::pending().await,
                }
            } => {
                match line {
                    Ok(Some(line)) => {
                        // JSON dialect lines carry an explicit call id and
                        // are ignored when malformed or already answered;
                        // text dialect lines answer the oldest parked
                        // request, and unrecognized input denies (same
                        // as the REPL).
                        let answer = if json {
                            parse_approval_line(&line)
                        } else {
                            pending_approvals
                                .front()
                                .cloned()
                                .map(|id| (id, decide_approval(&line)))
                        };
                        let Some((target, decision)) = answer else {
                            continue;
                        };
                        if !pending_approvals.iter().any(|id| *id == target) {
                            continue; // not parked (already answered): ignore
                        }
                        pending_approvals.retain(|id| *id != target);
                        client
                            .submit(Submission {
                                id: format!("approval-{target}"),
                                op: Op::ExecApproval {
                                    call_id: target,
                                    decision,
                                },
                            })
                            .await
                            .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                    }
                    // stdin closed or unreadable: everything still parked
                    // denies now rather than at the gate timeout.
                    _ => {
                        input_closed = true;
                        stdin_lines = None;
                        for call_id in pending_approvals.drain(..) {
                            client
                                .submit(Submission {
                                    id: format!("approval-{call_id}"),
                                    op: Op::ExecApproval {
                                        call_id,
                                        decision: WireDecision::Deny {
                                            reason: "stdin closed".to_string(),
                                        },
                                    },
                                })
                                .await
                                .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;
                        }
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
            stream_json_line(&mut out, &event, &mut broken)?;
        } else {
            render_event(&event.msg, &mut stdout_text, &mut stderr_text);
            stream_out(&mut out, &stdout_text, &mut printed_out, &mut broken)?;
            stream_out(&mut err, &stderr_text, &mut printed_err, &mut broken)?;
        }
    }
    if !json {
        stream_out(&mut out, &stdout_text, &mut printed_out, &mut broken)?;
    }
    stream_out(&mut err, &stderr_text, &mut printed_err, &mut broken)?;
    let end = if failed { Outcome::Failed } else { outcome };
    if let Some(sess) = &session {
        sess.journal(prompt, &stdout_text, &end);
    }
    Ok(if broken {
        // Closed stdout pipe (e.g. `| head`): the user took what they
        // needed; a clean end, not an error.
        Outcome::Completed
    } else {
        end
    })
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
                                images: Vec::new(),
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
                        op: Op::Compact { instruction: None },
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
                        op: Op::UserInput {
                            text,
                            images: Vec::new(),
                        },
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

    /// `--image` loading: known extensions map to mimes, unknown ones
    /// fail, and the bytes come back base64-encoded.
    #[test]
    fn load_image_sniffs_mime_and_encodes() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("pic.png");
        std::fs::write(&png, b"\x89PNG-fake-bytes").unwrap();
        let image = load_image(&png).unwrap();
        assert_eq!(image.mime, "image/png");
        use base64::Engine as _;
        assert_eq!(
            image.base64,
            base64::engine::general_purpose::STANDARD.encode(b"\x89PNG-fake-bytes")
        );
        let bad = dir.path().join("pic.bmp");
        std::fs::write(&bad, b"x").unwrap();
        assert!(load_image(&bad).is_err());
    }

    /// The size cap fires on the file metadata before the bytes are
    /// read, so an oversized image fails without loading into memory.
    #[test]
    fn load_image_rejects_oversized_file_by_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.png");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(5 * 1024 * 1024_u64 + 1)
            .unwrap();
        let error = load_image(&big).unwrap_err().to_string();
        assert!(error.contains("the limit is"), "{error}");
    }

    #[test]
    fn global_permission_mode_flag_parses() {
        let args =
            Args::try_parse_from(["wavecode", "--permission-mode", "plan", "exec", "hi"]).unwrap();
        assert_eq!(args.permission_mode.as_deref(), Some("plan"));
        let args = Args::try_parse_from(["wavecode", "repl"]).unwrap();
        assert_eq!(args.permission_mode, None);
    }

    #[test]
    fn plan_and_yolo_shortcuts_map_onto_wire_modes() {
        let args = Args::try_parse_from(["wavecode", "--plan", "exec", "hi"]).unwrap();
        assert_eq!(effective_permission_mode(&args).as_deref(), Some("plan"));
        let args = Args::try_parse_from(["wavecode", "-y", "repl"]).unwrap();
        assert_eq!(effective_permission_mode(&args).as_deref(), Some("auto"));
        // An explicit mode wins the (clap-rejected) combination check,
        // so the helper simply passes it through.
        let args = Args::try_parse_from(["wavecode", "--permission-mode", "wave"]).unwrap();
        assert_eq!(effective_permission_mode(&args).as_deref(), Some("wave"));
        let args = Args::try_parse_from(["wavecode"]).unwrap();
        assert_eq!(effective_permission_mode(&args), None);
    }

    #[test]
    fn mode_shortcuts_conflict_with_permission_mode() {
        let result = Args::try_parse_from(["wavecode", "-y", "--plan"]);
        assert!(result.is_err(), "combining shortcuts must be rejected");
        let result =
            Args::try_parse_from(["wavecode", "--permission-mode", "auto", "-y", "exec", "hi"]);
        assert!(result.is_err(), "flag + explicit mode must be rejected");
    }

    /// A home with nothing set up degrades gracefully: exactly the
    /// missing-config check fails, everything else reads as defaults.
    #[test]
    fn doctor_on_a_fresh_home_fails_only_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let checks = doctor_checks(None, Some(dir.path()));
        let failed: Vec<&str> = checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.line.as_str())
            .collect();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].contains("config"), "{failed:?}");
        let lines: Vec<&str> = checks.iter().map(|c| c.line.as_str()).collect();
        assert!(
            lines.iter().any(|l| l.contains("settings: defaults")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("sessions: none")),
            "{lines:?}"
        );
    }

    /// A complete setup passes every check, including provider key
    /// resolution through an inline api_key (never printed).
    #[test]
    fn doctor_passes_on_a_complete_setup() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "test-model"
model_provider = "test-provider"

[model_providers.test-provider]
type = "open-ai-compatible"
base_url = "http://127.0.0.1:9"
api_key = "doctor-test-secret"

[models.fast]
provider = "test-provider"
model = "test-model-fast"
"#,
        )
        .unwrap();
        let checks = doctor_checks(None, Some(dir.path()));
        let failed: Vec<&DoctorCheck> = checks.iter().filter(|check| !check.ok).collect();
        assert!(
            failed.is_empty(),
            "{:?}",
            failed.iter().map(|c| &c.line).collect::<Vec<_>>()
        );
        let lines: Vec<&str> = checks.iter().map(|c| c.line.as_str()).collect();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("models.fast: test-model-fast")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("key from inline api_key")),
            "{lines:?}"
        );
        // The key itself must never appear in doctor output.
        assert!(
            !lines.iter().any(|l| l.contains("doctor-test-secret")),
            "secret leaked: {lines:?}"
        );
    }

    /// A typo in a rule and an allow a deny rule fully shadows are both
    /// things the user cannot see from behavior alone: the first loads as
    /// nothing, the second loads and never fires.
    #[test]
    fn doctor_reports_invalid_and_dead_permission_rules() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "test-model"
model_provider = "test-provider"

[model_providers.test-provider]
type = "anthropic"
base_url = "https://api.example.com"
api_key = "k"

[permissions]
allow = ["not a rule", "Bash(git *)"]
deny = ["Bash(*)"]
"#,
        )
        .unwrap();
        let lines: Vec<String> = doctor_checks(None, Some(dir.path()))
            .into_iter()
            .filter(|check| check.line.starts_with("permissions:"))
            .map(|check| check.line)
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("invalid allow rule")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Bash(git *)") && l.contains("can never apply")),
            "{lines:?}"
        );
    }

    /// The denylist lives in console settings, so the grant report has to
    /// read that file to agree with what a session will enforce.
    #[test]
    fn doctor_sees_the_settings_denylist() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("console-settings.json"),
            r#"{"wave_denylist":["rm -rf"]}"#,
        )
        .unwrap();
        assert_eq!(settings_denylist(dir.path()), vec!["rm -rf".to_string()]);
        assert!(settings_denylist(&std::path::PathBuf::from("/no/such/home")).is_empty());
    }

    /// Grants CLI contract: list always answers, revoke reports what it
    /// could not find, and a real removal empties the table.
    #[test]
    fn grants_list_remove_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run_grants(Some(dir.path()), GrantsCommand::List));
        let grant = state_persistence::grants::Grant {
            rule: "Bash(cargo fmt)".to_string(),
            tool: "shell".to_string(),
            granted_at_secs: 1,
            session: "s1".to_string(),
        };
        state_persistence::grants::add_grant(dir.path(), &grant).unwrap();

        assert!(!run_grants(
            Some(dir.path()),
            GrantsCommand::Remove { index: 7 }
        ));
        assert!(run_grants(
            Some(dir.path()),
            GrantsCommand::Remove { index: 0 }
        ));
        assert!(
            state_persistence::grants::load_grants(dir.path())
                .grants
                .is_empty()
        );
        assert!(run_grants(Some(dir.path()), GrantsCommand::Clear));
        assert!(!run_grants(None, GrantsCommand::List));
    }

    /// Every run reports the machine boundary it actually has, so a partial
    /// backend is never presented as a jail.
    #[test]
    fn doctor_discloses_the_confinement_backend() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = doctor_checks(None, Some(dir.path()))
            .into_iter()
            .map(|check| check.line)
            .collect();
        assert!(
            lines.iter().any(|l| l.starts_with("sandbox: ")),
            "{lines:?}"
        );
    }

    /// A broken console-settings.json surfaces here even though runtime
    /// loading silently falls back to defaults.
    #[test]
    fn doctor_reports_broken_settings_and_unknown_model_providers() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "m"
model_provider = "p"

[model_providers.p]
type = "open-ai-compatible"
base_url = "http://127.0.0.1:9"
api_key = "inline-key"

[models.broken]
provider = "no-such-provider"
model = "m2"
"#,
        )
        .unwrap();
        std::fs::write(wave.join("console-settings.json"), "{not json").unwrap();
        let checks = doctor_checks(None, Some(dir.path()));
        let failed: Vec<&str> = checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.line.as_str())
            .collect();
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(
            failed.iter().any(|l| l.contains("models.broken")),
            "{failed:?}"
        );
        assert!(failed.iter().any(|l| l.contains("settings:")), "{failed:?}");
    }

    /// The secondary alias resolves through `[models]` for side
    /// sessions; unset degrades to None, a dangling alias warns and also
    /// degrades (doctor reports the same finding).
    #[test]
    fn secondary_model_resolution_has_three_outcomes() {
        let mut config = picker_config();
        assert_eq!(resolve_secondary(&config), None, "unset stays None");

        config.secondary_model = Some("fast".to_string());
        assert_eq!(
            resolve_secondary(&config),
            Some((
                "fast-model".to_string(),
                Some("fastp".to_string()),
                Some("low".to_string())
            ))
        );

        config.secondary_model = Some("ghost".to_string());
        assert_eq!(
            resolve_secondary(&config),
            None,
            "a dangling alias degrades instead of failing assembly"
        );
    }

    /// An empty-but-valid session index is healthy; a broken one fails.
    /// `list_sessions` reads both as empty, so doctor must parse the
    /// file itself to tell them apart.
    #[test]
    fn doctor_tells_an_empty_index_apart_from_a_broken_one() {
        // The other checks (config, settings) are out of scope here;
        // only the sessions line is asserted.
        let sessions_line = |dir: &tempfile::TempDir| {
            doctor_checks(None, Some(dir.path()))
                .into_iter()
                .find(|check| check.line.starts_with("sessions:"))
                .unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".wavecode").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        std::fs::write(state_persistence::sessions::index_path(dir.path()), "[]").unwrap();
        let check = sessions_line(&dir);
        assert!(check.ok, "{}", check.line);
        assert_eq!(check.line, "sessions: index present, none recorded");

        std::fs::write(
            state_persistence::sessions::index_path(dir.path()),
            "{not json",
        )
        .unwrap();
        let check = sessions_line(&dir);
        assert!(!check.ok, "{}", check.line);
        assert!(check.line.contains("does not parse"), "{}", check.line);
    }

    /// Crate boundary: the binary's workspace edges stay exactly the
    /// composition it was assembled against (actor, bootstrap, gateway,
    /// observe, eval, wire, persistence, config, tui). Concrete capability
    /// crates are reached only through bootstrap surfaces; new internal
    /// deps need a deliberate matrix update, not a silent Cargo.toml line.
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
                "operations-eval",
                "operations-gateway",
                "operations-observe",
                "state-persistence",
                "wavecode-config",
                "wavecode-wire",
            ],
            "harness internal deps changed; update the matrix deliberately",
        );
    }

    /// The machine approval dialect: `<call_id> <allow|always|deny[:reason]>`.
    /// Unrecognized tokens are `None` (re-sendable), never silently deny.
    #[test]
    fn approval_line_parsing() {
        assert_eq!(
            parse_approval_line("c1 allow"),
            Some(("c1".to_string(), WireDecision::AllowOnce))
        );
        assert_eq!(
            parse_approval_line("  c-2   ALWAYS "),
            Some(("c-2".to_string(), WireDecision::AllowAlways))
        );
        assert_eq!(
            parse_approval_line("c3 deny"),
            Some((
                "c3".to_string(),
                WireDecision::Deny {
                    reason: String::new()
                }
            ))
        );
        assert_eq!(
            parse_approval_line("c3 deny:tests are flaky"),
            Some((
                "c3".to_string(),
                WireDecision::Deny {
                    reason: "tests are flaky".to_string()
                }
            ))
        );
        // The verb matches case-insensitively, but the reason is user
        // content: its casing must survive the trip.
        assert_eq!(
            parse_approval_line("c4 DENY:Keep Original Case"),
            Some((
                "c4".to_string(),
                WireDecision::Deny {
                    reason: "Keep Original Case".to_string()
                }
            ))
        );
        // Unrecognized shapes are None, never a misread denial.
        assert_eq!(parse_approval_line("c3 yes"), None);
        assert_eq!(parse_approval_line("allow"), None);
        assert_eq!(parse_approval_line(""), None);
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
                    recoverable: true,
                    code: None
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
                    recoverable: false,
                    code: None
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
            rpm_limit: None,
            reasoning_effort: None,
            thinking_budget_tokens: None,
            prompt_caching: None,
            prompt_cache_ttl: None,
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
            permissions: wavecode_config::PermissionsConfig::default(),
            hooks: std::collections::HashMap::new(),
            mcp_servers: std::collections::HashMap::new(),
            models,
            secondary_model: None,
            max_tool_rounds: None,
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
            &|t: &str| t.to_string(),
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
