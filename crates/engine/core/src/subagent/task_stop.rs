use super::*;

/// `task_stop` 工具：停止后台子代理。
pub struct TaskStop {
    runtime: Arc<SubagentRuntime>,
}

impl TaskStop {
    /// 以 Runtime 共享句柄构造（`Session::with_subagents` 装配）。
    pub fn new(runtime: Arc<SubagentRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait::async_trait]
impl Tool for TaskStop {
    fn name(&self) -> &str {
        "task_stop"
    }

    fn description(&self) -> &str {
        "Stop a running background task spawned with the task tool. The subagent is interrupted \
         at its next safe point; its partial result is still available via task_output."
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
        match self.runtime.stop(&task_id).await {
            None => err(format!("unknown task id: {task_id}")),
            Some(TaskState::Finished(result)) => {
                // 任务已自行到达终态（Completed/Failed）：如实说明"停止
                // 动作未生效"，统一输出 stopped 会误导模型以为是它调用
                // 的效果；Stopped 是本次或先前停止请求的结果，照常说。
                let lead = if result.status == SubagentStatus::Stopped {
                    format!("{task_id} stopped.")
                } else {
                    format!("{task_id} had already finished before the stop; no action taken.")
                };
                Ok(ToolOutput {
                    content: format!("{lead}\n{}", format_result(&result)),
                    is_error: false,
                })
            }
            // 超时兜底：中断已置位但子代理未在时限内到达安全点（如挂起的
            // 工具执行）；如实回报仍在运行，不伪造已停止。
            Some(TaskState::Running) => Ok(ToolOutput {
                content: format!(
                    "stop signaled for {task_id}, but it is still running after {}s \
                     (it may be stuck in a tool execution)",
                    STOP_WAIT_TIMEOUT.as_secs()
                ),
                is_error: false,
            }),
        }
    }
}
