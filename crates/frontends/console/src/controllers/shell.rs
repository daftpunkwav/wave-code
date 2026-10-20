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
    /// the pump task. `cancel` tree-kills this pid: `taskkill /T` on
    /// Windows, and the process group on Unix (the child is spawned as
    /// the group leader).
    pid: Option<u32>,
    /// Set once the pump reported `Done` (or the channel closed), so
    /// `try_recv` stops reporting instead of feeding an endless stream
    /// of terminal events to the caller.
    ended: bool,
}

impl ShellJob {
    /// Spawn `command` under the shared platform shell resolution
    /// (`cmd /C` on Windows, `sh -c` elsewhere, overridable through
    /// `WAVECODE_SHELL`) with piped output and no stdin.
    ///
    /// The resolution is the shared one, not a local copy: the shell
    /// tool, the lifecycle hooks, the background jobs, and this console
    /// spawn command strings, so the operator's `WAVECODE_SHELL` override
    /// must reach all of them or none.
    pub fn spawn(command: &str) -> std::io::Result<Self> {
        let (program, flag) = infrastructure_base::shell_invocation();
        let mut cmd = tokio::process::Command::new(program);
        cmd.arg(flag)
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group so cancel can signal this shell and the
        // command it spawned without hitting this process.
        infrastructure_base::lead_process_group(cmd.as_std_mut());
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
        // `start_kill` terminates only the shell itself. Descendants
        // inherit the output pipes and keep them open, so the tree goes
        // through the shared helper (group signal on Unix, `taskkill /T`
        // on Windows). Windows waits for `taskkill`; that wait stays off
        // the UI thread. The pump still hard-kills the shell afterwards.
        let Some(pid) = self.pid else {
            return;
        };
        #[cfg(windows)]
        std::thread::spawn(move || infrastructure_base::kill_tree(pid));
        #[cfg(not(windows))]
        infrastructure_base::kill_tree(pid);
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

    /// Spawn with `WAVECODE_SHELL` cleared, so the suite does not depend
    /// on the developer's shell override. The resolved program and flag
    /// are baked into the spawned command, so the variable is restored
    /// as soon as the spawn returns.
    fn spawn_with_default_shell(command: &str) -> std::io::Result<ShellJob> {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("WAVECODE_SHELL").ok();
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::remove_var("WAVECODE_SHELL");
        }
        let job = ShellJob::spawn(command);
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("WAVECODE_SHELL", v);
            }
        }
        job
    }

    /// The `!` shell resolves its program through the shared
    /// infrastructure helper, so the `WAVECODE_SHELL` override reaches
    /// it the way it reaches the shell tool, the lifecycle hooks, and
    /// the background jobs.
    ///
    /// The override is set on a *child* copy of this test binary: the
    /// variable is process-global, so setting it in-process would make
    /// every parallel test that spawns a command fail for the wrong
    /// reason.
    #[test]
    fn the_shared_shell_override_reaches_the_repl_shell() {
        let output = crate::run_isolated_with_shell_override(
            "controllers::shell::tests::repl_shell_runs_the_override_program",
        );
        assert!(
            output.status.success(),
            "the override child must pass: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// The child half of
    /// [`the_shared_shell_override_reaches_the_repl_shell`]: it runs with
    /// `WAVECODE_SHELL` pointing at a program that does not exist, so
    /// the spawn must fail — the console ran the program the shared
    /// resolution handed it.
    #[test]
    #[ignore = "child of the_shared_shell_override_reaches_the_repl_shell"]
    fn repl_shell_runs_the_override_program() {
        assert!(
            ShellJob::spawn("echo hello").is_err(),
            "the shared shell override must decide which program the console spawns"
        );
    }

    #[tokio::test]
    async fn spawns_and_collects_output() {
        let mut job = spawn_with_default_shell("echo shell-job-ok").expect("spawn");
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
        let mut job = spawn_with_default_shell("exit 3").expect("spawn");
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
        let mut job = spawn_with_default_shell(sleep).expect("spawn");
        // No settle sleep is needed: `spawn` already awaited the child
        // process, so cancel deterministically lands on a live job.
        job.cancel();
        let (_out, _err, code) = run_to_done(&mut job).await;
        assert_eq!(code, Some(None), "cancelled job reports no exit code");
    }
}
