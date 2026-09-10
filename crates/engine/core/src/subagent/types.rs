use super::*;

/// 内置子代理类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentType {
    /// 全工具（builtin 全集，仍无 task 工具——深度上限 1）。
    GeneralPurpose,
    /// 只读工具（按 registry 过滤 `is_read_only`）：代码调查类任务。
    Explore,
}

impl SubagentType {
    /// 解析类型名；非法值返回 None（调用方转业务失败输出回给模型）。
    pub(super) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "general-purpose" => Some(Self::GeneralPurpose),
            "explore" => Some(Self::Explore),
            _ => None,
        }
    }

    /// 类型名（事件与通知文本用，与 parse 的合法值一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GeneralPurpose => "general-purpose",
            Self::Explore => "explore",
        }
    }

    /// 类型前言（拼在子代理 turn 输入前部；general-purpose 无需引导）。
    pub(super) fn preamble(self) -> Option<&'static str> {
        match self {
            Self::GeneralPurpose => None,
            Self::Explore => Some(EXPLORE_PREAMBLE),
        }
    }
}

/// 一次派生的任务规格（task 工具参数解析产物）。
#[derive(Debug, Clone)]
pub struct TaskSpec {
    /// 短标签（事件 / 通知展示用）。
    pub description: String,
    /// 完整任务指令（子代理 turn 的用户输入）。
    pub prompt: String,
    /// 内置类型（决定工具面与前言）。
    pub subagent_type: SubagentType,
    /// 自定义前言（P7 skill fork：skill 正文作指令拼在输入前部——自定义
    /// 系统提示词注入点仍待后续，见 EXPLORE_PREAMBLE 注释）。None 时用
    /// 内置类型前言。
    pub preamble: Option<String>,
    /// 工具面白名单（P7 skill fork 的 `allowed-tools`）：按名过滤 child
    /// registry（构造级限定整个子代理生命周期）；None = 按内置类型取
    /// 全集 / 只读子集。task 工具不暴露此字段（模型不可自定工具面）。
    pub allowed_tools: Option<Vec<String>>,
}

/// 子代理终态的结构化结果（task_output / 通知 / SubagentCompleted 同源）。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskResult {
    /// 终态：completed / failed / stopped。
    pub status: SubagentStatus,
    /// 最终文本摘要（子代理最后一条 assistant 完整文本；失败时为错误摘要）。
    pub summary: String,
    /// token 用量（子代理末轮 TokenCount；中断 / 失败路径可能无）。
    pub tokens_used: Option<u64>,
}

/// 任务状态（task_output 的查询面）。
#[derive(Debug, Clone, PartialEq)]
pub enum TaskState {
    /// 仍在运行。
    Running,
    /// 已到达终态。
    Finished(TaskResult),
}
