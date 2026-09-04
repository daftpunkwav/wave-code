use super::*;

/// `task_output` 工具：查询后台子代理结果。
pub struct TaskOutputTool {
    runtime: Arc<SubagentRuntime>,
}

impl TaskOutputTool {
    /// 以 Runtime 共享句柄构造（`Session::with_subagents` 装配）。
    pub fn new(runtime: Arc<SubagentRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait::async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Get the result of a background task spawned with the task tool. Returns immediately \
         with the current status: if the task is still running, its result will also arrive \
         as a <task-notification> when it completes, so you can continue other work instead \
         of polling."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task id returned by the task tool (e.g. \"task-1\")"
                }
            },
            "required": ["task_id"]
        })
    }

    fn is_read_only(&self) -> bool {
        // SPEC §11.2 清单登记为非只读（与 task / task_stop 同列）；实现上
        // 只读共享状态，但保持线型一致进串行段。
        false
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> wavecode_tools::Result<ToolOutput> {
        let err = |reason: String| {
            Ok(ToolOutput {
                content: reason,
                is_error: true,
            })
        };
        let task_id = match required_str(&input, "task_id") {
            Ok(s) => s.to_owned(),
            Err(e) => return err(e),
        };
        // 非阻塞轮询语义（择一注释）：阻塞等待会把父会话 turn 挂在工具执行
        // 内，中断安全点（工具迭代间）无法生效；立即返回状态 + 通知注入已
        // 覆盖结果回注，模型可稍后再次查询。
        match self.runtime.query(&task_id) {
            None => err(format!(
                "unknown task id: {task_id} (only background tasks spawned in this session can be queried)"
            )),
            Some(TaskState::Running) => Ok(ToolOutput {
                content: format!(
                    "{task_id} is still running; its result will arrive as a \
                     <task-notification> when it completes. Call task_output again later to poll."
                ),
                is_error: false,
            }),
            Some(TaskState::Finished(result)) => Ok(ToolOutput {
                is_error: result.status == SubagentStatus::Failed,
                content: format_result(&result),
            }),
        }
    }
}
