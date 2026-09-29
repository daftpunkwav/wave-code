//! Interactive REPL: slash commands and the multi-turn input loop.
//!
//! Owns the `Slash` grammar (`parse_slash`), help text, the skill
//! request phrasing, the permission-mode cycle helper, the readline
//! loop (`run_repl`), and the inline approval/question prompts drained
//! by `drain_turn`. Also hosts `run_resume`: the legacy-thread surface
//! imports history, then continues in the REPL. Event rendering and
//! the approval vocabulary come from `crate::exec`; this module must
//! not re-implement them.

use std::io::Write as _;
use std::path::PathBuf;

use operations_actor::ActorClient;
use operations_bootstrap::{AssembleOptions, DEFAULT_IDENTITY, assemble_session};
use wavecode_wire::{EventMsg, Op, Submission};

use crate::exec::{approval_what, decide_approval, render_event};

/// Resume a legacy session from rollout journals.
///
/// Without an id, lists recent threads newest-first. With an id, imports
/// its history into a fresh assembly and continues in the REPL. History
/// import is plain text: tool payloads collapse to bracketed lines.
pub(crate) async fn run_resume(
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
        // None: every surface loads the shared denylist store.
        wave_denylist: None,
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

/// Next mode in the shared Shift+Tab cycle order (plan → auto → wave
/// → plan), wrapping around. The rotation lives in wavecode-protocol
/// so the REPL cannot drift from the TUI.
fn next_mode(current: &str) -> &'static str {
    wavecode_protocol::PermissionMode::cycle_from_str(current).as_str()
}

/// Interactive multi-turn session over one shared conversation.
///
/// Turns accumulate in the actor-side conversation; `/compact` runs an
/// idle compaction between turns. Ctrl-C at the prompt starts a fresh
/// line; Ctrl-C mid-turn interrupts the running turn.
pub(crate) async fn run_repl(
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
    fn permission_modes_cycle_in_wire_order() {
        assert_eq!(next_mode("plan"), "auto");
        assert_eq!(next_mode("auto"), "wave");
        assert_eq!(next_mode("wave"), "plan");
        // Unknown names re-enter the cycle at the front (plan).
        assert_eq!(next_mode("garbage"), "plan");
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
}
