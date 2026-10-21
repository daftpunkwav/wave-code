/*!
 * @file ShellJob
 * @description Local shell-command execution for `!` REPL commands.
 *
 * Responsibilities:
 * - Spawn a command under the shared platform shell resolution with
 *   piped output and pump its lines through a bounded channel the UI
 *   drains non-blockingly.
 * - Own the cancel path: kill the shell's whole tree, then report the
 *   job's terminal `Done(None)` event.
 *
 * The cancel path must always report a terminal event: a descendant
 * that outlives the tree kill keeps the output pipe open, and the job
 * must not wait for that pipe forever.
 *
 * This module must not depend on: runtime, capability, or bootstrap
 * crates; the tree kill and the shell resolution ride
 * infrastructure-base.
 */

//! A [`ShellJob`] spawns the command under the shared platform shell
//! resolution with piped output and pumps lines through a channel the
//! UI drains non-blockingly; cancellation kills the child and is
//! reported as a `Done(None)` event after the pipes drain.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

/// Grace period for a cancelled job's pipes to drain after the tree
/// kill. The tree kill closes the shell's pipe ends, so EOF normally
/// lands within milliseconds; a descendant that outlived the tree kill
/// keeps them open, and the job must still report a terminal event.
const CANCEL_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// A spawned shell command.
pub struct ShellJob {
    rx: mpsc::Receiver<ShellEvent>,
    kill: Arc<Notify>,
    /// Signals that the cancel path's tree kill has landed. The pump
    /// waits for it before killing the shell itself: `taskkill /T`
    /// walks the tree from a live parent, so a shell that died first
    /// leaves its descendants behind.
    tree_killed: Arc<Notify>,
    /// Platform pid of the shell child, captured before it moves into
    /// the pump task. `cancel` tree-kills this pid: `taskkill /T` on
    /// Windows, and the process group on Unix (the child is spawned as
    /// the group leader).
    pid: Option<u32>,
    /// Set once a cancel has been requested.
    ///
    /// Esc and Ctrl+C both reach the same job, so `cancel` can be
    /// called twice for one shell. The second tree kill would target a
    /// pid the first one may already have released, and a reused pid
    /// belongs to somebody else by then.
    cancelled: AtomicBool,
    /// The tree kills this job started (test seam): a second cancel
    /// must not start a second one.
    #[cfg(test)]
    tree_kills: AtomicUsize,
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
        let mut cmd = infrastructure_base::shell_command(command);
        cmd.stdin(std::process::Stdio::null())
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
        let tree_killed = Arc::new(Notify::new());
        let tree_killer = Arc::clone(&tree_killed);
        let child_pid = child.id();
        tokio::spawn(async move {
            let mut out = BufReader::new(stdout).lines();
            let mut err = BufReader::new(stderr).lines();
            let mut killed = false;
            let mut out_open = true;
            let mut err_open = true;
            // Deadline for the cancelled job's pipes to drain; the
            // timer only runs once the cancel landed.
            let mut drain_deadline = tokio::time::Instant::now();
            let mut draining = false;
            while out_open || err_open {
                tokio::select! {
                    biased;
                    _ = killer.notified(), if !killed => {
                        killed = true;
                        // The shell's descendants inherit the output
                        // pipes, so killing only the shell orphans them
                        // and the pipes never reach EOF. `cancel` runs
                        // the tree kill; wait for it to land before
                        // killing the shell itself, because `taskkill /T`
                        // walks the tree from a live parent and a shell
                        // that died first leaves its descendants behind.
                        tree_killer.notified().await;
                        let _ = child.start_kill();
                        drain_deadline = tokio::time::Instant::now() + CANCEL_DRAIN_GRACE;
                        draining = true;
                    }
                    _ = tokio::time::sleep_until(drain_deadline), if draining => {
                        // A descendant outlived the tree kill and still
                        // holds the pipe open: stop waiting for EOF so
                        // the job still reports a terminal event.
                        out_open = false;
                        err_open = false;
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
            tree_killed,
            pid: child_pid,
            cancelled: AtomicBool::new(false),
            #[cfg(test)]
            tree_kills: AtomicUsize::new(0),
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
    ///
    /// Only the first call acts: Esc and Ctrl+C both reach this job, and
    /// a second tree kill would target a pid the first one may already
    /// have released.
    pub fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wake the pump task so it reports the terminal event and
        // orders its own kill after the tree kill below.
        self.kill.notify_one();
        // The tree kill covers the shell and every descendant it
        // spawned: descendants inherit the output pipes, so killing
        // only the shell orphans them and the pipes never reach EOF.
        // It runs on a dedicated thread because `taskkill /T` waits for
        // the whole tree to die, and the UI thread may not block on
        // that wait. The signal tells the pump the tree kill landed.
        let pid = self.pid;
        let landed = Arc::clone(&self.tree_killed);
        #[cfg(test)]
        self.tree_kills.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            if let Some(pid) = pid {
                infrastructure_base::kill_tree(pid);
            }
            landed.notify_one();
        });
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

    /// A cancelled job must report a terminal event even when a
    /// descendant outlives the tree kill and keeps the output pipe
    /// open.
    ///
    /// `start /B` returns at once, so the shell is gone while the
    /// descendant it started keeps writing to the inherited pipe. The
    /// tree kill walks the tree from a live parent, so it cannot reach
    /// that descendant and the pipes never reach EOF; the job must
    /// still end.
    #[cfg(windows)]
    #[tokio::test]
    async fn cancel_reports_done_when_a_descendant_holds_the_pipe() {
        let mut job = spawn_with_default_shell("start /B ping -n 30 127.0.0.1").expect("spawn");
        // The descendant's output proves it holds the pipe the shell
        // left behind.
        let mut descendant_alive = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !descendant_alive && tokio::time::Instant::now() < deadline {
            if let Some(ShellEvent::Out(_)) = job.try_recv() {
                descendant_alive = true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(descendant_alive, "the descendant must hold the output pipe");
        job.cancel();
        let (_out, _err, code) = run_to_done(&mut job).await;
        assert_eq!(code, Some(None), "cancelled job reports no exit code");
    }

    /// Esc and Ctrl+C both reach the same job, so `cancel` can be
    /// called twice for one shell. The second tree kill would target a
    /// pid the first one may already have released, and a reused pid
    /// belongs to somebody else by then.
    #[tokio::test]
    async fn a_second_cancel_does_not_start_a_second_tree_kill() {
        // The child must still be alive when the cancels land: a
        // short-lived command would leave the second tree kill without
        // a live pid to hit.
        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1"
        } else {
            "sleep 30"
        };
        let mut job = spawn_with_default_shell(sleep).expect("spawn");
        job.cancel();
        job.cancel();
        assert_eq!(
            job.tree_kills.load(Ordering::SeqCst),
            1,
            "the second cancel started another tree kill"
        );
        let (_out, _err, code) = run_to_done(&mut job).await;
        assert_eq!(code, Some(None), "cancelled job reports no exit code");
    }
}
