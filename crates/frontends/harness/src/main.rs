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
use std::path::PathBuf;

use clap::Parser;
use operations_actor::ActorClient;
use operations_bootstrap::{AssembleOptions, DEFAULT_IDENTITY, assemble_session};
use operations_wire::{EventMsg, Op, Submission};

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
/// Returns a terminal outcome when the event ends the turn.
fn render_event(msg: &EventMsg, stdout: &mut String, stderr: &mut String) -> Option<Outcome> {
    match msg {
        EventMsg::TurnStarted => {
            stderr.push_str("[turn started]\n");
            None
        }
        EventMsg::AgentMessageDelta { text } => {
            stdout.push_str(text);
            None
        }
        EventMsg::AgentMessageComplete { text } => {
            // Deltas already streamed this text; the completion repeats
            // the full message for transcript use. Skip text already on
            // stdout so answers are not printed twice, while still
            // covering senders that emit a completion without deltas.
            if !text.is_empty() && !stdout.ends_with(text.as_str()) {
                stdout.push_str(text);
            }
            if !stdout.is_empty() && !stdout.ends_with('\n') {
                stdout.push('\n');
            }
            None
        }
        EventMsg::ToolCallBegin { call_id, name, .. } => {
            stderr.push_str(&format!("[tool] {name} ({call_id})\n"));
            None
        }
        EventMsg::ToolCallEnd { call_id, is_error } => {
            if *is_error {
                stderr.push_str(&format!("[tool] {call_id} reported an error\n"));
            }
            None
        }
        EventMsg::ApprovalRequested { call_id, kind, .. } => {
            // Neutral line: exec leaves the denial to the headless gate
            // while the REPL answers below via an inline prompt.
            stderr.push_str(&format!(
                "[approval] {call_id} wants to {}\n",
                approval_what(kind)
            ));
            None
        }
        EventMsg::TokenCount {
            input_tokens,
            output_tokens,
        } => {
            stderr.push_str(&format!("[usage] in={input_tokens} out={output_tokens}\n"));
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
        EventMsg::Warning { message } => {
            stderr.push_str(&format!("[warn] {message}\n"));
            None
        }
        EventMsg::Error {
            message,
            recoverable,
        } => {
            stderr.push_str(&format!("[error] {message}\n"));
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
    // Bare invocation: fullscreen TUI on a TTY, line REPL otherwise.
    if args.command.is_none() {
        if std::io::stdout().is_terminal() {
            run_tui_new(args.config, args.model, cwd, home).await?;
            std::process::exit(Outcome::Completed.exit_code())
        }
        let mut handle = assemble_session(AssembleOptions {
            config_path: args.config,
            model_override: args.model,
            cwd,
            home: home.clone(),
            identity: DEFAULT_IDENTITY.to_string(),
            headless: false,
            initial_history: Vec::new(),
        })
        .map_err(|e| anyhow::anyhow!("session assembly failed: {e}"))?;
        for warning in &handle.warnings {
            eprintln!("[warn] {warning}");
        }
        run_repl(
            &mut handle.client,
            &handle.memory_index,
            &handle.mcp_servers,
        )
        .await?;
        std::process::exit(Outcome::Completed.exit_code())
    }
    // Only headless exec denies approvals openly: the REPL parks them on
    // the gate and answers inline, which needs parking enabled here.
    let headless = matches!(args.command, Some(Command::Exec { .. }));
    let mut handle = assemble_session(AssembleOptions {
        config_path: args.config,
        model_override: args.model,
        cwd,
        home: home.clone(),
        identity: DEFAULT_IDENTITY.to_string(),
        headless,
        initial_history: Vec::new(),
    })
    .map_err(|e| anyhow::anyhow!("session assembly failed: {e}"))?;
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
            )
            .await?;
            std::process::exit(Outcome::Completed.exit_code())
        }
        Some(Command::Resume { thread_id }) => {
            run_resume(thread_id, home).await?;
            std::process::exit(Outcome::Completed.exit_code())
        }
        None => unreachable!("bare invocation returns above"),
    }
}

/// Fullscreen TUI over a live harness session.
///
/// Interactive approval parking works here (headless stays false);
/// config failures keep the exit-code-2-with-guidance contract.
async fn run_tui_new(
    config: Option<PathBuf>,
    model: Option<String>,
    cwd: PathBuf,
    home: Option<PathBuf>,
) -> anyhow::Result<()> {
    let handle = match assemble_session(AssembleOptions {
        config_path: config,
        model_override: model,
        cwd: cwd.clone(),
        home,
        identity: DEFAULT_IDENTITY.to_string(),
        headless: false,
        initial_history: Vec::new(),
    }) {
        Ok(handle) => handle,
        Err(operations_bootstrap::SessionError::Config(e)) => {
            print_config_error(&e);
            std::process::exit(2)
        }
    };
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }
    let ctx = wavecode_tui::TuiContext {
        model_name: handle.model_name,
        cwd,
        permission_mode: tui_permission_mode(&handle.permission_mode),
        skill_names: handle.skill_names,
        mcp_server_lines: handle.mcp_servers,
        memory_index: handle.memory_index,
    };
    wavecode_tui::run(handle.client, ctx).await
}

/// Map an assembled permission-mode wire name onto the TUI enum.
///
/// Unknown names fall back to Default so the TUI never shows a mode the
/// policy rejected.
fn tui_permission_mode(wire: &str) -> wavecode_tui::PermissionMode {
    match wire {
        "plan" => wavecode_tui::PermissionMode::Plan,
        "acceptEdits" => wavecode_tui::PermissionMode::AcceptEdits,
        "bypassPermissions" => wavecode_tui::PermissionMode::BypassPermissions,
        _ => wavecode_tui::PermissionMode::Default,
    }
}

/// Print a config error with creation guidance on missing files.
fn print_config_error(err: &wavecode_config::ConfigError) {
    eprintln!("错误：{err}");
    if let wavecode_config::ConfigError::NotFound(path) = err {
        eprintln!(
            r#"
请创建配置文件 {}，内容示例：

model = "claude-sonnet-4-5"
model_provider = "anthropic"

[model_providers.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com"
# api key 二选一（env_key 优先）：
# 方式一（推荐）：env_key 指向环境变量名，运行时从该变量读取 key
env_key = "ANTHROPIC_API_KEY"
# 方式二：内联 api_key（注意保密，勿提交版本库）
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
async fn run_resume(thread_id: Option<String>, home: Option<PathBuf>) -> anyhow::Result<()> {
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
        cwd,
        home: Some(home),
        identity: DEFAULT_IDENTITY.to_string(),
        // Resumed sessions continue interactively: park approvals for
        // inline answers instead of denying them openly.
        headless: false,
        initial_history: history,
    })
    .map_err(|e| anyhow::anyhow!("session assembly failed: {e}"))?;
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }
    run_repl(
        &mut handle.client,
        &handle.memory_index,
        &handle.mcp_servers,
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
    /// Cycle the session permission mode.
    Permissions,
    /// Show help text.
    Help,
    /// Unknown slash command with its name.
    Unknown(String),
    /// Plain user input for the next turn.
    Text(String),
}

/// Parse one REPL line into slash commands or plain text.
fn parse_slash(line: &str) -> Slash {
    let trimmed = line.trim();
    if let Some(command) = trimmed.strip_prefix('/') {
        let name = command.split_whitespace().next().unwrap_or("");
        return match name {
            "quit" | "exit" => Slash::Quit,
            "compact" => Slash::Compact,
            "memory" => Slash::Memory,
            "mcp" => Slash::Mcp,
            "permissions" => Slash::Permissions,
            "help" => Slash::Help,
            _ => Slash::Unknown(name.to_string()),
        };
    }
    Slash::Text(trimmed.to_string())
}

/// REPL help text printed for `/help` and unknown commands.
const REPL_HELP: &str = "commands: /compact (compress context now), /memory (show memory index), /mcp (list servers), /permissions (cycle approval mode), /quit (end session), /help";

/// Permission modes in `/permissions` cycle order (wire names).
const PERMISSION_CYCLE: &[&str] = &["default", "plan", "acceptEdits", "bypassPermissions"];

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
            json_lines.push_str(
                &serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string()),
            );
            json_lines.push('\n');
        } else {
            render_event(&event.msg, &mut stdout_text, &mut stderr_text);
        }
    }
    let end = if failed {
        Outcome::Failed
    } else {
        outcome
    };
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
/// Returns true on a closed stdout pipe; panicking `print!` would turn
/// `| head` into a crash, while `writeln!` lets a broken pipe read as a
/// clean end. Stderr failures stay hard errors.
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
    err.write_all(stderr_text.as_bytes())?;
    err.flush()?;
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
) -> anyhow::Result<()> {
    use rustyline::error::ReadlineError;

    let mut editor = rustyline::DefaultEditor::new()?;
    let mut turn: u64 = 0;
    let mut permission_mode = "default";
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
            Slash::Unknown(name) => println!("unknown command /{name}; {REPL_HELP}"),
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
fn approval_what(kind: &operations_wire::ApprovalKind) -> &'static str {
    match kind {
        operations_wire::ApprovalKind::Exec => "execute a command",
        operations_wire::ApprovalKind::Write => "modify files",
    }
}

/// Map one approval answer line to a wire decision: y/yes approves once,
/// anything else (including an empty line) denies without a reason.
fn decide_approval(line: &str) -> operations_wire::WireDecision {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => operations_wire::WireDecision::AllowOnce,
        _ => operations_wire::WireDecision::Deny {
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
) -> anyhow::Result<operations_wire::WireDecision> {
    use rustyline::error::ReadlineError;
    println!("allow {what} ({call_id})? [y/N] ");
    match editor.readline("> ") {
        Ok(line) => Ok(decide_approval(&line)),
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            Ok(operations_wire::WireDecision::Deny {
                reason: String::new(),
            })
        }
        Err(e) => {
            eprintln!("[approval] input error, denying: {e}");
            Ok(operations_wire::WireDecision::Deny {
                reason: String::new(),
            })
        }
    }
}

/// Drain events until the turn ends, rendering as they arrive.
///
/// Parked approvals are answered inline on the REPL editor: the gate holds
/// the turn until this submits a decision, so REPL sessions must assemble
/// with parking enabled (exec keeps the headless gate and never calls this
/// with a live turn expecting answers).
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
    fn outcomes_map_to_exit_codes() {
        assert_eq!(Outcome::Completed.exit_code(), 0);
        assert_eq!(Outcome::Interrupted.exit_code(), 130);
        assert_eq!(Outcome::Failed.exit_code(), 1);
    }

    #[test]
    fn permission_modes_cycle_in_wire_order() {
        assert_eq!(next_mode("default"), "plan");
        assert_eq!(next_mode("plan"), "acceptEdits");
        assert_eq!(next_mode("acceptEdits"), "bypassPermissions");
        assert_eq!(next_mode("bypassPermissions"), "default");
        assert_eq!(next_mode("garbage"), "plan");
    }

    #[test]
    fn slash_lines_route_to_commands() {
        assert_eq!(parse_slash("/quit"), Slash::Quit);
        assert_eq!(parse_slash("/exit"), Slash::Quit);
        assert_eq!(parse_slash("  /compact  "), Slash::Compact);
        assert_eq!(parse_slash("/memory"), Slash::Memory);
        assert_eq!(parse_slash("/mcp"), Slash::Mcp);
        assert_eq!(parse_slash("/permissions"), Slash::Permissions);
        assert_eq!(parse_slash("/help"), Slash::Help);
        assert_eq!(parse_slash("/nope"), Slash::Unknown("nope".to_string()));
        assert_eq!(
            parse_slash("hello there"),
            Slash::Text("hello there".to_string())
        );
        // A leading slash with no name is unknown, not text.
        assert_eq!(parse_slash("/"), Slash::Unknown(String::new()));
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
        use operations_wire::WireDecision;
        assert_eq!(decide_approval("y"), WireDecision::AllowOnce);
        assert_eq!(decide_approval("YES"), WireDecision::AllowOnce);
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
            approval_what(&operations_wire::ApprovalKind::Exec),
            "execute a command"
        );
        assert_eq!(
            approval_what(&operations_wire::ApprovalKind::Write),
            "modify files"
        );
    }

    #[test]
    fn tui_permission_modes_follow_wire_names() {
        use wavecode_tui::PermissionMode;
        assert_eq!(tui_permission_mode("plan"), PermissionMode::Plan);
        assert_eq!(
            tui_permission_mode("acceptEdits"),
            PermissionMode::AcceptEdits
        );
        assert_eq!(
            tui_permission_mode("bypassPermissions"),
            PermissionMode::BypassPermissions
        );
        assert_eq!(tui_permission_mode("default"), PermissionMode::Default);
        assert_eq!(tui_permission_mode("typo"), PermissionMode::Default);
    }
}

