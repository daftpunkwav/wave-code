//! `wavecode` binary entrypoint: argument definitions, dispatch, and
//! the few helpers every surface shares.
//!
//! Each CLI surface lives in its own module: `exec` (headless single
//! turn), `repl` (interactive loop and legacy resume), `tui`
//! (fullscreen assembly), `serve` (mcp/acp/plugin/serve runners), and
//! `surfaces/*` (doctor, metrics, grants, update). This file owns the
//! clap `Args` tree, the permission-mode shortcut mapping, the
//! `Outcome` exit-code mapping, the model/provider override pairing
//! shared by every assembly path, and the `main` dispatch that
//! assembles one session for the interactive and exec surfaces.

use std::io::IsTerminal as _;
use std::path::PathBuf;

use clap::Parser;
use operations_bootstrap::{AssembleOptions, DEFAULT_IDENTITY, assemble_session};
use uuid::Uuid;

mod exec;
mod logging;
mod repl;
mod serve;
mod surfaces;
mod task_eval;
mod tui;
mod update;

use exec::{ExecSession, load_images, run_exec};
use repl::{run_repl, run_resume};
use serve::{run_acp, run_mcp_serve, run_plugin_list, run_serve};
use surfaces::doctor::doctor_checks;
use surfaces::grants::run_grants;
use surfaces::metrics::run_metrics;
use surfaces::update_cmd::{run_update_check, run_update_install};
use tui::run_tui_new;

/// Single-turn headless execution and interactive REPL.
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

/// Frontend surfaces.
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

/// MCP surfaces.
#[derive(Debug, Parser)]
enum McpCommand {
    /// Serve the builtin tools over stdio as an MCP server.
    Serve,
}

/// Plugin surfaces.
#[derive(Debug, Parser)]
enum PluginCommand {
    /// List installed plugins with skill/MCP/hook counts.
    List,
}

/// Grant surfaces.
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
            // None: every surface loads the shared denylist store.
            wave_denylist: None,
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
        // Cloned: the resume surface below re-passes the same override.
        config_path: args.config.clone(),
        model_override,
        provider_override,
        permission_override: permission_mode.clone(),
        thinking_override: settings.default_effort.clone(),
        cwd: cwd.clone(),
        home: home.clone(),
        // None: every surface loads the shared denylist store.
        wave_denylist: None,
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
            run_resume(thread_id, permission_mode, args.config, home).await?;
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

    /// Crate boundary: the binary's workspace edges stay exactly the
    /// composition it was assembled against (actor, bootstrap, gateway,
    /// observe, eval, wire, protocol, persistence, config, tui).
    /// Concrete capability crates are reached only through bootstrap
    /// surfaces; new internal deps need a deliberate matrix update, not
    /// a silent Cargo.toml line.
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
                // Mode cycle vocabulary shared with the TUI.
                "wavecode-protocol",
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

    /// `update` probes the release feed; the mutating self-install is
    /// opt-in behind `--install`. Flipping that default would make a
    /// bare `wavecode update` replace the running binary.
    #[test]
    fn update_install_is_opt_in() {
        let args = Args::try_parse_from(["wavecode", "update"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Update { install: false })
        ));
        let args = Args::try_parse_from(["wavecode", "update", "--install"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Update { install: true })
        ));
    }

    #[test]
    fn outcomes_map_to_exit_codes() {
        assert_eq!(Outcome::Completed.exit_code(), 0);
        assert_eq!(Outcome::Interrupted.exit_code(), 130);
        assert_eq!(Outcome::Failed.exit_code(), 1);
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
}
