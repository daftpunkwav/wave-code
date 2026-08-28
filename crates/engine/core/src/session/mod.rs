//! Session 与 turn 状态机循环（SPEC §5.1 的 M1 子集 + P2 审批 + P3 上下文管线）。
//!
//! 一轮 turn 的循环：push 用户消息 → PreTurn 预算检查（三级阈值）→ 组装
//! 请求流式采样 → 消费流组装 assistant 消息 → 有 tool_use 则编排执行、
//! 结果回灌为新的 user 消息 → 再次采样，直至模型不再发起工具调用
//!（`end_turn` / `max_tokens` 等终态）。
//!
//! P2 落地 AwaitApproval（SPEC §5.1 / §12）：非只读 / 破坏性工具执行前经
//! sandbox 判定；`Ask` 时发 `ApprovalRequested` 事件并 park 等待——审批
//! 反向通道复用 interrupt_handle 模式（§17.5 M3）：[`Session::approval_handle`]
//! 共享槽由驱动方（app-server actor）在 in-turn `select!` 中路由
//! `Op::ExecApproval` 唤醒。中断在审批等待中同样生效。
//!
//! P3 落地上下文管线（SPEC §5.2 / §6）：
//! - PreTurn 预算检查：警告线发 Warning（每 turn 一次）；自动压缩线 / 阻塞线
//!   触发模型摘要压缩（CompactStarted / CompactCompleted 事件）；
//! - reactive compact：`prompt_too_long` 类错误压缩后重试，连续 3 次熔断；
//! - `max_output_tokens` 续写：stop_reason == "max_tokens" 时以续写提示
//!   继续，最多 2 次；
//! - 历史存 `Arc<Vec<Message>>`，每轮请求 O(1) 指针克隆快照（§17.5 M4
//!   的 `messages.clone()` O(n²) 消除）。
//!
//! P4 落地规划系统（deepagents planning，SPEC §5.4 / §11.2）：
//! - 系统提示词经 [`crate::prompt`] 分层组装（静态层字节稳定 + 动态层集中）；
//! - 任务清单非空时每轮以 `<system-reminder>` 注入 system 尾部；
//! - stop steering：终态无 tool_use 且清单仍有未完成项时注入提醒继续 turn，
//!   连续 3 次后放行（防提前收工，上限防死循环）。
//!
//! 边界（YAGNI，循环结构留扩展位）：无 hooks；中断经
//! [`Session::interrupt_handle`] 置标志，在安全点（循环头、流消费循环内、
//! 工具执行前、串行工具迭代间、审批等待中）检查。
//!
//! P5 落地子代理（deepagents subagents，SPEC §5.3 / §11.2）：
//! [`Session::with_subagents`] 装配 [`crate::subagent::SubagentManager`] 并
//! 注册 task / task_output / task_stop 工具；子代理以独立 Session 运行
//!（隔离消息历史），后台终态以 `<task-notification>` user 消息在 turn
//! 循环头注入父会话（注入机制与 P4 steering 同路径：`push_message`）。
//!
//! P7 落地 skills 与 hooks（SPEC §8 / §9）：
//! - skills：清单注入 prompt 分层 builder 的 skills 槽位（启动时按 1%
//!   窗口预算渲染，会话内恒定）；`skill` 工具（inline 展开回灌 / fork
//!   派生后台子代理）；`/name [args]` slash 直调经 [`Session::invoke_skill`]
//!   （Op::SlashCommand 路由入口）；`allowed-tools` 为 turn 级工具面
//!   白名单（registry 共享句柄，执行管道在 hook / 审批前拦截，turn 入口
//!   清零——首版语义见 skills 模块注释）；
//! - hooks：八个事件点挂接——PreToolUse / PostToolUse 在工具执行管道
//!   （SPEC §11.1 顺序：查找 → PreToolUse → 审批 → execute → PostToolUse），
//!   UserPromptSubmit 在 turn 入口，Stop 在终态（次序择一：先 todo
//!   steering 后 Stop hook，阻塞以 stderr 回灌模型继续 turn，上限 3 防
//!   死循环），PreCompact / PostCompact 在压缩管线；SessionStart /
//!   SessionEnd 挂 cli bootstrap / 退出路径。
//!
//! P10 落地会话持久化（SPEC §16）：[`SessionConfig::rollout`] 配置后，
//! 构造即 replay 已存在的 rollout 文件恢复历史（resume），历史每次追加
//!（[`Session::push_message`]）与压缩替换（压缩管线）同步落盘为带序号
//! 的 jsonl 记录。子代理会话不持久化（`child_config` 置 `rollout: None`）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use wavecode_llm::Message;
use wavecode_protocol::{Event, PermissionMode, StopReason};

// —— 阶段 1b/1c:子模块声明与重导出(拆自原 session.rs 单文件)——
mod compact;
mod config;
mod events;
mod memory_extract;
mod skill_invoke;
mod tool_dispatch;
mod turn;

use events::*;
use self::turn::TurnRunner; // run_turn_inner 委托(pub(super) struct)
pub use config::{SessionConfig, SessionConfigBuilder};
pub use memory_extract::MemoryExtractionHandle;
pub use tool_dispatch::ApprovalGate;

#[cfg(test)]
mod tests;

/// 审批反向通道的共享槽（§17.5 M3"复用 interrupt_handle 模式"）。
///
/// 驱动方（app-server actor）收到 `Op::ExecApproval` 时经
/// [`ApprovalGate::decide`] 存入决策并唤醒等待者；`run_turn` 在 AwaitApproval
/// 状态按 call_id 取走决策。以 call_id 为键：同一时刻只有一个待决调用
///（非只读串行段逐一审批），键控是为防迟到 / 错号决策被错误消费。
pub struct Session {
    cfg: SessionConfig,
    /// 完整消息历史（`Arc` 共享快照：每轮请求 O(1) 指针克隆；
    /// 变更经 `Arc::make_mut` 写时复制——生产路径下上一轮请求快照在
    /// `stream()` 返回后已释放，make_mut 原址生效；测试 mock 持有请求
    /// 快照时退化为一次克隆，可接受）。
    messages: Arc<Vec<Message>>,
    interrupted: Arc<AtomicBool>,
    approval_gate: Arc<ApprovalGate>,
    /// 最近一次权威 token 占用（上一 turn 结束时的 input+output，或压缩后
    /// 的估算值）：下一 turn 首次 PreTurn 预算检查的输入；本 turn 内则由
    /// 各轮 usage 直接覆盖（provider 的 input_tokens 是权威值，SPEC §6）。
    usage_carry: Option<u64>,
    /// P5 子代理管理器（仅 [`Session::with_subagents`] 装配）：task 工具
    /// 经此派生 / 查询 / 停止子代理；后台终态通知在 turn 循环头注入。
    /// `None` = 无子代理能力——子代理自身的 Session 即为此形态
    ///（深度上限 1，见 subagent 模块注释）。
    subagents: Option<Arc<crate::subagent::SubagentManager>>,
    /// P7 skills 清单注入文本（启动时按 1% 窗口预算渲染一次，会话内
    /// 恒定——与记忆索引快照同纪律，见 prompt 模块注释）；空串 = 无注入。
    skills_catalog: String,
    /// P10 rollout 记录器（仅配置了 [`SessionConfig::rollout`] 时存在）：
    /// 历史每次追加 / 压缩替换同步落盘（追加写，SPEC §16）。
    recorder: Option<crate::rollout::RolloutRecorder>,
}

impl Session {
    /// 新建会话：冻结配置快照，历史为空，中断标志清零。
    /// 配置了记忆面（`SessionConfig.memory`）时注册 `memory_write` 工具
    ///（审批经 sandbox 非只读默认策略挂接，见 memory 模块注释）；
    /// 配置了技能面（`SessionConfig.skills`）时注册 `skill` 工具并预渲染
    /// 清单注入文本（P7；`with_subagents` 已注册带子代理管理器的 skill
    /// 工具时跳过——Registry 按名覆盖，后注册者优先，此处只补缺）。
    pub fn new(cfg: SessionConfig) -> Self {
        let mut cfg = cfg;
        if let Some(mem) = &cfg.memory {
            cfg.registry
                .register(Arc::new(crate::memory::MemoryWrite::new(
                    mem.store_root.clone(),
                )));
        }
        if let Some(skills) = &cfg.skills
            && cfg.registry.get("skill").is_none()
        {
            cfg.registry
                .register(Arc::new(crate::skills::SkillTool::new(
                    skills.set.clone(),
                    cfg.registry.allowlist(),
                    None,
                )));
        }
        // P7：清单启动时渲染一次（1% 窗口预算，SPEC §8.2），会话内恒定。
        let skills_catalog = cfg
            .skills
            .as_ref()
            .map(|skills| {
                skills.set.catalog(crate::skills::catalog_budget_chars(
                    cfg.context_window,
                    cfg.context.estimate_chars_per_token,
                ))
            })
            .unwrap_or_default();
        // P10：rollout 持久化（SPEC §16）——文件已存在且非空即 replay
        // 恢复历史（resume：压缩点之后原文 + 摘要即新历史），随后追加写
        // 继续记录；失败显式 warn 降级，不阻塞会话（见 rollout 模块注释）。
        let (messages, recorder) = match &cfg.rollout {
            Some(rollout) => crate::rollout::open_session_rollout(rollout),
            None => (Vec::new(), None),
        };
        Self {
            cfg,
            messages: Arc::new(messages),
            interrupted: Arc::new(AtomicBool::new(false)),
            approval_gate: Arc::new(ApprovalGate::new()),
            usage_carry: None,
            subagents: None,
            skills_catalog,
            recorder,
        }
    }

    /// 新建具备子代理能力的会话（P5）：创建
    /// [`crate::subagent::SubagentManager`]（父配置快照：Arc 共享模型通道、
    /// 继承 sandbox / cwd）并把 task / task_output / task_stop 注册进
    /// registry。父会话用本构造器；子代理自身经 [`Session::new`] 构造
    ///（registry 不含 task 工具——深度上限 1 由构造保证）。
    /// P7：技能面存在时注册带子代理管理器的 `skill` 工具（fork 执行面；
    /// `Session::new` 只补无管理器的缺省注册）。
    pub fn with_subagents(mut cfg: SessionConfig) -> Self {
        let manager = crate::subagent::SubagentManager::from_config(&cfg);
        cfg.registry
            .register(Arc::new(crate::subagent::TaskSpawn::new(manager.clone())));
        cfg.registry
            .register(Arc::new(crate::subagent::TaskOutputTool::new(
                manager.clone(),
            )));
        cfg.registry
            .register(Arc::new(crate::subagent::TaskStop::new(manager.clone())));
        if let Some(skills) = &cfg.skills {
            cfg.registry
                .register(Arc::new(crate::skills::SkillTool::new(
                    skills.set.clone(),
                    cfg.registry.allowlist(),
                    Some(manager.clone()),
                )));
        }
        let mut session = Self::new(cfg);
        session.subagents = Some(manager);
        session
    }

    /// 历史追加一条消息（写时复制，见 [`Session::messages`] 注释）。
    /// P10：先落盘再入内存——崩溃时 rollout 不落后于历史（追加写为
    /// 打开句柄的一次 syscall 量级；同步写保证记录与历史变更同序，
    /// spawn_blocking 会引入乱序与额外任务，知情决策见 rollout 模块）。
    /// 执行一轮 turn，返回终止原因。
    pub async fn run_turn(
        &mut self,
        submission_id: &str,
        text: &str,
        events: mpsc::Sender<Event>,
    ) -> anyhow::Result<StopReason> {
        self.run_turn_inner(submission_id, text, events, None).await
    }

    /// turn 实现：委托 [`TurnRunner`]——把 turn 级状态机与跨轮状态从 Session
    /// 方法迁入独立执行器（`allowed_tools`：P7 inline skill 工具面白名单）。
    async fn run_turn_inner(
        &mut self,
        submission_id: &str,
        text: &str,
        events: mpsc::Sender<Event>,
        allowed_tools: Option<Vec<String>>,
    ) -> anyhow::Result<StopReason> {
        TurnRunner::new(self)
            .run(submission_id, text, events, allowed_tools)
            .await
    }

    /// 取中断标志的共享句柄（T8 驱动模式：驱动 turn 前克隆，`select!`
    /// over submission 与 turn future，收到 Interrupt 时经句柄置位；
    /// `run_turn(&mut self)` 持有可变借用期间无法经 Session 方法置位）。
    pub fn interrupt_handle(&self) -> Arc<AtomicBool> {
        self.interrupted.clone()
    }

    /// 取审批共享槽的句柄（与 [`Session::interrupt_handle`] 同驱动模式）：
    /// actor 在 in-turn `select!` 中收到 `Op::ExecApproval` 时经
    /// [`ApprovalGate::decide`] 回填，唤醒 park 在 AwaitApproval 的 turn。
    pub fn approval_handle(&self) -> Arc<ApprovalGate> {
        self.approval_gate.clone()
    }

    /// 取权限模式的共享句柄：actor 收到 `Op::SetPermissionMode` 时经此
    /// 切换（turn 进行中亦可），下一次 sandbox 判定即生效。
    pub fn permission_mode_handle(&self) -> Arc<Mutex<PermissionMode>> {
        self.cfg.sandbox.mode_handle()
    }
}

