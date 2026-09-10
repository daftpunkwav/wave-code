/*!
 * @file HarnessCli
 * @description Non-interactive single-turn CLI over the new harness stack.
 *
 * Responsibilities:
 * - Assemble a headless session from config and arguments.
 * - Stream one turn to stdout with progress on stderr.
 * - Map terminal outcomes to process exit codes.
 *
 * This binary is the first frontend on the new stack. Interactive
 * frontends (REPL, TUI) port separately; nothing here is shared with
 * the legacy CLI.
 */

//! Headless `harness exec`: one prompt in, streamed answer out.
//!
//! Approvals deny openly (nothing can answer), interrupts arrive via
//! Ctrl-C, and every event lands either on stdout (answer text) or
//! stderr (progress, usage, diagnostics).

use std::io::Write as _;
use std::path::PathBuf;

use clap::Parser;
use operations_bootstrap::{AssembleOptions, DEFAULT_IDENTITY, assemble_session};
use operations_wire::{EventMsg, Op, Submission};

/// Single-turn headless execution over the new harness.
#[derive(Debug, Parser)]
#[command(name = "harness", version)]
struct Args {
    /// Config file path; defaults to the user-level config.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Model override winning over the configured model.
    #[arg(long)]
    model: Option<String>,

    /// Prompt text for the single turn.
    prompt: String,
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
        EventMsg::AgentMessageComplete => {
            stdout.push('\n');
            None
        }
        EventMsg::ToolCallBegin { call_id, name } => {
            stderr.push_str(&format!("[tool] {name} ({call_id})\n"));
            None
        }
        EventMsg::ToolCallEnd { call_id, is_error } => {
            if *is_error {
                stderr.push_str(&format!("[tool] {call_id} reported an error\n"));
            }
            None
        }
        EventMsg::ApprovalRequested { call_id, .. } => {
            stderr.push_str(&format!(
                "[approval] {call_id} denied: non-interactive session\n"
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
        EventMsg::CompactStarted => {
            stderr.push_str("[compact started]\n");
            None
        }
        EventMsg::CompactCompleted => {
            stderr.push_str("[compact completed]\n");
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
        EventMsg::TurnCompleted => Some(Outcome::Completed),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir()?;
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let mut handle = assemble_session(AssembleOptions {
        config_path: args.config,
        model_override: args.model,
        cwd,
        home,
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
    })
    .map_err(|e| anyhow::anyhow!("session assembly failed: {e}"))?;
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }

    handle
        .client
        .submit(Submission {
            id: "exec-1".to_string(),
            op: Op::UserInput { text: args.prompt },
        })
        .await
        .map_err(|e| anyhow::anyhow!("submission failed: {e}"))?;

    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    let mut failed = false;
    let mut interrupted = false;
    let outcome = loop {
        tokio::select! {
            event = handle.client.next_event() => {
                let Some(event) = event else {
                    // Actor exited without TurnCompleted: treat as failure.
                    break Outcome::Failed;
                };
                if let Some(end) = render_event(&event.msg, &mut stdout_text, &mut stderr_text) {
                    if end == Outcome::Failed {
                        failed = true;
                    } else {
                        break if interrupted { Outcome::Interrupted } else { end };
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = handle
                    .client
                    .submit(Submission { id: "exec-interrupt".to_string(), op: Op::Interrupt })
                    .await;
                interrupted = true;
            }
        }
    };
    print!("{stdout_text}");
    eprint!("{stderr_text}");
    std::io::stdout().flush()?;
    std::io::stderr().flush()?;
    let outcome = if failed { Outcome::Failed } else { outcome };
    std::process::exit(outcome.exit_code())
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
        assert!(render_event(&EventMsg::AgentMessageComplete, &mut out, &mut err).is_none());
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
            render_event(&EventMsg::TurnCompleted, &mut out, &mut err),
            Some(Outcome::Completed)
        );
    }
}
