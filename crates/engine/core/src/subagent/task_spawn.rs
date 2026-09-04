use super::*;

/// `task` 工具：派生子代理（同步等待或后台运行）。
pub struct TaskSpawn {
    runtime: Arc<SubagentRuntime>,
}

impl TaskSpawn {
    /// 以 Runtime 共享句柄构造（`Session::with_subagents` 装配）。
    pub fn new(runtime: Arc<SubagentRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait::async_trait]
impl Tool for TaskSpawn {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Spawn a subagent to handle a task in an isolated context. The subagent runs its own \
         independent session; only its final summary comes back, keeping the parent context clean. \
         Use run_in_background=true to run it in parallel and be notified via a \
         <task-notification> when it finishes (poll with task_output). Subagent types: \
         'general-purpose' (all tools, default) and 'explore' (read-only tools, for codebase \
         investigation). Subagents cannot spawn further subagents."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short (3-5 word) label for the task"
                },
                "prompt": {
                    "type": "string",
                    "description": "Complete, self-contained instructions for the subagent; \
                                    it does not see the parent conversation"
                },
                "subagent_type": {
                    "type": "string",
                    "enum": ["general-purpose", "explore"],
                    "description": "Subagent type (default: general-purpose)"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Run asynchronously and notify on completion (default: false = wait for the result)"
                }
            },
            "required": ["description", "prompt"]
        })
    }

    fn is_read_only(&self) -> bool {
        // 派生执行体（子代理可写文件 / 跑命令）：非只读，进串行段过审批门。
        false
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> wavecode_tools::Result<ToolOutput> {
        let err = |reason: String| {
            Ok(ToolOutput {
                content: reason,
                is_error: true,
            })
        };
        let description = match required_str(&input, "description") {
            Ok(s) => s.to_owned(),
            Err(e) => return err(e),
        };
        let prompt = match required_str(&input, "prompt") {
            Ok(s) => s.to_owned(),
            Err(e) => return err(e),
        };
        let subagent_type = match input.get("subagent_type") {
            None | Some(Value::Null) => SubagentType::GeneralPurpose,
            Some(v) => match v.as_str().and_then(SubagentType::parse) {
                Some(t) => t,
                None => {
                    return err(format!(
                        "invalid subagent_type {v} (expected general-purpose | explore)"
                    ));
                }
            },
        };
        let run_in_background = input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let spec = TaskSpec {
            description,
            prompt,
            subagent_type,
            // task 工具不暴露自定义前言与工具面白名单（P7：这两个字段是
            // skill fork 的编排面，模型不可自定）。
            preamble: None,
            allowed_tools: None,
        };

        if run_in_background {
            let type_label = spec.subagent_type.as_str();
            let id = self.runtime.spawn_background(spec);
            return Ok(ToolOutput {
                content: format!(
                    "Spawned background task {id} ({type_label}).\n\
                     Its result will arrive as a <task-notification> when it completes; \
                     use task_output with task_id \"{id}\" to poll, task_stop to stop it.",
                ),
                is_error: false,
            });
        }
        // 同步形态：结果直接作为 ToolResult 回灌（不发通知、不进任务表）。
        let result = self.runtime.run_sync(spec).await;
        Ok(ToolOutput {
            is_error: result.status == SubagentStatus::Failed,
            content: format_result(&result),
        })
    }
}
