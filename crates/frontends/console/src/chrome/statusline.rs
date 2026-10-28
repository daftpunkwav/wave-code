//! External status-line command protocol.
//!
//! When `status_line_command` is set in the console settings, the
//! command receives one JSON snapshot of the session state on stdin
//! about once per second; its first stdout line replaces footer row 1.
//! A slow or silent command is killed after 300ms and the footer keeps
//! showing the last good line. Command output is sanitized before
//! rendering: terminal control sequences in the line are stripped, so
//! external color codes do not reach the diff renderer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tui_engine::sanitize::sanitize_terminal;

/// Cadence: the command runs at most once per second.
const RUN_EVERY: Duration = Duration::from_secs(1);
/// A run that has not printed its first line within this budget dies.
const KILL_AFTER: Duration = Duration::from_millis(300);

/// Shared slot the spawned task writes its finished line into.
type Slot = Arc<Mutex<Option<String>>>;

/// Owns the external status-line command lifecycle.
pub struct StatusLine {
    command: Option<String>,
    last_spawn: Option<Instant>,
    latest: Slot,
}

impl StatusLine {
    /// A runner for `command` (empty = disabled); pick the setting up
    /// again later with [`StatusLine::set_command`].
    pub fn new(command: Option<String>) -> Self {
        Self {
            command: command.filter(|c| !c.trim().is_empty()),
            last_spawn: None,
            latest: Arc::new(Mutex::new(None)),
        }
    }

    /// Replace the command (from a settings reload); `None` or blank
    /// disables the external line and clears the last output.
    pub fn set_command(&mut self, command: Option<String>) {
        self.command = command.filter(|c| !c.trim().is_empty());
        if self.command.is_none() {
            *self.slot() = None;
        }
        self.last_spawn = None;
    }

    /// The configured command, when enabled.
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// The newest line the command produced, if any.
    pub fn current(&self) -> Option<String> {
        self.slot().clone()
    }

    /// Spawn the command when due; `snapshot` is the JSON state it
    /// reads on stdin.
    pub fn maybe_spawn(&mut self, snapshot: serde_json::Value) {
        let Some(command) = self.command.clone() else {
            return;
        };
        let now = Instant::now();
        if self
            .last_spawn
            .is_some_and(|t| now.duration_since(t) < RUN_EVERY)
        {
            return;
        }
        self.last_spawn = Some(now);
        let latest = Arc::clone(&self.latest);
        tokio::spawn(async move {
            if let Ok(line) = run_once(&command, snapshot).await {
                *lock(&latest) = Some(line);
            }
        });
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        lock(&self.latest)
    }
}

/// Lock a slot, recovering from poison: the slot is a plain `Option`
/// with no invariant a panic could have broken, so a poisoned guard
/// (a panic in another thread mid-write) degrades to the inner value
/// instead of taking down the render loop.
fn lock(slot: &Mutex<Option<String>>) -> std::sync::MutexGuard<'_, Option<String>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run the command once: JSON snapshot in, first stdout line out.
///
/// The command runs through the shared platform shell resolution: the
/// status line is one more command-string spawn, so the operator's
/// `WAVECODE_SHELL` override must reach it the way it reaches the
/// shell tool, the lifecycle hooks, the background jobs, and the
/// console's `!` shell.
async fn run_once(command: &str, snapshot: serde_json::Value) -> anyhow::Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut child = infrastructure_base::shell_command(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(snapshot.to_string().as_bytes()).await.ok();
        stdin.shutdown().await.ok();
        drop(stdin);
    }
    let out = tokio::time::timeout(KILL_AFTER, child.wait_with_output()).await??;
    // The console code-page fallback, not a bare lossy UTF-8 decode:
    // children inherit the console code page, so a non-ASCII path from
    // a GBK-host builtin would otherwise render as diamond noise.
    let text = infrastructure_base::console_codec::decode_console_output(&out.stdout);
    let line = sanitize_terminal(text.lines().next().unwrap_or_default().trim());
    if line.is_empty() {
        anyhow::bail!("empty status line");
    }
    Ok(line.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> serde_json::Value {
        serde_json::json!({
            "model": "test-model",
            "cwd": "/tmp/proj",
        })
    }

    /// Run the command with `WAVECODE_SHELL` cleared, so the suite does
    /// not depend on the developer's shell override. The resolved
    /// program and flag are baked into the spawned command, so the
    /// variable is restored as soon as the run returns.
    // Touches the process-global environment: held under the crate's
    // ENV_LOCK so no parallel test observes the mutation.
    #[allow(clippy::await_holding_lock)]
    async fn run_with_default_shell(
        command: &str,
        snapshot: serde_json::Value,
    ) -> anyhow::Result<String> {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("WAVECODE_SHELL").ok();
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::remove_var("WAVECODE_SHELL");
        }
        let outcome = run_once(command, snapshot).await;
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("WAVECODE_SHELL", v);
            }
        }
        outcome
    }

    /// The status line resolves its program through the shared
    /// infrastructure helper, so the `WAVECODE_SHELL` override reaches
    /// it the way it reaches the shell tool, the lifecycle hooks, the
    /// background jobs, and the console's `!` shell.
    ///
    /// The override is set on a *child* copy of this test binary: the
    /// variable is process-global, so setting it in-process would make
    /// every parallel test that spawns a command fail for the wrong
    /// reason.
    #[test]
    fn the_shared_shell_override_reaches_the_status_line() {
        let output = crate::run_isolated_with_shell_override(
            "chrome::statusline::tests::status_line_runs_the_override_program",
        );
        assert!(
            output.status.success(),
            "the override child must pass: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// The child half of
    /// [`the_shared_shell_override_reaches_the_status_line`]: it runs
    /// with `WAVECODE_SHELL` pointing at a program that does not exist,
    /// so the spawn fails instead of producing a status line — the
    /// console ran the program the shared resolution handed it.
    #[tokio::test]
    #[ignore = "child of the_shared_shell_override_reaches_the_status_line"]
    async fn status_line_runs_the_override_program() {
        assert!(
            run_once("echo hello-status", snapshot()).await.is_err(),
            "the shared shell override must decide which program the status line spawns"
        );
    }

    #[tokio::test]
    async fn first_stdout_line_becomes_the_status() {
        let line = run_with_default_shell("echo hello-status", snapshot())
            .await
            .unwrap();
        assert_eq!(line, "hello-status");
    }

    #[tokio::test]
    async fn control_sequences_are_stripped() {
        // The escape bytes ride a temp file: `echo` cannot emit ESC under
        // cmd, and `printf` is not a Windows command — only `type`/`cat`
        // print it identically on both platforms. The path must ride
        // unquoted: std's command-line escaping rewrites inner `"` into
        // `\"`, which `cmd /C` mangles into a syntax error. Windows skips
        // the test when the temp directory itself contains a space.
        let dir =
// nosemgrep: rust.lang.security.temp-dir.temp-dir
            std::env::temp_dir().join(format!("wavecode-statusline-esc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("red-line.txt");
        std::fs::write(&file, b"\x1b[31mred-line\x1b[0m\n").unwrap();
        #[cfg(windows)]
        if dir.to_string_lossy().contains(' ') {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        #[cfg(windows)]
        let cmd = format!("type {}", file.display());
        #[cfg(not(windows))]
        let cmd = format!("cat '{}'", file.display());
        let line = run_with_default_shell(&cmd, snapshot()).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(line, "red-line");
    }

    #[tokio::test]
    async fn empty_output_is_an_error() {
        #[cfg(windows)]
        let cmd = "exit 0";
        #[cfg(not(windows))]
        let cmd = "true";
        assert!(run_with_default_shell(cmd, snapshot()).await.is_err());
    }

    #[tokio::test]
    async fn silent_command_is_killed_after_the_budget() {
        let start = std::time::Instant::now();
        #[cfg(windows)]
        let cmd = "ping -n 30 127.0.0.1 > nul";
        #[cfg(not(windows))]
        let cmd = "sleep 30";
        assert!(run_with_default_shell(cmd, snapshot()).await.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "killed well before the command would exit"
        );
    }

    #[test]
    fn blank_command_is_disabled() {
        let mut line = StatusLine::new(Some("  ".to_string()));
        assert!(line.command().is_none());
        line.maybe_spawn(snapshot());
        assert!(line.current().is_none());
        line.set_command(None);
        assert!(line.command().is_none());
        line.set_command(Some("echo on".to_string()));
        assert_eq!(line.command(), Some("echo on"));
    }
}
