/*!
 * @file AgentTaskTool
 * @description Model-invoked subagent delegation (the `task` tool).
 *
 * Responsibilities:
 * - Spawn a child task from a free-form prompt (unlike `skill`, which is
 *   bound to discovered skills, `task` is the generic delegation surface).
 * - Resolve optional named agent definitions from `.wavecode/agents/` and
 *   `.claude/agents/` (first directory wins per name) into tool-surface
 *   restrictions and an identity preamble.
 * - Wait for the child to finish and return its summary inline, so the
 *   model can use the result in the same turn.
 *
 * This module must not depend on: drivers, actors, or sessions. Execution
 * goes through the [`TaskService`] seam only.
 */

//! `task`: free-form subagent delegation with named agent definitions.

use std::path::Path;

use crate::{Result, Tool, ToolCtx, ToolOutput};
use action_tasks::{TaskKind, TaskOutcome, TaskRequest, TaskService, TaskState};

/// How long the tool waits for the child to finish before giving up and
/// reporting a still-running id. Generous by design: a delegation that
/// exceeds it can still be observed via `task_output` / stopped via
/// `task_stop`, so the timeout only bounds the *blocking* wait.
const TASK_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// Poll interval while waiting for the child to finish.
const TASK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// One named agent definition (frontmatter of an agents/*.md file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// Agent name (frontmatter `name`, falling back to the file stem).
    pub name: String,
    /// When to delegate to this agent (frontmatter `description`).
    pub description: String,
    /// Restricted tool surface (frontmatter `tools`); an empty set falls
    /// back to the registry surface minus the child-forbidden spawn tools
    /// (see `ChildSurface`), so no fork can ever re-spawn children.
    pub allowed_tools: Vec<String>,
    /// `true` when the definition marks the agent read-only
    /// (`kind: explore` or `readonly`), selecting the read-only profile.
    pub read_only: bool,
}

/// Discover named agent definitions for `cwd`.
///
/// Scans `.wavecode/agents/*.md` then `.claude/agents/*.md` (interop with
/// the cross-tool convention); the first definition of a name wins, so a
/// repo can override a global one by shadowing. Unparseable files are
/// skipped — discovery is best-effort and must never fail the call.
pub fn discover_agent_defs(cwd: &Path) -> Vec<AgentDef> {
    let mut defs = Vec::new();
    for dir in [
        cwd.join(".wavecode").join("agents"),
        cwd.join(".claude").join("agents"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<std::path::PathBuf> = entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(mut def) = parse_agent_def(&content) else {
                continue;
            };
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if def.name.is_empty() {
                def.name = stem;
            }
            if !def.name.is_empty() && !defs.iter().any(|d: &AgentDef| d.name == def.name) {
                defs.push(def);
            }
        }
    }
    defs
}

/// Parse one agent definition file: `---` frontmatter with `key: value`
/// lines (a `tools:` key may carry a comma-separated value or `- item`
/// list lines), followed by an ignored body. `None` when no frontmatter
/// block exists at all.
fn parse_agent_def(content: &str) -> Option<AgentDef> {
    let mut lines = content.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut name = String::new();
    let mut description = String::new();
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut read_only = false;
    let mut in_tools_list = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            if in_tools_list {
                allowed_tools.push(item.trim().to_string());
            }
            continue;
        }
        let (key, value) = match trimmed.split_once(':') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => continue,
        };
        in_tools_list = false;
        match key.as_str() {
            "name" => name = value.to_string(),
            "description" => description = value.to_string(),
            "tools" => {
                in_tools_list = true;
                allowed_tools = value
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
            }
            "kind" => {
                read_only = matches!(
                    value.to_ascii_lowercase().as_str(),
                    "explore" | "readonly" | "read-only"
                )
            }
            _ => {}
        }
    }
    Some(AgentDef {
        name,
        description,
        allowed_tools,
        read_only,
    })
}

/// `task`: delegate work to a child agent and return its result inline.
///
/// The generic delegation surface: unlike `skill` (bound to discovered
/// skills), the model supplies a free-form prompt. An optional
/// `subagent_type` names a discovered agent definition (its tool surface
/// and identity preamble apply) or the built-in `explore` profile
/// (read-only). The call blocks until the child finishes (bounded by the
/// wait timeout) so the summary can be used in the same turn.
#[derive(Clone)]
pub struct TaskTool {
    tasks: Arc<dyn TaskService>,
}

use std::sync::Arc;

impl TaskTool {
    /// Wrap the child task service.
    pub fn new(tasks: Arc<dyn TaskService>) -> Self {
        Self { tasks }
    }
}

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Delegate a self-contained subtask to a child agent and wait for its \
         result. The child runs with its own conversation and a restricted \
         tool surface, so it cannot interrupt this session. Use it for \
         focused work that would otherwise flood this conversation (wide \
         searches, bulk reading, independent sub-analyses). Pass \
         subagent_type=explore for read-only investigation, or a discovered \
         agent name for a specialized profile."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Self-contained instructions for the child agent (it sees no part of this conversation)"
                },
                "subagent_type": {
                    "type": "string",
                    "description": "Optional: 'explore' (read-only) or a discovered agent definition name"
                },
                "allowed_tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional explicit tool allowlist; overrides the agent definition's surface"
                },
                "background": {
                    "type": "boolean",
                    "description": "Optional: spawn and return the task id immediately instead of blocking up to the wait limit; observe with task_output, stop with task_stop (default false)"
                }
            },
            "required": ["prompt"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if prompt.is_empty() {
            return Ok(ToolOutput {
                content: "missing parameter 'prompt' (non-empty string required)".into(),
                is_error: true,
            });
        }
        let subagent = input
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let mut kind = TaskKind::Standard;
        let mut preamble = String::new();
        let mut allowed_tools: Vec<String> = Vec::new();

        if subagent == "explore" {
            kind = TaskKind::ReadOnly;
        } else if !subagent.is_empty() {
            let defs = discover_agent_defs(&ctx.cwd);
            match defs.iter().find(|d| d.name == subagent) {
                Some(def) => {
                    if def.read_only {
                        kind = TaskKind::ReadOnly;
                    }
                    preamble = format!(
                        "You are the `{}` agent. Purpose: {}\n\n",
                        def.name, def.description
                    );
                    allowed_tools = def.allowed_tools.clone();
                }
                None => {
                    let available: Vec<String> = std::iter::once("explore".to_string())
                        .chain(defs.iter().map(|d| d.name.clone()))
                        .collect();
                    return Ok(ToolOutput {
                        content: format!(
                            "unknown subagent_type '{subagent}' (available: {})",
                            available.join(", ")
                        ),
                        is_error: true,
                    });
                }
            }
        }
        if let Some(explicit) = input.get("allowed_tools").and_then(|v| v.as_array()) {
            allowed_tools = explicit
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }

        let id = self.tasks.spawn(TaskRequest {
            kind,
            input: format!("{preamble}{prompt}"),
            parent_run_id: String::new(),
            allowed_tools,
            depth: 0,
            parent: None,
        });

        // Fire-and-forget mode: the caller judged the result unnecessary for
        // this turn, so return the id immediately instead of blocking.
        if input.get("background").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(ToolOutput {
                content: format!(
                    "task {id} spawned in the background; observe it with \
                     task_output or stop it with task_stop"
                ),
                is_error: false,
            });
        }

        // Inline wait: the value of `task` over background forks is the
        // model getting the result in the same turn. Bounded by
        // TASK_WAIT_TIMEOUT; past it the id stays usable via task_output.
        let deadline = tokio::time::Instant::now() + TASK_WAIT_TIMEOUT;
        loop {
            tokio::time::sleep(TASK_POLL_INTERVAL).await;
            match self.tasks.query(&id) {
                Some(info) if info.state == TaskState::Finished => {
                    return Ok(match info.outcome {
                        Some(TaskOutcome::Completed { summary }) => ToolOutput {
                            content: summary,
                            is_error: false,
                        },
                        Some(TaskOutcome::Failed { reason }) => ToolOutput {
                            content: format!("task {id} failed: {reason}"),
                            is_error: true,
                        },
                        Some(TaskOutcome::Stopped) => ToolOutput {
                            content: format!("task {id} was stopped before completing"),
                            is_error: true,
                        },
                        None => ToolOutput {
                            content: format!("task {id} finished without a recorded outcome"),
                            is_error: true,
                        },
                    });
                }
                Some(_) => {}
                None => {
                    return Ok(ToolOutput {
                        content: format!("task {id} vanished from the task table"),
                        is_error: true,
                    });
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(ToolOutput {
                    content: format!(
                        "task {id} is still running after the wait limit; \
                         observe it with task_output or stop it with task_stop"
                    ),
                    is_error: true,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use action_tasks::TaskInfo;
    use std::sync::Mutex;

    struct FakeTasks {
        spawned: Mutex<Vec<TaskRequest>>,
        /// Outcome surfaced by query; None keeps the task Running forever.
        outcome: Mutex<Option<TaskOutcome>>,
    }

    impl FakeTasks {
        fn finishing(outcome: TaskOutcome) -> Self {
            Self {
                spawned: Mutex::new(Vec::new()),
                outcome: Mutex::new(Some(outcome)),
            }
        }
    }

    impl TaskService for FakeTasks {
        fn spawn(&self, request: TaskRequest) -> String {
            let mut spawned = self.spawned.lock().unwrap();
            let id = format!("task-{}", spawned.len() + 1);
            spawned.push(request);
            id
        }

        fn query(&self, _id: &str) -> Option<TaskInfo> {
            Some(TaskInfo {
                state: TaskState::Finished,
                outcome: self.outcome.lock().unwrap().clone(),
                lineage: Vec::new(),
            })
        }

        fn stop(&self, _id: &str) -> bool {
            false
        }
    }

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    fn write_agent(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn discovery_reads_both_dirs_and_first_name_wins() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            ".wavecode/agents/explorer.md",
            "---\nname: explorer\ndescription: Wide search\ntools: read_file, grep\n---\nbody",
        );
        write_agent(
            dir.path(),
            ".claude/agents/explorer.md",
            "---\nname: explorer\ndescription: shadowed\n---\nbody",
        );
        write_agent(
            dir.path(),
            ".claude/agents/auditor.md",
            "---\nname: auditor\ndescription: Audit\ntools:\n  - read_file\n  - glob\nkind: explore\n---\nbody",
        );
        write_agent(
            dir.path(),
            ".wavecode/agents/broken.md",
            "no frontmatter here",
        );
        let defs = discover_agent_defs(dir.path());
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["explorer", "auditor"],
            "wavecode dir shadows claude dir"
        );
        let explorer = &defs[0];
        assert_eq!(explorer.description, "Wide search");
        assert_eq!(explorer.allowed_tools, vec!["read_file", "grep"]);
        assert!(!explorer.read_only);
        let auditor = &defs[1];
        assert_eq!(auditor.allowed_tools, vec!["read_file", "glob"]);
        assert!(auditor.read_only, "kind: explore selects read-only");
    }

    #[test]
    fn name_falls_back_to_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            ".wavecode/agents/scout.md",
            "---\ndescription: no explicit name\n---\nbody",
        );
        let defs = discover_agent_defs(dir.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "scout");
    }

    #[tokio::test]
    async fn bare_prompt_spawns_standard_and_returns_summary() {
        let tasks = Arc::new(FakeTasks::finishing(TaskOutcome::Completed {
            summary: "found it".into(),
        }));
        let tool = TaskTool::new(tasks.clone());
        let (_d, c) = ctx();
        let out = tool
            .execute(serde_json::json!({"prompt": "find the bug"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "found it");
        let spawned = tasks.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].kind, TaskKind::Standard);
        assert_eq!(spawned[0].input, "find the bug");
        assert!(spawned[0].allowed_tools.is_empty());
        assert!(!tool.is_read_only());
    }

    #[tokio::test]
    async fn background_true_returns_id_without_waiting() {
        let tasks = Arc::new(FakeTasks::finishing(TaskOutcome::Completed {
            summary: "found it".into(),
        }));
        let tool = TaskTool::new(tasks.clone());
        let (_d, c) = ctx();
        let out = tool
            .execute(
                serde_json::json!({"prompt": "find the bug", "background": true}),
                &c,
            )
            .await
            .unwrap();
        // Fire-and-forget: the summary must NOT surface even though the fake
        // child finishes immediately; the id is the result.
        assert!(!out.is_error);
        assert!(
            out.content
                .contains("task task-1 spawned in the background"),
            "background spawn reports the id: {}",
            out.content
        );
        let spawned = tasks.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
    }

    #[tokio::test]
    async fn named_agent_applies_profile_and_preamble() {
        let tasks = Arc::new(FakeTasks::finishing(TaskOutcome::Completed {
            summary: "done".into(),
        }));
        let tool = TaskTool::new(tasks.clone());
        let (dir, c) = ctx();
        write_agent(
            dir.path(),
            ".wavecode/agents/explorer.md",
            "---\nname: explorer\ndescription: wide search\ntools: read_file, grep\n---\nbody",
        );
        let out = tool
            .execute(
                serde_json::json!({"prompt": "map the crate", "subagent_type": "explorer"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let spawned = tasks.spawned.lock().unwrap();
        assert_eq!(
            spawned[0].allowed_tools,
            vec!["read_file".to_string(), "grep".to_string()]
        );
        assert!(
            spawned[0]
                .input
                .starts_with("You are the `explorer` agent. Purpose: wide search"),
            "identity preamble composes with the prompt: {}",
            spawned[0].input
        );
        assert!(spawned[0].input.contains("map the crate"));
    }

    #[tokio::test]
    async fn explore_type_selects_read_only() {
        let tasks = Arc::new(FakeTasks::finishing(TaskOutcome::Completed {
            summary: "ok".into(),
        }));
        let tool = TaskTool::new(tasks.clone());
        let (_d, c) = ctx();
        tool.execute(
            serde_json::json!({"prompt": "look around", "subagent_type": "explore"}),
            &c,
        )
        .await
        .unwrap();
        let spawned = tasks.spawned.lock().unwrap();
        assert_eq!(spawned[0].kind, TaskKind::ReadOnly);
    }

    #[tokio::test]
    async fn explicit_allowed_tools_override_the_definition() {
        let tasks = Arc::new(FakeTasks::finishing(TaskOutcome::Completed {
            summary: "ok".into(),
        }));
        let tool = TaskTool::new(tasks.clone());
        let (dir, c) = ctx();
        write_agent(
            dir.path(),
            ".wavecode/agents/explorer.md",
            "---\nname: explorer\ndescription: x\ntools: read_file\n---\nbody",
        );
        tool.execute(
            serde_json::json!({
                "prompt": "p",
                "subagent_type": "explorer",
                "allowed_tools": ["read_file", "grep"]
            }),
            &c,
        )
        .await
        .unwrap();
        let spawned = tasks.spawned.lock().unwrap();
        assert_eq!(
            spawned[0].allowed_tools,
            vec!["read_file".to_string(), "grep".to_string()]
        );
    }

    #[tokio::test]
    async fn unknown_subagent_lists_available_names() {
        let tasks = Arc::new(FakeTasks::finishing(TaskOutcome::Completed {
            summary: "ok".into(),
        }));
        let tool = TaskTool::new(tasks.clone());
        let (dir, c) = ctx();
        write_agent(
            dir.path(),
            ".wavecode/agents/auditor.md",
            "---\nname: auditor\ndescription: x\n---\nbody",
        );
        let out = tool
            .execute(
                serde_json::json!({"prompt": "p", "subagent_type": "ghost"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("available:")
                && out.content.contains("explore")
                && out.content.contains("auditor")
        );
        assert!(
            tasks.spawned.lock().unwrap().is_empty(),
            "nothing spawns on an unknown name"
        );
    }

    #[tokio::test]
    async fn empty_prompt_is_a_business_error() {
        let tasks = Arc::new(FakeTasks::finishing(TaskOutcome::Completed {
            summary: "ok".into(),
        }));
        let tool = TaskTool::new(tasks.clone());
        let (_d, c) = ctx();
        let out = tool
            .execute(serde_json::json!({"prompt": "  "}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("prompt"));
    }

    #[tokio::test]
    async fn failed_and_stopped_outcomes_are_errors() {
        let (_d, c) = ctx();
        for (outcome, needle) in [
            (
                TaskOutcome::Failed {
                    reason: "boom".into(),
                },
                "boom",
            ),
            (TaskOutcome::Stopped, "stopped"),
        ] {
            let tasks = Arc::new(FakeTasks::finishing(outcome));
            let tool = TaskTool::new(tasks);
            let out = tool
                .execute(serde_json::json!({"prompt": "p"}), &c)
                .await
                .unwrap();
            assert!(out.is_error, "{needle}");
            assert!(out.content.contains(needle));
        }
    }
}
