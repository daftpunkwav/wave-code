/*!
 * @file JobService
 * @description Background shell jobs with wait/cancel/notice semantics.
 *
 * Responsibilities:
 * - Spawn shell commands as tracked background jobs with a per-owner cap.
 * - Wait with a timeout that snapshots without killing the job.
 * - Cancel by killing the whole process group, never just the shell.
 * - File completion notices on the shared child-runtime channel.
 * - Host the model-invokable `job_*` tools over the service (`tools` module).
 *
 * The service itself must not depend on: policy, drivers, actors,
 * sessions, or any execution layer above the child runtime notification
 * queue; the `tools` module adds the single capability edge
 * (wavecode-tools) that renders it model-invokable.
 */

//! Background jobs: long shell work that stops blocking the turn.
//!
//! A job is a shell command under `tokio::process` with piped output
//! captured into a bounded log. Spawning admits or rejects against a
//! per-owner cap; `wait` snapshots without killing; `cancel` kills the
//! whole process tree; every terminal path files one notice on the
//! [`ChildRuntime`] queue the parent loop already drains for child tasks.

pub mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use infrastructure_base::shell_invocation;
use runtime_child::ChildRuntime;
use tokio::io::AsyncReadExt;

/// Concurrent running jobs admitted per owner; the next spawn rejects.
pub const MAX_JOBS_PER_OWNER: usize = 10;
/// Cap on the retained job table; finished entries prune past this size.
const MAX_TRACKED_JOBS: usize = 64;
/// Total captured log bytes kept per job (head dropped past the cap).
const MAX_LOG_BYTES: usize = 64 * 1024;
/// Log bytes surfaced in one snapshot.
const LOG_TAIL_BYTES: usize = 8 * 1024;
/// Upper bound for one `wait` call; waiting longer means polling again.
const MAX_WAIT_MS: u64 = 300_000;

/// One background job request.
#[derive(Debug, Clone)]
pub struct JobRequest {
    /// Owner charged against the per-owner cap (e.g. the session id).
    pub owner: String,
    /// Shell command string, run via the platform shell.
    pub command: String,
    /// Working directory for the child process.
    pub cwd: PathBuf,
    /// Environment variable names stripped before spawn.
    pub deny_env: Vec<String>,
    /// Run deadline; `None` runs until it exits or is cancelled.
    pub timeout_ms: Option<u64>,
    /// Foreground run: the caller waits inline and delivers the result
    /// itself, so the completion notice stays off unless promotion
    /// (`mark_notified`) turns it on. `false` (the `job_spawn` case)
    /// notifies on completion as usual.
    pub foreground: bool,
}

/// Spawn rejection: the owner already holds the maximum running jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// Owner is at capacity; cancel or await a job, then retry.
    #[error("job capacity reached")]
    AtCapacity,
}

/// Lifecycle state of one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Process spawning or running; no terminal status yet.
    Running,
    /// Reached a terminal status; the notice is already filed.
    Finished,
}

/// Service-wide view of one job (the `/agents` display): the row the
/// snapshot alone cannot name — which command runs and who owns it.
#[derive(Debug, Clone)]
pub struct JobOverview {
    /// Job id (`job-N`).
    pub id: String,
    /// The shell command the job runs.
    pub command: String,
    /// Session id charged at spawn.
    pub owner: String,
    /// The lifecycle snapshot.
    pub snapshot: JobSnapshot,
}

/// Point-in-time view of one job for `wait` and `read`.
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    /// Job id (`job-N`).
    pub id: String,
    /// Current lifecycle state.
    pub state: JobState,
    /// Process exit code; `None` when signal-killed or never ran.
    pub exit_code: Option<i32>,
    /// True when `cancel` (or a pre-start stop) ended the job.
    pub cancelled: bool,
    /// True when the spawn `timeout_ms` deadline killed the job.
    pub timed_out: bool,
    /// Last log bytes, UTF-8 lossy.
    pub log_tail: String,
}

impl JobSnapshot {
    /// One-line status for tool output, e.g. `running` or `finished (exit 0)`.
    pub fn status_line(&self) -> String {
        match self.state {
            JobState::Running => format!("job {}: running", self.id),
            JobState::Finished if self.cancelled => {
                format!("job {}: finished (cancelled)", self.id)
            }
            JobState::Finished if self.timed_out => {
                format!("job {}: finished (timed out)", self.id)
            }
            JobState::Finished => match self.exit_code {
                Some(0) => format!("job {}: finished (exit 0)", self.id),
                Some(code) => format!("job {}: finished (exit {code})", self.id),
                None => format!("job {}: finished (signal-killed)", self.id),
            },
        }
    }

    /// True for terminal states the model should treat as failures.
    ///
    /// Cancellation is the model acting on purpose, so it is not a
    /// failure; timeouts and nonzero exits are.
    pub fn is_failure(&self) -> bool {
        self.state == JobState::Finished
            && !self.cancelled
            && (self.timed_out || self.exit_code != Some(0))
    }
}

/// Live tracking slot for one job.
struct JobSlot {
    /// Owner charged at spawn, for cap accounting.
    owner: String,
    /// The shell command string, for service-wide displays.
    command: String,
    /// Current lifecycle state.
    state: Mutex<JobState>,
    /// Terminal exit code once reaped.
    exit_code: Mutex<Option<i32>>,
    /// Stop requested via `cancel`, observed by the driver.
    cancelled: AtomicBool,
    /// Run deadline fired, observed in the snapshot.
    timed_out: AtomicBool,
    /// Started as a foreground run whose result is delivered inline, so
    /// `finish` keeps the completion notice off unless promotion flips
    /// this off (`mark_notified`) — otherwise every ordinary shell command
    /// would file a duplicate notice for output the model already has.
    foreground: AtomicBool,
    /// Captured stdout/stderr bytes, head-dropped past the cap.
    log: Mutex<Vec<u8>>,
    /// Leader pid for tree kills; `None` before a successful spawn.
    pid: Mutex<Option<u32>>,
    /// Wakeup for `wait` callers on terminal transitions.
    done: tokio::sync::Notify,
}

impl JobSlot {
    /// Recover guards after a poison; critical sections are single short
    /// writes that leave no half-written invariant behind.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, JobState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_log(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_running(&self) -> bool {
        matches!(*self.lock_state(), JobState::Running)
    }
}

/// Tracked background shell jobs with wait/cancel/notice.
///
/// Completion notices land on the injected [`ChildRuntime`], the same
/// instance the parent loop drains, so no second channel exists.
pub struct JobService {
    /// Job table: id -> tracking slot.
    jobs: Mutex<HashMap<String, Arc<JobSlot>>>,
    /// Monotonic id allocator; ids look like `job-N` starting at 1.
    next_id: AtomicUsize,
    /// Shared completion channel with the parent loop.
    notifications: Arc<ChildRuntime>,
}

impl JobService {
    /// Create a service filing notices on the given child runtime.
    pub fn new(notifications: Arc<ChildRuntime>) -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicUsize::new(0),
            notifications,
        }
    }

    /// Recover the guard after a poison; inserts and lookups are single
    /// short critical sections.
    fn lock_jobs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<JobSlot>>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admit one job or reject at capacity; returns `job-N` immediately.
    ///
    /// Must be called inside a tokio runtime: the driver task spawns here
    /// and the process starts within milliseconds on the same runtime.
    pub fn spawn(&self, request: JobRequest) -> Result<String, JobError> {
        let mut jobs = self.lock_jobs();
        let running = jobs
            .values()
            .filter(|slot| slot.owner == request.owner && slot.is_running())
            .count();
        if running >= MAX_JOBS_PER_OWNER {
            return Err(JobError::AtCapacity);
        }
        // Bound the table: finished entries are query history, not live
        // state, so prune them once the table grows past its cap.
        if jobs.len() >= MAX_TRACKED_JOBS {
            jobs.retain(|_, slot| slot.is_running());
        }
        let n = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("job-{n}");
        let slot = Arc::new(JobSlot {
            owner: request.owner.clone(),
            command: request.command.clone(),
            state: Mutex::new(JobState::Running),
            exit_code: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            foreground: AtomicBool::new(request.foreground),
            log: Mutex::new(Vec::new()),
            pid: Mutex::new(None),
            done: tokio::sync::Notify::new(),
        });
        jobs.insert(id.clone(), slot.clone());
        let notifications = self.notifications.clone();
        // Panic isolation, mirroring the child runtime: a panicking driver
        // must still record a terminal state, or the ghost Running slot
        // would hold one of the per-owner capacity slots forever.
        let handle = tokio::spawn(drive_job(
            id.clone(),
            request,
            slot.clone(),
            notifications.clone(),
        ));
        let watch_id = id.clone();
        tokio::spawn(async move {
            if let Err(join) = handle.await
                && join.is_panic()
            {
                finish(&watch_id, &slot, &notifications, None);
            }
        });
        Ok(id)
    }

    /// Point-in-time snapshot; `None` for unknown ids.
    pub fn read(&self, id: &str) -> Option<JobSnapshot> {
        self.lock_jobs()
            .get(id)
            .cloned()
            .map(|slot| snapshot(id, &slot))
    }

    /// Every tracked job, oldest first — the service-wide view behind
    /// the `/agents` display (ids allocate monotonically, so the sort
    /// key is the numeric suffix, not the string).
    pub fn overview(&self) -> Vec<JobOverview> {
        let jobs = self.lock_jobs();
        let mut rows: Vec<(usize, String, Arc<JobSlot>)> = jobs
            .iter()
            .map(|(id, slot)| {
                let index = id
                    .strip_prefix("job-")
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(usize::MAX);
                (index, id.clone(), slot.clone())
            })
            .collect();
        rows.sort_by_key(|(index, _, _)| *index);
        rows.into_iter()
            .map(|(_, id, slot)| JobOverview {
                id: id.clone(),
                command: slot.command.clone(),
                owner: slot.owner.clone(),
                snapshot: snapshot(&id, &slot),
            })
            .collect()
    }

    /// Block up to `timeout_ms`, then snapshot without killing.
    ///
    /// A timed-out wait leaves the job running: the caller polls again
    /// later. `None` for unknown ids only.
    pub async fn wait(&self, id: &str, timeout_ms: u64) -> Option<JobSnapshot> {
        let slot = self.lock_jobs().get(id).cloned()?;
        // Register interest before the state check so a finish landing
        // between the two still wakes this waiter.
        let notified = slot.done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !slot.is_running() {
            return Some(snapshot(id, &slot));
        }
        let clamped = Duration::from_millis(timeout_ms.min(MAX_WAIT_MS));
        let _ = tokio::time::timeout(clamped, notified).await;
        Some(snapshot(id, &slot))
    }

    /// Re-arm the completion notice for a foreground run: called once at
    /// promotion, when the waiting caller reports the run as still going
    /// and hands it to the model as background work. From here on the run
    /// notifies like any `job_spawn`-started job. `false` for unknown ids.
    ///
    /// If the run already reached a terminal state while still quiet (the
    /// caller's wait snapshot predated the exit), the suppressed notice is
    /// filed retroactively here, so a promoted run's completion can never
    /// be lost to that race.
    pub fn mark_notified(&self, id: &str) -> bool {
        let slot = match self.lock_jobs().get(id).cloned() {
            Some(slot) => slot,
            None => return false,
        };
        // Serialize with `finish` on the state lock so exactly one side
        // files the notice: arming first lets `finish` announce later;
        // finding the terminal state first announces here.
        let ended_quiet = {
            let state = slot.lock_state();
            slot.foreground.store(false, Ordering::SeqCst);
            *state == JobState::Finished
        };
        if ended_quiet {
            self.notifications
                .notify_completion(format!("{id} finished"));
        }
        true
    }

    /// Request cancellation; true when a tracked job accepted the signal.
    ///
    /// Kills the whole process tree (never just the shell); already
    /// finished jobs accept trivially. The driver reaps the exit, marks
    /// the job cancelled, and files the completion notice.
    pub fn cancel(&self, id: &str) -> bool {
        let slot = match self.lock_jobs().get(id).cloned() {
            Some(slot) => slot,
            None => return false,
        };
        slot.cancelled.store(true, Ordering::SeqCst);
        // Narrow the pid-reuse window: only signal a job that still
        // looks alive; an exited pid may already belong to someone else.
        if slot.is_running()
            && let Some(pid) = *slot.pid.lock().unwrap_or_else(|e| e.into_inner())
        {
            kill_tree(pid);
        }
        true
    }
}

/// Platform shell for one command string (the shared `shell_invocation`
/// resolution in infrastructure-base): `cmd /C` on Windows, `sh -c` on Unix,
/// `WAVECODE_SHELL` override.
fn shell_command(command: &str) -> tokio::process::Command {
    let (program, flag) = shell_invocation();
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg(flag).arg(command);
    cmd
}

/// Kill a job's whole process tree by leader pid: the shared
/// [`infrastructure_base::kill_tree`] (jobs spawn as their own process
/// group, the precondition that helper documents).
fn kill_tree(pid: u32) {
    infrastructure_base::kill_tree(pid)
}

/// Driver future: spawn, capture, reap, then file the shared notice.
async fn drive_job(
    id: String,
    request: JobRequest,
    slot: Arc<JobSlot>,
    notifications: Arc<ChildRuntime>,
) {
    // Pre-start stop: admit-time cancellation finishes without spawning.
    if slot.cancelled.load(Ordering::SeqCst) {
        finish(&id, &slot, &notifications, None);
        return;
    }
    let mut cmd = shell_command(&request.command);
    cmd.current_dir(&request.cwd)
        // Non-interactive: null stdin so the job can never steal input.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Pairs with cancel semantics: an aborted driver still reaps the shell.
        .kill_on_drop(true);
    for name in &request.deny_env {
        cmd.env_remove(name);
    }
    // Own group so `cancel` kills descendants with one killpg / taskkill /T.
    infrastructure_base::lead_process_group(cmd.as_std_mut());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            append_log(&slot, format!("failed to spawn shell: {e}\n").as_bytes());
            finish(&id, &slot, &notifications, None);
            return;
        }
    };
    *slot.pid.lock().unwrap_or_else(|e| e.into_inner()) = child.id();
    // Cancel raced the spawn: kill immediately instead of running.
    if slot.cancelled.load(Ordering::SeqCst) {
        if let Some(pid) = child.id() {
            kill_tree(pid);
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
        finish(&id, &slot, &notifications, None);
        return;
    }
    let mut drains = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        drains.push(tokio::spawn(drain_pipe(pipe, slot.clone())));
    }
    if let Some(pipe) = child.stderr.take() {
        drains.push(tokio::spawn(drain_pipe(pipe, slot.clone())));
    }
    let status: Option<std::process::ExitStatus> = match request.timeout_ms {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(e)) => {
                append_log(&slot, format!("failed waiting on shell: {e}\n").as_bytes());
                None
            }
            Err(_) => {
                // Run deadline: the only spawn-side kill in this crate;
                // wait-timeouts never reach here and never kill.
                if let Some(pid) = child.id() {
                    kill_tree(pid);
                }
                let _ = child.kill().await;
                slot.timed_out.store(true, Ordering::SeqCst);
                child.wait().await.ok()
            }
        },
        None => match child.wait().await {
            Ok(status) => Some(status),
            Err(e) => {
                append_log(&slot, format!("failed waiting on shell: {e}\n").as_bytes());
                None
            }
        },
    };
    // Join the drains before finishing so the snapshot log is complete.
    for drain in drains {
        let _ = drain.await;
    }
    finish(&id, &slot, &notifications, status.and_then(|s| s.code()));
}

/// Record the terminal status, wake waiters, and file the shared notice —
/// except for a still-foreground run, whose result the caller delivers
/// inline (promotion via `mark_notified` re-arms the notice; a quiet run
/// that ended before the arming landed is announced retroactively there).
fn finish(
    id: &str,
    slot: &Arc<JobSlot>,
    notifications: &Arc<ChildRuntime>,
    exit_code: Option<i32>,
) {
    *slot.exit_code.lock().unwrap_or_else(|e| e.into_inner()) = exit_code;
    // Read `foreground` under the state lock so the decision pairs
    // atomically with `mark_notified`'s arming: whichever takes the lock
    // first decides who files the notice, and exactly one is filed.
    let notify = {
        let mut state = slot.lock_state();
        *state = JobState::Finished;
        !slot.foreground.load(Ordering::SeqCst)
    };
    slot.done.notify_waiters();
    if notify {
        notifications.notify_completion(format!("{id} finished"));
    }
}

/// Drain one pipe into the bounded log until EOF or error.
async fn drain_pipe<T: tokio::io::AsyncRead + Unpin>(mut pipe: T, slot: Arc<JobSlot>) {
    let mut buf = [0u8; 4096];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => append_log(&slot, &buf[..n]),
        }
    }
}

/// Append bytes, dropping the head past the cap so the tail survives.
fn append_log(slot: &Arc<JobSlot>, bytes: &[u8]) {
    let mut log = slot.lock_log();
    log.extend_from_slice(bytes);
    if log.len() > MAX_LOG_BYTES {
        let drop = log.len() - MAX_LOG_BYTES;
        log.drain(..drop);
    }
}

/// Build a snapshot from live slot state.
fn snapshot(id: &str, slot: &Arc<JobSlot>) -> JobSnapshot {
    let log = slot.lock_log();
    JobSnapshot {
        id: id.to_string(),
        state: *slot.lock_state(),
        exit_code: *slot.exit_code.lock().unwrap_or_else(|e| e.into_inner()),
        cancelled: slot.cancelled.load(Ordering::SeqCst),
        timed_out: slot.timed_out.load(Ordering::SeqCst),
        log_tail: tail_text(&log),
    }
}

/// Last bytes as text, starting on a UTF-8 boundary.
fn tail_text(log: &[u8]) -> String {
    let mut start = log.len().saturating_sub(LOG_TAIL_BYTES);
    // Continuation bytes (10xxxxxx) cannot open a char; skip them.
    while start < log.len() && (log[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    String::from_utf8_lossy(&log[start..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long-running command per platform: no grandchildren, quick to kill.
    #[cfg(unix)]
    const LONG_CMD: &str = "sleep 30";
    #[cfg(windows)]
    const LONG_CMD: &str = "for /l %i in (1,1,1000000000) do @rem";

    fn harness() -> (Arc<ChildRuntime>, JobService) {
        let runtime = Arc::new(ChildRuntime::new());
        let service = JobService::new(runtime.clone());
        (runtime, service)
    }

    /// `overview` lists every job oldest first with its command and
    /// owner — the `/agents` table.
    #[tokio::test]
    async fn overview_lists_jobs_oldest_first() {
        let (_runtime, service) = harness();
        service.spawn(request("s1", "first command")).unwrap();
        service.spawn(request("s2", "second command")).unwrap();
        let rows = service.overview();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "job-1");
        assert_eq!(rows[0].command, "first command");
        assert_eq!(rows[0].owner, "s1");
        assert_eq!(rows[1].id, "job-2");
        assert_eq!(rows[1].command, "second command");
        assert!(matches!(rows[0].snapshot.state, JobState::Running));
    }

    fn request(owner: &str, command: &str) -> JobRequest {
        JobRequest {
            owner: owner.to_string(),
            command: command.to_string(),
            // nosemgrep: Semgrep_rust.lang.security.temp-dir.temp-dir
            cwd: std::env::temp_dir(),
            deny_env: Vec::new(),
            timeout_ms: None,
            foreground: false,
        }
    }

    /// Poll `read` until the job finishes, failing the test on timeout.
    async fn poll_finished(service: &JobService, id: &str) -> JobSnapshot {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let snap = service.read(id).expect("job must stay tracked");
            if snap.state == JobState::Finished {
                return snap;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job {id} did not finish in time"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn spawn_runs_echo_and_files_a_notice() {
        let (runtime, jobs) = harness();
        let id = jobs.spawn(request("t1", "echo hello")).unwrap();
        assert_eq!(id, "job-1");
        let snap = jobs.wait(&id, 10_000).await.expect("job must be tracked");
        assert_eq!(snap.state, JobState::Finished);
        assert_eq!(snap.exit_code, Some(0));
        assert!(!snap.cancelled && !snap.timed_out);
        assert!(!snap.is_failure());
        assert!(snap.log_tail.contains("hello"));
        assert!(snap.status_line().contains("exit 0"));
        let notes = runtime.drain_notifications();
        assert!(notes.iter().any(|n| n == &format!("{id} finished")));
    }

    #[tokio::test]
    async fn cap_rejects_the_eleventh_job_per_owner() {
        let (runtime, jobs) = harness();
        let mut ids = Vec::new();
        for _ in 0..MAX_JOBS_PER_OWNER {
            ids.push(jobs.spawn(request("capped", LONG_CMD)).unwrap());
        }
        assert_eq!(
            jobs.spawn(request("capped", "echo x")),
            Err(JobError::AtCapacity)
        );
        // A different owner still admits while the first is full.
        let other = jobs.spawn(request("other", "echo y")).unwrap();
        let done = poll_finished(&jobs, &other).await;
        assert_eq!(done.exit_code, Some(0));
        // Release the cap and confirm every job noticed its end.
        for id in &ids {
            assert!(jobs.cancel(id));
        }
        for id in &ids {
            let end = poll_finished(&jobs, id).await;
            assert!(end.cancelled, "job {id} must report cancellation");
        }
        let notes = runtime.drain_notifications();
        for id in ids.iter().chain(std::iter::once(&other)) {
            assert!(
                notes.iter().any(|n| n == &format!("{id} finished")),
                "missing notice for {id}"
            );
        }
    }

    #[tokio::test]
    async fn wait_timeout_snapshots_without_killing() {
        let (_runtime, jobs) = harness();
        let id = jobs.spawn(request("t", LONG_CMD)).unwrap();
        let snap = jobs.wait(&id, 150).await.expect("job must be tracked");
        assert_eq!(snap.state, JobState::Running);
        assert!(snap.status_line().contains("running"));
        // The wait never kills: the job is still alive afterwards.
        let again = jobs.read(&id).expect("job must be tracked");
        assert_eq!(again.state, JobState::Running);
        assert!(jobs.cancel(&id));
        let end = poll_finished(&jobs, &id).await;
        assert!(end.cancelled);
        assert!(!end.is_failure());
    }

    #[tokio::test]
    async fn cancel_kills_and_confirms() {
        let (runtime, jobs) = harness();
        let id = jobs.spawn(request("t", LONG_CMD)).unwrap();
        // Let the process start so the kill targets a live pid.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(jobs.cancel(&id));
        let end = poll_finished(&jobs, &id).await;
        assert!(end.cancelled);
        assert_eq!(end.state, JobState::Finished);
        assert_ne!(end.exit_code, Some(0));
        // The process group is really gone: no pid lingers to re-notify,
        // and exactly one completion notice exists for this job.
        let notes = runtime.drain_notifications();
        assert_eq!(
            notes
                .iter()
                .filter(|n| *n == &format!("{id} finished"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn run_deadline_times_out() {
        let (_runtime, jobs) = harness();
        let mut req = request("t", LONG_CMD);
        req.timeout_ms = Some(300);
        let id = jobs.spawn(req).unwrap();
        let end = poll_finished(&jobs, &id).await;
        assert!(end.timed_out);
        assert!(!end.cancelled);
        assert!(end.is_failure());
        assert!(end.status_line().contains("timed out"));
    }

    #[tokio::test]
    async fn unknown_ids_stay_explicit() {
        let (_runtime, jobs) = harness();
        assert!(jobs.read("job-999").is_none());
        assert!(jobs.wait("job-999", 100).await.is_none());
        assert!(!jobs.cancel("job-999"));
    }

    #[tokio::test]
    async fn deny_env_is_stripped_before_spawn() {
        const NAME: &str = "WAVECODE_JOB_TEST_SECRET";
        // SAFETY: unique name; no other test reads it.
        // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::set_var(NAME, "leaked");
        }
        let (_runtime, jobs) = harness();
        let command = if cfg!(windows) {
            "echo %WAVECODE_JOB_TEST_SECRET%"
        } else {
            "echo $WAVECODE_JOB_TEST_SECRET"
        };
        let mut req = request("t", command);
        req.deny_env = vec![NAME.to_string()];
        let id = jobs.spawn(req).unwrap();
        let end = poll_finished(&jobs, &id).await;
        assert_eq!(end.exit_code, Some(0));
        assert!(
            !end.log_tail.contains("leaked"),
            "denied env var must not reach the job: {}",
            end.log_tail
        );
        // SAFETY: restores the pre-test environment.
        unsafe {
            std::env::remove_var(NAME);
        }
    }

    /// A foreground run delivers its result inline, so `finish` keeps the
    /// completion notice off; promotion (`mark_notified`) re-arms it, and
    /// from then on the run notifies like any spawned job.
    #[tokio::test]
    async fn foreground_job_stays_quiet_until_promoted() {
        let (runtime, jobs) = harness();
        let mut foreground_request = request("t1", "echo foreground");
        foreground_request.foreground = true;
        let id = jobs.spawn(foreground_request).unwrap();
        let snap = poll_finished(&jobs, &id).await;
        assert_eq!(snap.exit_code, Some(0));
        // Inline delivery: no notice for output the caller already has.
        assert!(
            runtime.drain_notifications().is_empty(),
            "an inline-delivered foreground run stays quiet"
        );
        // A promoted-while-running job notifies on completion like any
        // spawned job once `mark_notified` has armed it.
        let mut slow = request("t1", LONG_CMD);
        slow.foreground = true;
        let slow_id = jobs.spawn(slow).unwrap();
        assert!(jobs.mark_notified(&slow_id), "promotion lands on a live id");
        assert!(jobs.cancel(&slow_id));
        let end = poll_finished(&jobs, &slow_id).await;
        assert!(end.cancelled);
        let notes = runtime.drain_notifications();
        assert!(
            notes.iter().any(|n| n == &format!("{slow_id} finished")),
            "a promoted run notifies on completion: {notes:?}"
        );
        assert!(!jobs.mark_notified("job-999"));
    }

    /// A foreground run that quietly finished before the caller armed the
    /// notice (the wait snapshot predated the exit) must still announce:
    /// `mark_notified` finds the terminal state and files the notice
    /// retroactively, so a promoted run's completion is never lost.
    #[tokio::test]
    async fn mark_notified_after_a_quiet_finish_announces_retroactively() {
        let (runtime, jobs) = harness();
        let mut quiet = request("t1", "echo foreground");
        quiet.foreground = true;
        let id = jobs.spawn(quiet).unwrap();
        let _ = poll_finished(&jobs, &id).await;
        assert!(
            runtime.drain_notifications().is_empty(),
            "the inline-delivered run stays quiet"
        );
        assert!(jobs.mark_notified(&id));
        let notes = runtime.drain_notifications();
        assert!(
            notes.iter().any(|n| n == &format!("{id} finished")),
            "the retroactive notice must fire: {notes:?}"
        );
    }

    /// Filling the table to its cap with a mix of running and finished
    /// jobs, then spawning one more, prunes only the finished entries:
    /// query history is dropped, every live job stays tracked (a pruned
    /// running job would orphan its notice and lose `wait`/`cancel`).
    #[tokio::test]
    async fn table_cap_prunes_finished_and_keeps_running() {
        let (_runtime, jobs) = harness();
        // Six long-running jobs under one owner (below the per-owner
        // cap) survive the prune.
        let mut running_ids = Vec::new();
        for _ in 0..6 {
            running_ids.push(jobs.spawn(request("bg", LONG_CMD)).unwrap());
        }
        // Fifty-eight quick jobs fill the rest of the table and finish;
        // ten fit per owner while all ten drivers are still Running.
        let mut finished_ids = Vec::new();
        for owner in 0..6 {
            let owner = format!("w{owner}");
            let count = if owner == "w5" { 8 } else { 10 };
            for _ in 0..count {
                finished_ids.push(jobs.spawn(request(&owner, "echo done")).unwrap());
            }
        }
        assert_eq!(jobs.overview().len(), MAX_TRACKED_JOBS);
        // Every filler reached its terminal state, so the prune has no
        // in-flight filler to spare.
        for id in &finished_ids {
            let end = poll_finished(&jobs, id).await;
            assert_eq!(end.exit_code, Some(0), "{id} must finish cleanly");
        }
        // The 65th spawn trips the cap: finished entries prune, the new
        // job is admitted, and every running job keeps its slot.
        let probe = jobs.spawn(request("bg", "echo probe")).unwrap();
        for id in &running_ids {
            let snap = jobs.read(id).unwrap_or_else(|| panic!("{id} pruned"));
            assert_eq!(snap.state, JobState::Running, "{id} must survive");
        }
        for id in &finished_ids {
            assert!(jobs.read(id).is_none(), "{id} must be pruned");
        }
        let probe_end = poll_finished(&jobs, &probe).await;
        assert_eq!(probe_end.exit_code, Some(0));
        assert_eq!(
            jobs.overview().len(),
            running_ids.len() + 1,
            "only the running jobs plus the probe remain"
        );
        // Cleanup: release the live jobs so no process outlives the test.
        for id in &running_ids {
            assert!(jobs.cancel(id));
        }
        for id in &running_ids {
            assert!(poll_finished(&jobs, id).await.cancelled);
        }
    }
}
