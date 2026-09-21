/*
 * @file TaskEval
 * @description Task-level benchmark manifests, assertion scoring, pass rates.
 *
 * Responsibilities:
 * - Parse one-task-per-file TOML manifests from a tasks directory.
 * - Score a task's assertions against a caller-supplied `World`
 *   (filesystem reads + command exits), never against conversation text.
 * - Aggregate per-task scores into a suite report with a pass rate,
 *   rendered as human text or JSON.
 *
 * This module must not depend on: processes, models, the agent, or any
 * concrete fixture layout. It defines the judging protocol; the caller
 * owns execution and hands observations back through `World`.
 */

//! Task-level evaluation: an agent edits a workspace, assertions judge it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Default wall cap for one agent step (`TaskSpec::agent_timeout`).
pub const DEFAULT_AGENT_TIMEOUT_SECS: u64 = 900;
/// Default wall cap for one check command (`Assertion::Command`).
pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 120;
/// File name that marks a directory as a task. Fixture content may hold any
/// other `.toml` file, so extension alone cannot identify a manifest.
pub const MANIFEST_NAME: &str = "task.toml";
/// Tail of command output kept in a report line, per stream.
pub const OUTPUT_TAIL_CHARS: usize = 2000;

/// One benchmark task: a prompt, the workspace it starts from, and the
/// assertions that decide whether the work landed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    /// Stable task identifier; also the manifest's file stem.
    pub id: String,
    /// Prompt handed to the agent, verbatim.
    pub prompt: String,
    /// Fixture directory copied into the work root before the prompt runs.
    /// Resolved relative to the manifest's own directory.
    #[serde(default)]
    pub fixture: Option<PathBuf>,
    /// Extra directories copied in the same work root, applied in order
    /// after `fixture`.
    #[serde(default)]
    pub overlays: Vec<PathBuf>,
    /// Wall cap for the agent step.
    #[serde(default = "default_agent_timeout")]
    pub agent_timeout: u64,
    /// What must hold once the agent has run. An empty list scores as a
    /// failure: a task with no assertions judges nothing.
    pub assertion: Vec<Assertion>,
    /// Free-form labels for filtering (`"rust"`, `"safety"`, …).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Directory the manifest lives in; every relative path in it resolves
    /// against this. Filled in by the loader, never written in the file.
    #[serde(skip, default = "unknown_base")]
    pub base: PathBuf,
}

fn default_agent_timeout() -> u64 {
    DEFAULT_AGENT_TIMEOUT_SECS
}

fn unknown_base() -> PathBuf {
    PathBuf::from(".")
}

impl TaskSpec {
    /// Agent-step wall cap.
    pub fn agent_timeout_dur(&self) -> Duration {
        Duration::from_secs(self.agent_timeout.max(1))
    }

    /// Fixture and overlay directories, resolved against the manifest's home.
    pub fn sources(&self) -> Vec<PathBuf> {
        self.fixture
            .iter()
            .chain(&self.overlays)
            .map(|source| self.base.join(source))
            .collect()
    }
}

/// A single post-condition on the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Assertion {
    /// Run a program by argv (no shell, so the same manifest works on
    /// every platform) and require its exit code.
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        /// Directory to run in, relative to the work root (default: root).
        #[serde(default)]
        cwd: Option<PathBuf>,
        /// Required exit code (default 0).
        #[serde(default)]
        expect_exit: Option<i32>,
        /// Wall cap for this command.
        #[serde(default = "default_check_timeout")]
        timeout_secs: u64,
    },
    /// File must exist and match `content` (newline-normalized).
    FileEquals { path: PathBuf, content: String },
    /// File must exist and contain `text` (newline-normalized).
    FileContains { path: PathBuf, text: String },
    /// File must exist and must not contain `text`.
    FileNotContains { path: PathBuf, text: String },
    /// Path must exist (file or directory).
    FileExists { path: PathBuf },
    /// Path must not exist.
    FileAbsent { path: PathBuf },
    /// Path must hold exactly the bytes it held before the agent ran.
    FileUnchanged { path: PathBuf },
}

fn default_check_timeout() -> u64 {
    DEFAULT_CHECK_TIMEOUT_SECS
}

impl Assertion {
    /// Short, stable description of what is being checked.
    pub fn label(&self) -> String {
        match self {
            Self::Command { program, args, .. } => {
                let mut label = program.clone();
                for arg in args {
                    label.push(' ');
                    label.push_str(arg);
                }
                label
            }
            Self::FileEquals { path, .. } => format!("file_equals {}", display(path)),
            Self::FileContains { path, .. } => format!("file_contains {}", display(path)),
            Self::FileNotContains { path, .. } => format!("file_not_contains {}", display(path)),
            Self::FileExists { path } => format!("file_exists {}", display(path)),
            Self::FileAbsent { path } => format!("file_absent {}", display(path)),
            Self::FileUnchanged { path } => format!("file_unchanged {}", display(path)),
        }
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A command outcome as observed by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Process exit code; `None` when the process never produced one
    /// (launch failed or the wall cap killed it).
    pub exit_code: Option<i32>,
    /// Why there is no exit code, or how the run was capped.
    pub note: String,
    /// Trailing stdout, decoded lossily.
    pub stdout: String,
    /// Trailing stderr, decoded lossily.
    pub stderr: String,
}

/// The world an assertion is judged against. Implementations read the
/// work root the agent just modified and run check commands in it.
pub trait World {
    /// Run `program args…` under `cwd` (work-root relative), capped.
    fn run(&mut self, program: &str, args: &[String], cwd: &Path, cap: Duration) -> CommandOutput;
    /// Current bytes at a work-root-relative path, `None` if unreadable.
    fn read(&self, path: &Path) -> Option<Vec<u8>>;
    /// Whether a work-root-relative path exists.
    fn exists(&self, path: &Path) -> bool;
    /// Bytes the same path held before the agent ran. `None` means it did
    /// not exist (or was unreadable) at setup time.
    fn baseline(&self, path: &Path) -> Option<Vec<u8>>;
}

/// One assertion's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertionResult {
    /// [`Assertion::label`].
    pub label: String,
    /// Whether the workspace satisfies it.
    pub passed: bool,
    /// Why not, or a short note about the run.
    pub detail: String,
}

/// What the agent step reported before scoring began.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRecord {
    /// The step ran and ended cleanly. A step that never started, was
    /// capped, or exited non-zero is not `ok`: a task whose agent crashed
    /// must not pass on files an earlier step happened to leave behind.
    pub ok: bool,
    /// Why the step was not `ok`, when it was not.
    pub reason: String,
    /// Rounds the loop reported, when the driver exposed them.
    pub tool_calls: u32,
    /// Stop reason reported by the loop (`"completed"`, …).
    pub stop_reason: Option<String>,
    /// Model the step ran against, for report attribution.
    pub model: String,
}

impl AgentRecord {
    /// A step that never started or ended badly.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
            tool_calls: 0,
            stop_reason: None,
            model: String::new(),
        }
    }
}

/// One task's full verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskScore {
    /// Task id.
    pub id: String,
    /// Agent-step outcome.
    pub agent: AgentRecord,
    /// Assertion verdicts in manifest order.
    pub results: Vec<AssertionResult>,
}

impl TaskScore {
    /// A task passes when its agent step ran cleanly and every assertion holds.
    pub fn passed(&self) -> bool {
        self.agent.ok && self.results.iter().all(|r| r.passed) && !self.results.is_empty()
    }
}

/// Run every assertion of `task` against `world`.
pub fn score_task(task: &TaskSpec, world: &mut dyn World) -> Vec<AssertionResult> {
    task.assertion
        .iter()
        .map(|assertion| score_assertion(assertion, world))
        .collect()
}

/// Judge one assertion.
pub fn score_assertion(assertion: &Assertion, world: &mut dyn World) -> AssertionResult {
    let label = assertion.label();
    let (passed, detail) = match assertion {
        Assertion::Command {
            program,
            args,
            cwd,
            expect_exit,
            timeout_secs,
        } => {
            let want = expect_exit.unwrap_or(0);
            let output = world.run(
                program,
                args,
                &cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
                Duration::from_secs((*timeout_secs).max(1)),
            );
            let mut tail = String::new();
            tail.push_str(tail_of(&output.stdout).as_str());
            if !output.stderr.is_empty() {
                if !tail.is_empty() {
                    tail.push('\n');
                }
                tail.push_str(&tail_of(&output.stderr));
            }
            let detail = match output.exit_code {
                None => format!("no exit code: {}{}", output.note, suffix(&tail)),
                Some(code) if code == want => {
                    if output.note.is_empty() {
                        format!("exit {code}")
                    } else {
                        format!("exit {code} ({})", output.note)
                    }
                }
                Some(code) => format!("exit {code}, wanted {want}{}", suffix(&tail)),
            };
            (output.exit_code == Some(want), detail)
        }
        Assertion::FileEquals { path, content } => match world.read(path) {
            None => (false, "missing".to_string()),
            Some(bytes) => {
                let got = normalize(&bytes);
                let want = normalize(content.as_bytes());
                if got == want {
                    (true, format!("{} bytes", got.len()))
                } else {
                    (
                        false,
                        format!("content differs: {want:?} != {:?}", tail_of(&got)),
                    )
                }
            }
        },
        Assertion::FileContains { path, text } => match world.read(path) {
            None => (false, "missing".to_string()),
            Some(bytes) => {
                let got = normalize(&bytes);
                let want = normalize(text.as_bytes());
                (
                    got.contains(&want),
                    if got.contains(&want) {
                        "found".to_string()
                    } else {
                        format!("{want:?} not found")
                    },
                )
            }
        },
        Assertion::FileNotContains { path, text } => match world.read(path) {
            None => (false, "missing".to_string()),
            Some(bytes) => {
                let got = normalize(&bytes);
                let want = normalize(text.as_bytes());
                (
                    !got.contains(&want),
                    if got.contains(&want) {
                        format!("{want:?} still present")
                    } else {
                        "absent".to_string()
                    },
                )
            }
        },
        Assertion::FileExists { path } => (
            world.exists(path),
            if world.exists(path) {
                "present".to_string()
            } else {
                "missing".to_string()
            },
        ),
        Assertion::FileAbsent { path } => (
            !world.exists(path),
            if world.exists(path) {
                "still present".to_string()
            } else {
                "absent".to_string()
            },
        ),
        Assertion::FileUnchanged { path } => {
            let before = world.baseline(path);
            let after = world.read(path);
            match (before, after) {
                (None, None) => (false, "missing before and after".to_string()),
                (Some(_), None) => (false, "deleted".to_string()),
                (None, Some(_)) => (false, "created".to_string()),
                (Some(b), Some(a)) => (
                    b == a,
                    if b == a {
                        "unchanged".to_string()
                    } else {
                        format!("modified ({} -> {} bytes)", b.len(), a.len())
                    },
                ),
            }
        }
    };
    AssertionResult {
        label,
        passed,
        detail,
    }
}

fn suffix(tail: &str) -> String {
    if tail.trim().is_empty() {
        String::new()
    } else {
        format!(" | {}", tail.trim())
    }
}

/// Decode and collapse newlines so `\r\n` and `\n` compare equal: fixtures
/// reach the work root through git checkout, which may convert line endings
/// per platform, while manifests and agent writes use `\n`.
fn normalize(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.replace("\r\n", "\n")
}

/// Keep the trailing `OUTPUT_TAIL_CHARS` characters of a stream.
pub fn tail_of(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= OUTPUT_TAIL_CHARS {
        return text.to_string();
    }
    let cut = chars.len() - OUTPUT_TAIL_CHARS;
    let mut out = String::from("…");
    out.extend(&chars[cut..]);
    out
}

/// Why a manifest could not be loaded or is unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLoadError {
    /// Path that failed.
    pub path: PathBuf,
    /// Human-readable cause.
    pub reason: String,
}

impl std::fmt::Display for TaskLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", display(&self.path), self.reason)
    }
}

impl std::error::Error for TaskLoadError {}

/// Parse one manifest. `base` resolves the manifest's relative paths.
pub fn parse_task(text: &str, base: &Path, path: &Path) -> Result<TaskSpec, TaskLoadError> {
    let mut spec: TaskSpec = toml::from_str(text).map_err(|e| TaskLoadError {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    spec.base = base.to_path_buf();
    validate_task(&spec, path)?;
    Ok(spec)
}

fn validate_task(spec: &TaskSpec, path: &Path) -> Result<(), TaskLoadError> {
    let fail = |reason: String| TaskLoadError {
        path: path.to_path_buf(),
        reason,
    };
    if spec.id.trim().is_empty() {
        return Err(fail("id must not be empty".to_string()));
    }
    if spec.prompt.trim().is_empty() {
        return Err(fail("prompt must not be empty".to_string()));
    }
    if spec.assertion.is_empty() {
        return Err(fail("at least one assertion is required".to_string()));
    }
    for source in spec.fixture.iter().chain(&spec.overlays) {
        if source.is_absolute() {
            return Err(fail(format!(
                "fixture path must be relative: {}",
                display(source)
            )));
        }
    }
    for resolved in spec.sources() {
        if !resolved.is_dir() {
            return Err(fail(format!(
                "fixture directory not found: {}",
                display(&resolved)
            )));
        }
    }
    Ok(())
}

/// Load every task manifest under a directory, sorted by id.
///
/// A manifest is a file named [`MANIFEST_NAME`], at any depth — the fixture
/// it points at may contain other `.toml` files (`Cargo.toml`, app config),
/// and those are workspace content, not tasks.
///
/// A bad manifest is reported and skipped rather than aborting the suite:
/// one typo must not hide the other tasks' numbers.
pub fn load_tasks(dir: &Path) -> (Vec<TaskSpec>, Vec<TaskLoadError>) {
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    let mut files = Vec::new();
    collect_manifests(dir, &mut files, &mut errors);
    files.sort();
    for path in files {
        let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                errors.push(TaskLoadError {
                    path,
                    reason: format!("cannot read: {e}"),
                });
                continue;
            }
        };
        match parse_task(&text, &base, &path) {
            Ok(spec) => specs.push(spec),
            Err(e) => errors.push(e),
        }
    }
    specs.sort_by(|a, b| a.id.cmp(&b.id));
    (specs, errors)
}

/// Gather manifests named [`MANIFEST_NAME`] under `dir` at any depth,
/// pushing an unreadable directory into `errors` instead of pretending it
/// held nothing.
fn collect_manifests(dir: &Path, files: &mut Vec<PathBuf>, errors: &mut Vec<TaskLoadError>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push(TaskLoadError {
                path: dir.to_path_buf(),
                reason: format!("cannot read directory: {e}"),
            });
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_manifests(&path, files, errors);
        } else if path.file_name().is_some_and(|name| name == MANIFEST_NAME) {
            files.push(path);
        }
    }
}

/// Every task in a suite.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SuiteReport {
    /// Scores in task-id order.
    pub tasks: Vec<TaskScore>,
    /// Manifests that could not be loaded, rendered alongside the totals so
    /// a silent gap never looks like a clean run.
    pub load_errors: Vec<String>,
    /// Model the suite ran against.
    pub model: String,
    /// Wall time of the whole suite.
    pub elapsed_secs: u64,
}

impl SuiteReport {
    /// Tasks with every assertion holding.
    pub fn passed(&self) -> usize {
        self.tasks.iter().filter(|t| t.passed()).count()
    }

    /// Fraction of passing tasks; 0.0 for an empty suite, so a suite that
    /// loaded nothing never reports a perfect score.
    pub fn pass_rate(&self) -> f64 {
        if self.tasks.is_empty() {
            return 0.0;
        }
        self.passed() as f64 / self.tasks.len() as f64
    }

    /// Per-task lines with the failing (or all) assertion details.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for score in &self.tasks {
            let verdict = if score.passed() { "PASS" } else { "FAIL" };
            let suffix = match &score.agent.stop_reason {
                Some(reason) => format!(" ({reason})"),
                None => String::new(),
            };
            out.push_str(&format!(
                "{verdict}  {:<28} {} rounds{suffix}\n",
                score.id, score.agent.tool_calls
            ));
            if !score.agent.ok && !score.agent.reason.is_empty() {
                out.push_str(&format!("      agent: {}\n", score.agent.reason));
            }
            for result in score.results.iter().filter(|r| !r.passed) {
                out.push_str(&format!("      {}: {}\n", result.label, result.detail));
            }
        }
        for error in &self.load_errors {
            out.push_str(&format!("BAD   {error}\n"));
        }
        out.push_str(&format!(
            "\npass rate: {}/{} ({:.1}%) in {}s\n",
            self.passed(),
            self.tasks.len(),
            self.pass_rate() * 100.0,
            self.elapsed_secs
        ));
        if !self.model.is_empty() {
            out.push_str(&format!("model: {}\n", self.model));
        }
        out
    }

    /// Machine-readable report for nightly tracking.
    pub fn render_json(&self) -> serde_json::Value {
        let tasks: Vec<serde_json::Value> = self
            .tasks
            .iter()
            .map(|score| {
                let assertions: Vec<serde_json::Value> = score
                    .results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "label": r.label,
                            "passed": r.passed,
                            "detail": r.detail,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "id": score.id,
                    "passed": score.passed(),
                    "agent": {
                        "ok": score.agent.ok,
                        "reason": score.agent.reason,
                        "tool_calls": score.agent.tool_calls,
                        "stop_reason": score.agent.stop_reason,
                        "model": score.agent.model,
                    },
                    "assertions": assertions,
                })
            })
            .collect();
        serde_json::json!({
            "model": self.model,
            "elapsed_secs": self.elapsed_secs,
            "passed": self.passed(),
            "total": self.tasks.len(),
            "pass_rate": self.pass_rate(),
            "load_errors": self.load_errors,
            "tasks": tasks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory workspace: files plus canned command exits.
    struct FakeWorld {
        files: BTreeMap<String, Vec<u8>>,
        baseline: BTreeMap<String, Vec<u8>>,
        runs: Vec<(String, Vec<String>, PathBuf)>,
        exit: Option<i32>,
    }

    impl FakeWorld {
        fn new(files: &[(&str, &str)]) -> Self {
            Self {
                files: files
                    .iter()
                    .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
                    .collect(),
                baseline: BTreeMap::new(),
                runs: Vec::new(),
                exit: Some(0),
            }
        }

        fn get(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.get(&display(path)).cloned()
        }
    }

    impl World for FakeWorld {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
            cwd: &Path,
            _cap: Duration,
        ) -> CommandOutput {
            self.runs
                .push((program.to_string(), args.to_vec(), cwd.to_path_buf()));
            CommandOutput {
                exit_code: self.exit,
                note: String::new(),
                stdout: "out".to_string(),
                stderr: String::new(),
            }
        }
        fn read(&self, path: &Path) -> Option<Vec<u8>> {
            self.get(path)
        }
        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(&display(path))
        }
        fn baseline(&self, path: &Path) -> Option<Vec<u8>> {
            self.baseline.get(&display(path)).cloned()
        }
    }

    fn task(assertion: Vec<Assertion>) -> TaskSpec {
        TaskSpec {
            id: "t".to_string(),
            prompt: "do it".to_string(),
            fixture: None,
            overlays: Vec::new(),
            agent_timeout: DEFAULT_AGENT_TIMEOUT_SECS,
            assertion,
            tags: Vec::new(),
            base: PathBuf::from("."),
        }
    }

    /// An agent step that ran to completion (the scoring tests focus on
    /// assertions, so they need a record that does not itself fail a task).
    fn ran() -> AgentRecord {
        AgentRecord {
            ok: true,
            reason: String::new(),
            tool_calls: 3,
            stop_reason: Some("completed".to_string()),
            model: "test-model".to_string(),
        }
    }

    #[test]
    fn text_assertions_ignore_line_ending_style() {
        let mut world = FakeWorld::new(&[("a.txt", "one\r\ntwo\r\n")]);
        let eq = score_assertion(
            &Assertion::FileEquals {
                path: "a.txt".into(),
                content: "one\ntwo\n".to_string(),
            },
            &mut world,
        );
        assert!(eq.passed, "{}", eq.detail);
        let has = score_assertion(
            &Assertion::FileContains {
                path: "a.txt".into(),
                text: "two".to_string(),
            },
            &mut world,
        );
        assert!(has.passed, "{}", has.detail);
    }

    #[test]
    fn missing_files_fail_every_read_assertion() {
        let mut world = FakeWorld::new(&[]);
        for assertion in [
            Assertion::FileEquals {
                path: "x".into(),
                content: String::new(),
            },
            Assertion::FileContains {
                path: "x".into(),
                text: "a".to_string(),
            },
            Assertion::FileNotContains {
                path: "x".into(),
                text: "a".to_string(),
            },
        ] {
            let r = score_assertion(&assertion, &mut world);
            assert!(!r.passed, "{} must fail when the file is gone", r.label);
            assert_eq!(r.detail, "missing");
        }
    }

    #[test]
    fn unchanged_compares_bytes_not_text() {
        let mut world = FakeWorld::new(&[("a.txt", "same\r\n")]);
        world
            .baseline
            .insert("a.txt".to_string(), b"same\n".to_vec());
        let r = score_assertion(
            &Assertion::FileUnchanged {
                path: "a.txt".into(),
            },
            &mut world,
        );
        assert!(!r.passed, "a rewritten file must fail");
        assert!(r.detail.starts_with("modified"), "{}", r.detail);
    }

    #[test]
    fn command_assertion_wants_the_configured_exit_and_runs_argv() {
        let mut world = FakeWorld::new(&[]);
        world.exit = Some(101);
        let r = score_assertion(
            &Assertion::Command {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
                cwd: Some("sub".into()),
                expect_exit: Some(101),
                timeout_secs: DEFAULT_CHECK_TIMEOUT_SECS,
            },
            &mut world,
        );
        assert!(r.passed, "{}", r.detail);
        assert_eq!(
            world.runs,
            vec![(
                "cargo".to_string(),
                vec!["test".to_string()],
                PathBuf::from("sub")
            )]
        );
    }

    #[test]
    fn a_command_that_never_ran_is_a_failure_not_a_zero() {
        let mut world = FakeWorld::new(&[]);
        world.exit = None;
        let r = score_assertion(
            &Assertion::Command {
                program: "cargo".to_string(),
                args: vec![],
                cwd: None,
                expect_exit: None,
                timeout_secs: 1,
            },
            &mut world,
        );
        assert!(!r.passed);
        assert!(r.detail.starts_with("no exit code"), "{}", r.detail);
    }

    #[test]
    fn score_task_requires_all_assertions_and_a_ran_agent() {
        let spec = task(vec![
            Assertion::FileExists {
                path: "a.txt".into(),
            },
            Assertion::FileAbsent {
                path: "a.txt".into(),
            },
        ]);
        let mut world = FakeWorld::new(&[("a.txt", "x")]);
        let score = TaskScore {
            id: spec.id.clone(),
            agent: ran(),
            results: score_task(&spec, &mut world),
        };
        assert!(!score.passed(), "a failing assertion fails the task");
        assert_eq!(score.results.len(), 2);
        assert!(score.results[0].passed, "a.txt does exist");
        assert!(!score.results[1].passed, "a.txt is not absent");
    }

    #[test]
    fn an_unrun_agent_fails_the_task_even_when_files_match() {
        let spec = task(vec![Assertion::FileExists {
            path: "a.txt".into(),
        }]);
        let mut world = FakeWorld::new(&[("a.txt", "x")]);
        let score = TaskScore {
            id: "a".to_string(),
            agent: AgentRecord::failed("launch failed"),
            results: score_task(&spec, &mut world),
        };
        assert!(!score.passed());
    }

    #[test]
    fn suite_rate_counts_only_fully_passing_tasks() {
        let passing = TaskScore {
            id: "pass".to_string(),
            agent: ran(),
            results: vec![AssertionResult {
                label: "l".to_string(),
                passed: true,
                detail: String::new(),
            }],
        };
        let failing = TaskScore {
            id: "fail".to_string(),
            agent: ran(),
            results: vec![AssertionResult {
                label: "l".to_string(),
                passed: false,
                detail: "nope".to_string(),
            }],
        };
        let report = SuiteReport {
            tasks: vec![passing, failing],
            ..SuiteReport::default()
        };
        assert_eq!(report.passed(), 1);
        assert!((report.pass_rate() - 0.5).abs() < f64::EPSILON);
        let text = report.render_text();
        assert!(text.contains("PASS  pass"), "{text}");
        assert!(text.contains("3 rounds (completed)"), "{text}");
        assert!(text.contains("l: nope"), "{text}");
        assert!(text.contains("1/2 (50.0%)"), "{text}");
        let json = report.render_json();
        assert_eq!(json["passed"], 1);
        assert_eq!(json["total"], 2);
        assert_eq!(json["tasks"][1]["assertions"][0]["detail"], "nope");
    }

    #[test]
    fn an_empty_suite_scores_zero_not_one() {
        let report = SuiteReport::default();
        assert_eq!(report.pass_rate(), 0.0);
        assert!(report.render_text().contains("0/0 (0.0%)"));
    }

    #[test]
    fn a_manifest_loads_with_defaults_when_optional_fields_are_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("fixtures").join("write-hello")).unwrap();
        let text = r#"
id = "write-hello"
prompt = "write hello.txt containing `hi`"
fixture = "fixtures/write-hello"
tags = ["basic"]

[[assertion]]
kind = "file_equals"
path = "hello.txt"
content = "hi\n"

[[assertion]]
kind = "command"
program = "cargo"
args = ["test", "--offline"]
cwd = "app"
"#;
        let spec = parse_task(text, dir.path(), Path::new("write-hello.toml")).unwrap();
        assert_eq!(
            spec.fixture,
            Some(PathBuf::from("fixtures").join("write-hello"))
        );
        assert_eq!(spec.agent_timeout, DEFAULT_AGENT_TIMEOUT_SECS);
        assert_eq!(spec.assertion.len(), 2);
        assert_eq!(spec.tags, vec!["basic".to_string()]);
        match &spec.assertion[1] {
            Assertion::Command {
                program,
                args,
                cwd,
                expect_exit,
                timeout_secs,
            } => {
                assert_eq!(program, "cargo");
                assert_eq!(args, &["test".to_string(), "--offline".to_string()]);
                assert_eq!(cwd.as_deref(), Some(Path::new("app")));
                assert_eq!(*expect_exit, None, "default exit expectation is 0");
                assert_eq!(*timeout_secs, DEFAULT_CHECK_TIMEOUT_SECS);
            }
            other => panic!("expected a command assertion, got {other:?}"),
        }
    }

    #[test]
    fn a_manifest_naming_a_missing_fixture_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let text = "id = \"t\"\nprompt = \"p\"\nfixture = \"fixtures/nope\"\n\n[[assertion]]\nkind = \"file_exists\"\npath = \"a\"\n";
        let err = parse_task(text, dir.path(), Path::new("t.toml")).unwrap_err();
        assert!(err.reason.contains("fixture directory not found"), "{err}");
    }

    #[test]
    fn bad_manifests_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        // One folder per task; the manifest is always `task.toml`.
        for (id, text) in [
            (
                "good",
                "id = \"good\"\nprompt = \"p\"\n\n[[assertion]]\nkind = \"file_exists\"\npath = \"a\"\n",
            ),
            ("bad", "id = \nnot-toml = ["),
            ("empty", "id = \"empty\"\nprompt = \"p\"\nassertion = []\n"),
        ] {
            let folder = dir.path().join(id);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(MANIFEST_NAME), text).unwrap();
        }
        // Fixture config is workspace content, never a task, even at top level.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "ignored").unwrap();
        let (specs, errors) = load_tasks(dir.path());
        assert_eq!(specs.len(), 1, "only the good manifest loads");
        assert_eq!(specs[0].id, "good");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().all(|e| e.reason.len() > 3));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let base = tempfile::tempdir().unwrap();
        let bad_field = "id = \"x\"\nprompt = \"p\"\nnope = 1\nassertion = []\n";
        assert!(parse_task(bad_field, base.path(), Path::new("a.toml")).is_err());
    }

    #[test]
    fn an_absolute_fixture_is_rejected() {
        let base = tempfile::tempdir().unwrap();
        // Forward slashes: a TOML basic string would read `\` as an escape,
        // and every platform's path parser accepts `/`.
        let absolute = format!(
            "id = \"x\"\nprompt = \"p\"\nfixture = \"{}/x\"\n\n[[assertion]]\nkind = \"file_exists\"\npath = \"a\"\n",
            display(base.path())
        );
        let err = parse_task(&absolute, base.path(), Path::new("a.toml")).unwrap_err();
        assert!(err.reason.contains("must be relative"), "{err}");
    }
}
