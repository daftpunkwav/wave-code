/*!
 * @file SkillInvokeTool
 * @description Model-invoked skill execution (inline expansion or fork).
 *
 * Responsibilities:
 * - Resolve skill names against the discovered set with helpful errors.
 * - Expand inline skills to text the model reads and follows.
 * - Spawn fork skills as child tasks through the task service.
 *
 * This module must not depend on: drivers, actors, or sessions. Routing
 * follows the skill's declared context; the `user_invocable` gate constrains
 * frontend slash calls only, never model invocation (the catalog already
 * invites the model to trigger skills on its own).
 */

//! Skill invocation: the model entry point to discovered skills.

use std::sync::Arc;

use crate::{SkillContext, SkillSet};
use action_tasks::{TaskKind, TaskRequest, TaskService};
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// `skill`: invoke one discovered skill by name.
///
/// Inline skills expand to their body (with `$ARGUMENTS` substituted) as
/// the result text. Fork skills spawn a background child task carrying the
/// expanded body as its instructions; completion arrives back through the
/// existing child-task notifications, so the result only names the task.
#[derive(Clone)]
pub struct SkillTool {
    set: Arc<SkillSet>,
    tasks: Arc<dyn TaskService>,
}

impl SkillTool {
    /// Wrap the discovered set and the child task service.
    pub fn new(set: Arc<SkillSet>, tasks: Arc<dyn TaskService>) -> Self {
        Self { set, tasks }
    }
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Invoke one discovered skill by name with optional arguments. \
         Use it when the skill catalog matches the current task better than \
         raw tools. Fork skills run as background child tasks and report \
         back on completion; inline skills expand to instructions in this result."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name from the catalog",
                },
                "args": {
                    "type": "string",
                    "description": "Arguments substituted for $ARGUMENTS (may be empty)",
                },
            },
            "required": ["name"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let Some(skill) = self.set.get(name) else {
            let available: Vec<&str> = self.set.iter().map(|s| s.name.as_str()).collect();
            return Ok(ToolOutput {
                content: format!(
                    "unknown skill: {name} (available: {})",
                    if available.is_empty() {
                        "(none)".to_owned()
                    } else {
                        available.join(", ")
                    }
                ),
                is_error: true,
            });
        };
        let args = input.get("args").and_then(|v| v.as_str()).unwrap_or("");
        let expanded = skill.expand(args);
        match skill.meta.context {
            SkillContext::Inline => Ok(ToolOutput {
                content: expanded,
                is_error: false,
            }),
            SkillContext::Fork => {
                // Fork runs on the shared driver under a per-run allowlist:
                // a declared `allowed-tools` set restricts the child's
                // surface; an empty set falls back to the registry surface
                // minus the child-forbidden spawn tools (see `ChildSurface`),
                // so no fork can ever re-spawn children. The spawned id
                // stays the correlation handle.
                let id = self.tasks.spawn(TaskRequest {
                    kind: TaskKind::Standard,
                    input: expanded,
                    parent_run_id: String::new(),
                    allowed_tools: skill.meta.allowed_tools.clone(),
                    depth: 0,
                    parent: None,
                });
                Ok(ToolOutput {
                    content: format!(
                        "spawned child task {id} for skill `{name}`; its completion arrives as a task notification"
                    ),
                    is_error: false,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Skill, SkillContext, SkillMeta, SkillSource};
    use action_tasks::TaskInfo;

    struct FakeTasks {
        spawned: std::sync::Mutex<Vec<TaskRequest>>,
    }

    #[async_trait::async_trait]
    impl TaskService for FakeTasks {
        fn spawn(&self, request: TaskRequest) -> String {
            let id = format!("task-{}", self.spawned.lock().unwrap().len() + 1);
            self.spawned.lock().unwrap().push(request);
            id
        }

        fn query(&self, _id: &str) -> Option<TaskInfo> {
            None
        }

        fn stop(&self, _id: &str) -> bool {
            false
        }
    }

    fn skill(name: &str, context: SkillContext, body: &str) -> Skill {
        skill_with_tools(name, context, body, Vec::new())
    }

    fn skill_with_tools(
        name: &str,
        context: SkillContext,
        body: &str,
        allowed_tools: Vec<String>,
    ) -> Skill {
        Skill {
            name: name.to_string(),
            dir: std::path::PathBuf::from("/skills"),
            source: SkillSource::User,
            meta: SkillMeta {
                description: "test skill".to_string(),
                when_to_use: None,
                allowed_tools,
                context,
                user_invocable: true,
                argument_hint: None,
                paths: Vec::new(),
            },
            body: body.to_string(),
        }
    }

    fn tool() -> (SkillTool, Arc<FakeTasks>) {
        let mut set = crate::Discovery::default().set;
        set.add(skill(
            "review",
            SkillContext::Inline,
            "Review $ARGUMENTS carefully.",
        ));
        set.add(skill(
            "deep",
            SkillContext::Fork,
            "Research thoroughly in ${WAVECODE_SKILL_DIR}.",
        ));
        set.add(skill_with_tools(
            "focused",
            SkillContext::Fork,
            "Read only.",
            vec!["read_file".to_string(), "grep".to_string()],
        ));
        let tasks = Arc::new(FakeTasks {
            spawned: std::sync::Mutex::new(Vec::new()),
        });
        (SkillTool::new(Arc::new(set), tasks.clone()), tasks)
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::path::PathBuf::from("/tmp"),
            deny_env: Vec::new(),
        }
    }

    #[tokio::test]
    async fn inline_skills_expand_with_args() {
        let (tool, tasks) = tool();
        let output = tool
            .execute(
                serde_json::json!({"name": "review", "args": "the diff"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("the diff"));
        // Inline expansion spawns nothing.
        assert!(tasks.spawned.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fork_skills_spawn_with_expanded_input() {
        let (tool, tasks) = tool();
        let output = tool
            .execute(serde_json::json!({"name": "deep"}), &ctx())
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("task-1"));
        let spawned = tasks.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert!(spawned[0].input.contains("/skills"));
        assert!(!tool.is_read_only());
    }

    #[tokio::test]
    async fn fork_skills_carry_their_allowed_tools() {
        let (tool, tasks) = tool();
        let output = tool
            .execute(serde_json::json!({"name": "focused"}), &ctx())
            .await
            .unwrap();
        assert!(!output.is_error);
        let spawned = tasks.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(
            spawned[0].allowed_tools,
            vec!["read_file".to_string(), "grep".to_string()]
        );
    }

    #[tokio::test]
    async fn fork_skills_without_a_surface_stay_unrestricted() {
        let (tool, tasks) = tool();
        tool.execute(serde_json::json!({"name": "deep"}), &ctx())
            .await
            .unwrap();
        let spawned = tasks.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert!(spawned[0].allowed_tools.is_empty());
    }

    #[tokio::test]
    async fn unknown_skills_list_available_names() {
        let (tool, _) = tool();
        let output = tool
            .execute(serde_json::json!({"name": "ghost"}), &ctx())
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("available:"));
        assert!(output.content.contains("review"));
    }
}
