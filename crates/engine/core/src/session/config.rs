//! 会话配置（阶段 1c 拆分自 session/mod.rs）：SessionConfig 字段定义与
//! 构造器。builder 是字段增长的收敛点——新增配置项在此追加带默认值的
//! 链式方法，存量构造点（装配层与测试夹具）不受影响。

use std::sync::Arc;
use std::time::Duration;

use wavecode_context::ContextConfig;
use wavecode_protocol::PermissionMode;
use wavecode_sandbox::Sandbox;
use wavecode_tools::Registry;

/// builder 默认上下文窗口（装配层生产路径以 provider 配置覆盖）。
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
/// builder 默认单轮采样输出 token 上限（同上）。
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;
/// builder 默认审批等待超时：无人回填决策时按拒绝收尾，turn 不永久 park。
const DEFAULT_APPROVAL_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
/// builder 默认单 turn 工具轮数上限：模型持续 tool_use 的失控熔断。
const DEFAULT_MAX_TOOL_ROUNDS: u32 = 32;

/// Session 配置（`Session::new` 后冻结为快照）。
pub struct SessionConfig {
    /// 模型名（注入采样请求的 `model` 字段）。
    pub model_name: String,
    /// 上下文窗口大小（TokenCount 事件的 `window` 字段）。
    pub context_window: u64,
    /// 单轮采样输出 token 上限（请求的 `max_tokens`）。
    pub max_output_tokens: u32,
    /// 审批 Ask 等待决策的超时：前端崩溃 / 弹窗丢失时按拒绝回灌收尾
    ///（工具不执行），turn 不永久 park；用户可重新发起。
    pub approval_wait_timeout: Duration,
    /// 单 turn 工具轮数上限：模型连续 tool_use 达上限后不再采样，turn
    /// 以 Completed 收尾（上一轮工具结果已回灌，历史配对完整）。
    pub max_tool_rounds: u32,
    /// 模型通道（流式采样）。
    pub model: Arc<dyn wavecode_llm::ChatModel>,
    /// 工具注册表：每轮请求注入 specs，执行管道按名查找。
    pub registry: Registry,
    /// 工作目录：系统提示词展示与 [`wavecode_tools::ToolCtx::cwd`] 的根。
    pub cwd: std::path::PathBuf,
    /// 敏感环境变量名（装配层注入 provider 的 `env_key` 等显式名单），
    /// 透传 [`wavecode_tools::ToolCtx::deny_env`]：shell 工具 spawn 前从子进程
    /// 环境剔除。
    pub deny_env: Vec<String>,
    /// 权限状态（模式 + allow/deny 规则）：非只读 / 破坏性工具执行前的
    /// 审批判定（P2，SPEC §12）。
    pub sandbox: Sandbox,
    /// 上下文管线配置（P3，SPEC §6）：三级阈值 / 保留条数 / 摘要预算 /
    /// 估算比率。构造后冻结，会话内不变。
    pub context: ContextConfig,
    /// P6 记忆装配（SPEC §7）：指令记忆 / 记忆索引的注入内容与
    /// memory_write / 自动提取的存储根。`None` = 无记忆能力——子代理
    /// 自身的 Session 即此形态（隔离上下文不挂持久记忆写入面）。
    pub memory: Option<crate::memory::MemorySessionConfig>,
    /// P7 skills 装配（SPEC §8）：skill 工具触发面与清单注入的技能集。
    /// `None` = 无 skills 能力——子代理自身的 Session 即此形态（隔离
    /// 上下文不挂 skill 触发面）。
    pub skills: Option<crate::skills::SkillSessionConfig>,
    /// P7 hooks 装配（SPEC §9）：command hook 引擎（事件点执行与阻塞
    /// 语义）。`None` = 无 hooks。once 语义以引擎实例为界（= 会话级）。
    pub hooks: Option<Arc<wavecode_hooks::HookEngine>>,
    /// P10 会话持久化装配（SPEC §16）：rollout 文件根目录与 thread id。
    /// `None` = 不持久化——子代理自身的 Session 即此形态（隔离上下文，
    /// 持久化以父会话为单位）。构造时文件已存在且非空即 replay 恢复
    ///（resume 语义：压缩点之后原文 + 摘要即新历史，见 rollout 模块注释）。
    pub rollout: Option<crate::rollout::RolloutConfig>,
    /// 共享审批槽（子代理会话注入父会话的 gate）：子代理的
    /// `ApprovalRequested` 经父事件流冒泡给前端，决策经同一 `Op::ExecApproval`
    /// 落到共享槽（call_id 键控，父子不冲突）。`None` = 自建独立槽
    ///（普通会话形态）。子代理事件汇未挂接（无人能应答）时由 manager
    /// 直接拒绝，不 park 挂死。
    pub approval_gate: Option<Arc<crate::session::ApprovalGate>>,
}

impl SessionConfig {
    /// 构造器入口：必填四项——模型名 / 模型通道 / 工具注册表 / 工作目录；
    /// 其余字段经 [`SessionConfigBuilder`] 链式方法覆盖。
    pub fn builder(
        model_name: impl Into<String>,
        model: Arc<dyn wavecode_llm::ChatModel>,
        registry: Registry,
        cwd: std::path::PathBuf,
    ) -> SessionConfigBuilder {
        SessionConfigBuilder::new(model_name, model, registry, cwd)
    }
}

/// [`SessionConfig`] 的构造器。
///
/// 必填项之外的字段全部带默认值：能力面（memory / skills / hooks /
/// rollout）默认 `None`（无该能力）；sandbox 默认 `default` 模式空规则
/// 表（安全默认）；context 默认管线参数；窗口 200_000 / 输出 8192。
/// 新增配置字段时在此追加带默认值的链式方法即可。
pub struct SessionConfigBuilder {
    cfg: SessionConfig,
}

impl SessionConfigBuilder {
    fn new(
        model_name: impl Into<String>,
        model: Arc<dyn wavecode_llm::ChatModel>,
        registry: Registry,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self {
            cfg: SessionConfig {
                model_name: model_name.into(),
                context_window: DEFAULT_CONTEXT_WINDOW,
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                approval_wait_timeout: DEFAULT_APPROVAL_WAIT_TIMEOUT,
                max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
                model,
                registry,
                cwd,
                deny_env: Vec::new(),
                sandbox: Sandbox::without_rules(PermissionMode::Default),
                context: ContextConfig::default(),
                memory: None,
                skills: None,
                hooks: None,
                rollout: None,
                approval_gate: None,
            },
        }
    }

    /// 上下文窗口大小（默认 200_000）。
    pub fn context_window(mut self, context_window: u64) -> Self {
        self.cfg.context_window = context_window;
        self
    }

    /// 单轮采样输出 token 上限（默认 8192）。
    pub fn max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.cfg.max_output_tokens = max_output_tokens;
        self
    }

    /// 审批等待超时（默认 300s）。
    pub fn approval_wait_timeout(mut self, approval_wait_timeout: Duration) -> Self {
        self.cfg.approval_wait_timeout = approval_wait_timeout;
        self
    }

    /// 单 turn 工具轮数上限（默认 32）。
    pub fn max_tool_rounds(mut self, max_tool_rounds: u32) -> Self {
        self.cfg.max_tool_rounds = max_tool_rounds;
        self
    }

    /// 敏感环境变量名（默认空）。
    pub fn deny_env(mut self, deny_env: Vec<String>) -> Self {
        self.cfg.deny_env = deny_env;
        self
    }

    /// 权限状态（默认 `default` 模式空规则表）。
    pub fn sandbox(mut self, sandbox: Sandbox) -> Self {
        self.cfg.sandbox = sandbox;
        self
    }

    /// 上下文管线配置（默认管线参数）。
    pub fn context(mut self, context: ContextConfig) -> Self {
        self.cfg.context = context;
        self
    }

    /// P6 记忆装配（默认 `None`）。
    pub fn memory(mut self, memory: Option<crate::memory::MemorySessionConfig>) -> Self {
        self.cfg.memory = memory;
        self
    }

    /// P7 skills 装配（默认 `None`）。
    pub fn skills(mut self, skills: Option<crate::skills::SkillSessionConfig>) -> Self {
        self.cfg.skills = skills;
        self
    }

    /// P7 hooks 装配（默认 `None`）。
    pub fn hooks(mut self, hooks: Option<Arc<wavecode_hooks::HookEngine>>) -> Self {
        self.cfg.hooks = hooks;
        self
    }

    /// P10 会话持久化装配（默认 `None`）。
    pub fn rollout(mut self, rollout: Option<crate::rollout::RolloutConfig>) -> Self {
        self.cfg.rollout = rollout;
        self
    }

    /// 共享审批槽（子代理 `child_config` 注入父 gate；默认 `None` 自建）。
    pub fn approval_gate(mut self, gate: Arc<crate::session::ApprovalGate>) -> Self {
        self.cfg.approval_gate = Some(gate);
        self
    }

    /// 装配完成（`Session::new` 后冻结为快照）。
    pub fn build(self) -> SessionConfig {
        self.cfg
    }
}
