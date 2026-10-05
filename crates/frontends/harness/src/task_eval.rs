/*
 * @file TaskEvalRunner
 * @description Runs task-level benchmarks: fixture, agent step, checks.
 *
 * Responsibilities:
 * - Prepare one isolated work root per task from its fixture and overlays.
 * - Drive the agent step through this binary's own `exec` surface, behind
 *   the `AgentStep` seam so the pipeline is testable without a model.
 * - Implement the eval `World` with real processes: argv (no shell), wall
 *   caps, output tails, and work-root-contained file reads.
 * - Report the suite as text or JSON.
 *
 * This module must not depend on: scoring rules (operations-eval owns them),
 * provider credentials, or any interactive frontend.
 */

//! Task-level eval execution. The judging lives in `operations-eval`; this
//! file only produces the observations it consumes.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use operations_eval::{
    AgentRecord, Assertion, CommandOutput, SuiteReport, TaskScore, TaskSpec, World, load_tasks,
    score_task, tail_of,
};

/// How far a step ran before it was cut off.
#[derive(Debug)]
pub struct RawRun {
    /// Exit code, `None` when the process never produced one.
    pub exit_code: Option<i32>,
    /// Why there is no exit code, or a note about the cap.
    pub note: String,
    /// Full stdout bytes.
    pub stdout: Vec<u8>,
    /// Full stderr bytes.
    pub stderr: Vec<u8>,
}

/// Spawn `program args…` in `cwd` and wait at most `cap`.
///
/// Never goes through a shell: the same argv must mean the same thing on
/// Windows and POSIX, and a shell would put quoting between the manifest and
/// the check. Output drains in reader threads so a chatty check cannot
/// deadlock on a full pipe while we poll for exit.
fn spawn_capped(program: &OsStr, args: &[String], cwd: &Path, cap: Duration) -> RawRun {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own group so a timeout can signal the whole tree. Without it a
    // grandchild holding the pipes blocks the reader joins forever, and
    // a group signal would hit this process.
    infrastructure_base::lead_process_group(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return RawRun {
                exit_code: None,
                note: format!("cannot launch {}: {e}", program.to_string_lossy()),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
        }
    };
    let stdout = child
        .stdout
        .take()
        .map(spawn_reader)
        .unwrap_or_else(empty_reader);
    let stderr = child
        .stderr
        .take()
        .map(spawn_reader)
        .unwrap_or_else(empty_reader);
    let started = Instant::now();
    let mut note = String::new();
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if started.elapsed() >= cap {
                    note = format!("killed after {}s", cap.as_secs());
                    infrastructure_base::kill_tree(child.id());
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                note = format!("wait failed: {e}");
                break None;
            }
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    RawRun {
        exit_code,
        note,
        stdout,
        stderr,
    }
}

/// Per-stream output cap for one check run: beyond it the reader drains
/// and discards (keeping the pipe empty so the child never blocks) and
/// the retained bytes stop growing.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

fn spawn_reader(pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut pipe = pipe;
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < MAX_OUTPUT_BYTES {
                        let keep = n.min(MAX_OUTPUT_BYTES - buf.len());
                        buf.extend_from_slice(&chunk[..keep]);
                    }
                }
                Err(_) => break,
            }
        }
        buf
    })
}

fn empty_reader() -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(Vec::new)
}

/// The world an eval judges: files under one work root, commands run in it.
pub struct FsWorld {
    root: PathBuf,
    baselines: BTreeMap<String, Vec<u8>>,
}

impl FsWorld {
    /// Bind to a prepared work root, with the pre-agent content of every
    /// path a `file_unchanged` assertion names.
    pub fn new(root: PathBuf, baselines: BTreeMap<String, Vec<u8>>) -> Self {
        Self { root, baselines }
    }

    /// Resolve a manifest path inside the work root. Absolute paths and `..`
    /// are refused: a manifest must not reach outside the copy it is judging.
    fn contain(&self, path: &Path) -> Option<PathBuf> {
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            return None;
        }
        Some(self.root.join(path))
    }
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl World for FsWorld {
    fn run(&mut self, program: &str, args: &[String], cwd: &Path, cap: Duration) -> CommandOutput {
        let dir = match self.contain(cwd) {
            Some(dir) => dir,
            None => {
                return CommandOutput {
                    exit_code: None,
                    note: format!("cwd outside the work root: {}", key(cwd)),
                    stdout: String::new(),
                    stderr: String::new(),
                };
            }
        };
        let run = spawn_capped(OsStr::new(program), args, &dir, cap);
        CommandOutput {
            exit_code: run.exit_code,
            note: run.note,
            stdout: tail_of(&String::from_utf8_lossy(&run.stdout)),
            stderr: tail_of(&String::from_utf8_lossy(&run.stderr)),
        }
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        let resolved = self.contain(path)?;
        std::fs::read(resolved).ok()
    }

    fn exists(&self, path: &Path) -> bool {
        self.contain(path)
            .and_then(|resolved| resolved.try_exists().ok())
            == Some(true)
    }

    fn baseline(&self, path: &Path) -> Option<Vec<u8>> {
        self.baselines.get(&key(path)).cloned()
    }
}

/// One agent step over a prepared work root.
pub trait AgentStep {
    /// Run `prompt` with `cwd` as the workspace and report what happened.
    fn run(&self, cwd: &Path, prompt: &str, cap: Duration) -> AgentRecord;
}

/// Drives the agent through `wavecode exec --json` of a real binary.
pub struct ExecAgent {
    /// Path to the `wavecode` executable to invoke.
    pub program: PathBuf,
    /// Global flags forwarded verbatim (config path, model, permission mode).
    pub forward: Vec<String>,
}

impl AgentStep for ExecAgent {
    fn run(&self, cwd: &Path, prompt: &str, cap: Duration) -> AgentRecord {
        let mut args = vec!["exec".to_string(), prompt.to_string(), "--json".to_string()];
        args.extend(self.forward.iter().cloned());
        let run = spawn_capped(self.program.as_os_str(), &args, cwd, cap);
        let (tool_calls, model) = scan_events(&run.stdout);
        let stop_reason = match run.exit_code {
            Some(0) => Some("completed".to_string()),
            Some(130) => Some("interrupted".to_string()),
            Some(_) => Some("failed".to_string()),
            None => None,
        };
        let reason = match run.exit_code {
            Some(0) => String::new(),
            Some(code) => format!(
                "exit {code}: {}",
                tail_of(&String::from_utf8_lossy(&run.stderr))
            ),
            None => run.note.clone(),
        };
        AgentRecord {
            ok: run.exit_code == Some(0),
            reason,
            tool_calls,
            stop_reason,
            model,
        }
    }
}

/// Count tool calls and pick up the model name from a JSONL event stream.
///
/// Malformed lines are ignored rather than fatal: a truncated tail is normal
/// when a step is capped, and the counts stay best-effort.
fn scan_events(jsonl: &[u8]) -> (u32, String) {
    let mut tool_calls = 0;
    let mut model = String::new();
    for line in String::from_utf8_lossy(jsonl).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("tool_call_begin") => tool_calls += 1,
            Some("turn_started") => {
                if let Some(name) = value.get("model").and_then(|m| m.as_str())
                    && !name.is_empty()
                {
                    model = name.to_string();
                }
            }
            _ => {}
        }
    }
    (tool_calls, model)
}

/// Copy `src` into `dst`, merging with anything already there.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
        }
    }
    Ok(())
}

/// Create an empty work root for one task and copy its fixture in.
fn prepare(task: &TaskSpec, work_root: &Path) -> Result<PathBuf, String> {
    let root = work_root.join(&task.id);
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    }
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    for source in task.sources() {
        copy_tree(&source, &root)?;
    }
    Ok(root)
}

/// Snapshot the content `file_unchanged` assertions will compare against.
fn snapshot_baselines(task: &TaskSpec, root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    for assertion in &task.assertion {
        if let Assertion::FileUnchanged { path } = assertion
            && let Ok(bytes) = std::fs::read(root.join(path))
        {
            map.insert(key(path), bytes);
        }
    }
    map
}

/// Run one task end to end: prepare, agent step, judge.
pub fn run_task(task: &TaskSpec, work_root: &Path, agent: &dyn AgentStep) -> TaskScore {
    let root = match prepare(task, work_root) {
        Ok(root) => root,
        Err(e) => {
            return TaskScore {
                id: task.id.clone(),
                agent: AgentRecord::failed(format!("setup failed: {e}")),
                results: Vec::new(),
            };
        }
    };
    let baselines = snapshot_baselines(task, &root);
    let record = agent.run(&root, &task.prompt, task.agent_timeout_dur());
    let mut world = FsWorld::new(root, baselines);
    let results = score_task(task, &mut world);
    TaskScore {
        id: task.id.clone(),
        agent: record,
        results,
    }
}

/// Load the suite and run every task that survives the filters.
pub fn run_suite(
    tasks_dir: &Path,
    work_root: &Path,
    filter: Option<&str>,
    tag: Option<&str>,
    agent: &dyn AgentStep,
) -> (SuiteReport, Vec<PathBuf>) {
    let (specs, errors) = load_tasks(tasks_dir);
    let selected: Vec<TaskSpec> = specs
        .into_iter()
        .filter(|t| filter.is_none_or(|f| t.id.contains(f)))
        .filter(|t| tag.is_none_or(|g| t.tags.iter().any(|x| x == g)))
        .collect();
    let started = Instant::now();
    let mut scores = Vec::new();
    let mut roots = Vec::new();
    for task in &selected {
        let score = run_task(task, work_root, agent);
        roots.push(work_root.join(&task.id));
        scores.push(score);
    }
    let model = scores
        .iter()
        .find_map(|s| {
            if s.agent.model.is_empty() {
                None
            } else {
                Some(s.agent.model.clone())
            }
        })
        .unwrap_or_default();
    let report = SuiteReport {
        tasks: scores,
        load_errors: errors.iter().map(|e| e.to_string()).collect(),
        model,
        elapsed_secs: started.elapsed().as_secs(),
    };
    (report, roots)
}

/// Everything the `eval tasks` surface needs, already resolved from flags.
pub struct TasksRequest {
    /// Directory of task manifests.
    pub tasks_dir: PathBuf,
    /// Keep only tasks whose id contains this substring.
    pub filter: Option<String>,
    /// Keep only tasks carrying this tag.
    pub tag: Option<String>,
    /// Where work roots are created; a temp directory when unset.
    pub work_root: Option<PathBuf>,
    /// Binary the agent step invokes; this executable when unset.
    pub agent_bin: Option<PathBuf>,
    /// Global flags forwarded to each `exec` child (config, model, mode).
    pub forward: Vec<String>,
    /// Print JSON instead of a table.
    pub json: bool,
    /// Also write the JSON report here.
    pub out: Option<PathBuf>,
}

/// `eval tasks`: run the suite and report. True when everything passed.
///
/// Work roots are left on disk and their parent is printed, so a failing
/// task can be inspected as-is instead of being reproduced.
pub fn run_tasks(request: TasksRequest) -> anyhow::Result<bool> {
    if !request.tasks_dir.is_dir() {
        anyhow::bail!(
            "no task directory at {} (run this from the repository root, or pass --dir)",
            request.tasks_dir.display()
        );
    }
    let temp = request.work_root.clone().unwrap_or_else(|| {
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        std::env::temp_dir().join(format!("wavecode-eval-{}", std::process::id()))
    });
    std::fs::create_dir_all(&temp)?;
    let program = match request.agent_bin.clone() {
        Some(program) => program,
        // nosemgrep: rust.lang.security.current-exe.current-exe
        None => std::env::current_exe()?,
    };
    let agent = ExecAgent {
        program,
        forward: request.forward.clone(),
    };
    let (report, roots) = run_suite(
        &request.tasks_dir,
        &temp,
        request.filter.as_deref(),
        request.tag.as_deref(),
        &agent,
    );
    if request.json {
        println!("{}", report.render_json());
    } else {
        print!("{}", report.render_text());
        println!("work roots: {}", temp.display());
    }
    if let Some(out) = &request.out {
        std::fs::write(
            out,
            serde_json::to_string_pretty(&report.render_json())?.into_bytes(),
        )?;
        eprintln!("[eval] wrote {out:?} ({} roots)", roots.len());
    }
    Ok(report.tasks.iter().all(|s| s.passed())
        && !report.tasks.is_empty()
        && report.load_errors.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Agent step that edits the work root directly — no model involved.
    struct Scripted<F>
    where
        F: Fn(&Path),
    {
        edit: F,
    }

    impl<F: Fn(&Path)> AgentStep for Scripted<F> {
        fn run(&self, cwd: &Path, _prompt: &str, _cap: Duration) -> AgentRecord {
            (self.edit)(cwd);
            AgentRecord {
                ok: true,
                reason: String::new(),
                tool_calls: 2,
                stop_reason: Some("completed".to_string()),
                model: "scripted".to_string(),
            }
        }
    }

    /// Write a manifest plus its fixture under a temp `tasks/` layout that
    /// mirrors `benchmarks/tasks`: one folder per task, fixture beside it.
    fn layout(name: &str, files: &[(&str, &str)], manifest: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        let fixture = tasks.join(name).join("workspace");
        std::fs::create_dir_all(&fixture).unwrap();
        for (path, content) in files {
            let target = fixture.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(target, content).unwrap();
        }
        std::fs::write(tasks.join(name).join("task.toml"), manifest).unwrap();
        (dir, tasks)
    }

    fn spec(base: &Path, id: &str, assertion: Vec<Assertion>) -> TaskSpec {
        TaskSpec {
            id: id.to_string(),
            prompt: "do it".to_string(),
            fixture: Some("workspace".into()),
            overlays: Vec::new(),
            agent_timeout: operations_eval::DEFAULT_AGENT_TIMEOUT_SECS,
            assertion,
            tags: vec!["unit".to_string()],
            base: base.join(id),
        }
    }

    #[test]
    fn a_passing_task_copies_the_fixture_into_a_throwaway_work_root() {
        let (dir, tasks) = layout(
            "write-hello",
            &[("keep.txt", "keep me\n")],
            "unused manifest, the spec is built in code",
        );
        let work = dir.path().join("work");
        let task = spec(
            &tasks,
            "write-hello",
            vec![
                Assertion::FileEquals {
                    path: "hello.txt".into(),
                    content: "hi\n".to_string(),
                },
                Assertion::FileUnchanged {
                    path: "keep.txt".into(),
                },
            ],
        );
        let agent = Scripted {
            edit: |root: &Path| {
                std::fs::write(root.join("hello.txt"), "hi\n").unwrap();
            },
        };
        let score = run_task(&task, &work, &agent);
        assert!(score.passed(), "{:?}", score.results);
        assert_eq!(score.agent.model, "scripted");
        // The fixture really arrived in a copy, leaving the original in place.
        assert!(work.join("write-hello").join("keep.txt").exists());
        assert!(
            tasks
                .join("write-hello")
                .join("workspace")
                .join("keep.txt")
                .exists()
        );
        assert!(
            !work.join("write-hello").join("task.toml").exists(),
            "the manifest is not part of the workspace"
        );
    }

    #[test]
    fn a_wrong_edit_and_a_collateral_edit_both_fail_the_task() {
        let (dir, tasks) = layout("edit", &[("keep.txt", "keep me\n")], "manifest");
        let work = dir.path().join("work");
        let task = spec(
            &tasks,
            "edit",
            vec![
                Assertion::FileEquals {
                    path: "hello.txt".into(),
                    content: "hi\n".to_string(),
                },
                Assertion::FileUnchanged {
                    path: "keep.txt".into(),
                },
            ],
        );
        let agent = Scripted {
            edit: |root: &Path| {
                std::fs::write(root.join("hello.txt"), "ho\n").unwrap();
                std::fs::write(root.join("keep.txt"), "trampled\n").unwrap();
            },
        };
        let score = run_task(&task, &work, &agent);
        assert!(!score.passed());
        assert!(!score.results[0].passed);
        assert!(
            score.results[1].detail.starts_with("modified"),
            "{:?}",
            score.results
        );
    }

    #[test]
    fn setup_failure_scores_the_task_without_running_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        let work = dir.path().join("work");
        let task = spec(
            &tasks,
            "ghost",
            vec![Assertion::FileExists { path: "a".into() }],
        );
        let agent = Scripted {
            edit: |_: &Path| panic!("the agent must not run when setup failed"),
        };
        let score = run_task(&task, &work, &agent);
        assert!(!score.passed());
        assert!(
            score.agent.reason.starts_with("setup failed"),
            "{:?}",
            score.agent
        );
        assert!(score.results.is_empty());
    }

    #[test]
    fn suite_discovers_nested_manifests_and_applies_filters() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        for id in ["alpha", "beta"] {
            // One folder per task: `tasks/<id>/task.toml` + `workspace/`.
            let folder = tasks.join(id);
            std::fs::create_dir_all(folder.join("workspace")).unwrap();
            std::fs::write(
                folder.join("task.toml"),
                format!(
                    "id = \"{id}\"\nprompt = \"write out.txt\"\nfixture = \"workspace\"\ntags = [\"unit\"]\n\n[[assertion]]\nkind = \"file_exists\"\npath = \"out.txt\"\n"
                ),
            )
            .unwrap();
        }
        let work = dir.path().join("work");
        let agent = Scripted {
            edit: |root: &Path| {
                std::fs::write(root.join("out.txt"), "x").unwrap();
            },
        };
        let (report, roots) = run_suite(&tasks, &work, Some("alp"), None, &agent);
        assert_eq!(report.tasks.len(), 1, "the filter kept one task");
        assert!(report.load_errors.is_empty(), "{:?}", report.load_errors);
        assert_eq!(report.passed(), 1);
        assert_eq!(roots.len(), 1);
        assert_eq!(report.model, "scripted");
        let (both, _) = run_suite(&tasks, &work, None, None, &agent);
        assert_eq!(both.tasks.len(), 2, "both nested manifests loaded");
        assert!(both.passed() == 2, "{:?}", both.tasks);
        let (none, _) = run_suite(&tasks, &work, None, Some("none"), &agent);
        assert!(none.tasks.is_empty());
        assert_eq!(none.pass_rate(), 0.0, "an empty suite is not a clean pass");
    }

    #[test]
    fn world_runs_real_argv_and_refuses_paths_outside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "content\n").unwrap();
        let mut world = FsWorld::new(root.to_path_buf(), BTreeMap::new());
        let outside = world.read(Path::new("../../Windows/win.ini"));
        assert!(outside.is_none(), "parent traversal must not read");
        assert!(!world.exists(Path::new("/etc/hostname")));
        assert!(!world.exists(Path::new("..")));
        // `cargo` is on PATH whenever these tests run through cargo itself.
        let output = world.run(
            env!("CARGO"),
            &["--version".to_string()],
            Path::new("."),
            Duration::from_secs(60),
        );
        assert_eq!(output.exit_code, Some(0), "{}", output.note);
        assert!(output.stdout.contains("cargo"), "{}", output.stdout);
        let missing = world.run(
            "definitely-not-a-real-program-xyz",
            &[],
            Path::new("."),
            Duration::from_secs(5),
        );
        assert_eq!(missing.exit_code, None);
        assert!(missing.note.contains("cannot launch"), "{}", missing.note);
    }

    #[test]
    fn event_scan_counts_tool_calls_and_reads_the_model() {
        let jsonl = concat!(
            "{\"id\":1,\"type\":\"turn_started\",\"model\":\"sonnet-test\"}\n",
            "not json at all\n",
            "{\"id\":2,\"type\":\"tool_call_begin\",\"call_id\":\"c\",\"tool\":\"read\",\"args\":{}}\n",
            "{\"id\":3,\"type\":\"tool_call_begin\",\"call_id\":\"d\",\"tool\":\"write\",\"args\":{}}\n",
            "{\"id\":4,\"type\":\"turn_completed\",\"interrupted\":false}\n",
        );
        let (calls, model) = scan_events(jsonl.as_bytes());
        assert_eq!(calls, 2);
        assert_eq!(model, "sonnet-test");
        let (calls, model) = scan_events(b"");
        assert_eq!((calls, model), (0, String::new()));
    }

    #[test]
    fn exec_agent_maps_exit_codes_onto_the_record() {
        // The harness's own test binary stands in for `wavecode`: it exits
        // non-zero immediately for an unknown subcommand, which is exactly
        // the path a failed turn takes.
        let agent = ExecAgent {
            // nosemgrep: rust.lang.security.current-exe.current-exe
            program: std::env::current_exe().unwrap(),
            forward: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let record = agent.run(dir.path(), "impossible-prompt", Duration::from_secs(60));
        assert!(!record.ok);
        assert!(
            record.stop_reason == Some("failed".to_string())
                || record.stop_reason == Some("interrupted".to_string()),
            "a stray test-binary invocation must not look like a completed turn: {:?}",
            record.stop_reason
        );
        assert!(!record.reason.is_empty());
    }

    #[test]
    fn the_cli_handler_reports_a_failing_suite() {
        // End to end through the same entry point `wavecode eval tasks`
        // uses, with no model: `--agent-bin` points the agent step at this
        // test binary, which cannot run a turn, so every task must come back
        // failed and the handler must say so. One task only, to keep the
        // spawned child a single quick process.
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("report.json");
        let ok = run_tasks(TasksRequest {
            tasks_dir: committed_tasks_dir(),
            filter: Some("write-hello".to_string()),
            tag: None,
            work_root: Some(dir.path().join("work")),
            // nosemgrep: rust.lang.security.current-exe.current-exe
            agent_bin: Some(std::env::current_exe().unwrap()),
            forward: Vec::new(),
            json: false,
            out: Some(out.clone()),
        })
        .expect("the handler runs offline");
        assert!(!ok, "an agent that cannot run a turn must not pass");
        assert!(dir.path().join("work").join("write-hello").is_dir());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(report["total"], 1);
        assert_eq!(report["passed"], 0);
        assert_eq!(report["tasks"][0]["id"], "write-hello");
    }

    #[test]
    fn a_missing_tasks_directory_is_an_error_not_an_empty_pass() {
        let err = run_tasks(TasksRequest {
            tasks_dir: PathBuf::from("no-such-suite"),
            filter: None,
            tag: None,
            work_root: None,
            agent_bin: None,
            forward: Vec::new(),
            json: false,
            out: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("no task directory"), "{err}");
    }

    #[test]
    fn copy_tree_merges_and_keeps_nested_layout() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("nested/in.txt"), "x").unwrap();
        let dst = dir.path().join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("pre.txt"), "keep").unwrap();
        copy_tree(&src, &dst).unwrap();
        assert!(dst.join("nested/in.txt").exists());
        assert!(
            dst.join("pre.txt").exists(),
            "existing files survive a merge"
        );
    }

    /// Where the committed suite lives — resolved from this crate's manifest
    /// dir so the test never depends on the caller's working directory.
    fn committed_tasks_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("benchmarks")
            .join("tasks")
    }

    /// The committed suite, loaded the way the CLI loads it.
    fn committed_suite() -> Vec<TaskSpec> {
        let (specs, errors) = load_tasks(&committed_tasks_dir());
        assert!(
            errors.is_empty(),
            "every committed manifest must load: {errors:?}"
        );
        assert!(
            specs.len() >= 8,
            "the committed suite is too small: {}",
            specs.len()
        );
        specs
    }

    #[test]
    fn committed_tasks_are_well_formed() {
        let specs = committed_suite();
        let mut seen = std::collections::BTreeSet::new();
        let mut tiers = std::collections::BTreeSet::new();
        for spec in &specs {
            assert!(
                seen.insert(spec.id.clone()),
                "duplicate task id {}",
                spec.id
            );
            assert_eq!(
                spec.base.file_name().and_then(|s| s.to_str()),
                Some(spec.id.as_str()),
                "{}: the manifest must live in benchmarks/tasks/<id>/",
                spec.id
            );
            assert!(
                !spec.tags.is_empty(),
                "{}: untagged tasks cannot be filtered",
                spec.id
            );
            assert!(
                spec.sources().len() == 1,
                "{}: expected one fixture dir",
                spec.id
            );
            for assertion in &spec.assertion {
                if matches!(assertion, Assertion::Command { .. }) {
                    tiers.insert("command");
                }
                if matches!(assertion, Assertion::FileUnchanged { .. }) {
                    tiers.insert("no-collateral");
                }
            }
            // A fixture that is really a cargo crate must detach itself, or
            // the outer workspace swallows it and its command assertions
            // lie about the build.
            let fixture = &spec.sources()[0];
            let manifest = fixture.join("Cargo.toml");
            let is_crate = manifest.is_file()
                && (fixture.join("src/lib.rs").is_file() || fixture.join("src/main.rs").is_file());
            if is_crate {
                let text = std::fs::read_to_string(&manifest).unwrap();
                assert!(
                    text.contains("[workspace]"),
                    "{}: a cargo fixture needs its own [workspace] table",
                    spec.id
                );
            }
        }
        assert!(
            tiers.contains("command"),
            "no task exercises the test-pass tier"
        );
        assert!(
            tiers.contains("no-collateral"),
            "no task guards against collateral edits"
        );
        assert!(
            specs.iter().any(|s| {
                s.sources().first().is_some_and(|f| {
                    s.assertion
                        .iter()
                        .any(|a| matches!(a, Assertion::Command { .. }))
                        && f.join("Cargo.toml").is_file()
                })
            }),
            "the suite lost its buildable fixtures"
        );
    }

    #[test]
    fn no_committed_task_is_already_solved() {
        // A task whose file assertions hold before the agent touches anything
        // is a free point, so it measures nothing. Command assertions are
        // skipped here: they would run a real build per task, so a task with
        // nothing but a build is counted and left to the live suite.
        let mut build_only = Vec::new();
        for spec in committed_suite() {
            let mut gating = spec.clone();
            gating
                .assertion
                .retain(|a| !matches!(a, Assertion::Command { .. }));
            if gating.assertion.is_empty() {
                build_only.push(spec.id.clone());
                continue;
            }
            let work = tempfile::tempdir().unwrap();
            let root = prepare(&gating, work.path()).unwrap();
            let mut world = FsWorld::new(root, BTreeMap::new());
            let results = score_task(&gating, &mut world);
            assert!(
                results.iter().any(|r| !r.passed),
                "{} passes on its untouched fixture: {:?}",
                spec.id,
                results
                    .iter()
                    .map(|r| (&r.label, r.passed))
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            build_only.len() <= 2,
            "too many tasks can only be judged by a live build: {build_only:?}"
        );
    }

    /// The known-good end state of every committed task, as file writes and
    /// deletes. A task missing from this table is one the file-level oracle
    /// cannot express (its only gate is a live build).
    fn oracle(id: &str) -> Vec<(String, Option<String>)> {
        let write = |path: &str, content: &str| (path.to_string(), Some(content.to_string()));
        let rm = |path: &str| (path.to_string(), None);
        match id {
            "write-hello" => vec![write("hello.txt", "wavecode-smoke-ok")],
            "fix-config-timeout" => vec![write("app.toml", "[client]\nretry = 2\ntimeout = 30\n")],
            "append-changelog" => vec![write(
                "CHANGELOG.md",
                "# Changelog\n\n## Unreleased\n\n- Raised the client timeout to 30 seconds.\n\n## 1.3.0 - 2026-08-14\n\n- Cached tool results per session.\n",
            )],
            "rename-symbol" => vec![
                write(
                    "src/lib.rs",
                    "pub mod report;\n\npub fn load_events(table: &str) -> Vec<String> {\n    query_rows(table)\n}\n\nfn query_rows(table: &str) -> Vec<String> {\n    vec![table.to_string()]\n}\n",
                ),
                write(
                    "src/report.rs",
                    "use crate::load_events;\n\npub fn headline() -> String {\n    format!(\"{} rows\", load_events(\"events\").len())\n}\n",
                ),
            ],
            "sum-data" => vec![write("total.txt", "49")],
            "make-test-pass" => vec![write(
                "src/lib.rs",
                "/// Count the words in `text`, where words are whitespace-separated runs.\npub fn word_count(text: &str) -> usize {\n    text.split_whitespace().count()\n}\n",
            )],
            "prune-temp-files" => vec![rm("build/output.tmp"), rm("build/cache.tmp")],
            "preserve-env-file" => vec![write(
                "src/app.py",
                "import os\n\nTIMEOUT = 30\n\n\ndef fetch(url):\n    return url, TIMEOUT\n",
            )],
            "bump-version" => vec![
                write(
                    "Cargo.toml",
                    "[package]\nname = \"demo\"\nversion = \"1.4.0\"\nedition = \"2021\"\n\n# Detaches this fixture from any enclosing cargo workspace.\n[workspace]\n",
                ),
                write(
                    "docs/install.md",
                    "The current release is 1.4.0. Download `demo-1.4.0.tar.gz`.\n",
                ),
                write("src/lib.rs", "pub const VERSION: &str = \"1.4.0\";\n"),
            ],
            _ => Vec::new(),
        }
    }

    #[test]
    fn every_committed_task_has_a_solution_that_passes() {
        // The other half of the gate: assertions must not only be unsatisfied
        // up front, they must be satisfiable at all. Without this, a typo in
        // an expected string costs a live model run to discover.
        let specs = committed_suite();
        let mut oracle_free = Vec::new();
        for spec in &specs {
            let steps = oracle(&spec.id);
            if steps.is_empty() {
                oracle_free.push(spec.id.clone());
                continue;
            }
            let work = tempfile::tempdir().unwrap();
            let root = prepare(spec, work.path()).unwrap();
            let baselines = snapshot_baselines(spec, &root);
            for (path, content) in &steps {
                let target = root.join(path);
                match content {
                    Some(text) => {
                        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                        std::fs::write(target, text).unwrap();
                    }
                    None => match std::fs::remove_file(&target) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => panic!("{}: cannot remove {path}: {e}", spec.id),
                    },
                }
            }
            // Command assertions still need a real build, so they are judged
            // by the live suite; everything else must hold now.
            let mut gating = spec.clone();
            gating
                .assertion
                .retain(|a| !matches!(a, Assertion::Command { .. }));
            let mut world = FsWorld::new(root, baselines);
            let failed: Vec<_> = score_task(&gating, &mut world)
                .into_iter()
                .filter(|r| !r.passed)
                .map(|r| format!("{}: {}", r.label, r.detail))
                .collect();
            assert!(
                failed.is_empty(),
                "{} is unsolvable as written: {failed:#?}",
                spec.id
            );
        }
        assert!(
            oracle_free.len() <= 2,
            "tasks with no oracle solution: {oracle_free:?}"
        );
    }
    /// The output reader stops retaining bytes at the cap: a child that
    /// keeps producing past [`MAX_OUTPUT_BYTES`] cannot grow the harness
    /// without bound (the pipe itself keeps draining).
    #[test]
    fn output_reader_caps_retained_bytes() {
        struct Burst {
            remaining: usize,
        }
        impl std::io::Read for Burst {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Ok(0);
                }
                let n = buf.len().min(64 * 1024).min(self.remaining);
                buf[..n].fill(b'x');
                self.remaining -= n;
                Ok(n)
            }
        }
        let buf = spawn_reader(Burst {
            remaining: MAX_OUTPUT_BYTES + 4096,
        })
        .join()
        .unwrap();
        assert_eq!(buf.len(), MAX_OUTPUT_BYTES);
    }
}
