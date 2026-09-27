//! shell tool: cross-platform command execution (non-interactive: stdin is nulled; stdout/stderr are piped and captured).
//! Failure semantics match the `fs` file tools: business failures (non-zero exit code, timeout, spawn failure, missing/mistyped params)
//! return `Ok(is_error=true)` with the reason fed back to the model; `Err` is only for implementation-level failures.
//!
//! Known limitation: `kill_on_drop` only kills the shell process itself; grandchildren already
//! inherited copies of cmd's pipe handles at spawn (command-line redirection cannot prevent that) and survive as
//! orphans after the shell is killed -- the real variable is how long those orphaned grandchildren live.
//! Production risk: when wavecode exits and drops the tokio runtime it may block for an arbitrarily long time
//! (e.g. when a grandchild is a dev server). The proper fix is process-group-level reaping (Windows Job
//! Object / Unix killpg); it is not implemented yet.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use infrastructure_base::shell_invocation;
use serde_json::{Value, json};

use crate::is_sensitive_env_name;

use crate::{Result, Tool, ToolCtx, ToolOutput, err_output, lock};

/// Default timeout: 60 s.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// Timeout cap: 300 s, clamped to the cap when exceeded.
const MAX_TIMEOUT_MS: u64 = 300_000;
/// Per-stream stdout / stderr output cap: 30 KB.
pub(crate) const MAX_OUTPUT_BYTES: usize = 30 * 1024;

/// Post-run drain grace: how long the reader tasks may take to reach EOF
/// after the child exited or was killed, before the capture-so-far is
/// returned anyway. Death normally closes the pipe write ends and EOF lands
/// within milliseconds; a grandchild that inherited the write ends and
/// outlives the shell (see the module limitation note) must not extend the
/// run past its own bound — the detached readers keep draining (buffering
/// stays capped) until real EOF.
const KILL_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Whether OS-level confinement applies to shell spawns.
///
/// Default off: set `WAVECODE_SANDBOX_OS=1` in the environment to require it
/// (there is no config-file equivalent — the variable is the only switch).
/// When enabled and the platform has no backend, the command fails closed
/// with a business error — execution never silently downgrades to an
/// unconfined spawn.
fn os_sandbox_enabled() -> bool {
    std::env::var("WAVECODE_SANDBOX_OS").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Decode and truncate one output stream: UTF-8 boundary safe, appending `[truncated]` past the cap.
pub(crate) fn truncate_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.into_owned();
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]", &text[..cut])
}

/// [`truncate_output`], plus a pointer to the full text when truncation
/// fired and a spill store is configured: the full captured stream (lossily
/// decoded; output past the capture cap was already drained, and the store
/// caps its own total size) lands in the store and the result names its
/// `spill://` URI so the model can page the middle back through the `spill`
/// tool instead of never seeing it. Store failures degrade to plain
/// truncation — the pointer is an addition, never a replacement for the
/// capped text.
pub(crate) fn truncate_output_spilled(
    bytes: &[u8],
    store: Option<&wavecode_context::SpillStore>,
) -> String {
    // Truncation is decided on the captured length, not the marker: a
    // stream that itself ends with the literal `[truncated]` line must not
    // spill as if it had been cut.
    let full = String::from_utf8_lossy(bytes);
    let truncated = full.len() > MAX_OUTPUT_BYTES;
    let text = truncate_output(bytes);
    if !truncated {
        return text;
    }
    let Some(store) = store else {
        return text;
    };
    match store.spill(&full) {
        // The URI sits whitespace-delimited with no glued punctuation, so
        // both the model and the spill tool can lift it out verbatim.
        Ok(uri) => format!(
            "{text}\nfull output saved to {uri} (read it back with the spill tool)"
        ),
        Err(_) => text,
    }
}

/// Strip sensitive environment variables before spawn, so the model cannot read secrets via `set` / `env` / `echo %VAR%`.
///
/// Two layers:
/// 1. The explicit `ctx.deny_env` list (the assembly layer injects the provider's `env_key`, e.g. `MINIMAX_API_KEY`);
/// 2. A shape fallback ([`is_sensitive_env_name`]): common secret shapes stay stripped even without deny_env.
///
/// Threat-model boundary: this only guards "leaks via child-process environment inheritance"; reading an inline
/// api_key straight from `type config.toml` is an accepted M1 surface (on record in the M1 review), out of scope here.
pub(crate) fn sanitize_env(cmd: &mut tokio::process::Command, ctx: &ToolCtx) {
    for name in &ctx.deny_env {
        cmd.env_remove(name);
    }
    for (key, _) in std::env::vars_os() {
        if is_sensitive_env_name(&key.to_string_lossy()) {
            cmd.env_remove(&key);
        }
    }
}

/// Per-stream capture cap.
///
/// Bounded well above [`MAX_OUTPUT_BYTES`] so the visible truncation
/// contract is unchanged (`truncate_output` still cuts to the display cap
/// and appends its marker), while a chatty child can no longer grow the
/// process by output-rate x timeout: buffering stops at the cap and the
/// rest is drained and discarded.
pub(crate) const STREAM_CAPTURE_CAP: usize = MAX_OUTPUT_BYTES + 1024 * 1024;

/// Collect one child output stream into the shared buffer, at most `cap`
/// bytes. Reads run to EOF so the child never blocks on a full pipe; bytes
/// past the cap are discarded. Read errors degrade to a truncated capture
/// (the exit code still surfaces) instead of failing the whole call. The
/// buffer is shared so the bounded wait can snapshot the capture-so-far
/// without depending on the readers reaching EOF; the lock is never held
/// across the read await.
async fn collect_capped<R: tokio::io::AsyncRead + Unpin>(
    stream: Option<R>,
    cap: usize,
    sink: Arc<Mutex<Vec<u8>>>,
) {
    use tokio::io::AsyncReadExt;
    let Some(mut stream) = stream else {
        return;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut buf = lock(&sink);
                let take = n.min(cap.saturating_sub(buf.len()));
                buf.extend_from_slice(&chunk[..take]);
            }
        }
    }
}

/// One child run's captured output.
///
/// `status` is `None` when the timeout fired first: the child was killed and
/// whatever the streams had produced by then is kept, so a long build's log
/// head survives its own timeout instead of vanishing with the process.
pub(crate) struct Collected {
    /// Exit status; `None` means the timeout killed the child.
    pub status: Option<std::process::ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Spawn `cmd` and collect its piped streams into at most `cap` bytes each,
/// bounded by `timeout` across the whole run (spawn + reads + wait).
///
/// Reads run to EOF so the child never blocks on a full pipe; bytes past
/// the cap are drained and discarded. Read errors degrade to a truncated
/// capture (the exit code still surfaces) instead of failing the call.
/// On timeout the child is killed and [`Collected::status`] comes back
/// `None` with the partial streams attached. The post-run reader join is
/// grace-bounded ([`KILL_DRAIN_GRACE`]) so a pipe-inheriting grandchild
/// that outlives the shell cannot stretch the run past its bound. Shared
/// by `Shell` and the script tools so no child-output path buffers
/// unbounded or leaks a killed process's captured output.
pub(crate) async fn spawn_collect_bounded(
    cmd: &mut tokio::process::Command,
    cap: usize,
    timeout: Duration,
) -> std::io::Result<Collected> {
    let mut child = cmd.spawn()?;
    // The capture buffers live outside the reader tasks so the bounded
    // wait below can return the partial streams without depending on the
    // readers reaching EOF (a grandchild holding the write ends open can
    // delay that indefinitely).
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let out_task = tokio::spawn(collect_capped(child.stdout.take(), cap, stdout.clone()));
    let err_task = tokio::spawn(collect_capped(child.stderr.take(), cap, stderr.clone()));
    // Tokio's kill awaits the child's exit (reaping it); killing an
    // already-exited child degrades to Err here, which changes nothing —
    // its pipe ends are closed either way.
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => {
            let _ = child.kill().await;
            None
        }
    };
    // Grace-bounded reader join: the run's exit (or the kill) closes the
    // shell's pipe ends, so EOF normally lands within milliseconds and the
    // grace never binds. A grandchild that inherited the write ends and
    // outlives the shell gets exactly the grace, then the partial capture
    // is returned and the detached readers (still capped) drain to EOF on
    // their own. kill_on_drop remains the drop-path backstop.
    //
    // `tokio::join!` expands to an immediately-awaited expression, so it
    // wraps in an async block to become a future the grace can bound.
    let _ = tokio::time::timeout(KILL_DRAIN_GRACE, async {
        // Join results are irrelevant: the capture lives in the shared
        // buffers (a panicking reader leaves its partial capture there).
        let _ = tokio::join!(out_task, err_task);
    })
    .await;
    Ok(Collected {
        status,
        stdout: lock(&stdout).clone(),
        stderr: lock(&stderr).clone(),
    })
}

/// Execute a shell command (a writing tool: may modify files or spawn processes, needs serial scheduling).
///
/// The optional `spill` store receives the *full* text of any output that
/// had to be truncated, so the model can page the middle back through the
/// `spill` tool; `None` (tests, hermetic registries) degrades to plain
/// truncation.
pub struct Shell {
    spill: Option<wavecode_context::SpillStore>,
}

impl Shell {
    /// Configure the spill store for truncated full outputs.
    pub(crate) fn new(spill: Option<wavecode_context::SpillStore>) -> Self {
        Self { spill }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new(None)
    }
}

#[async_trait::async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a shell command in the working directory. The default shell is platform-dependent \
         (cmd /C on Windows, sh -c on Unix; override with the WAVECODE_SHELL env var). \
         Use timeout_ms to bound execution (default 60000 ms, clamped to 300000 ms); on timeout \
         the process is killed and the output produced so far is reported. stdout and stderr are \
         captured separately, each truncated at 30KB; truncated completed runs also save the \
         full output to a spill:// URI the spill tool can read back. The command runs \
         non-interactive (stdin is closed)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute in the working directory"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 60000, clamped to max 300000)"
                }
            },
            "required": ["command"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let command = match input.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => {
                return Ok(err_output(
                    "missing or invalid parameter 'command' (string required)",
                ));
            }
        };
        let timeout_ms = match input.get("timeout_ms") {
            None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
            Some(v) => match v.as_u64() {
                Some(n) => n.min(MAX_TIMEOUT_MS),
                None => {
                    return Ok(err_output(
                        "invalid parameter 'timeout_ms' (non-negative integer required)",
                    ));
                }
            },
        };

        let (program, flag) = shell_invocation();
        let mut cmd = tokio::process::Command::new(program);
        cmd.arg(flag).arg(command).current_dir(&ctx.cwd);
        // OS confinement (chain: bwrap -> Landlock -> seatbelt -> Windows
        // job object; opt-in via WAVECODE_SANDBOX_OS): confine FIRST — rewriting backends replace
        // `cmd` wholesale, so stdio, env scrubbing and kill_on_drop below
        // apply to the final confined command. Any failure fails closed
        // here (SANDBOX_UNAVAILABLE when no backend is available) — the
        // command never runs unconfined when confinement was requested.
        // Timeout / kill / truncate behavior below is unchanged.
        if os_sandbox_enabled() {
            let profile = wavecode_sandbox::ConfinementProfile::for_shell(&ctx.cwd);
            let backend = wavecode_sandbox::detect_backend();
            if let Err(e) = backend.spawn_confined(&mut cmd, &profile) {
                return Ok(err_output(format!(
                    "OS sandbox confinement failed ({}): {e}",
                    backend.backend_name()
                )));
            }
        }
        // Scrubbing lands on the final command (post-confinement): strip
        // deny_env entries and sensitive-shape variables to prevent leaks.
        sanitize_env(&mut cmd, ctx);
        cmd
            // Non-interactive: null stdin so interactive commands (read/pause/npm init) cannot steal the host terminal's input.
            .stdin(std::process::Stdio::null())
            // wait_with_output only collects piped streams; the default inherit would read nothing.
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Backstop for the drop path; spawn_collect_bounded kills
            // explicitly on the timeout path so the pipes close and the
            // readers hand back what they captured.
            .kill_on_drop(true);
        // The bounded run covers spawn + reads + wait in one contract; both
        // streams drain concurrently, so a full pipe buffer cannot deadlock
        // it, and each stream stops buffering at STREAM_CAPTURE_CAP so a
        // chatty child cannot grow memory without bound.
        let output = match spawn_collect_bounded(
            &mut cmd,
            STREAM_CAPTURE_CAP,
            Duration::from_millis(timeout_ms),
        )
        .await
        {
            Ok(output) => output,
            // Spawn failures (e.g. missing shell) are business output for the model, not Err.
            Err(e) => {
                return Ok(err_output(format!("failed to spawn shell: {e}")));
            }
        };

        // Timeout: report the partial streams alongside the reason — a
        // long build's log head is exactly what diagnosing the timeout needs
        // — and name the background path, so work that must outlive the
        // foreground limit continues as a job instead of being re-run.
        let Some(status) = output.status else {
            let mut content = format!(
                "timeout after {timeout_ms}ms, the process was killed: {command}\npartial output before the kill:"
            );
            let stdout = truncate_output(&output.stdout);
            let stderr = truncate_output(&output.stderr);
            if !stdout.is_empty() {
                content.push_str(&format!("\n--- stdout ---\n{stdout}"));
            }
            if !stderr.is_empty() {
                content.push_str(&format!("\n--- stderr ---\n{stderr}"));
            }
            content.push_str(
                "\nfor commands expected to run longer than the limit, start them with \
                 job_spawn instead: the job keeps running and job_wait / job_output collect \
                 its result without blocking the turn",
            );
            return Ok(err_output(content));
        };

        // On Unix, code() is None when killed by a signal; record -1 (still non-zero, so is_error holds).
        let code = status.code().unwrap_or(-1);
        let stdout = truncate_output_spilled(&output.stdout, self.spill.as_ref());
        let stderr = truncate_output_spilled(&output.stderr, self.spill.as_ref());
        let mut content = format!("exit code: {code}");
        if !stdout.is_empty() {
            content.push_str(&format!("\n--- stdout ---\n{stdout}"));
        }
        if !stderr.is_empty() {
            content.push_str(&format!("\n--- stderr ---\n{stderr}"));
        }
        Ok(ToolOutput {
            content,
            is_error: code != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCtx};

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn captures_stdout_and_exit_code() {
        // Spawns a child: held under ENV_LOCK (see its docs).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, c) = ctx();
        let out = Shell::default()
            .execute(serde_json::json!({"command": "echo hello"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("exit code: 0"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn nonzero_exit_is_error_but_captured() {
        // Spawns a child: held under ENV_LOCK (see its docs).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, c) = ctx();
        let out = Shell::default()
            .execute(serde_json::json!({"command": "exit 3"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("exit code: 3"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn respects_timeout() {
        // Spawns a child: held under ENV_LOCK (see its docs).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, c) = ctx();
        let cmd = if cfg!(windows) {
            // cmd builtin busy-wait: spawns no grandchildren, so no orphans remain after a timeout kill (ping would leave some).
            "for /l %i in (1,1,1000000000) do @rem"
        } else {
            "sleep 10"
        };
        let out = Shell::default()
            .execute(serde_json::json!({"command": cmd, "timeout_ms": 500}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.to_lowercase().contains("timeout"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn captures_stderr() {
        // Spawns a child: held under ENV_LOCK (see its docs).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, c) = ctx();
        let cmd = if cfg!(windows) {
            "echo err 1>&2"
        } else {
            "echo err >&2"
        };
        let out = Shell::default()
            .execute(serde_json::json!({"command": cmd}), &c)
            .await
            .unwrap();
        assert!(out.content.contains("stderr"));
        assert!(out.content.contains("err"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn chatty_output_is_capped_not_buffered_unbounded() {
        // Spawns a child: held under ENV_LOCK (see its docs).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, c) = ctx();
        // Produce ~2MB of stdout, far past STREAM_CAPTURE_CAP: the tool
        // must still return promptly with the usual truncation marker
        // (the cap only bounds buffering, not the visible contract).
        let cmd: String = if cfg!(windows) {
            // Few iterations of long lines: totals ~1.2MB (past
            // STREAM_CAPTURE_CAP) while staying fast (cmd for-loops are
            // slow per iteration, and the line stays under cmd's 8k limit).
            let line = "A".repeat(6000);
            format!("for /l %i in (1,1,200) do @echo {line}")
        } else {
            "yes 0123456789012345678901234567890123456789 | head -c 2000000".to_string()
        };
        let out = Shell::default()
            .execute(serde_json::json!({"command": cmd}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("[truncated]"));
        // Well under the captured stream: the cap stopped buffering.
        assert!(out.content.len() < 4 * MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn missing_command_is_error_output() {
        let (_d, c) = ctx();
        let out = Shell::default().execute(serde_json::json!({}), &c).await.unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn runs_in_cwd() {
        // Spawns a child: held under ENV_LOCK (see its docs).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("marker.txt"), "x").unwrap();
        let cmd = if cfg!(windows) {
            "dir /b marker.txt"
        } else {
            "ls marker.txt"
        };
        let out = Shell::default()
            .execute(serde_json::json!({"command": cmd}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("marker.txt"));
    }

    #[test]
    fn truncate_cuts_at_char_boundary_without_mojibake() {
        // Multi-byte chars ('€', 3 bytes each) straddling the 30 KB boundary: the 1-byte prefix pushes the boundary inside a char.
        let text = format!("a{}", "€".repeat(10240)); // 1 + 30720 bytes
        let out = truncate_output(text.as_bytes());
        assert!(out.ends_with("[truncated]"));
        // Truncation falls back to a char boundary: '€' is never split, no replacement-char mojibake (U+FFFD).
        assert!(!out.contains('\u{FFFD}'));
        let body = out.strip_suffix("\n[truncated]").unwrap();
        assert!(body.len() <= MAX_OUTPUT_BYTES);
        // String itself guarantees valid UTF-8; the boundary really falls back (30720 is not a boundary -> 30718).
        assert_eq!(body.len(), 1 + 3 * 10239);
    }

    #[test]
    fn truncate_handles_invalid_utf8_without_panic() {
        // All-0xFF invalid bytes: from_utf8_lossy replaces each byte with U+FFFD, must not panic.
        let bytes = vec![0xFF; MAX_OUTPUT_BYTES + 100];
        let out = truncate_output(&bytes);
        assert!(out.ends_with("[truncated]"));
        assert!(out.len() <= MAX_OUTPUT_BYTES + "\n[truncated]".len());
    }

    /// On timeout the output produced before the kill rides along with the
    /// timeout reason — the log head is what diagnosing the timeout needs.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn timeout_reports_partial_output() {
        // Spawns a child: held under ENV_LOCK (see its docs).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, c) = ctx();
        let cmd = if cfg!(windows) {
            // cmd builtin busy-wait hanger, no outer parens (a grouped
            // compound mangles the for syntax) and no grandchildren, so
            // nothing is orphaned after the kill.
            "echo started& for /l %i in (1,1,1000000000) do @rem"
        } else {
            // `exec` folds sleep into the shell itself: the kill closes
            // every pipe write end at once (the orphaned-grandchild shape
            // is covered by the test below).
            "echo started; exec sleep 30"
        };
        let out = Shell::default()
            .execute(
                serde_json::json!({"command": cmd, "timeout_ms": 800}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("timeout after 800ms"), "{}", out.content);
        assert!(
            out.content.contains("started"),
            "pre-kill output survives the kill: {}",
            out.content
        );
    }

    /// A grandchild that inherits the pipes and outlives the killed shell
    /// must not extend the run past the post-kill drain grace: the partial
    /// capture comes back instead of waiting on the orphan. Unix-only: the
    /// orphan shape needs real grandchild pipe inheritance, which cmd's
    /// `start` does not model deterministically.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn timeout_does_not_wait_for_orphaned_grandchildren() {
        if cfg!(windows) {
            return;
        }
        // Spawns a child: held under ENV_LOCK (see its docs).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, c) = ctx();
        // sleep is a grandchild holding the pipe write ends; killing the
        // shell cannot close them until it exits (30s) — the grace must
        // bound the run first.
        let started = std::time::Instant::now();
        let out = Shell::default()
            .execute(
                serde_json::json!({"command": "echo started; sleep 30", "timeout_ms": 300}),
                &c,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("started"),
            "partial capture survives the orphaned grandchild: {}",
            out.content
        );
        // Bounded by timeout + drain grace, not by the 30s orphan.
        assert!(
            elapsed < Duration::from_secs(15),
            "the run waited on the orphaned grandchild: {elapsed:?}"
        );
    }

    /// A truncated stream's full text lands in the spill store and the
    /// result names the URI; without a store the marker stands alone.
    #[test]
    fn truncated_output_spills_the_full_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = wavecode_context::SpillStore::new(dir.path().to_path_buf());
        let mut bytes = String::new();
        for i in 0..4000 {
            bytes.push_str(&format!("line-{i:04} tail tail tail tail tail tail\n"));
        }
        assert!(bytes.len() > MAX_OUTPUT_BYTES);

        let out = truncate_output_spilled(bytes.as_bytes(), Some(&store));
        assert!(out.contains("[truncated]"));
        let uri_pos = out.find("full output saved to spill://").expect("pointer present");
        let uri: String = out[uri_pos..]
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(4)
            .unwrap()
            .to_string();
        let full = store.read(&uri).unwrap();
        assert!(full.contains("line-3999"), "spill holds the whole stream");
        assert!(!out.contains("line-3999"), "display stays capped");

        // No store: behavior identical to plain truncation.
        assert_eq!(
            truncate_output_spilled(bytes.as_bytes(), None),
            truncate_output(bytes.as_bytes())
        );
    }

    #[test]
    fn sensitive_names_cover_secret_shapes_without_over_stripping() {
        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "API_KEY",
            "MY_API_KEY",
            "GITHUB_PAT",
            "DB_PRIVATE_KEY",
            "NPM_TOKEN",
            "SECRET_MANAGER_TOKEN",
            "DB_PASSWORD",
            "SECRET",
        ] {
            assert!(is_sensitive_env_name(name), "{name}");
        }
        for name in [
            "PATH",
            "HOME",
            "FOO_NORMAL",
            "SSH_AUTH_SOCK",
            "KUBECONFIG",
            "NODE_ENV",
            "NUMBER_OF_PROCESSORS",
            "GITHUB_ACTIONS",
            "RUSTUP_HOME",
        ] {
            assert!(!is_sensitive_env_name(name), "{name}");
        }
    }

    // Environment variables are process-global state, and the OS-sandbox
    // watcher pairs this process' new direct children with pending jobs —
    // tests that touch env or spawn shells must therefore run mutually
    // exclusive, or the watcher can pair another test's child with the
    // sandbox test's job and leave the confined child suspended forever.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    // Touches env and spawns a child: held under ENV_LOCK (see its docs).
    #[allow(clippy::await_holding_lock)]
    async fn shell_strips_sensitive_env_but_keeps_normal() {
        // Poison recovery: one panicking env test must not cascade into
        // unrelated tests failing on PoisonError.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Three variable classes: a suffix-pattern hit, a deny_env-list hit, and an unaffected normal variable.
        unsafe {
            std::env::set_var("FOO_API_KEY", "secret123");
            std::env::set_var("FOO_PROVIDER_KEY", "secret456");
            std::env::set_var("FOO_NORMAL", "visible789");
        }
        let (_d, mut c) = ctx();
        c.deny_env = vec!["FOO_PROVIDER_KEY".to_owned()];
        // One command prints all three: stripped variables expand to empty (sh) or stay literal (cmd).
        let cmd = if cfg!(windows) {
            "echo %FOO_API_KEY%& echo %FOO_PROVIDER_KEY%& echo %FOO_NORMAL%"
        } else {
            "echo \"$FOO_API_KEY\"; echo \"$FOO_PROVIDER_KEY\"; echo \"$FOO_NORMAL\""
        };
        let out = Shell::default()
            .execute(serde_json::json!({"command": cmd}), &c)
            .await
            .unwrap();
        unsafe {
            std::env::remove_var("FOO_API_KEY");
            std::env::remove_var("FOO_PROVIDER_KEY");
            std::env::remove_var("FOO_NORMAL");
        }
        // Secrets do not leak: both suffix-pattern and deny_env entries are stripped.
        assert!(!out.content.contains("secret123"));
        assert!(!out.content.contains("secret456"));
        // No over-stripping: normal variables remain visible to the child.
        assert!(out.content.contains("visible789"));
    }

    #[test]
    fn os_sandbox_flag_defaults_off_and_parses() {
        // Poison recovery, same reason as the scrubbing test above.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("WAVECODE_SANDBOX_OS").ok();
        unsafe {
            std::env::remove_var("WAVECODE_SANDBOX_OS");
        }
        assert!(!os_sandbox_enabled(), "confinement defaults off");
        for on in ["1", "true", "TRUE"] {
            unsafe {
                std::env::set_var("WAVECODE_SANDBOX_OS", on);
            }
            assert!(os_sandbox_enabled(), "{on} enables confinement");
        }
        for off in ["0", "false", "yes", ""] {
            unsafe {
                std::env::set_var("WAVECODE_SANDBOX_OS", off);
            }
            assert!(!os_sandbox_enabled(), "{off:?} must not enable confinement");
        }
        unsafe {
            std::env::remove_var("WAVECODE_SANDBOX_OS");
            if let Some(v) = prior {
                std::env::set_var("WAVECODE_SANDBOX_OS", v);
            }
        }
    }

    #[tokio::test]
    // Arms a sandbox job and spawns a child: held under ENV_LOCK (see its docs).
    #[allow(clippy::await_holding_lock)]
    async fn os_sandbox_enabled_runs_confined_or_fails_closed() {
        // Poison recovery, same reason as the scrubbing test above.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("WAVECODE_SANDBOX_OS").ok();
        unsafe {
            std::env::set_var("WAVECODE_SANDBOX_OS", "1");
        }
        let (_d, c) = ctx();
        let out = Shell::default()
            .execute(serde_json::json!({"command": "echo hello"}), &c)
            .await
            .unwrap();
        unsafe {
            std::env::remove_var("WAVECODE_SANDBOX_OS");
            if let Some(v) = prior {
                std::env::set_var("WAVECODE_SANDBOX_OS", v);
            }
        }
        if wavecode_sandbox::detect_backend().is_available() {
            // Confinement armed and the shell still runs inside it.
            assert!(!out.is_error, "confined echo must succeed: {}", out.content);
            assert!(out.content.contains("hello"));
        } else {
            // No backend: fail closed, never silently unconfined.
            assert!(out.is_error);
            assert!(
                out.content.contains("OS sandbox confinement failed"),
                "hard error names confinement: {}",
                out.content
            );
        }
    }
}
