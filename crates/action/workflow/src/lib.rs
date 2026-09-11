/*!
 * @file WorkflowEngine
 * @description Validated DAG execution and Ralph loops over TaskService.
 *
 * Responsibilities:
 * - Validate workflow specs as DAGs (unknown deps and cycles are errors).
 * - Run ready steps through the injected TaskService with bounded fan-out.
 * - Drive Ralph loops with an immutable objective until the done marker.
 *
 * This module must not depend on: runtime, state, operations, transport,
 * frontends, or any execution layer. It only names the action-tasks seam.
 */

//! Workflow engine: fan-out DAG runs plus Ralph improvement loops.
//!
//! A [`WorkflowSpec`] is a set of steps with `depends_on` edges. The
//! executor validates the DAG, then runs dependency waves: every ready
//! wave spawns before collection so independent steps overlap, while each
//! `fanout` step expands its input list in chunks of `max_parallel`. Step
//! summaries merge into one JSON object keyed by step id. Any step failure
//! fails the whole run naming the step id; v1 performs no partial retry.
//!
//! A Ralph loop respawns a fresh child per round with the SAME immutable
//! objective plus the prior round summary, stopping when a child reports
//! a line containing only `RALPH_DONE` or when rounds exhaust.

use std::collections::{BTreeMap, HashMap};

use action_tasks::{TaskKind, TaskOutcome, TaskRequest, TaskService, TaskState};

/// Completion marker: a child reports this alone on one summary line.
pub const RALPH_DONE_MARKER: &str = "RALPH_DONE";
/// Hard cap on Ralph rounds per run.
pub const MAX_RALPH_ROUNDS: u32 = 10;
/// Default Ralph rounds when the caller passes none.
pub const DEFAULT_RALPH_ROUNDS: u32 = 5;
/// Default fan-out width per step when the step passes none.
pub const DEFAULT_FANOUT_PARALLEL: usize = 4;
/// Upper clamp for a step `max_parallel` (zero or less means default).
pub const MAX_FANOUT_PARALLEL: usize = 16;
/// Cap on ready steps spawned per wave before collection.
pub const MAX_WAVE_PARALLEL: usize = 8;
/// Poll interval while waiting on a spawned child task.
const POLL_INTERVAL_MS: u64 = 10;
/// Poll attempts before waiting gives up (10ms x 30000 = 300s).
const MAX_POLLS: u32 = 30_000;

/// One workflow step kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    /// One child task over the step input.
    #[default]
    Task,
    /// One child task per input list item, bounded by `max_parallel`.
    Fanout,
}

/// One DAG step.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct WorkflowStep {
    /// Stable id; result summaries key by this.
    pub id: String,
    /// Step kind, defaulting to a single task.
    #[serde(default)]
    pub kind: StepKind,
    /// String input for `task`, string array for `fanout`.
    #[serde(default)]
    pub input: serde_json::Value,
    /// Ids that must complete before this step runs.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Fan-out width; unset or zero means the default.
    #[serde(default)]
    pub max_parallel: Option<usize>,
}

/// A set of steps forming a dependency DAG.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct WorkflowSpec {
    /// Steps in any order; edges come from `depends_on`.
    pub steps: Vec<WorkflowStep>,
}

/// Workflow validation and execution failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowError {
    /// Two steps share one id.
    #[error("duplicate step id: {0}")]
    DuplicateStep(String),
    /// A step names a dependency that does not exist.
    #[error("step '{step}' depends on unknown step '{dep}'")]
    UnknownDependency {
        /// Step holding the dangling edge.
        step: String,
        /// Missing dependency id.
        dep: String,
    },
    /// Edges form a cycle (or a wave otherwise stalls).
    #[error("dependency cycle detected")]
    Cycle,
    /// The spec shape itself is unusable.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),
    /// A step child failed; the run stops with the step id named.
    #[error("step '{id}' failed: {reason}")]
    StepFailed {
        /// Failing step id.
        id: String,
        /// Child failure reason.
        reason: String,
    },
}

/// Ralph loop failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RalphError {
    /// The objective is blank.
    #[error("objective must not be blank")]
    EmptyObjective,
    /// Round budget outside 1..=MAX_RALPH_ROUNDS.
    #[error("max_rounds must be 1..={MAX_RALPH_ROUNDS}, got {0}")]
    BadRounds(u32),
    /// A round child failed; the loop stops with the round named.
    #[error("round {round} failed: {reason}")]
    RoundFailed {
        /// Failing round number, 1-based.
        round: u32,
        /// Child failure reason.
        reason: String,
    },
}

/// One finished Ralph round.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RalphRound {
    /// Round number, 1-based.
    pub round: u32,
    /// Fresh child id spawned for this round.
    pub task_id: String,
    /// Child summary text.
    pub summary: String,
    /// True when the summary carried the done marker.
    pub done: bool,
}

/// Full Ralph loop outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RalphReport {
    /// The immutable objective every round received.
    pub objective: String,
    /// True when a round reported the done marker.
    pub completed: bool,
    /// Per-round log in execution order.
    pub rounds: Vec<RalphRound>,
}

/// True when any summary line is exactly the done marker.
pub fn contains_done_marker(summary: &str) -> bool {
    summary.lines().any(|line| line.trim() == RALPH_DONE_MARKER)
}

/// Validate step ids and edges: duplicates, blanks, unknown deps, cycles.
pub fn validate_spec(spec: &WorkflowSpec) -> Result<(), WorkflowError> {
    let mut seen = std::collections::HashSet::new();
    for step in &spec.steps {
        if step.id.trim().is_empty() {
            return Err(WorkflowError::InvalidSpec(
                "step id must not be blank".to_string(),
            ));
        }
        if !seen.insert(step.id.clone()) {
            return Err(WorkflowError::DuplicateStep(step.id.clone()));
        }
    }
    let ids: std::collections::HashSet<&str> =
        spec.steps.iter().map(|step| step.id.as_str()).collect();
    for step in &spec.steps {
        for dep in &step.depends_on {
            if !ids.contains(dep.as_str()) {
                return Err(WorkflowError::UnknownDependency {
                    step: step.id.clone(),
                    dep: dep.clone(),
                });
            }
            if dep == &step.id {
                return Err(WorkflowError::Cycle);
            }
        }
    }
    // Kahn drain: leftovers mean a cycle.
    let mut indegree: HashMap<&str, usize> = HashMap::new();
    for step in &spec.steps {
        indegree.entry(step.id.as_str()).or_insert(0);
        for dep in &step.depends_on {
            *indegree.entry(step.id.as_str()).or_insert(0) += 1;
            indegree.entry(dep.as_str()).or_insert(0);
        }
    }
    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut drained = 0usize;
    while let Some(id) = ready.pop() {
        drained += 1;
        for step in &spec.steps {
            if step.depends_on.iter().any(|dep| dep == id)
                && let Some(degree) = indegree.get_mut(step.id.as_str())
            {
                *degree -= 1;
                if *degree == 0 {
                    ready.push(step.id.as_str());
                }
            }
        }
    }
    if drained != spec.steps.len() {
        return Err(WorkflowError::Cycle);
    }
    Ok(())
}

/// Render one child input: strings pass through, other JSON stringifies.
fn render_input(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        _ => value.to_string(),
    }
}

/// Expand one step into its child input list.
fn step_inputs(step: &WorkflowStep) -> Result<Vec<String>, WorkflowError> {
    match step.kind {
        StepKind::Task => Ok(vec![render_input(&step.input)]),
        StepKind::Fanout => match &step.input {
            serde_json::Value::Array(items) => Ok(items.iter().map(render_input).collect()),
            _ => Err(WorkflowError::InvalidSpec(format!(
                "fanout step '{}' needs an input array",
                step.id
            ))),
        },
    }
}

/// Effective fan-out width: default on unset/zero, clamped to the max.
fn fanout_width(step: &WorkflowStep) -> usize {
    match step.max_parallel.unwrap_or(0) {
        0 => DEFAULT_FANOUT_PARALLEL,
        width => width.clamp(1, MAX_FANOUT_PARALLEL),
    }
}

/// Spawn one child for `input` on the workflow owner lane.
fn spawn_child(tasks: &dyn TaskService, input: String) -> String {
    tasks.spawn(TaskRequest {
        kind: TaskKind::Standard,
        input,
        parent_run_id: "workflow".to_string(),
        allowed_tools: Vec::new(),
        depth: 0,
        parent: None,
    })
}

/// Wait for one spawned child; `Ok` carries the summary, `Err` the reason.
async fn await_child(tasks: &dyn TaskService, task_id: &str) -> Result<String, String> {
    let mut polls = 0u32;
    loop {
        match tasks.query(task_id) {
            Some(info) => match info.state {
                TaskState::Finished => match info.outcome {
                    Some(TaskOutcome::Completed { summary }) => return Ok(summary),
                    Some(TaskOutcome::Failed { reason }) => return Err(reason),
                    Some(TaskOutcome::Stopped) => {
                        return Err("child was stopped".to_string());
                    }
                    None => return Err("child finished without an outcome".to_string()),
                },
                TaskState::Running => {
                    polls += 1;
                    if polls > MAX_POLLS {
                        return Err("timed out waiting for child".to_string());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
                }
            },
            None => return Err("child handle unknown".to_string()),
        }
    }
}

/// Run one step: spawn its children in `max_parallel` chunks, collect.
///
/// Chunks bound in-flight children per step; chunks run in input order so
/// merged summaries stay deterministic.
async fn run_step(
    step: &WorkflowStep,
    tasks: &dyn TaskService,
) -> Result<serde_json::Value, WorkflowError> {
    let inputs = step_inputs(step)?;
    let mut summaries = Vec::with_capacity(inputs.len());
    let width = fanout_width(step);
    for chunk in inputs.chunks(width.max(1)) {
        let ids: Vec<String> = chunk
            .iter()
            .cloned()
            .map(|input| spawn_child(tasks, input))
            .collect();
        for task_id in &ids {
            match await_child(tasks, task_id).await {
                Ok(summary) => summaries.push(summary),
                Err(reason) => {
                    return Err(WorkflowError::StepFailed {
                        id: step.id.clone(),
                        reason,
                    });
                }
            }
        }
    }
    match step.kind {
        StepKind::Task => Ok(serde_json::Value::String(
            summaries.into_iter().next().unwrap_or_default(),
        )),
        StepKind::Fanout => Ok(serde_json::Value::Array(
            summaries
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        )),
    }
}

/// Run a validated spec; returns one JSON object keyed by step id.
///
/// Ready waves spawn in id order in chunks of [`MAX_WAVE_PARALLEL`]
/// before collection, so independent steps overlap without unbounded
/// fan-out. The first failing step aborts the run with its id named;
/// v1 performs no partial retry.
pub async fn run_workflow(
    spec: &WorkflowSpec,
    tasks: &dyn TaskService,
) -> Result<BTreeMap<String, serde_json::Value>, WorkflowError> {
    validate_spec(spec)?;
    let mut done: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    while done.len() < spec.steps.len() {
        let mut ready: Vec<&WorkflowStep> = spec
            .steps
            .iter()
            .filter(|step| {
                !done.contains_key(&step.id)
                    && step.depends_on.iter().all(|dep| done.contains_key(dep))
            })
            .collect();
        if ready.is_empty() {
            // Unreachable after validation; kept as a backstop.
            return Err(WorkflowError::Cycle);
        }
        ready.sort_by(|left, right| left.id.cmp(&right.id));
        for wave in ready.chunks(MAX_WAVE_PARALLEL) {
            for step in wave {
                let summary = run_step(step, tasks).await?;
                done.insert(step.id.clone(), summary);
            }
        }
    }
    Ok(done)
}

/// Run a Ralph loop: fresh child per round, same immutable objective.
///
/// Each round prompt carries the objective verbatim plus the prior round
/// summary; the loop stops at the first summary with a [`RALPH_DONE_MARKER`]
/// line or when `max_rounds` exhausts.
pub async fn ralph_run(
    objective: &str,
    max_rounds: u32,
    tasks: &dyn TaskService,
) -> Result<RalphReport, RalphError> {
    if objective.trim().is_empty() {
        return Err(RalphError::EmptyObjective);
    }
    if max_rounds == 0 || max_rounds > MAX_RALPH_ROUNDS {
        return Err(RalphError::BadRounds(max_rounds));
    }
    let mut rounds = Vec::with_capacity(max_rounds as usize);
    let mut prior = String::new();
    let mut completed = false;
    for round in 1..=max_rounds {
        let input = if prior.is_empty() {
            format!(
                "{objective}\n\nEnd your final summary with a line containing only \
                 {RALPH_DONE_MARKER} when the objective is fully met."
            )
        } else {
            format!(
                "{objective}\n\nPrior round summary:\n{prior}\n\nSame objective as before: \
                 keep improving until it is fully met, then end your final summary \
                 with a line containing only {RALPH_DONE_MARKER}."
            )
        };
        let task_id = tasks.spawn(TaskRequest {
            kind: TaskKind::Standard,
            input,
            parent_run_id: "ralph".to_string(),
            allowed_tools: Vec::new(),
            depth: 0,
            parent: None,
        });
        match await_child(tasks, &task_id).await {
            Ok(summary) => {
                let done = contains_done_marker(&summary);
                prior = summary.clone();
                rounds.push(RalphRound {
                    round,
                    task_id,
                    summary,
                    done,
                });
                if done {
                    completed = true;
                    break;
                }
            }
            Err(reason) => return Err(RalphError::RoundFailed { round, reason }),
        }
    }
    Ok(RalphReport {
        objective: objective.to_string(),
        completed,
        rounds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn task_step(id: &str, input: &str, depends_on: &[&str]) -> WorkflowStep {
        WorkflowStep {
            id: id.to_string(),
            kind: StepKind::Task,
            input: serde_json::Value::String(input.to_string()),
            depends_on: depends_on.iter().map(|dep| dep.to_string()).collect(),
            max_parallel: None,
        }
    }

    fn spec_from_json(text: &str) -> WorkflowSpec {
        serde_json::from_str(text).expect("test spec parses")
    }

    #[test]
    fn rejects_unknown_dependency() {
        let spec = WorkflowSpec {
            steps: vec![task_step("a", "x", &["ghost"])],
        };
        assert_eq!(
            validate_spec(&spec),
            Err(WorkflowError::UnknownDependency {
                step: "a".to_string(),
                dep: "ghost".to_string(),
            })
        );
    }

    #[test]
    fn rejects_cycles_and_self_edges() {
        let looped = WorkflowSpec {
            steps: vec![task_step("a", "x", &["b"]), task_step("b", "y", &["a"])],
        };
        assert_eq!(validate_spec(&looped), Err(WorkflowError::Cycle));
        let selfie = WorkflowSpec {
            steps: vec![task_step("a", "x", &["a"])],
        };
        assert_eq!(validate_spec(&selfie), Err(WorkflowError::Cycle));
    }

    #[test]
    fn rejects_duplicate_and_blank_ids() {
        let duped = WorkflowSpec {
            steps: vec![task_step("a", "x", &[]), task_step("a", "y", &[])],
        };
        assert_eq!(
            validate_spec(&duped),
            Err(WorkflowError::DuplicateStep("a".to_string()))
        );
        let blank = WorkflowSpec {
            steps: vec![task_step("  ", "x", &[])],
        };
        assert!(matches!(
            validate_spec(&blank),
            Err(WorkflowError::InvalidSpec(_))
        ));
    }

    #[tokio::test]
    async fn fanout_merges_summaries_after_dependencies() {
        let spec = spec_from_json(
            r#"{"steps": [
                {"id": "a", "kind": "task", "input": "alpha"},
                {"id": "b", "kind": "fanout", "input": ["x", "y", "z"],
                 "depends_on": ["a"], "max_parallel": 2}
            ]}"#,
        );
        let service = action_tasks::FakeTaskService::new();
        let merged = run_workflow(&spec, &service).await.expect("run succeeds");
        assert_eq!(
            merged.get("a"),
            Some(&serde_json::Value::String("alpha".to_string()))
        );
        assert_eq!(merged.get("b"), Some(&serde_json::json!(["x", "y", "z"])));
        // Dependency order holds: the lone task spawns before fan-out items.
        let inputs: Vec<String> = service
            .spawned()
            .into_iter()
            .map(|(_, request)| request.input)
            .collect();
        assert_eq!(inputs, vec!["alpha", "x", "y", "z"]);
    }

    /// Immediate stub failing every child, so step errors name the step.
    struct FailAll;

    impl TaskService for FailAll {
        fn spawn(&self, _request: TaskRequest) -> String {
            "task-1".to_string()
        }

        fn query(&self, _id: &str) -> Option<action_tasks::TaskInfo> {
            Some(action_tasks::TaskInfo {
                state: TaskState::Finished,
                outcome: Some(TaskOutcome::Failed {
                    reason: "boom".to_string(),
                }),
                lineage: vec!["task-1".to_string()],
            })
        }

        fn stop(&self, _id: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn step_failure_names_the_step_id() {
        let spec = WorkflowSpec {
            steps: vec![task_step("fragile", "x", &[])],
        };
        let err = run_workflow(&spec, &FailAll).await.expect_err("run fails");
        assert_eq!(
            err,
            WorkflowError::StepFailed {
                id: "fragile".to_string(),
                reason: "boom".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn fanout_rejects_non_array_input() {
        let spec = WorkflowSpec {
            steps: vec![WorkflowStep {
                id: "b".to_string(),
                kind: StepKind::Fanout,
                input: serde_json::Value::String("nope".to_string()),
                depends_on: Vec::new(),
                max_parallel: None,
            }],
        };
        assert!(matches!(
            run_workflow(&spec, &FailAll).await,
            Err(WorkflowError::InvalidSpec(_))
        ));
    }

    /// Ordered stub: spawn order selects the canned summary by index.
    struct OrderedEcho {
        summaries: Vec<String>,
        spawned: Mutex<Vec<TaskRequest>>,
    }

    impl OrderedEcho {
        fn new(summaries: Vec<&str>) -> Self {
            Self {
                summaries: summaries.into_iter().map(|text| text.to_string()).collect(),
                spawned: Mutex::new(Vec::new()),
            }
        }
    }

    impl TaskService for OrderedEcho {
        fn spawn(&self, request: TaskRequest) -> String {
            let mut spawned = self.spawned.lock().unwrap_or_else(|e| e.into_inner());
            spawned.push(request);
            format!("task-{}", spawned.len())
        }

        fn query(&self, id: &str) -> Option<action_tasks::TaskInfo> {
            let spawned = self.spawned.lock().unwrap_or_else(|e| e.into_inner());
            let index: usize = id.strip_prefix("task-")?.parse().ok()?;
            if index == 0 || index > spawned.len() {
                return None;
            }
            let summary = self
                .summaries
                .get(index - 1)
                .cloned()
                .unwrap_or_else(|| self.summaries.last().cloned().unwrap_or_default());
            Some(action_tasks::TaskInfo {
                state: TaskState::Finished,
                outcome: Some(TaskOutcome::Completed { summary }),
                lineage: vec![id.to_string()],
            })
        }

        fn stop(&self, _id: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn ralph_stops_on_the_done_marker() {
        let service = OrderedEcho::new(vec!["still working", "polished\nRALPH_DONE"]);
        let report = ralph_run("polish the draft", 5, &service)
            .await
            .expect("ralph runs");
        assert!(report.completed);
        assert_eq!(report.rounds.len(), 2);
        assert!(!report.rounds[0].done);
        assert!(report.rounds[1].done);
        // Same immutable objective seeds every round prompt.
        let spawned = service.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 2);
        for request in spawned.iter() {
            assert!(request.input.starts_with("polish the draft"));
        }
        assert!(spawned[1].input.contains("still working"));
    }

    #[tokio::test]
    async fn ralph_exhausts_rounds_without_a_marker() {
        let service = action_tasks::FakeTaskService::new();
        let report = ralph_run("never done", 3, &service)
            .await
            .expect("ralph runs");
        assert!(!report.completed);
        assert_eq!(report.rounds.len(), 3);
    }

    #[tokio::test]
    async fn ralph_rejects_blank_objectives_and_bad_budgets() {
        let service = action_tasks::FakeTaskService::new();
        assert_eq!(
            ralph_run("  ", 3, &service).await,
            Err(RalphError::EmptyObjective)
        );
        assert_eq!(
            ralph_run("work", 0, &service).await,
            Err(RalphError::BadRounds(0))
        );
        assert_eq!(
            ralph_run("work", MAX_RALPH_ROUNDS + 1, &service).await,
            Err(RalphError::BadRounds(MAX_RALPH_ROUNDS + 1))
        );
    }

    #[test]
    fn done_marker_needs_its_own_line() {
        assert!(contains_done_marker("polished\nRALPH_DONE"));
        assert!(contains_done_marker("RALPH_DONE\ntrailing"));
        assert!(contains_done_marker("  RALPH_DONE  "));
        assert!(!contains_done_marker("almost RALPH_DONE-ish"));
        assert!(!contains_done_marker("RALPH_DONE marker missing alone"));
    }
}
