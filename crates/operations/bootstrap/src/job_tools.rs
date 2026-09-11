/*!
 * @file JobTools
 * @description Model-invoked background shell jobs: spawn, wait, cancel, output.
 *
 * Responsibilities:
 * - Spawn shell commands as background jobs that outlive the tool call.
 * - Wait with a timeout that snapshots without killing the job.
 * - Cancel a running job through the service tree-kill.
 * - Report one job's status plus its truncated log tail.
 *
 * This module must not depend on: drivers, actors, or sessions. Tools read
 * the job service only; completion still arrives via notifications.
 */

//! Background job tools: long work that stops blocking the turn.
//!
//! `job_spawn` returns a `job-N` id immediately; the model polls with
//! `job_wait` (bounded block, never kills) or `job_output`, and stops
//! unneeded work with `job_cancel`. Completion notices re-enter turns
//! through the same child-runtime channel child tasks use.

use std::sync::Arc;

use action_jobs::{JobRequest, JobService};
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// Owner charged against the per-owner cap: one session, one owner.
const JOB_OWNER: &str = "session";
/// Default `job_wait` block when the caller passes no timeout.
const DEFAULT_WAIT_MS: u64 = 30_000;

/// Shared lookup helper: unknown ids become business errors.
fn lookup(
    jobs: &Arc<JobService>,
    id: &str,
) -> std::result::Result<action_jobs::JobSnapshot, ToolOutput> {
    match jobs.read(id) {
        Some(snapshot) => Ok(snapshot),
        None => Err(ToolOutput {
            content: format!("unknown job id: {id}"),
            is_error: true,
        }),
    }
}

/// Shared timeout helper: absent means `default_ms`, present must be a
/// non-negative integer. Business errors, never panics.
fn parse_timeout(
    input: &serde_json::Value,
    default_ms: u64,
) -> std::result::Result<u64, ToolOutput> {
    match input.get("timeout_ms") {
        None | Some(serde_json::Value::Null) => Ok(default_ms),
        Some(value) => match value.as_u64() {
            Some(ms) => Ok(ms),
            None => Err(ToolOutput {
                content: "invalid parameter 'timeout_ms' (non-negative integer required)"
                    .to_string(),
                is_error: true,
            }),
        },
    }
}

/// Render one snapshot: status line plus log tail when non-empty.
///
/// Terminal failures (nonzero exit, run deadline) surface as tool errors
/// so the model retries or explains; cancellation is the model acting on
/// purpose, so it stays non-error like `task_stop` outcomes.
fn render(snapshot: &action_jobs::JobSnapshot) -> ToolOutput {
    let mut content = snapshot.status_line();
    if !snapshot.log_tail.is_empty() {
        content.push_str("\n--- log tail ---\n");
        content.push_str(&snapshot.log_tail);
    }
    ToolOutput {
        content,
        is_error: snapshot.is_failure(),
    }
}

/// Required string field helper: missing or blank becomes a business error.
fn required_str<'a>(
    input: &'a serde_json::Value,
    field: &str,
) -> std::result::Result<&'a str, ToolOutput> {
    match input.get(field).and_then(|v| v.as_str()) {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ToolOutput {
            content: format!("missing required field: {field}"),
            is_error: true,
        }),
    }
}

/// `job_spawn`: start a shell command in the background, return `job-N`.
///
/// Writing (it launches a process), but not destructive: cancelling or
/// bounding comes later via `job_cancel` or the spawn `timeout_ms` run
/// deadline. Rejects at the per-owner cap instead of queueing.
pub struct JobSpawnTool {
    jobs: Arc<JobService>,
}

impl JobSpawnTool {
    /// Wrap the job service.
    pub fn new(jobs: Arc<JobService>) -> Self {
        Self { jobs }
    }
}

#[async_trait::async_trait]
impl Tool for JobSpawnTool {
    fn name(&self) -> &str {
        "job_spawn"
    }

    fn description(&self) -> &str {
        "Start a shell command as a background job and return its id \
         (job-N) immediately; long work stops blocking the turn. Poll with \
         `job_wait` or `job_output`, stop with `job_cancel`. Completion \
         arrives as a notification. Optional timeout_ms bounds the run: \
         past the deadline the whole process tree is killed."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to run in the working directory",
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Run deadline in milliseconds; past it the process tree is killed",
                },
            },
            "required": ["command"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let command = match required_str(&input, "command") {
            Ok(command) => command.to_string(),
            Err(output) => return Ok(output),
        };
        let timeout_ms = match parse_timeout(&input, 0) {
            Ok(0) => None,
            Ok(ms) => Some(ms),
            Err(output) => return Ok(output),
        };
        let request = JobRequest {
            owner: JOB_OWNER.to_string(),
            command,
            cwd: ctx.cwd.clone(),
            deny_env: ctx.deny_env.clone(),
            timeout_ms,
        };
        match self.jobs.spawn(request) {
            Ok(id) => Ok(ToolOutput {
                content: format!(
                    "spawned job {id}; poll with job_wait or job_output, stop with job_cancel"
                ),
                is_error: false,
            }),
            Err(action_jobs::JobError::AtCapacity) => Ok(ToolOutput {
                content: format!(
                    "job capacity reached ({} per owner); cancel or await a job, then retry",
                    action_jobs::MAX_JOBS_PER_OWNER
                ),
                is_error: true,
            }),
        }
    }
}

/// `job_wait`: block up to `timeout_ms`, then report without killing.
///
/// A timed-out wait leaves the job running: the model does other work
/// and polls again. Unknown ids stay explicit business errors.
pub struct JobWaitTool {
    jobs: Arc<JobService>,
}

impl JobWaitTool {
    /// Wrap the job service.
    pub fn new(jobs: Arc<JobService>) -> Self {
        Self { jobs }
    }
}

#[async_trait::async_trait]
impl Tool for JobWaitTool {
    fn name(&self) -> &str {
        "job_wait"
    }

    fn description(&self) -> &str {
        "Wait for one background job up to timeout_ms (default 30000), \
         then report its status and log tail. Waiting never kills the job: \
         a still-running result just means poll again later."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Job id, e.g. job-1",
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "How long to block in milliseconds (default 30000)",
                },
            },
            "required": ["id"],
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let id = match required_str(&input, "id") {
            Ok(id) => id.trim().to_string(),
            Err(output) => return Ok(output),
        };
        let timeout_ms = match parse_timeout(&input, DEFAULT_WAIT_MS) {
            Ok(ms) => ms,
            Err(output) => return Ok(output),
        };
        match self.jobs.wait(&id, timeout_ms).await {
            Some(snapshot) => Ok(render(&snapshot)),
            None => Ok(ToolOutput {
                content: format!("unknown job id: {id}"),
                is_error: true,
            }),
        }
    }
}

/// `job_cancel`: kill one job's whole process tree by id.
///
/// Destructive (approval-gated): running work dies mid-flight. Finished
/// jobs accept trivially; unknown ids stay explicit business errors.
pub struct JobCancelTool {
    jobs: Arc<JobService>,
}

impl JobCancelTool {
    /// Wrap the job service.
    pub fn new(jobs: Arc<JobService>) -> Self {
        Self { jobs }
    }
}

#[async_trait::async_trait]
impl Tool for JobCancelTool {
    fn name(&self) -> &str {
        "job_cancel"
    }

    fn description(&self) -> &str {
        "Cancel one background job by id, killing its whole process tree. \
         Use it when the job is no longer needed or went down the wrong \
         path; its partial log stays readable via `job_output`."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Job id, e.g. job-1",
                },
            },
            "required": ["id"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        true
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let id = match required_str(&input, "id") {
            Ok(id) => id.trim().to_string(),
            Err(output) => return Ok(output),
        };
        if self.jobs.cancel(&id) {
            Ok(ToolOutput {
                content: format!("cancel requested for job {id}"),
                is_error: false,
            })
        } else {
            Ok(ToolOutput {
                content: format!("unknown job id: {id}"),
                is_error: true,
            })
        }
    }
}

/// `job_output`: report one job's status and truncated log tail.
///
/// Pure observation: never blocks past a snapshot, never kills.
pub struct JobOutputTool {
    jobs: Arc<JobService>,
}

impl JobOutputTool {
    /// Wrap the job service.
    pub fn new(jobs: Arc<JobService>) -> Self {
        Self { jobs }
    }
}

#[async_trait::async_trait]
impl Tool for JobOutputTool {
    fn name(&self) -> &str {
        "job_output"
    }

    fn description(&self) -> &str {
        "Report one background job by id (from `job_spawn`): running state \
         or terminal status plus the truncated log tail. Poll running jobs \
         again after doing other work."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Job id, e.g. job-1",
                },
            },
            "required": ["id"],
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let id = match required_str(&input, "id") {
            Ok(id) => id.trim().to_string(),
            Err(output) => return Ok(output),
        };
        match lookup(&self.jobs, &id) {
            Ok(snapshot) => Ok(render(&snapshot)),
            Err(output) => Ok(output),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keep the tempdir alive across the await points of one test.
    struct Ctx {
        _dir: tempfile::TempDir,
        ctx: ToolCtx,
    }

    fn ctx() -> Ctx {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        Ctx { _dir: dir, ctx }
    }

    fn jobs() -> Arc<JobService> {
        Arc::new(JobService::new(
            Arc::new(runtime_child::ChildRuntime::new()),
        ))
    }

    #[tokio::test]
    async fn spawn_rejects_missing_command() {
        let jobs = jobs();
        let tool = JobSpawnTool::new(jobs);
        assert!(!tool.is_read_only());
        assert!(!tool.is_destructive());
        let out = tool
            .execute(serde_json::json!({}), &ctx().ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("missing required field: command"));
    }

    #[tokio::test]
    async fn spawn_rejects_bad_timeout() {
        let jobs = jobs();
        let tool = JobSpawnTool::new(jobs);
        let out = tool
            .execute(
                serde_json::json!({"command": "echo hi", "timeout_ms": "soon"}),
                &ctx().ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("timeout_ms"));
    }

    #[tokio::test]
    async fn spawn_wait_output_roundtrip() {
        let jobs = jobs();
        let held = ctx();
        let spawn = JobSpawnTool::new(jobs.clone());
        let out = spawn
            .execute(
                serde_json::json!({"command": "echo wavecode-job-ok"}),
                &held.ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("job-1"));
        let wait = JobWaitTool::new(jobs.clone());
        assert!(wait.is_read_only());
        let waited = wait
            .execute(
                serde_json::json!({"id": "job-1", "timeout_ms": 10_000}),
                &held.ctx,
            )
            .await
            .unwrap();
        assert!(!waited.is_error);
        assert!(waited.content.contains("exit 0"));
        assert!(waited.content.contains("wavecode-job-ok"));
        let output = JobOutputTool::new(jobs);
        assert!(output.is_read_only());
        let seen = output
            .execute(serde_json::json!({"id": "job-1"}), &held.ctx)
            .await
            .unwrap();
        assert!(!seen.is_error);
        assert!(seen.content.contains("wavecode-job-ok"));
    }

    #[tokio::test]
    async fn wait_and_output_report_unknown() {
        let jobs = jobs();
        let held = ctx();
        let wait = JobWaitTool::new(jobs.clone());
        let ghost = wait
            .execute(serde_json::json!({"id": "job-9"}), &held.ctx)
            .await
            .unwrap();
        assert!(ghost.is_error);
        assert!(ghost.content.contains("unknown job id"));
        let output = JobOutputTool::new(jobs);
        let ghost = output
            .execute(serde_json::json!({}), &held.ctx)
            .await
            .unwrap();
        assert!(ghost.is_error);
    }

    #[tokio::test]
    async fn cancel_kills_a_running_job() {
        let jobs = jobs();
        let held = ctx();
        let spawn = JobSpawnTool::new(jobs.clone());
        let kind = if cfg!(windows) {
            "for /l %i in (1,1,1000000000) do @rem"
        } else {
            "sleep 30"
        };
        let out = spawn
            .execute(serde_json::json!({"command": kind}), &held.ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        let cancel = JobCancelTool::new(jobs.clone());
        assert!(!cancel.is_read_only());
        assert!(cancel.is_destructive());
        let stopped = cancel
            .execute(serde_json::json!({"id": "job-1"}), &held.ctx)
            .await
            .unwrap();
        assert!(!stopped.is_error);
        assert!(stopped.content.contains("cancel requested"));
        // Unknown ids stay explicit errors, not silent success.
        let ghost = cancel
            .execute(serde_json::json!({"id": "job-9"}), &held.ctx)
            .await
            .unwrap();
        assert!(ghost.is_error);
        // The partial job really ended cancelled.
        let output = JobOutputTool::new(jobs.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let seen = output
                .execute(serde_json::json!({"id": "job-1"}), &held.ctx)
                .await
                .unwrap();
            if seen.content.contains("cancelled") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job did not cancel in time: {}",
                seen.content
            );
            tokio::task::yield_now().await;
        }
    }
}
