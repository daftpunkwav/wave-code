//! Headless exec turn: event rendering, streaming, approvals, journaling.
//!
//! Owns the single-turn pipeline shared by the `exec` surface: event
//! rendering into the text streams (`render_event`), the approval
//! vocabulary (`approval_what`, `decide_approval`,
//! `parse_approval_line`), stdin streaming helpers, image loading, and
//! the resumable `ExecSession` journal. The REPL renders events through
//! `render_event` as well, so rendering and the approval vocabulary
//! live here rather than in either surface.

use std::path::PathBuf;

use operations_actor::ActorClient;
use wavecode_wire::{EventMsg, Op, Submission, WireDecision};

use crate::Outcome;

/// Render one event to the text streams.
///
/// Every model- or tool-sourced string (deltas, completions, plan/goal
/// bodies, warnings, errors, tool names, call ids) passes
/// `sanitize_terminal` before reaching the streams, matching the TUI's
/// threat model: ANSI / OSC sequences must not reach the terminal.
///
/// Returns a terminal outcome when the event ends the turn.
pub(crate) fn render_event(
    msg: &EventMsg,
    stdout: &mut String,
    stderr: &mut String,
) -> Option<Outcome> {
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
pub(crate) fn load_images(
    paths: &[std::path::PathBuf],
) -> anyhow::Result<Vec<wavecode_wire::UserImage>> {
    paths.iter().map(|p| load_image(p)).collect()
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
pub(crate) struct ExecSession {
    pub(crate) id: String,
    pub(crate) home: PathBuf,
    pub(crate) cwd: String,
    /// Credential mask for the journal; filled once the session handle
    /// exists (constructed before assembly, gate arrives after).
    pub(crate) redactor: Option<console_ui::Redactor>,
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
/// original casing, with leading whitespace after the separator trimmed
/// (ASCII lowercasing preserves byte offsets, so the reason is sliced from
/// the untouched token).
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
                .trim_start()
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
pub(crate) async fn run_exec(
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

/// Human phrase for an approval kind, shared by progress lines and prompts.
pub(crate) fn approval_what(kind: &wavecode_wire::ApprovalKind) -> &'static str {
    match kind {
        wavecode_wire::ApprovalKind::Exec => "execute a command",
        wavecode_wire::ApprovalKind::Write => "modify files",
    }
}

/// Map one approval answer line to a wire decision: y/yes approves once,
/// a/always approves with a session rule, anything else (including an
/// empty line) denies without a reason.
pub(crate) fn decide_approval(line: &str) -> wavecode_wire::WireDecision {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => wavecode_wire::WireDecision::AllowOnce,
        "a" | "always" => wavecode_wire::WireDecision::AllowAlways,
        _ => wavecode_wire::WireDecision::Deny {
            reason: String::new(),
        },
    }
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
        // content: its casing must survive the trip. The space-separated
        // `deny: reason` spelling trims like the contracted `deny:reason`.
        assert_eq!(
            parse_approval_line("c4 DENY:Keep Original Case"),
            Some((
                "c4".to_string(),
                WireDecision::Deny {
                    reason: "Keep Original Case".to_string()
                }
            ))
        );
        assert_eq!(
            parse_approval_line("c5 deny: spaced reason"),
            Some((
                "c5".to_string(),
                WireDecision::Deny {
                    reason: "spaced reason".to_string()
                }
            ))
        );
        // Unrecognized shapes are None, never a misread denial.
        assert_eq!(parse_approval_line("c3 yes"), None);
        assert_eq!(parse_approval_line("allow"), None);
        assert_eq!(parse_approval_line(""), None);
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
}
