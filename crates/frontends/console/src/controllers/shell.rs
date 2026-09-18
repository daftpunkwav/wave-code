//! Local shell-command execution for `!` REPL commands.
//!
//! A [`ShellJob`] spawns the command under the platform shell with
//! piped output and pumps lines through a channel the UI drains
//! non-blockingly; cancellation kills the child and is reported as a
//! `Done(None)` event after the pipes drain.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Notify, mpsc};

/// One event from a running shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    /// One stdout line.
    Out(String),
    /// One stderr line.
    Err(String),
    /// The process exited: `Some(code)` on a normal exit, `None` when
    /// killed or reaped without an exit code.
    Done(Option<i32>),
}

/// Output channel capacity; a full channel applies backpressure to the
/// child instead of buffering an unbounded firehose in memory.
const CHANNEL_CAP: usize = 1024;

/// A spawned shell command.
pub struct ShellJob {
    rx: mpsc::Receiver<ShellEvent>,
    kill: Arc<Notify>,
    /// Platform pid of the shell child, captured before it moves into
    /// the pump task (used for tree-kill on Windows).
    pid: Option<u32>,
    /// Set once the pump reported `Done` (or the channel closed), so
    /// `try_recv` stops reporting instead of feeding an endless stream
    /// of terminal events to the caller.
    ended: bool,
}

impl ShellJob {
    /// Spawn `command` under the platform shell (`cmd /C` on Windows,
    /// `sh -c` elsewhere) with piped output and no stdin.
    pub fn spawn(command: &str) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(platform_shell());
        if cfg!(windows) {
            cmd.arg("/C");
        } else {
            cmd.arg("-c");
        }
        cmd.arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let Some(stdout) = child.stdout.take() else {
            return Err(std::io::Error::other("child stdout unavailable"));
        };
        let Some(stderr) = child.stderr.take() else {
            return Err(std::io::Error::other("child stderr unavailable"));
        };
        let (tx, rx) = mpsc::channel(CHANNEL_CAP);
        let kill = Arc::new(Notify::new());
        let killer = Arc::clone(&kill);
        let pid = child.id();
        tokio::spawn(async move {
            let mut out = BufReader::new(stdout).lines();
            let mut err = BufReader::new(stderr).lines();
            let mut killed = false;
            let mut out_open = true;
            let mut err_open = true;
            while out_open || err_open {
                tokio::select! {
                    biased;
                    _ = killer.notified(), if !killed => {
                        killed = true;
                        #[cfg(not(windows))]
                        {
                            let _ = child.start_kill();
                        }
                        #[cfg(windows)]
                        // `cancel` tree-kills the command (taskkill /T);
                        // give it a moment to land, then hard-kill
                        // cmd.exe itself as a fallback. Killing cmd
                        // immediately would orphan its children, which
                        // inherit the output pipes and keep them open.
                        {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let _ = child.start_kill();
                        }
                    }
                    line = out.next_line(), if out_open => match line {
                        Ok(Some(line)) => {
                            if tx.send(ShellEvent::Out(line)).await.is_err() {
                                return;
                            }
                        }
                        _ => out_open = false,
                    },
                    line = err.next_line(), if err_open => match line {
                        Ok(Some(line)) => {
                            if tx.send(ShellEvent::Err(line)).await.is_err() {
                                return;
                            }
                        }
                        _ => err_open = false,
                    },
                }
            }
            let status = child.wait().await;
            let code = if killed {
                None
            } else {
                status.ok().and_then(|status| status.code())
            };
            let _ = tx.send(ShellEvent::Done(code)).await;
        });
        Ok(Self {
            rx,
            kill,
            pid,
            ended: false,
        })
    }

    /// Poll one pending event; `None` when nothing is ready (or the
    /// job already ended). A closed channel — the pump task gone
    /// without a `Done` — reports `Done(None)` exactly once.
    pub fn try_recv(&mut self) -> Option<ShellEvent> {
        if self.ended {
            return None;
        }
        match self.rx.try_recv() {
            Ok(event) => {
                if matches!(event, ShellEvent::Done(_)) {
                    self.ended = true;
                }
                Some(event)
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                self.ended = true;
                Some(ShellEvent::Done(None))
            }
            Err(mpsc::error::TryRecvError::Empty) => None,
        }
    }

    /// Kill the child; the remaining piped output still drains and the
    /// job ends with a `Done(None)` event.
    pub fn cancel(&self) {
        // Wake the pump task so the job always reports `Done(None)`,
        // however the child ends up dying.
        self.kill.notify_one();
        // `start_kill` on Windows terminates only `cmd.exe` itself; the
        // command tree it spawned survives and keeps the output pipes
        // open. Kill the whole tree instead (best effort).
        #[cfg(windows)]
        if let Some(pid) = self.pid {
            // Fire-and-forget: taskkill exits on its own once the tree
            // is terminated (no kill_on_drop — dropping the handle here
            // must not kill it mid-run).
            let _ = tokio::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .spawn();
        }
    }
}

/// The platform shell program: `%COMSPEC%` (default `cmd`) on Windows,
/// `sh` elsewhere.
pub(crate) fn platform_shell() -> String {
    if cfg!(windows) {
        std::env::var_os("COMSPEC")
            .map(|shell| shell.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cmd".to_string())
    } else {
        "sh".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Poll until `Done`, draining output, with a hard timeout. The
    /// result wraps the exit code: `Some(None)` means killed.
    async fn run_to_done(job: &mut ShellJob) -> (Vec<String>, Vec<String>, Option<Option<i32>>) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut code: Option<Option<i32>> = None;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while code.is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "shell job did not finish in time"
            );
            match job.try_recv() {
                Some(ShellEvent::Out(line)) => out.push(line),
                Some(ShellEvent::Err(line)) => err.push(line),
                Some(ShellEvent::Done(done)) => code = Some(done),
                None => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        (out, err, code)
    }

    #[tokio::test]
    async fn spawns_and_collects_output() {
        let mut job = ShellJob::spawn("echo shell-job-ok").expect("spawn");
        let (out, _err, code) = run_to_done(&mut job).await;
        assert_eq!(code, Some(Some(0)));
        assert!(
            out.iter().any(|line| line.contains("shell-job-ok")),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        // `cmd /C exit 3` and `sh -c 'exit 3'` both exit with 3.
        let mut job = ShellJob::spawn("exit 3").expect("spawn");
        let (_out, _err, code) = run_to_done(&mut job).await;
        assert_eq!(code, Some(Some(3)));
    }

    #[tokio::test]
    async fn cancel_terminates_with_done_none() {
        // A command that would run far past the test budget.
        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1"
        } else {
            "sleep 30"
        };
        let mut job = ShellJob::spawn(sleep).expect("spawn");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        job.cancel();
        let (_out, _err, code) = run_to_done(&mut job).await;
        assert_eq!(code, Some(None), "cancelled job reports no exit code");
    }
}
