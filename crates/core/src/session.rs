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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{Notify, mpsc};
use wavecode_context::{BudgetLevel, ContextConfig, ModelSummary};
use wavecode_llm::{ChatRequest, ContentBlock, LlmError, Message, Role, StreamEvent};
use wavecode_protocol::{
    ApprovalDecision, CompactTrigger, Event, EventMsg, PermissionMode, StopReason,
};
use wavecode_sandbox::{Sandbox, Verdict};
use wavecode_tools::{Registry, ToolCtx, ToolOutput};

/// ToolCallEnd 事件回显工具输出的字符上限（回灌模型的 ToolResult 不截断）。
const TOOL_OUTPUT_EVENT_MAX_CHARS: usize = 2000;

/// 审批等待的兜底轮询间隔：正常路径由 [`ApprovalGate`] 的 `Notify` 即时
/// 唤醒；超时分支只为"裸 interrupt_handle 驱动"（无人戳 gate 的驱动形态，
/// 如单测直接 run_turn）兜底，最坏多等一个间隔。
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// `max_output_tokens` 续写提示（SPEC §5.2）：模型在 max_tokens 截断后
/// 从中断处继续，不重复已产出内容。
const CONTINUATION_PROMPT: &str = "Output token limit reached. Continue exactly from where you stopped; \
     do not repeat content already produced.";

/// 续写次数上限（SPEC §5.2 "最多续 2 次"）。
const MAX_CONTINUATIONS: u32 = 2;

/// reactive compact 熔断阈值（SPEC §5.2 "连续 3 次失败熔断并上报"）：
/// 第 3 次连续 prompt_too_long 即熔断，不再压缩重试。
const MAX_REACTIVE_COMPACT_RETRIES: u32 = 3;

/// stop steering 连续上限（P4）：清单有未完成项而模型想收工时注入提醒
/// 继续 turn；连续 3 次后放行——模型可能坚持任务已完成但忘了更新清单，
/// 无上限会死循环。
const MAX_TODO_STEERINGS: u32 = 3;

/// Stop hook 连续阻塞上限（P7）：阻塞以 stderr 回灌模型继续 turn；连续
/// 3 次后放行——hook 配置错误（恒定阻塞）不能锁死会话（SPEC §5.2 恢复
/// 策略表"Stop hook 阻塞"行的首版上限，同 steering 纪律）。
const MAX_STOP_HOOK_BLOCKS: u32 = 3;

/// stop steering 提醒消息（模型面向，英文）：继续完成剩余项，或确实全部
/// 完成时先用 todo_write 更新清单再收工。
const TODO_STEERING_PROMPT: &str = "\
The task list still has unfinished items. If the task is not actually complete, \
continue working on the remaining items now. If everything is really done, update \
the list with todo_write (mark items completed) before finishing.";

/// 审批反向通道的共享槽（§17.5 M3"复用 interrupt_handle 模式"）。
///
/// 驱动方（app-server actor）收到 `Op::ExecApproval` 时经
/// [`ApprovalGate::decide`] 存入决策并唤醒等待者；`run_turn` 在 AwaitApproval
/// 状态按 call_id 取走决策。以 call_id 为键：同一时刻只有一个待决调用
///（非只读串行段逐一审批），键控是为防迟到 / 错号决策被错误消费。
pub struct ApprovalGate {
    decisions: Mutex<HashMap<String, ApprovalDecision>>,
    notify: Notify,
}

impl ApprovalGate {
    fn new() -> Self {
        Self {
            decisions: Mutex::new(HashMap::new()),
            notify: Notify::new(),
        }
    }

    /// 存入一个审批决策并唤醒等待者（actor 路由 `Op::ExecApproval` 用）。
    /// `Notify::notify_one` 在无等待者时留存一个 permit，与取走决策后的
    /// 下一轮等待无丢失唤醒竞态。
    pub fn decide(&self, call_id: String, decision: ApprovalDecision) {
        self.decisions
            .lock()
            .expect("审批槽锁中毒即进程已有 panic")
            .insert(call_id, decision);
        self.notify.notify_one();
    }

    /// 按 call_id 取走决策（一次性消费）。
    fn take(&self, call_id: &str) -> Option<ApprovalDecision> {
        self.decisions
            .lock()
            .expect("审批槽锁中毒即进程已有 panic")
            .remove(call_id)
    }

    /// turn 开始时清空残留决策（与中断标志每 turn 自清同理：上一 turn
    /// 未被消费的迟到决策不得影响本轮）。
    fn clear(&self) {
        self.decisions
            .lock()
            .expect("审批槽锁中毒即进程已有 panic")
            .clear();
    }
}

/// 审批等待的终局。
enum ApprovalWait {
    /// 收到决策（放行 / 拒绝）。
    Decision(ApprovalDecision),
    /// 等待期间被中断。
    Interrupted,
}

/// Session 配置（`Session::new` 后冻结为快照）。
pub struct SessionConfig {
    /// 模型名（注入采样请求的 `model` 字段）。
    pub model_name: String,
    /// 上下文窗口大小（TokenCount 事件的 `window` 字段）。
    pub context_window: u64,
    /// 单轮采样输出 token 上限（请求的 `max_tokens`）。
    pub max_output_tokens: u32,
    /// 模型通道（流式采样）。
    pub model: Arc<dyn wavecode_llm::ChatModel>,
    /// 工具注册表：每轮请求注入 specs，执行管道按名查找。
    pub registry: Registry,
    /// 工作目录：系统提示词展示与 [`ToolCtx::cwd`] 的根。
    pub cwd: std::path::PathBuf,
    /// 敏感环境变量名（装配层注入 provider 的 `env_key` 等显式名单），
    /// 透传 [`ToolCtx::deny_env`]：shell 工具 spawn 前从子进程环境剔除。
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
}

/// 记忆自动提取的预捕获句柄（P6）：`run_turn(&mut self)` 借用期间无法
/// 经 `&Session` 调用提取入口，actor 在驱动 turn 前预取本句柄（Arc 共享
/// 模型通道与历史快照），Shutdown 路径经 [`MemoryExtractionHandle::spawn`]
/// 派生 detached 提取任务。
///
/// 快照语义（诚实声明）：历史为预取时点的 Arc 快照——会话历史的
/// 写时复制（`Arc::make_mut`）使快照冻结，in-turn Shutdown 时被中断 turn
/// 的部分内容不进提取（不完整内容本就不是好的提取素材，可接受的近似）。
pub struct MemoryExtractionHandle {
    mgr: Arc<crate::subagent::SubagentManager>,
    history: Arc<Vec<Message>>,
    store_root: std::path::PathBuf,
}

impl MemoryExtractionHandle {
    /// 派生 detached 提取任务：失败静默记 warning，不阻塞退出；进程随即
    /// 退出时任务可能未跑完——尽力而为语义（SPEC"不阻塞主会话"）。
    pub fn spawn(self) {
        tokio::spawn(async move {
            match crate::memory::extract_with_manager(self.mgr, self.history, self.store_root).await
            {
                Ok(n) => tracing::debug!(entries = n, "记忆自动提取完成"),
                Err(e) => tracing::warn!(error = %e, "记忆自动提取失败（静默，不阻塞退出）"),
            }
        });
    }
}

/// 一次会话：配置快照 + 完整消息历史 + 中断标志 + 审批共享槽。
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

/// 一轮流式采样中累计的内容块产物。
#[derive(Default)]
struct RoundBlocks {
    /// 已关闭的内容块（text / tool_use），按模型产出序。
    blocks: Vec<ContentBlock>,
    /// 当前未关闭文本块的累计内容。
    cur_text: String,
    /// 当前未关闭 tool_use 块：`(id, name, partial_json 缓冲)`。
    cur_tool: Option<(String, String, String)>,
    /// tool input JSON 解析失败的预置结果：不实际执行，直接回灌 is_error。
    preset_results: Vec<ContentBlock>,
}

impl RoundBlocks {
    /// 关闭当前打开的块（`BlockEnd` 语义；流提前结束时也用作收尾）。
    fn close_open(&mut self) {
        if let Some((id, name, raw)) = self.cur_tool.take() {
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(input) => self.blocks.push(ContentBlock::ToolUse { id, name, input }),
                Err(e) => {
                    // 解析失败不中断 turn：ToolUse 以空对象入历史保持配对，
                    // 预置 is_error 结果直接回灌，不实际执行。
                    let content = format!("invalid tool input json: {raw} ({e})");
                    self.blocks.push(ContentBlock::ToolUse {
                        id: id.clone(),
                        name,
                        input: serde_json::json!({}),
                    });
                    self.preset_results.push(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error: true,
                    });
                }
            }
        } else if !self.cur_text.is_empty() {
            self.blocks.push(ContentBlock::Text {
                text: std::mem::take(&mut self.cur_text),
            });
        }
    }

    /// 流耗尽后收尾：关闭未关闭的块，取出按序内容块。
    fn finish(&mut self) -> Vec<ContentBlock> {
        self.close_open();
        std::mem::take(&mut self.blocks)
    }
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
    fn push_message(&mut self, msg: Message) {
        if let Some(recorder) = &mut self.recorder {
            recorder.record_message(&msg);
        }
        Arc::make_mut(&mut self.messages).push(msg);
    }

    /// `/compact`（Op::Compact）入口：无论阈值立即压缩。
    /// 失败时发 Error 事件（recoverable，会话可继续）并以 Err 返回。
    pub async fn compact(
        &mut self,
        submission_id: &str,
        events: mpsc::Sender<Event>,
    ) -> anyhow::Result<u64> {
        match self
            .compact_with_trigger(&events, submission_id, CompactTrigger::Manual)
            .await
        {
            Ok(summary_tokens) => Ok(summary_tokens),
            Err(e) => {
                emit(
                    &events,
                    submission_id,
                    EventMsg::Error {
                        message: format!("上下文压缩失败: {e:#}"),
                        recoverable: true,
                    },
                )
                .await;
                Err(e)
            }
        }
    }

    /// 压缩管线统一入口（三类触发 + 手动共用，SPEC §6 "触发管线只有一条"）：
    /// 发 CompactStarted → 模型摘要压缩（normalize 保证配对完整）→ 替换历史
    /// → 发 CompactCompleted{summary_tokens}。返回摘要 token 估算。
    async fn compact_with_trigger(
        &mut self,
        events: &mpsc::Sender<Event>,
        submission_id: &str,
        trigger: CompactTrigger,
    ) -> anyhow::Result<u64> {
        // P7：PreCompact hook（不可阻塞，压缩前留档；SPEC §9）。
        if let Some(engine) = &self.cfg.hooks {
            let report = engine
                .run(
                    wavecode_hooks::HookEventPoint::PreCompact,
                    &wavecode_hooks::HookInput {
                        cwd: &self.cfg.cwd,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    },
                )
                .await;
            emit_hook_warnings(events, submission_id, &report.warnings).await;
        }
        emit(events, submission_id, EventMsg::CompactStarted { trigger }).await;
        let strategy = ModelSummary::new(self.cfg.model.clone(), self.cfg.model_name.clone());
        let outcome =
            wavecode_context::compact_history(&self.messages, &strategy, &self.cfg.context).await?;
        let summary_tokens = wavecode_context::estimate_tokens(
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: outcome.summary.clone(),
                }],
            }],
            self.cfg.context.estimate_chars_per_token,
        );
        self.messages = Arc::new(outcome.messages);
        // P10：压缩记录落盘——承载压缩后的完整新历史（replay 遇此记录
        // 即重置历史，恢复语义见 rollout 模块注释）。
        if let Some(recorder) = &mut self.recorder {
            recorder.record_compaction(trigger, summary_tokens, &self.messages);
        }
        // 压缩后 usage_carry 以新历史的估算重置：下次采样回传权威 usage 前，
        // PreTurn 阈值判断用这个估算（误差边界见 estimate_tokens 注释）。
        self.usage_carry = Some(
            wavecode_context::estimate_tokens(
                &self.messages,
                self.cfg.context.estimate_chars_per_token,
            ) + wavecode_context::SYSTEM_OVERHEAD_TOKENS,
        );
        emit(
            events,
            submission_id,
            EventMsg::CompactCompleted { summary_tokens },
        )
        .await;
        // P7：PostCompact hook（不可阻塞，压缩后留档；SPEC §9）。
        if let Some(engine) = &self.cfg.hooks {
            let report = engine
                .run(
                    wavecode_hooks::HookEventPoint::PostCompact,
                    &wavecode_hooks::HookInput {
                        cwd: &self.cfg.cwd,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    },
                )
                .await;
            emit_hook_warnings(events, submission_id, &report.warnings).await;
        }
        tracing::debug!(?trigger, summary_tokens, "上下文压缩完成");
        Ok(summary_tokens)
    }

    /// PreTurn 预算检查（SPEC §6 三级阈值）：
    /// - 警告线：发 Warning（每 turn 至多一次，`warned` 去重）；
    /// - 自动压缩线 / 阻塞线：触发压缩（阻塞 = 强制先压缩再采样）。
    ///
    /// 每 turn 至多自动压缩一次（`compacted` 去重）：压缩后水位仍超标
    ///（极端：最近 N 条原文本身就逼近窗口）时不反复压缩空转——阻塞线
    /// 改发 Warning 放行，reactive compact（prompt_too_long 重试）是兜底。
    async fn check_budget(
        &mut self,
        events: &mpsc::Sender<Event>,
        submission_id: &str,
        used: u64,
        warned: &mut bool,
        compacted: &mut bool,
    ) -> anyhow::Result<()> {
        match self
            .cfg
            .context
            .thresholds
            .check(used, self.cfg.context_window)
        {
            BudgetLevel::Ok => {}
            BudgetLevel::Warning => {
                if !*warned {
                    *warned = true;
                    emit(
                        events,
                        submission_id,
                        EventMsg::Warning {
                            message: format!(
                                "context near limit: {used}/{} tokens used",
                                self.cfg.context_window
                            ),
                        },
                    )
                    .await;
                }
            }
            level @ (BudgetLevel::AutoCompact | BudgetLevel::Blocking) => {
                if *compacted {
                    // 本 turn 已压缩过仍超标：不再空转，警告后放行（兜底见上）。
                    if !*warned {
                        *warned = true;
                        emit(
                            events,
                            submission_id,
                            EventMsg::Warning {
                                message: format!(
                                    "context still near limit after compaction: {used}/{} tokens",
                                    self.cfg.context_window
                                ),
                            },
                        )
                        .await;
                    }
                    return Ok(());
                }
                *compacted = true;
                let trigger = match level {
                    BudgetLevel::AutoCompact => CompactTrigger::Auto,
                    _ => CompactTrigger::Blocking,
                };
                // 压缩失败：自动线降级为警告放行（仍低于阻塞线，可继续）；
                // 阻塞线无法再安全采样，错误上抛由调用方收尾。
                if let Err(e) = self
                    .compact_with_trigger(events, submission_id, trigger)
                    .await
                {
                    if trigger == CompactTrigger::Blocking {
                        return Err(e);
                    }
                    emit(
                        events,
                        submission_id,
                        EventMsg::Warning {
                            message: format!("auto compaction failed, continuing: {e:#}"),
                        },
                    )
                    .await;
                }
            }
        }
        Ok(())
    }

    /// 执行一轮 turn，返回终止原因。
    pub async fn run_turn(
        &mut self,
        submission_id: &str,
        text: &str,
        events: mpsc::Sender<Event>,
    ) -> anyhow::Result<StopReason> {
        self.run_turn_inner(submission_id, text, events, None).await
    }

    /// turn 实现（`allowed_tools`：P7 slash 直调 inline skill 的工具面
    /// 白名单——turn 级语义：入口统一清零后按需设置；模型经 skill 工具
    /// 的激活在工具执行内写入同一句柄，见 skills 模块注释）。
    async fn run_turn_inner(
        &mut self,
        submission_id: &str,
        text: &str,
        events: mpsc::Sender<Event>,
        allowed_tools: Option<Vec<String>>,
    ) -> anyhow::Result<StopReason> {
        // P7：turn 级工具面白名单每 turn 入口清零后按需设置（上一 turn
        // 的 skill 激活不得泄漏进本轮）。
        self.cfg
            .registry
            .allowlist()
            .set(allowed_tools.map(|names| names.into_iter().collect()));
        // 中断标志每 turn 自清：上一 turn 的 interrupt 不影响本轮。
        self.interrupted.store(false, Ordering::SeqCst);
        // 审批槽每 turn 自清：上一 turn 的迟到决策不得被本轮误消费。
        self.approval_gate.clear();
        // P5：子代理事件汇挂接——SubagentStarted/Completed 以本 turn 的
        // submission_id 回填，前端可见子代理起止（中间过程不进父事件流）。
        if let Some(mgr) = &self.subagents {
            mgr.set_event_sink(events.clone(), submission_id);
        }

        // 步骤 1：用户消息入历史，发出 TurnStarted。
        // P7：UserPromptSubmit hook（可阻塞，SPEC §9）：阻塞时输入不进
        // 历史、不发起采样，stderr 以 Error 事件展示给用户（此处无模型
        // 可回灌——TurnStarted 未发，补 TurnCompleted 防前端悬挂）。
        if let Some(engine) = &self.cfg.hooks {
            let report = engine
                .run(
                    wavecode_hooks::HookEventPoint::UserPromptSubmit,
                    &wavecode_hooks::HookInput {
                        cwd: &self.cfg.cwd,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    },
                )
                .await;
            emit_hook_warnings(&events, submission_id, &report.warnings).await;
            if let wavecode_hooks::HookVerdict::Block(stderr) = report.verdict {
                let reason = if stderr.is_empty() {
                    "(hook 未给出原因)".to_owned()
                } else {
                    stderr
                };
                emit(
                    &events,
                    submission_id,
                    EventMsg::Error {
                        message: format!("输入被 UserPromptSubmit hook 拦截: {reason}"),
                        recoverable: true,
                    },
                )
                .await;
                emit(
                    &events,
                    submission_id,
                    EventMsg::TurnCompleted {
                        stop_reason: StopReason::Completed,
                    },
                )
                .await;
                return Ok(StopReason::Completed);
            }
        }
        let turn_id = uuid::Uuid::new_v4().to_string();
        self.push_message(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        });
        emit(
            &events,
            submission_id,
            EventMsg::TurnStarted {
                turn_id: turn_id.clone(),
            },
        )
        .await;
        tracing::debug!(turn_id = %turn_id, submission_id, "turn 开始");

        // 系统提示词不再在 turn 入口构建一次——P4 起每轮采样前经
        // `prompt::build_system_prompt` 重建（清单快照在轮间可变），见步骤 2。
        // deny_env 由装配层注入（cli bootstrap 注入 provider 的 env_key）；
        // shell 工具层的敏感后缀模式剔除（sanitize_env）在此基础上叠加生效。
        let tool_ctx = ToolCtx {
            cwd: self.cfg.cwd.clone(),
            deny_env: self.cfg.deny_env.clone(),
        };
        // 上下文占用估算：末轮 input_tokens + 各轮 output_tokens 累计。
        // 单轮内可有多个 MessageComplete（中途 message_delta 的 stop_reason 为
        // 空串），其 output_tokens 为该轮累计值——轮内覆盖取末次，跨轮再累加。
        // last_input_tokens 每轮采样后必先赋值才 break，故 break 路径 expect 安全。
        let mut last_input_tokens: Option<u64> = None;
        let mut total_output_tokens = 0u64;
        // P3 状态：预算警告 / 自动压缩每 turn 去重；续写与 reactive compact
        // 为连续计数（成功即清零 / 达上限熔断）。
        let mut budget_warned = false;
        let mut budget_compacted = false;
        let mut continuations = 0u32;
        let mut reactive_compacts = 0u32;
        // P4 stop steering：连续提醒计数（模型再次发起 tool_use 即清零）。
        let mut todo_steerings = 0u32;
        // P7 Stop hook：连续阻塞计数（上限后放行，同 steering 防死循环纪律）。
        let mut stop_hook_blocks = 0u32;

        let stop_reason = loop {
            // 安全点：循环头检查中断。触发场景：步骤 5 串行工具段中断后
            // 回到循环头——结果消息已完整回灌（配对完整），不再发起多余
            // 采样请求，直接收尾。
            if self.interrupted.load(Ordering::SeqCst) {
                emit(
                    &events,
                    submission_id,
                    EventMsg::TurnCompleted {
                        stop_reason: StopReason::Interrupted,
                    },
                )
                .await;
                return Ok(StopReason::Interrupted);
            }

            // —— P5：后台子代理终态通知注入（SPEC §5.3 <task-notification>）——
            // 注入点取循环头（与 P4 steering 同 push_message 路径）：此处历史
            // 尾部必为 user 消息（turn 输入 / 工具结果 / 上轮注入）或无
            // tool_use 的 assistant 消息（终态分支），tool_use 配对安全。
            // turn 已结束才到达的通知留在队列，下一 turn 循环头注入。
            if let Some(mgr) = &self.subagents {
                for note in mgr.drain_notifications() {
                    self.push_message(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text { text: note }],
                    });
                }
            }

            // —— PreTurn 预算检查（P3，SPEC §5.1/§6 三级阈值）——
            // 有 usage 用 usage（input_tokens 是覆盖完整历史的权威值）；
            // 首个 turn / 压缩后未再采样时回退字符估算 + 系统开销定额。
            let used = match last_input_tokens {
                Some(input) => input + total_output_tokens,
                None => self.usage_carry.unwrap_or_else(|| {
                    wavecode_context::estimate_tokens(
                        &self.messages,
                        self.cfg.context.estimate_chars_per_token,
                    ) + wavecode_context::SYSTEM_OVERHEAD_TOKENS
                }),
            };
            if let Err(e) = self
                .check_budget(
                    &events,
                    submission_id,
                    used,
                    &mut budget_warned,
                    &mut budget_compacted,
                )
                .await
            {
                // 阻塞线压缩失败：无法再安全采样，收尾并上抛。
                fail_turn(&events, submission_id, format!("{e:#}")).await;
                return Err(e);
            }

            // —— 步骤 2：组装请求，发起流式采样 ——
            // messages 为 O(1) Arc 指针克隆的当轮快照（§17.5 M4：取代逐轮
            // 深拷贝的 O(n²)）；provider 在 stream() 内即完成序列化。
            // 系统提示词每轮重建（P4，SPEC §5.4）：任务清单快照在轮间可变，
            // 注入在 system 尾部；清单不变时整串字节稳定（prompt cache 纪律）。
            // P6 记忆槽位（WAVECODE.md / 索引）为启动时收集的会话内常量。
            let (instruction_memory, memory_index) = match &self.cfg.memory {
                Some(mem) => (mem.instruction_memory.as_str(), mem.memory_index.as_str()),
                None => ("", ""),
            };
            let system = crate::prompt::build_system_prompt(
                &self.cfg.cwd,
                instruction_memory,
                &self.skills_catalog,
                memory_index,
                &self.cfg.registry.todos().snapshot(),
            )
            .await;
            let req = ChatRequest {
                model: self.cfg.model_name.clone(),
                system,
                messages: self.messages.clone(),
                tools: self.cfg.registry.specs(),
                max_tokens: self.cfg.max_output_tokens,
            };
            let mut stream = match self.cfg.model.stream(req).await {
                Ok(s) => {
                    reactive_compacts = 0; // 采样成功：连续失败计数清零
                    s
                }
                Err(e) => {
                    // reactive compact（SPEC §5.2）：prompt_too_long 类错误
                    // 压缩后以压缩历史重试，连续 3 次熔断并上报。
                    if is_prompt_too_long(&e) {
                        reactive_compacts += 1;
                        if reactive_compacts >= MAX_REACTIVE_COMPACT_RETRIES {
                            fail_turn(
                                &events,
                                submission_id,
                                format!(
                                    "prompt_too_long 连续 {reactive_compacts} 次，压缩重试熔断: {e}"
                                ),
                            )
                            .await;
                            return Err(e.into());
                        }
                        tracing::warn!(
                            attempt = reactive_compacts,
                            "prompt_too_long，压缩后以压缩历史重试"
                        );
                        match self
                            .compact_with_trigger(&events, submission_id, CompactTrigger::Reactive)
                            .await
                        {
                            // 以压缩历史回到循环头（中断检查 → 预算检查 → 重试）。
                            Ok(_) => continue,
                            Err(ce) => {
                                fail_turn(
                                    &events,
                                    submission_id,
                                    format!("reactive compact 失败: {ce:#}"),
                                )
                                .await;
                                return Err(ce);
                            }
                        }
                    }
                    // TurnStarted 已发出：发收尾事件防前端悬挂，
                    // 错误本身仍返回调用方。
                    fail_turn(&events, submission_id, e.to_string()).await;
                    return Err(e.into());
                }
            };

            // —— 步骤 3：消费流，累计内容块与终态 ——
            let mut round = RoundBlocks::default();
            let mut stop_reason = String::new();
            let mut round_input_tokens = 0u64;
            let mut round_output_tokens = 0u64;
            while let Some(item) = stream.next().await {
                // 安全点：流消费循环内检查中断，历史保留部分结果。
                if self.interrupted.load(Ordering::SeqCst) {
                    return Ok(self.finish_interrupted(&events, submission_id, round).await);
                }
                let event = match item {
                    Ok(ev) => ev,
                    Err(e) => {
                        // 流中途失败同样收尾；本轮部分产出未入历史
                        //（assistant 消息在步骤 4 才组装），无配对风险。
                        fail_turn(&events, submission_id, e.to_string()).await;
                        return Err(e.into());
                    }
                };
                match event {
                    StreamEvent::TextDelta { text } => {
                        emit(
                            &events,
                            submission_id,
                            EventMsg::AgentMessageDelta { text: text.clone() },
                        )
                        .await;
                        round.cur_text.push_str(&text);
                    }
                    StreamEvent::ToolUseBegin { id, name } => {
                        // 防御：畸形流未 BlockEnd 就开新块，先关闭上一个。
                        round.close_open();
                        round.cur_tool = Some((id, name, String::new()));
                    }
                    StreamEvent::ToolUseInputDelta { partial_json } => {
                        if let Some((_, _, buf)) = round.cur_tool.as_mut() {
                            buf.push_str(&partial_json);
                        }
                    }
                    StreamEvent::BlockEnd => round.close_open(),
                    // 回合终态取最后一个非空 stop_reason（T3 锁定行为）。
                    StreamEvent::MessageComplete {
                        stop_reason: sr,
                        usage,
                    } => {
                        if !sr.is_empty() {
                            stop_reason = sr;
                        }
                        round_input_tokens = usage.input_tokens;
                        round_output_tokens = usage.output_tokens;
                    }
                }
            }
            last_input_tokens = Some(round_input_tokens);
            total_output_tokens += round_output_tokens;

            // —— 步骤 4：组装 assistant 消息入历史，发出全量文本 ——
            let blocks = round.finish();
            let full_text: String = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let has_tool_use = blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
            // 空响应（畸形流零内容块）不入历史：Anthropic 拒绝空 content
            // 数组（400），入历史会污染后续每个 turn；终态收尾照常。
            if !blocks.is_empty() {
                self.push_message(Message {
                    role: Role::Assistant,
                    content: blocks,
                });
            }
            emit(
                &events,
                submission_id,
                EventMsg::AgentMessageComplete { text: full_text },
            )
            .await;

            if !has_tool_use {
                // max_output_tokens 续写（P3，SPEC §5.2）：截断后以续写提示
                // 继续，最多 MAX_CONTINUATIONS 次；中断标志由 continue 后的
                // 循环头安全点捕获。
                if stop_reason == "max_tokens" && continuations < MAX_CONTINUATIONS {
                    continuations += 1;
                    emit(
                        &events,
                        submission_id,
                        EventMsg::Warning {
                            message: format!(
                                "output truncated at max_tokens; continuing ({continuations}/{MAX_CONTINUATIONS})"
                            ),
                        },
                    )
                    .await;
                    self.push_message(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: CONTINUATION_PROMPT.to_owned(),
                        }],
                    });
                    continue;
                }
                // P4 stop steering（防提前收工，deepagents planning）：终态
                // 无 tool_use 且清单仍有 pending/in_progress 项时，注入提醒
                // 消息继续 turn；连续 MAX_TODO_STEERINGS 次后放行（防死循环——
                // 模型可能坚持任务已完成但忘了更新清单）。中断标志由
                // continue 后的循环头安全点捕获。
                let (pending, in_progress) = self.cfg.registry.todos().unfinished();
                if pending + in_progress > 0 && todo_steerings < MAX_TODO_STEERINGS {
                    todo_steerings += 1;
                    emit(
                        &events,
                        submission_id,
                        EventMsg::Warning {
                            message: format!(
                                "todo list has {} unfinished item(s); nudging the model to continue ({todo_steerings}/{MAX_TODO_STEERINGS})",
                                pending + in_progress
                            ),
                        },
                    )
                    .await;
                    let reminder = format!(
                        "{TODO_STEERING_PROMPT}\nCurrent task list:\n{}",
                        wavecode_tools::format_todos(&self.cfg.registry.todos().snapshot())
                    );
                    self.push_message(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text { text: reminder }],
                    });
                    continue;
                }
                // P7 Stop hook（SPEC §9，可阻塞）：与 todo steering 的次序
                // 择一——先 todo steering（上方，清单未完成时模型层面继续），
                // 都通过后才由 Stop hook 外部门禁最后把关。阻塞时 stderr 作为
                // user 消息回灌模型继续 turn；连续上限后放行（同 steering
                // 纪律：hook 配置错误不能锁死会话）。
                if let Some(engine) = self.cfg.hooks.clone() {
                    let report = engine
                        .run(
                            wavecode_hooks::HookEventPoint::Stop,
                            &wavecode_hooks::HookInput {
                                cwd: &self.cfg.cwd,
                                tool_name: None,
                                tool_input: None,
                                tool_output: None,
                            },
                        )
                        .await;
                    emit_hook_warnings(&events, submission_id, &report.warnings).await;
                    if let wavecode_hooks::HookVerdict::Block(stderr) = report.verdict
                        && stop_hook_blocks < MAX_STOP_HOOK_BLOCKS
                    {
                        stop_hook_blocks += 1;
                        emit(
                            &events,
                            submission_id,
                            EventMsg::Warning {
                                message: format!(
                                    "Stop hook blocked turn completion ({stop_hook_blocks}/{MAX_STOP_HOOK_BLOCKS})"
                                ),
                            },
                        )
                        .await;
                        let reason = if stderr.is_empty() {
                            "(hook 未给出原因)".to_owned()
                        } else {
                            stderr
                        };
                        self.push_message(Message {
                            role: Role::User,
                            content: vec![ContentBlock::Text {
                                text: format!("A Stop hook blocked turn completion:\n{reason}"),
                            }],
                        });
                        continue;
                    }
                }
                break stop_reason; // 无工具调用且非续写/steering/Stop 阻塞情形：进入步骤 6 终态
            }
            // 模型再次发起 tool_use：steering 连续计数清零。
            todo_steerings = 0;

            // 安全点：工具执行前检查中断。assistant 消息已入历史，
            // 为悬空 tool_use 合成 interrupted 结果保持配对。
            if self.interrupted.load(Ordering::SeqCst) {
                self.push_pairing_results(round.preset_results, "interrupted by user");
                emit(
                    &events,
                    submission_id,
                    EventMsg::TurnCompleted {
                        stop_reason: StopReason::Interrupted,
                    },
                )
                .await;
                return Ok(StopReason::Interrupted);
            }

            // —— 步骤 5：工具编排执行，结果作为一个 user 消息回灌，回步骤 2 ——
            let results = self
                .execute_tool_calls(&events, submission_id, round.preset_results, &tool_ctx)
                .await;
            self.push_message(Message {
                role: Role::User,
                content: results,
            });
        };

        // —— 步骤 6：终态（end_turn 或无 tool_use 的其他终态）——
        let last_input_tokens = last_input_tokens.expect("每轮采样后必先赋值才 break");
        if stop_reason == "max_tokens" {
            // 续写已达上限仍截断：警告后按 Completed 收尾（历史保留部分产出）。
            emit(
                &events,
                submission_id,
                EventMsg::Warning {
                    message: "output truncated: max_tokens reached".into(),
                },
            )
            .await;
        }
        // 权威占用跨 turn 结转：下一 turn 首次 PreTurn 检查用。
        self.usage_carry = Some(last_input_tokens + total_output_tokens);
        emit(
            &events,
            submission_id,
            EventMsg::TokenCount {
                used: last_input_tokens + total_output_tokens,
                window: self.cfg.context_window,
            },
        )
        .await;
        emit(
            &events,
            submission_id,
            EventMsg::TurnCompleted {
                stop_reason: StopReason::Completed,
            },
        )
        .await;
        tracing::debug!(turn_id = %turn_id, %stop_reason, "turn 结束");
        Ok(StopReason::Completed)
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

    /// P7：slash 直调 skill（`Op::SlashCommand` 路由入口，SPEC §8.2
    /// `/name [args]`）。
    ///
    /// - inline：展开正文作为 turn 输入驱动一轮完整 turn（`$ARGUMENTS`
    ///   替换在展开内完成；`allowed-tools` 经 run_turn_inner 注入为
    ///   turn 级白名单）；
    /// - fork：以 skill 正文为指令派生**后台**子代理（事件汇挂到本
    ///   submission 使 SubagentStarted/Completed 立即可见；终态通知按
    ///   既有机制在下一 turn 循环头回注——slash 派生不阻塞 actor）；
    /// - 错误（无技能面 / 未知名 / 不可直调 / fork 无子代理能力）发
    ///   Error 事件（recoverable）后收尾。任何路径都以 TurnCompleted
    ///   结束（前端按 TurnCompleted 收敛一次 slash 交互）。
    pub async fn invoke_skill(
        &mut self,
        submission_id: &str,
        name: &str,
        args: &str,
        events: mpsc::Sender<Event>,
    ) -> anyhow::Result<()> {
        let finish = |events: mpsc::Sender<Event>| async move {
            emit(
                &events,
                submission_id,
                EventMsg::TurnCompleted {
                    stop_reason: StopReason::Completed,
                },
            )
            .await;
        };
        let fail = |events: mpsc::Sender<Event>, message: String| async move {
            emit(
                &events,
                submission_id,
                EventMsg::Error {
                    message,
                    recoverable: true,
                },
            )
            .await;
            emit(
                &events,
                submission_id,
                EventMsg::TurnCompleted {
                    stop_reason: StopReason::Completed,
                },
            )
            .await;
        };
        let Some(skills) = &self.cfg.skills else {
            fail(events, "skills 不可用（启动时未发现任何 skill）".to_owned()).await;
            return Ok(());
        };
        match crate::skills::plan_invocation(&skills.set, name, args, true) {
            Err(reason) => {
                fail(events, reason).await;
            }
            Ok(crate::skills::SkillInvocation::Inline(expanded)) => {
                let allowed = self
                    .cfg
                    .skills
                    .as_ref()
                    .and_then(|s| s.set.get(name))
                    .filter(|skill| !skill.meta.allowed_tools.is_empty())
                    .map(|skill| skill.meta.allowed_tools.clone());
                // inline：展开正文作为 turn 输入（历史里可见完整展开——
                // slash 直调的透明性）；run_turn 自身发 TurnCompleted。
                self.run_turn_inner(submission_id, &expanded, events, allowed)
                    .await?;
            }
            Ok(crate::skills::SkillInvocation::Fork(spec)) => {
                let Some(mgr) = &self.subagents else {
                    fail(
                        events,
                        format!("skill `{name}` 需要子代理能力（context: fork），当前会话不可用"),
                    )
                    .await;
                    return Ok(());
                };
                // 事件汇挂到本 submission：SubagentStarted/Completed 随本次
                // slash 交互可见（中间过程不进父会话，同 P5 纪律）。
                mgr.set_event_sink(events.clone(), submission_id);
                mgr.spawn_background(spec);
                finish(events).await;
            }
        }
        Ok(())
    }

    /// P6：记忆自动提取（简化首版，SPEC §7.2）——可 awaiting 的入口：
    /// 派生同步子代理从会话历史提炼候选条目并追加到存储，返回写入条数。
    /// 无记忆配置 / 空历史 → Ok(0)；子代理失败以 Err 上抛（调用方决定
    /// 静默策略，见 [`Session::spawn_memory_extraction`]）。
    pub async fn extract_memories(&self) -> anyhow::Result<usize> {
        let Some(mem) = &self.cfg.memory else {
            return Ok(0);
        };
        if self.messages.is_empty() {
            return Ok(0);
        }
        let mgr = crate::subagent::SubagentManager::from_config(&self.cfg);
        crate::memory::extract_with_manager(mgr, self.messages.clone(), mem.store_root.clone())
            .await
    }

    /// P6：后台派生记忆自动提取（SessionEnd 挂接点：app-server actor 的
    /// Shutdown / 客户端断开路径）。detached tokio 任务——失败静默记
    /// warning，不阻塞退出；进程随即退出时任务可能未跑完，提取是尽力而
    /// 为语义（诚实声明，与 SPEC"不阻塞主会话"一致）。
    pub fn spawn_memory_extraction(&self) {
        if let Some(handle) = self.memory_extraction_handle() {
            handle.spawn();
        }
    }

    /// 预取记忆自动提取句柄（无记忆配置 / 空历史 → None）。
    /// `run_turn(&mut self)` 借用期间无法经 `&self` 调用——actor 在驱动
    /// turn 前预取，供 in-turn Shutdown 路径使用。
    pub fn memory_extraction_handle(&self) -> Option<MemoryExtractionHandle> {
        let mem = self.cfg.memory.as_ref()?;
        if self.messages.is_empty() {
            return None;
        }
        Some(MemoryExtractionHandle {
            mgr: crate::subagent::SubagentManager::from_config(&self.cfg),
            history: self.messages.clone(),
            store_root: mem.store_root.clone(),
        })
    }

    /// AwaitApproval（SPEC §5.1）：park 等待驱动方经共享槽回填决策；
    /// 等待中中断标志同样生效（审批等待也是中断安全点）。
    async fn await_approval(&self, call_id: &str) -> ApprovalWait {
        loop {
            // 先取决策再查中断：决策已到达按决策走（与工具执行前的中断
            // 检查点同序——中断不撤销已就绪的结果）。
            if let Some(decision) = self.approval_gate.take(call_id) {
                return ApprovalWait::Decision(decision);
            }
            if self.interrupted.load(Ordering::SeqCst) {
                return ApprovalWait::Interrupted;
            }
            tokio::select! {
                // 正常路径：actor 存决策时 notify_one 即时唤醒（permit 留存，
                // 与 take 无丢失唤醒竞态）。
                _ = self.approval_gate.notify.notified() => {}
                // 兜底轮询：裸 interrupt_handle 驱动形态（无人戳 gate，如
                // 单测直接 run_turn）也能在一个间隔内观察到中断。
                _ = tokio::time::sleep(APPROVAL_POLL_INTERVAL) => {}
            }
        }
    }

    /// 流内中断收尾：部分结果保留入历史；悬空 tool_use 合成结果保持配对。
    async fn finish_interrupted(
        &mut self,
        events: &mpsc::Sender<Event>,
        submission_id: &str,
        mut round: RoundBlocks,
    ) -> StopReason {
        let blocks = round.finish();
        if !blocks.is_empty() {
            self.push_message(Message {
                role: Role::Assistant,
                content: blocks,
            });
            self.push_pairing_results(round.preset_results, "interrupted by user");
        }
        emit(
            events,
            submission_id,
            EventMsg::TurnCompleted {
                stop_reason: StopReason::Interrupted,
            },
        )
        .await;
        tracing::debug!("turn 被中断，历史保留部分结果");
        StopReason::Interrupted
    }

    /// 为历史末尾 assistant 消息中的 tool_use 块补齐 ToolResult（user 消息），
    /// 保持 Anthropic tool_use/tool_result 配对约束：优先用预置结果
    ///（invalid json），其余合成 `fallback_content` 的 is_error 结果；
    /// 顺序与 tool_use 声明序一致。
    fn push_pairing_results(&mut self, mut presets: Vec<ContentBlock>, fallback_content: &str) {
        let Some(last) = self.messages.last() else {
            return;
        };
        if last.role != Role::Assistant {
            return;
        }
        let mut results = Vec::new();
        for block in &last.content {
            if let ContentBlock::ToolUse { id, .. } = block {
                let preset = presets
                    .iter()
                    .position(
                        |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
                    )
                    .map(|i| presets.remove(i));
                results.push(preset.unwrap_or(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: fallback_content.to_owned(),
                    is_error: true,
                }));
            }
        }
        if !results.is_empty() {
            self.push_message(Message {
                role: Role::User,
                content: results,
            });
        }
    }

    /// 步骤 5：取历史末尾 assistant 消息的 tool_use 块，编排执行并返回
    /// ToolResult 块数组（顺序与模型声明序一致）。
    ///
    /// 执行分组：批内 `is_read_only()` 的调用 `join_all` 并行（保序），
    /// 非只读串行。事件序：先按声明序发出全部 ToolCallBegin，执行结束后
    /// 按声明序发出全部 ToolCallEnd——并行批内无法逐调用穿插
    /// begin/execute/end，统一前置/后置是最简单且保序的形态。
    async fn execute_tool_calls(
        &self,
        events: &mpsc::Sender<Event>,
        submission_id: &str,
        mut preset_results: Vec<ContentBlock>,
        tool_ctx: &ToolCtx,
    ) -> Vec<ContentBlock> {
        // 声明序的调用清单：`(id, name, input)`。
        let calls: Vec<(String, String, serde_json::Value)> = self
            .messages
            .last()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        for (id, name, input) in &calls {
            emit(
                events,
                submission_id,
                EventMsg::ToolCallBegin {
                    call_id: id.clone(),
                    tool: name.clone(),
                    input: input.clone(),
                },
            )
            .await;
        }

        // 结果槽位（按声明序）；invalid-json 预置结果与 unknown-tool 先填，
        // 均不实际执行、不中断 turn。`executed` 标记实际执行过的调用
        //（PostToolUse hook 只对实际执行触发——预置 / 拦截 / 审批拒绝不算）。
        let mut slots: Vec<Option<ToolOutput>> = calls.iter().map(|_| None).collect();
        let mut executed = vec![false; calls.len()];
        for (i, (id, name, input)) in calls.iter().enumerate() {
            let preset = preset_results
                .iter()
                .position(
                    |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
                )
                .map(|j| preset_results.remove(j));
            if let Some(ContentBlock::ToolResult {
                content, is_error, ..
            }) = preset
            {
                slots[i] = Some(ToolOutput { content, is_error });
            } else if self.cfg.registry.get(name).is_none() {
                slots[i] = Some(ToolOutput {
                    content: format!("unknown tool: {name}"),
                    is_error: true,
                });
                continue;
            }
            if slots[i].is_some() {
                continue;
            }
            // P7：skill 激活的工具面白名单（allowed-tools，SPEC §8.2）——
            // 在 hook 与审批之前拦截：名单外工具直接 is_error 回灌，不实际
            // 执行（skill 工具自身也受限：白名单不含 `skill` 时激活后不能
            // 再触发其他 skill，首版语义见 skills 模块注释）。
            if !self.cfg.registry.allowlist().is_allowed(name) {
                slots[i] = Some(ToolOutput {
                    content: format!(
                        "tool `{name}` is not in the active skill's allowed-tools; the call was \
                         blocked (no changes were made)"
                    ),
                    is_error: true,
                });
                continue;
            }
            // P7：PreToolUse hook（可阻塞，SPEC §9 / §11.1 管道顺序：查找
            // → PreToolUse → 审批 → execute）。退出码 2 阻塞：stderr 以
            // is_error ToolResult 回灌模型，工具不执行、不进审批。
            if let Some(engine) = &self.cfg.hooks {
                let report = engine
                    .run(
                        wavecode_hooks::HookEventPoint::PreToolUse,
                        &wavecode_hooks::HookInput {
                            cwd: &tool_ctx.cwd,
                            tool_name: Some(name),
                            tool_input: Some(input),
                            tool_output: None,
                        },
                    )
                    .await;
                emit_hook_warnings(events, submission_id, &report.warnings).await;
                if let wavecode_hooks::HookVerdict::Block(stderr) = report.verdict {
                    let reason = if stderr.is_empty() {
                        "(hook 未给出原因)".to_owned()
                    } else {
                        stderr
                    };
                    slots[i] = Some(ToolOutput {
                        content: format!("blocked by PreToolUse hook:\n{reason}"),
                        is_error: true,
                    });
                    continue;
                }
            }
        }

        // 只读调用 join_all 并行（保序）。内置工具 execute 已为真 async
        //（tokio::fs / tokio::process；grep/glob 内部包 spawn_blocking 自理
        // 阻塞遍历），编排层直接 await 即得真实并行，无需再垫 spawn_blocking。
        // 知情延后：批内重排为"全部只读并行 → 非只读串行"，如 [R1, W1, R2]
        // 实际执行 R1∥R2 → W1——R2 读到 W1 写入前的内容；
        // 后续考虑按连续只读段分组以贴近声明执行序。
        // 只读但标记 destructive 的工具不进并行批：破坏性调用一律走串行段
        // 过审批门（P2，SPEC §12"破坏性工具默认需审批"）。
        let read_only: Vec<usize> = (0..calls.len())
            .filter(|&i| {
                slots[i].is_none()
                    && self
                        .cfg
                        .registry
                        .get(&calls[i].1)
                        .is_some_and(|t| t.is_read_only() && !t.is_destructive())
            })
            .collect();
        let ro_futures: Vec<_> = read_only
            .iter()
            .map(|&i| {
                // slots[i] 为空即工具存在（unknown 已预填），此处 expect 不会触发。
                let tool = self.cfg.registry.get(&calls[i].1).expect("tool 已判定存在");
                let input = calls[i].2.clone();
                let ctx = tool_ctx.clone();
                async move { tool.execute(input, &ctx).await }
            })
            .collect();
        let ro_outputs = futures::future::join_all(ro_futures).await;
        for (&i, out) in read_only.iter().zip(ro_outputs) {
            slots[i] = Some(output_or_err(out));
            executed[i] = true;
        }

        // 非只读（及只读但破坏性）串行（execute 同为真 async，直接 await）。
        for i in 0..calls.len() {
            if slots[i].is_none() {
                // 安全点：串行迭代间检查中断；剩余调用以 interrupted 结果
                // 收尾（不 break——ToolResult 必须与全部 tool_use 配对）。
                if self.interrupted.load(Ordering::SeqCst) {
                    slots[i] = Some(ToolOutput {
                        content: "interrupted by user".to_owned(),
                        is_error: true,
                    });
                    continue;
                }
                let tool = self.cfg.registry.get(&calls[i].1).expect("tool 已判定存在");
                // P2 审批门（SPEC §5.1 AwaitApproval）：执行前经 sandbox 判定。
                // Deny 不实际执行，reason 以 is_error 结果回灌模型；Ask 发
                // ApprovalRequested 事件并 park 等待 ExecApproval 回填。
                let verdict = self.cfg.sandbox.decide(
                    &calls[i].1,
                    &calls[i].2,
                    tool.is_read_only(),
                    tool.is_destructive(),
                );
                match verdict {
                    Verdict::Allow => {}
                    Verdict::Deny { reason } => {
                        slots[i] = Some(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                        continue;
                    }
                    Verdict::Ask { kind, detail } => {
                        emit(
                            events,
                            submission_id,
                            EventMsg::ApprovalRequested {
                                call_id: calls[i].0.clone(),
                                kind,
                                detail,
                            },
                        )
                        .await;
                        match self.await_approval(&calls[i].0).await {
                            ApprovalWait::Decision(ApprovalDecision::AllowOnce) => {}
                            ApprovalWait::Decision(ApprovalDecision::AllowAlways) => {
                                // TODO(P2 占位)：allow_always 应把该调用形态写入
                                // allow 规则；规则持久化待配置分层（§17.5 M3）
                                // 落地后接线，当前按 allow_once 处理（warn 留痕，
                                // 不静默吞掉语义差异）。
                                tracing::warn!(
                                    "allow_always 的规则写入未接线（P2 占位），按 allow_once 处理"
                                );
                            }
                            ApprovalWait::Decision(ApprovalDecision::Deny { reason }) => {
                                slots[i] = Some(ToolOutput {
                                    content: rejection_content(&reason),
                                    is_error: true,
                                });
                                continue;
                            }
                            ApprovalWait::Interrupted => {
                                // 与串行段中断检查点同形态：以 interrupted
                                // 结果收尾，后续调用在迭代头检查点同样收尾。
                                slots[i] = Some(ToolOutput {
                                    content: "interrupted by user".to_owned(),
                                    is_error: true,
                                });
                                continue;
                            }
                            // ApprovalDecision 标注 non_exhaustive：未来新增
                            // 决策变体按拒绝处理（安全默认），warn 留痕。
                            ApprovalWait::Decision(_) => {
                                tracing::warn!("未知审批决策变体，按拒绝处理");
                                slots[i] = Some(ToolOutput {
                                    content: "rejected: unsupported approval decision".to_owned(),
                                    is_error: true,
                                });
                                continue;
                            }
                        }
                    }
                }
                let out = tool.execute(calls[i].2.clone(), tool_ctx).await;
                slots[i] = Some(output_or_err(out));
                executed[i] = true;
            }
        }

        // ToolCallEnd + ToolResult 组装（声明序）。P7：PostToolUse hook
        //（不可阻塞，SPEC §9）挂在实际执行之后、ToolCallEnd 之前——只对
        // 实际执行的调用触发（预置 / 拦截 / 审批拒绝不算执行）。
        let mut results = Vec::with_capacity(calls.len());
        for (i, (id, name, input)) in calls.iter().enumerate() {
            let out = slots[i].take().expect("每个槽位必有结果");
            if executed[i]
                && let Some(engine) = &self.cfg.hooks
            {
                let report = engine
                    .run(
                        wavecode_hooks::HookEventPoint::PostToolUse,
                        &wavecode_hooks::HookInput {
                            cwd: &tool_ctx.cwd,
                            tool_name: Some(name),
                            tool_input: Some(input),
                            tool_output: Some(&out.content),
                        },
                    )
                    .await;
                emit_hook_warnings(events, submission_id, &report.warnings).await;
            }
            tracing::debug!(call_id = %id, ok = !out.is_error, "工具调用完成");
            emit(
                events,
                submission_id,
                EventMsg::ToolCallEnd {
                    call_id: id.clone(),
                    ok: !out.is_error,
                    output: out
                        .content
                        .chars()
                        .take(TOOL_OUTPUT_EVENT_MAX_CHARS)
                        .collect(),
                },
            )
            .await;
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: out.content,
                is_error: out.is_error,
            });
        }
        results
    }
}

/// execute 的 Err（io 故障等实现级错误）同样转为 is_error ToolResult
/// 回灌，不中断 turn。
fn output_or_err(out: wavecode_tools::Result<ToolOutput>) -> ToolOutput {
    match out {
        Ok(o) => o,
        Err(e) => ToolOutput {
            content: format!("tool execution failed: {e}"),
            is_error: true,
        },
    }
}

/// 识别 prompt_too_long 类错误（reactive compact 触发条件，SPEC §5.2）。
///
/// llm 的错误分类暂不够细：`LlmError::Api.kind` 只有 `http_{status}` 或
/// provider 的 error type 字符串（§17.5 M2 待办有 HttpKind 细化计划），
/// 故以 kind / message 字符串匹配识别已知形态（Anthropic 400
/// "prompt is too long" / 413 "request_too_large" 及 kind 含
/// prompt_too_long）。错误分类细化后应改为枚举匹配。
fn is_prompt_too_long(e: &LlmError) -> bool {
    let LlmError::Api { kind, message } = e else {
        return false;
    };
    kind.contains("prompt_too_long")
        || kind.contains("request_too_large")
        || message.contains("prompt is too long")
        || message.contains("prompt_too_long")
        || message.contains("request_too_large")
}

/// 用户拒绝审批的回灌文案：模型须能区分"被人拒绝"与"执行失败"；
/// 空原因补默认句，保证 reason 总是出现在回灌内容里。
fn rejection_content(reason: &str) -> String {
    let reason = reason.trim();
    if reason.is_empty() {
        "rejected by user (no reason given)".to_owned()
    } else {
        format!("rejected by user: {reason}")
    }
}

/// 错误路径收尾：TurnStarted 已发出，emit Error + TurnCompleted{Error}
/// 防前端悬挂等待；错误本身仍以 Err 返回调用方。
async fn fail_turn(events: &mpsc::Sender<Event>, submission_id: &str, message: String) {
    emit(
        events,
        submission_id,
        EventMsg::Error {
            message,
            recoverable: false,
        },
    )
    .await;
    emit(
        events,
        submission_id,
        EventMsg::TurnCompleted {
            stop_reason: StopReason::Error,
        },
    )
    .await;
}

/// 尽力投递事件：send 失败即 receiver 已关闭（前端断开），记 debug 日志
/// 并继续执行——M1 选择"继续"而非安全退出：历史一致性优先，事件流只是
/// 旁观通道，channel 满时 send 自然挂起形成背压，不会失败。
async fn emit(events: &mpsc::Sender<Event>, submission_id: &str, msg: EventMsg) {
    let ev = Event {
        id: submission_id.to_owned(),
        msg,
    };
    if events.send(ev).await.is_err() {
        tracing::debug!("事件接收端已断开，继续执行 turn（后续事件不再投递）");
    }
}

/// P7：hook 警告统一转 Warning 事件（非零退出码 / 超时 kill / spawn 失败
/// 等对前端可见；SPEC §9"警告放行"的可观测面）。
async fn emit_hook_warnings(
    events: &mpsc::Sender<Event>,
    submission_id: &str,
    warnings: &[String],
) {
    for message in warnings {
        emit(
            events,
            submission_id,
            EventMsg::Warning {
                message: message.clone(),
            },
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::sync::{Arc, Mutex};
    use wavecode_llm::{ChatModel, ChatRequest, StreamEvent, Usage};
    use wavecode_protocol::{Event, EventMsg, StopReason};

    /// 脚本化 mock：按调用次数返回预排事件序列
    struct MockModel {
        calls: Mutex<u32>,
        scripts: Vec<Vec<StreamEvent>>,
        /// 记录每次请求，供断言 tool_result 回灌
        seen: Mutex<Vec<ChatRequest>>,
    }

    impl MockModel {
        fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                calls: Mutex::new(0),
                scripts,
                seen: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait::async_trait]
    impl ChatModel for MockModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            self.seen.lock().unwrap().push(req);
            let mut n = self.calls.lock().unwrap();
            let idx = (*n as usize).min(self.scripts.len().saturating_sub(1));
            *n += 1;
            let events = self.scripts[idx].clone();
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
        }
    }

    /// 中断测试专用 mock：回放脚本后挂起，直到 `gate` 置位才再产出
    /// 一个 sentinel 事件并结束流——run_turn 流循环的 next() 收到它时
    /// 循环内中断检查点真正触发（覆盖 finish_interrupted）；
    /// sentinel 本身不会被分发处理（检查点在事件分发之前 return）。
    struct GatedModel {
        script: Vec<StreamEvent>,
        gate: Arc<AtomicBool>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for GatedModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            self.seen.lock().unwrap().push(req);
            let script = self.script.clone();
            let gate = self.gate.clone();
            let tail = stream::once(async move {
                while !gate.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                Ok(StreamEvent::TextDelta {
                    text: "tail-sentinel".into(),
                })
            });
            Ok(Box::pin(
                stream::iter(script.into_iter().map(Ok)).chain(tail),
            ))
        }
    }

    fn text_then_end(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta { text: text.into() },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 3,
                },
            },
        ]
    }

    /// 既有编排测试不涉审批：bypassPermissions 全放行，保持 P1 语义；
    /// 审批行为由 P2 专项测试（default / plan 模式）锁定。
    fn bypass_sandbox() -> Sandbox {
        Sandbox::without_rules(PermissionMode::BypassPermissions)
    }

    #[tokio::test]
    async fn turn_executes_tool_and_completes() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            vec![
                StreamEvent::TextDelta {
                    text: "好的，创建文件。".into(),
                },
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "write_file".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"path":"hello.txt","content":"hi"}"#.into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                },
            ],
            text_then_end("已创建。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session.run_turn("s-1", "创建 hello.txt", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        assert!(dir.path().join("hello.txt").exists());

        let mut msgs = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            msgs.push(ev.msg);
        }
        let kinds: Vec<String> = msgs
            .iter()
            .map(|m| {
                serde_json::to_value(m).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(kinds.first().unwrap(), "turn_started");
        assert!(kinds.contains(&"tool_call_begin".to_string()));
        assert!(kinds.contains(&"tool_call_end".to_string()));
        assert!(kinds.contains(&"token_count".to_string()));
        assert_eq!(kinds.last().unwrap(), "turn_completed");

        // tool_result 回灌：第二次请求的消息里应含 tool_result 块
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let second = &seen[1];
        let has_tool_result = second.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, wavecode_llm::ContentBlock::ToolResult { .. }))
        });
        assert!(has_tool_result);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_result_not_crash() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            vec![
                StreamEvent::ToolUseBegin {
                    id: "t9".into(),
                    name: "no_such_tool".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{}".into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                },
            ],
            text_then_end("工具不存在，换个方式。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session.run_turn("s-1", "试一下", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let mut tool_end_ok = None;
        while let Ok(ev) = rx.try_recv() {
            if let EventMsg::ToolCallEnd { ok, .. } = ev.msg {
                tool_end_ok = Some(ok);
            }
        }
        assert_eq!(tool_end_ok, Some(false));
        // 第二次请求应含 is_error=true 的 ToolResult 回灌
        let seen = model.seen.lock().unwrap();
        let second = &seen[1];
        let has_err_result = second.messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(
                    b,
                    wavecode_llm::ContentBlock::ToolResult { is_error: true, .. }
                )
            })
        });
        assert!(has_err_result);
    }

    #[tokio::test]
    async fn read_only_tools_run_in_batch_results_ordered() {
        // 两个只读调用一批发出：结果顺序须与声明序一致
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "A").unwrap();
        std::fs::write(dir.path().join("b.txt"), "B").unwrap();
        let scripts = vec![
            vec![
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "read_file".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"path":"a.txt"}"#.into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::ToolUseBegin {
                    id: "t2".into(),
                    name: "read_file".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"path":"b.txt"}"#.into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                },
            ],
            text_then_end("读完了。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        session.run_turn("s-1", "读两个文件", tx).await.unwrap();
        let seen = model.seen.lock().unwrap();
        let second = &seen[1];
        let results: Vec<&str> = second
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                wavecode_llm::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, vec!["A", "B"]);
    }

    #[tokio::test]
    async fn interrupt_in_stream_keeps_tool_pairing() {
        // 流给到一半（半个 tool_use）时中断：中断在流消费循环内被捕获
        //（finish_interrupted 路径），部分结果保留入历史，悬空 tool_use
        // 必须有配对的 is_error ToolResult。
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(AtomicBool::new(false));
        let model = Arc::new(GatedModel {
            script: vec![
                StreamEvent::TextDelta {
                    text: "先创建文件".into(),
                },
                // text 块先闭合（真实 SSE 形态），再开 tool_use 块
                StreamEvent::BlockEnd,
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "write_file".into(),
                },
                // 半个 input：之后无 BlockEnd / MessageComplete，流挂起
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"path":"x.txt""#.into(),
                },
            ],
            gate: gate.clone(),
            seen: Mutex::new(vec![]),
        });
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let handle = session.interrupt_handle();
        // join! 同 task 顺序 poll：run_turn 第一轮 poll 即从入口推进到
        // tail 挂起（脚本事件全部立即就绪、send 均不阻塞），故 signal
        // 收到 AgentMessageDelta 时 InputDelta 必已入 cur_tool——此时
        // 置位无竞争。handle 触发 session 中断（T8 驱动模式同款路径），
        // gate 放行 mock 流尾部产出 sentinel。
        let signal = async {
            let mut rx = rx;
            loop {
                let ev = rx.recv().await.unwrap();
                if matches!(&ev.msg, EventMsg::AgentMessageDelta { text } if text == "先创建文件")
                {
                    break;
                }
            }
            handle.store(true, Ordering::SeqCst);
            gate.store(true, Ordering::SeqCst);
            rx
        };
        let (reason, mut rx) = tokio::join!(session.run_turn("s-1", "干活", tx), signal);
        assert_eq!(reason.unwrap(), StopReason::Interrupted);
        // 中断于流消费循环内：无第二次采样请求
        assert_eq!(model.seen.lock().unwrap().len(), 1);
        // 未实际执行：半个 JSON 不触发 write_file
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());

        let mut saw_interrupted_complete = false;
        let mut saw_message_complete = false;
        while let Ok(ev) = rx.try_recv() {
            match ev.msg {
                EventMsg::TurnCompleted { stop_reason } => {
                    saw_interrupted_complete = stop_reason == StopReason::Interrupted;
                }
                EventMsg::AgentMessageComplete { .. } => saw_message_complete = true,
                _ => {}
            }
        }
        assert!(saw_interrupted_complete);
        // 触发点锁定：流内捕获（finish_interrupted）在步骤 4 之前
        // return，不会发出 AgentMessageComplete
        assert!(!saw_message_complete);

        // 历史保留部分结果（同文件测试模块可读私有字段）：
        // assistant 的悬空 tool_use 与 is_error ToolResult 配对
        let assistant = session
            .messages
            .iter()
            .find(|m| m.role == wavecode_llm::Role::Assistant)
            .expect("部分 assistant 消息应入历史");
        assert!(
            assistant
                .content
                .iter()
                .any(|b| matches!(b, wavecode_llm::ContentBlock::ToolUse { id, .. } if id == "t1"))
        );
        // sentinel 未入历史：流内检查点在事件分发前 return
        let full_text: String = assistant
            .content
            .iter()
            .filter_map(|b| match b {
                wavecode_llm::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(full_text, "先创建文件");
        let last = session.messages.last().unwrap();
        assert_eq!(last.role, wavecode_llm::Role::User);
        assert!(last.content.iter().any(
            |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1")
        ));
    }

    #[tokio::test]
    async fn invalid_tool_json_returns_error_result_not_execute() {
        // tool input JSON 解析失败：不实际执行，is_error 结果回灌且配对
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            vec![
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "write_file".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{not json".into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                },
            ],
            text_then_end("参数 JSON 坏了，重来。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session.run_turn("s-1", "写文件", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        // 未实际执行：tempdir 内不产生任何文件
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        let mut tool_end_ok = None;
        while let Ok(ev) = rx.try_recv() {
            if let EventMsg::ToolCallEnd { ok, .. } = ev.msg {
                tool_end_ok = Some(ok);
            }
        }
        assert_eq!(tool_end_ok, Some(false));
        // 第二次请求：assistant 的 tool_use 与 is_error ToolResult 配对回灌
        let seen = model.seen.lock().unwrap();
        let second = &seen[1];
        let has_tool_use = second.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, wavecode_llm::ContentBlock::ToolUse { id, .. } if id == "t1"))
        });
        assert!(has_tool_use);
        let has_err_pair = second.messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1"),
            )
        });
        assert!(has_err_pair);
    }

    #[tokio::test]
    async fn max_tokens_warns_then_completes() {
        // max_tokens 终态：先 Warning（message 含 max_tokens）再按 Completed 收尾
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![vec![
            StreamEvent::TextDelta {
                text: "写到一半被截断".into(),
            },
            StreamEvent::MessageComplete {
                stop_reason: "max_tokens".into(),
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 8192,
                },
            },
        ]];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session.run_turn("s-1", "写长文", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let mut has_warning = false;
        let mut completed = None;
        while let Ok(ev) = rx.try_recv() {
            match ev.msg {
                EventMsg::Warning { message } => {
                    assert!(message.contains("max_tokens"));
                    has_warning = true;
                }
                EventMsg::TurnCompleted { stop_reason } => completed = Some(stop_reason),
                _ => {}
            }
        }
        assert!(has_warning);
        assert_eq!(completed, Some(StopReason::Completed));
    }

    #[tokio::test]
    async fn interrupt_in_serial_tools_skips_resample() {
        // 中断落在串行工具执行段：剩余调用以 interrupted 收尾、结果完整
        // 回灌后，循环头检查点直接终结 turn——不发起第二次采样请求。
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "write_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"a.txt","content":"A"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::ToolUseBegin {
                id: "t2".into(),
                name: "write_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"b.txt","content":"B"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ]];
        let model = Arc::new(MockModel::new(scripts));
        // 容量 1 channel 形成逐滴同步：begin t2 的 send 必须等 signal
        // 取走 begin t1 才能完成——保证 signal 在串行段 i=0 检查点前
        // 完成置位（无 race）。
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let handle = session.interrupt_handle();
        // 收到首个 ToolCallBegin 即置位（此时串行执行段尚未开始）；
        // 之后继续 drain 直到 channel 关闭（run_turn 结束 tx drop），
        // 否则容量 1 下后续 send 会因无人接收而卡住。
        let signal = async {
            let mut rx = rx;
            let mut stored = false;
            let mut saw_interrupted_complete = false;
            while let Some(ev) = rx.recv().await {
                match ev.msg {
                    EventMsg::ToolCallBegin { .. } if !stored => {
                        handle.store(true, Ordering::SeqCst);
                        stored = true;
                    }
                    EventMsg::TurnCompleted { stop_reason } => {
                        saw_interrupted_complete = stop_reason == StopReason::Interrupted;
                    }
                    _ => {}
                }
            }
            saw_interrupted_complete
        };
        let (reason, saw_interrupted_complete) =
            tokio::join!(session.run_turn("s-1", "写两个文件", tx), signal);
        assert_eq!(reason.unwrap(), StopReason::Interrupted);
        // 循环头检查点：不发起第二次采样请求
        assert_eq!(model.seen.lock().unwrap().len(), 1);
        // 串行段检查点命中：两个 write_file 均未实际执行
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        assert!(saw_interrupted_complete);

        // 配对完整：末尾 user 消息按声明序含 t1/t2 两条 is_error ToolResult
        let last = session.messages.last().unwrap();
        assert_eq!(last.role, wavecode_llm::Role::User);
        let results: Vec<&str> = last
            .content
            .iter()
            .filter_map(|b| match b {
                wavecode_llm::ContentBlock::ToolResult {
                    tool_use_id,
                    is_error: true,
                    ..
                } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, vec!["t1", "t2"]);
    }

    /// 异步延迟 mock 工具：execute 内 tokio sleep 让出 executor，
    /// 用于验证只读批 join_all 的真实并行（编排层不再垫 spawn_blocking——
    /// 内置工具已是真 async，并行性由 future 本身的让出语义保证）。
    struct AsyncDelayTool {
        tool_name: &'static str,
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl wavecode_tools::Tool for AsyncDelayTool {
        fn name(&self) -> &str {
            self.tool_name
        }
        fn description(&self) -> &str {
            "read-only async-delay mock tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &wavecode_tools::ToolCtx,
        ) -> wavecode_tools::Result<ToolOutput> {
            tokio::time::sleep(self.delay).await;
            Ok(ToolOutput {
                content: self.tool_name.to_owned(),
                is_error: false,
            })
        }
    }

    #[tokio::test]
    async fn read_only_tools_run_in_parallel() {
        // 两个 200ms 延迟工具同批只读：join_all 并发下总耗时 ≈200ms；
        // 串行 await 则 ≥400ms。阈值 350ms 消除调度抖动。
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            vec![
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "slow_tool".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{}".into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::ToolUseBegin {
                    id: "t2".into(),
                    name: "fast_tool".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{}".into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                },
            ],
            text_then_end("并行读完了。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let mut registry = wavecode_tools::Registry::builtin();
        registry.register(Arc::new(AsyncDelayTool {
            tool_name: "slow_tool",
            delay: std::time::Duration::from_millis(200),
        }));
        registry.register(Arc::new(AsyncDelayTool {
            tool_name: "fast_tool",
            delay: std::time::Duration::from_millis(200),
        }));
        let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry,
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let start = std::time::Instant::now();
        let reason = session.run_turn("s-1", "并行读", tx).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(reason, StopReason::Completed);
        assert!(
            elapsed < std::time::Duration::from_millis(350),
            "只读工具未真并行：耗时 {elapsed:?} ≥ 350ms（串行应 ≥400ms）"
        );
        // 结果按声明序回灌（配对完整才发起第二轮采样）
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let results: Vec<&str> = seen[1]
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                wavecode_llm::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, vec!["slow_tool", "fast_tool"]);
    }

    /// 探针工具：记录执行时看到的 `ToolCtx.deny_env`，锁定
    /// SessionConfig → ToolCtx 的透传接线。
    struct CtxProbe {
        seen: Mutex<Option<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl wavecode_tools::Tool for CtxProbe {
        fn name(&self) -> &str {
            "ctx_probe"
        }
        fn description(&self) -> &str {
            "records ToolCtx.deny_env"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            ctx: &wavecode_tools::ToolCtx,
        ) -> wavecode_tools::Result<ToolOutput> {
            *self.seen.lock().unwrap() = Some(ctx.deny_env.clone());
            Ok(ToolOutput {
                content: "ok".into(),
                is_error: false,
            })
        }
    }

    /// deny_env 接线（批 C）：SessionConfig.deny_env 须原样透传到
    /// 工具执行时的 ToolCtx（shell 的 env 剔除依赖此通道）。
    #[tokio::test]
    async fn deny_env_flows_to_tool_ctx() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            vec![
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "ctx_probe".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{}".into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                },
            ],
            text_then_end("探测完毕。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let probe = Arc::new(CtxProbe {
            seen: Mutex::new(None),
        });
        let mut registry = wavecode_tools::Registry::builtin();
        registry.register(probe.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model,
            registry,
            cwd: dir.path().to_path_buf(),
            deny_env: vec!["MINIMAX_KEY".to_owned()],
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session.run_turn("s-1", "探测", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        assert_eq!(
            *probe.seen.lock().unwrap(),
            Some(vec!["MINIMAX_KEY".to_owned()]),
            "ToolCtx.deny_env 应透传 SessionConfig 的名单"
        );
    }

    #[tokio::test]
    async fn turn_uses_grep_and_glob_via_registry() {
        // P1 新工具接线验证：模型经 Registry 调 grep + glob（同为只读，
        // 一个并行批），结果正确回灌；请求侧 ToolSpec 清单含两个新工具。
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("src/b.txt"), "hello\n").unwrap();
        let scripts = vec![
            vec![
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "grep".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"pattern":"fn main"}"#.into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::ToolUseBegin {
                    id: "t2".into(),
                    name: "glob".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"pattern":"src/**/*.rs"}"#.into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                },
            ],
            text_then_end("检索完成。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session.run_turn("s-1", "找入口函数", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        // 请求侧 specs 含新工具
        let names: Vec<&str> = seen[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"grep") && names.contains(&"glob"));
        // 结果按声明序回灌：grep 带行号匹配，glob 列相对路径
        let results: Vec<&str> = seen[1]
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                wavecode_llm::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert!(results[0].contains("src/a.rs:1:fn main() {}"));
        assert!(results[0].contains("[1 matches in 1 files]"));
        assert_eq!(results[1], "src/a.rs");
    }

    /// P2 测试夹具：单轮 write_file 调用脚本（default 模式下触发审批门）。
    fn write_file_script(path: &str, content: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "write_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: format!(r#"{{"path":"{path}","content":"{content}"}}"#),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ]
    }

    /// P2 golden：审批放行——ApprovalRequested 事件 → ExecApproval 回填
    /// AllowOnce → 工具实际执行成功，非 is_error 结果回灌模型。
    #[tokio::test]
    async fn approval_allow_executes_tool_golden() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            write_file_script("hello.txt", "hi"),
            text_then_end("已创建。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: Sandbox::without_rules(PermissionMode::Default),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let gate = session.approval_handle();
        // 收到 ApprovalRequested 即回填放行决策（模拟前端 / actor 路由）；
        // 继续 drain 到通道关闭，顺带记录 ToolCallEnd。
        let signal = async {
            let mut rx = rx;
            let mut requested = None;
            let mut saw_ok_end = false;
            while let Some(ev) = rx.recv().await {
                match ev.msg {
                    EventMsg::ApprovalRequested {
                        call_id,
                        kind,
                        detail,
                    } => {
                        requested = Some((call_id.clone(), kind, detail));
                        gate.decide(call_id, wavecode_protocol::ApprovalDecision::AllowOnce);
                    }
                    EventMsg::ToolCallEnd { call_id, ok, .. } if call_id == "t1" => {
                        saw_ok_end = ok;
                    }
                    _ => {}
                }
            }
            (requested, saw_ok_end)
        };
        let (reason, (requested, saw_ok_end)) =
            tokio::join!(session.run_turn("s-1", "创建 hello.txt", tx), signal);
        assert_eq!(reason.unwrap(), StopReason::Completed);
        // 审批事件：call_id 关联、kind=Write、detail 含工具与路径
        let (call_id, kind, detail) = requested.expect("应发出 ApprovalRequested");
        assert_eq!(call_id, "t1");
        assert_eq!(kind, wavecode_protocol::ApprovalKind::Write);
        assert!(detail.contains("write_file") && detail.contains("hello.txt"));
        // 放行后实际执行
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "hi"
        );
        // 第二轮请求：非 is_error 的 ToolResult 回灌（配对 t1）
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let ok_result = seen[1].messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "t1"),
            )
        });
        assert!(ok_result, "放行结果应回灌: {:?}", seen[1].messages);
        // 事件流含 ToolCallEnd ok=true
        assert!(saw_ok_end);
    }

    /// P2 golden：审批拒绝——工具不执行，is_error 结果回灌且拒绝原因
    /// 出现在后续请求消息里。
    #[tokio::test]
    async fn approval_deny_skips_execution_and_feeds_reason() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            write_file_script("nope.txt", "x"),
            text_then_end("明白了，不写。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: Sandbox::without_rules(PermissionMode::Default),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let gate = session.approval_handle();
        let signal = async {
            let mut rx = rx;
            let mut saw_fail_end = false;
            while let Some(ev) = rx.recv().await {
                match ev.msg {
                    EventMsg::ApprovalRequested { call_id, .. } => {
                        gate.decide(
                            call_id,
                            wavecode_protocol::ApprovalDecision::Deny {
                                reason: "目录受保护，不要写".into(),
                            },
                        );
                    }
                    EventMsg::ToolCallEnd { ok, .. } => saw_fail_end = !ok,
                    _ => {}
                }
            }
            saw_fail_end
        };
        let (reason, saw_fail_end) =
            tokio::join!(session.run_turn("s-1", "创建 nope.txt", tx), signal);
        assert_eq!(reason.unwrap(), StopReason::Completed);
        // 未实际执行
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        assert!(saw_fail_end, "ToolCallEnd 应 ok=false");
        // 拒绝原因回灌模型：第二轮请求含 is_error ToolResult 且带原因原文
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let fed = seen[1].messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: true, content } if tool_use_id == "t1" && content.contains("目录受保护，不要写")),
            )
        });
        assert!(fed, "拒绝原因应回灌: {:?}", seen[1].messages);
    }

    /// P2：plan 模式拦截——写工具被 Deny（不发 ApprovalRequested、不执行），
    /// 拒绝原因回灌模型。
    #[tokio::test]
    async fn plan_mode_denies_write_tool_without_approval_request() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            write_file_script("plan.txt", "x"),
            text_then_end("plan 模式下只规划。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: Sandbox::without_rules(PermissionMode::Plan),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session.run_turn("s-1", "创建 plan.txt", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        // 不执行、不发审批请求
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        let mut saw_approval_request = false;
        let mut saw_fail_end = false;
        while let Ok(ev) = rx.try_recv() {
            match ev.msg {
                EventMsg::ApprovalRequested { .. } => saw_approval_request = true,
                EventMsg::ToolCallEnd { ok, .. } => saw_fail_end = !ok,
                _ => {}
            }
        }
        assert!(!saw_approval_request, "plan 模式拦截不应发审批请求");
        assert!(saw_fail_end, "ToolCallEnd 应 ok=false");
        // 拒绝原因（plan mode）回灌模型
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let fed = seen[1].messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { is_error: true, content, .. } if content.contains("plan mode")),
            )
        });
        assert!(fed, "plan 拦截原因应回灌: {:?}", seen[1].messages);
    }

    /// P2：审批等待中中断——park 在 AwaitApproval 时 interrupt 生效，
    /// 悬空 tool_use 以 interrupted 结果配对收尾，不发起第二次采样。
    ///（驱动方只置中断标志、不戳审批槽：走 APPROVAL_POLL_INTERVAL 兜底路径。）
    #[tokio::test]
    async fn interrupt_during_approval_wait_completes_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![write_file_script("x.txt", "x")];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: Sandbox::without_rules(PermissionMode::Default),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let interrupt = session.interrupt_handle();
        let signal = async {
            let mut rx = rx;
            let mut saw_interrupted_complete = false;
            while let Some(ev) = rx.recv().await {
                match ev.msg {
                    // 审批请求出现即中断（不回填决策：等待中的中断路径）
                    EventMsg::ApprovalRequested { .. } => {
                        interrupt.store(true, Ordering::SeqCst);
                    }
                    EventMsg::TurnCompleted { stop_reason } => {
                        saw_interrupted_complete = stop_reason == StopReason::Interrupted;
                    }
                    _ => {}
                }
            }
            saw_interrupted_complete
        };
        let (reason, saw_interrupted_complete) =
            tokio::join!(session.run_turn("s-1", "写文件", tx), signal);
        assert_eq!(reason.unwrap(), StopReason::Interrupted);
        assert!(saw_interrupted_complete);
        // 不执行、不再采样
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        assert_eq!(model.seen.lock().unwrap().len(), 1);
        // 配对完整：末尾 user 消息含 t1 的 is_error ToolResult
        let last = session.messages.last().unwrap();
        assert_eq!(last.role, wavecode_llm::Role::User);
        assert!(last.content.iter().any(
            |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1")
        ));
    }

    // ------------------------------------------------------------------
    // P3：上下文管线（PreTurn 三级阈值 / reactive compact / 续写 / /compact）
    // ------------------------------------------------------------------

    /// P3 测试夹具：识别摘要请求（ModelSummary 不带工具，tools 为空）回放
    /// 摘要脚本；采样请求按 `sampling` 队列逐次回放——`None` 表示该次
    /// 返回 prompt_too_long 类错误（reactive compact 触发条件）。
    struct CompactAwareMock {
        sampling: Mutex<Vec<Option<Vec<StreamEvent>>>>,
        summary_script: Vec<StreamEvent>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for CompactAwareMock {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            self.seen.lock().unwrap().push(req.clone());
            if req.tools.is_empty() {
                // 摘要请求：回放五要素摘要脚本
                return Ok(Box::pin(stream::iter(
                    self.summary_script.clone().into_iter().map(Ok),
                )));
            }
            let mut q = self.sampling.lock().unwrap();
            let next = if q.len() > 1 {
                q.remove(0)
            } else {
                q[0].clone()
            };
            match next {
                Some(events) => Ok(Box::pin(stream::iter(events.into_iter().map(Ok)))),
                None => Err(LlmError::Api {
                    kind: "http_400".into(),
                    message: "prompt is too long: 210000 tokens > 200000 maximum".into(),
                }),
            }
        }
    }

    /// 五要素摘要脚本（正文逐项含目标/进展/关键决策/文件清单/待办）。
    fn summary_script() -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta {
                text: "## 目标\nT\n## 进展\nP\n## 关键决策\nD\n## 文件清单\nF\n## 待办\nN".into(),
            },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                },
            },
        ]
    }

    fn p3_mock(sampling: Vec<Option<Vec<StreamEvent>>>) -> Arc<CompactAwareMock> {
        Arc::new(CompactAwareMock {
            sampling: Mutex::new(sampling),
            summary_script: summary_script(),
            seen: Mutex::new(vec![]),
        })
    }

    /// P3 配置：window=100_000，margin 200/100/10 → 三线 99800/99900/99990；
    /// 估算路径（~2k 开销定额）远低于警告线，threshold 测试经 usage_carry
    /// 种子精确控制水位；keep_recent=2 便于断言压缩后形态。
    fn p3_session(model: Arc<CompactAwareMock>) -> Session {
        Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 100_000,
            max_output_tokens: 8192,
            model,
            registry: wavecode_tools::Registry::builtin(),
            // tempdir 转持久路径放弃自动删除（与 app-server 测试同例）。
            cwd: tempfile::tempdir().unwrap().keep(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig {
                thresholds: wavecode_context::Thresholds {
                    warning_margin: 200,
                    auto_compact_margin: 100,
                    blocking_margin: 10,
                },
                keep_recent: 2,
                summary_max_tokens: 500,
                estimate_chars_per_token: 4,
            },
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        })
    }

    fn collect_events(rx: &mut mpsc::Receiver<Event>) -> Vec<EventMsg> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev.msg);
        }
        out
    }

    /// 阈值边界：used=99850 过警告线（99800）未及自动线（99900）——发一次
    /// Warning（"context near limit"），不压缩；两轮循环头检查只发一次。
    #[tokio::test]
    async fn preturn_warning_line_emits_warning_once_no_compact() {
        let model = p3_mock(vec![
            Some(vec![
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "read_file".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"path":"a.txt"}"#.into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage {
                        input_tokens: 99850,
                        output_tokens: 5,
                    },
                },
            ]),
            Some(text_then_end("读完了")),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = p3_session(model.clone());
        // 上一 turn 结转的权威占用：首次 PreTurn 检查即过警告线
        session.usage_carry = Some(99850);
        let reason = session.run_turn("s-1", "读文件", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let events = collect_events(&mut rx);
        let warnings: Vec<&EventMsg> = events
            .iter()
            .filter(|m| matches!(m, EventMsg::Warning { .. }))
            .collect();
        assert_eq!(warnings.len(), 1, "警告每 turn 至多一次: {events:?}");
        let EventMsg::Warning { message } = warnings[0] else {
            unreachable!()
        };
        assert!(message.contains("context near limit"));
        assert!(
            !events
                .iter()
                .any(|m| matches!(m, EventMsg::CompactStarted { .. })),
            "警告线不得触发压缩"
        );
    }

    /// 自动压缩线：used=99950 过自动线（99900）——PreTurn 触发压缩，
    /// CompactStarted{Auto} → CompactCompleted，随后采样请求的历史
    /// 首条为摘要消息；压缩后配对完整。
    #[tokio::test]
    async fn preturn_auto_line_compacts_before_sampling() {
        let model = p3_mock(vec![Some(text_then_end("好的"))]);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = p3_session(model.clone());
        session.usage_carry = Some(99950);
        let reason = session.run_turn("s-1", "继续干活", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);

        let events = collect_events(&mut rx);
        let started = events.iter().find_map(|m| match m {
            EventMsg::CompactStarted { trigger } => Some(*trigger),
            _ => None,
        });
        assert_eq!(
            started,
            Some(wavecode_protocol::CompactTrigger::Auto),
            "应以 Auto 触发压缩: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|m| matches!(m, EventMsg::CompactCompleted { .. }))
        );

        // 请求序：摘要（tools 空）→ 采样（历史首条为摘要消息）
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].tools.is_empty(), "首个请求应为摘要调用");
        let first = &seen[1].messages[0];
        assert!(
            matches!(&first.content[0], wavecode_llm::ContentBlock::Text { text } if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX)),
            "采样请求历史首条应为摘要消息: {:?}",
            seen[1].messages
        );
        // 摘要消息逐项含五要素（验收锚点：信息保留率）
        let wavecode_llm::ContentBlock::Text { text } = &first.content[0] else {
            unreachable!()
        };
        for element in ["目标", "进展", "关键决策", "文件清单", "待办"] {
            assert!(text.contains(element), "摘要缺要素「{element}」");
        }
        assert_eq!(
            wavecode_context::find_pairing_violations(&session.messages),
            Vec::<String>::new(),
            "压缩后历史配对须完整"
        );
    }

    /// 阻塞线：used=99995 过阻塞线（99990）——强制先压缩（Blocking 触发）再采样。
    #[tokio::test]
    async fn preturn_blocking_line_forces_compact_with_blocking_trigger() {
        let model = p3_mock(vec![Some(text_then_end("好的"))]);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = p3_session(model.clone());
        session.usage_carry = Some(99995);
        let reason = session.run_turn("s-1", "继续干活", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let events = collect_events(&mut rx);
        let started = events.iter().find_map(|m| match m {
            EventMsg::CompactStarted { trigger } => Some(*trigger),
            _ => None,
        });
        assert_eq!(
            started,
            Some(wavecode_protocol::CompactTrigger::Blocking),
            "阻塞线应以 Blocking 触发: {events:?}"
        );
        // 压缩先于采样完成
        let seen = model.seen.lock().unwrap();
        assert!(seen[0].tools.is_empty() && !seen[1].tools.is_empty());
    }

    /// reactive compact：首次采样 prompt_too_long → 压缩（Reactive）→
    /// 以压缩历史重试成功，turn 正常完成。
    #[tokio::test]
    async fn reactive_compact_recovers_from_prompt_too_long() {
        let model = p3_mock(vec![None, Some(text_then_end("压缩后重试成功"))]);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = p3_session(model.clone());
        let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);

        let events = collect_events(&mut rx);
        let started = events.iter().find_map(|m| match m {
            EventMsg::CompactStarted { trigger } => Some(*trigger),
            _ => None,
        });
        assert_eq!(
            started,
            Some(wavecode_protocol::CompactTrigger::Reactive),
            "prompt_too_long 应以 Reactive 触发: {events:?}"
        );

        // 请求序：采样（失败）→ 摘要 → 采样（压缩历史重试）
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(!seen[0].tools.is_empty() && seen[1].tools.is_empty());
        let retry_first = &seen[2].messages[0];
        assert!(
            matches!(&retry_first.content[0], wavecode_llm::ContentBlock::Text { text } if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX)),
            "重试应以压缩历史发起"
        );
    }

    /// reactive compact 熔断：连续 3 次 prompt_too_long → 熔断上报
    ///（Error + TurnCompleted{Error} + run_turn 返回 Err），期间压缩 2 次。
    #[tokio::test]
    async fn reactive_compact_circuit_breaks_after_three() {
        let model = p3_mock(vec![None]); // 队列复用末项：永远 prompt_too_long
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = p3_session(model.clone());
        let result = session.run_turn("s-1", "干活", tx).await;
        assert!(result.is_err(), "熔断后应上抛错误");

        let events = collect_events(&mut rx);
        let compacts = events
            .iter()
            .filter(|m| {
                matches!(m, EventMsg::CompactStarted { trigger } if *trigger == wavecode_protocol::CompactTrigger::Reactive)
            })
            .count();
        assert_eq!(compacts, 2, "3 次采样失败之间压缩 2 次: {events:?}");
        let error = events.iter().find_map(|m| match m {
            EventMsg::Error { message, .. } => Some(message.clone()),
            _ => None,
        });
        assert!(error.unwrap().contains("熔断"), "熔断须上报: {events:?}");
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::TurnCompleted {
                    stop_reason: StopReason::Error
                }
            )),
            "熔断 turn 应以 Error 收尾: {events:?}"
        );
        // 采样 3 次 + 摘要 2 次
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 5);
        assert_eq!(
            seen.iter().filter(|r| !r.tools.is_empty()).count(),
            3,
            "采样恰 3 次（熔断阈值）"
        );
    }

    /// max_output_tokens 续写：max_tokens 截断后以续写提示继续，
    /// 第二次截断再续一次，第三次正常结束——续写请求恰 2 次。
    #[tokio::test]
    async fn max_tokens_continues_up_to_twice() {
        let truncated = |text: &str| {
            Some(vec![
                StreamEvent::TextDelta { text: text.into() },
                StreamEvent::MessageComplete {
                    stop_reason: "max_tokens".into(),
                    usage: Usage {
                        input_tokens: 7,
                        output_tokens: 8192,
                    },
                },
            ])
        };
        let model = p3_mock(vec![
            truncated("前半"),
            truncated("中段"),
            Some(text_then_end("收尾")),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = p3_session(model.clone());
        let reason = session.run_turn("s-1", "写长文", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);

        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "初始 + 2 次续写");
        // 续写请求的历史末尾是续写提示（user 文本）
        for req in &seen[1..] {
            let last = req.messages.last().unwrap();
            assert!(
                matches!(&last.content[0], wavecode_llm::ContentBlock::Text { text } if text == CONTINUATION_PROMPT),
                "续写请求应以续写提示结尾: {:?}",
                req.messages
            );
        }
        // 两次续写警告，无"放弃"警告
        let events = collect_events(&mut rx);
        let warnings: Vec<String> = events
            .iter()
            .filter_map(|m| match m {
                EventMsg::Warning { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|w| w.contains("continuing")));
    }

    /// 续写熔断：连续 max_tokens 达上限后放弃——发 "max_tokens reached"
    /// 警告并按 Completed 收尾，采样恰 3 次（初始 + 2 续写）。
    #[tokio::test]
    async fn max_tokens_gives_up_after_two_continuations() {
        let model = p3_mock(vec![Some(vec![
            StreamEvent::TextDelta {
                text: "永远写不完".into(),
            },
            StreamEvent::MessageComplete {
                stop_reason: "max_tokens".into(),
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 8192,
                },
            },
        ])]);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = p3_session(model.clone());
        let reason = session.run_turn("s-1", "写长文", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        assert_eq!(model.seen.lock().unwrap().len(), 3, "初始 + 2 次续写后放弃");
        let events = collect_events(&mut rx);
        let last_warning = events.iter().rev().find_map(|m| match m {
            EventMsg::Warning { message } => Some(message.clone()),
            _ => None,
        });
        assert!(
            last_warning.unwrap().contains("max_tokens reached"),
            "放弃时须警告: {events:?}"
        );
    }

    /// `/compact`（Session::compact）：无论阈值立即压缩，CompactStarted
    /// {Manual} → CompactCompleted{summary_tokens}，历史首条为摘要消息。
    #[tokio::test]
    async fn manual_compact_via_session_method() {
        let model = p3_mock(vec![Some(text_then_end("unused"))]);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = p3_session(model.clone());
        let summary_tokens = session.compact("s-manual", tx).await.unwrap();
        assert!(summary_tokens > 0);

        let events = collect_events(&mut rx);
        let started = events.iter().find_map(|m| match m {
            EventMsg::CompactStarted { trigger } => Some(*trigger),
            _ => None,
        });
        assert_eq!(started, Some(wavecode_protocol::CompactTrigger::Manual));
        assert!(
            events
                .iter()
                .any(|m| matches!(m, EventMsg::CompactCompleted { summary_tokens: t } if *t == summary_tokens))
        );
        let first = &session.messages[0];
        assert!(
            matches!(&first.content[0], wavecode_llm::ContentBlock::Text { text } if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX))
        );
        assert_eq!(
            wavecode_context::find_pairing_violations(&session.messages),
            Vec::<String>::new()
        );
    }

    // —— P4 规划系统（todo_write / 清单注入 / stop steering）——

    /// todo_write 调用脚本（一轮：声明 + 输入 + 终态 tool_use）。
    fn todo_write_script(id: &str, todos_json: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseBegin {
                id: id.into(),
                name: "todo_write".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: todos_json.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ]
    }

    /// P4 会话构造：bypass 沙箱（todo_write 本就各模式免审批），返回
    /// session 与清单句柄（测试经句柄断言共享状态迁移）。
    fn p4_session(
        model: Arc<MockModel>,
        dir: &std::path::Path,
    ) -> (Session, wavecode_tools::TodoStore) {
        let registry = wavecode_tools::Registry::builtin();
        let todos = registry.todos();
        (
            Session::new(SessionConfig {
                model_name: "mock".into(),
                context_window: 200_000,
                max_output_tokens: 8192,
                model,
                registry,
                cwd: dir.to_path_buf(),
                deny_env: Vec::new(),
                sandbox: bypass_sandbox(),
                context: ContextConfig::default(),
                memory: None,
                skills: None,
                hooks: None,
                rollout: None,
            }),
            todos,
        )
    }

    /// P4 验收：mock 长任务 golden——模型先 todo_write 建立清单 → 逐步执行
    /// 并更新状态（事件流可观测状态迁移）→ 全部完成 → 收工（无 steering）。
    #[tokio::test]
    async fn todo_golden_task_lifecycle_observable() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            todo_write_script(
                "t1",
                r#"{"todos":[
                    {"content":"设计","status":"in_progress"},
                    {"content":"实现","status":"pending"},
                    {"content":"测试","status":"pending"}]}"#,
            ),
            todo_write_script(
                "t2",
                r#"{"todos":[
                    {"content":"设计","status":"completed"},
                    {"content":"实现","status":"in_progress"},
                    {"content":"测试","status":"pending"}]}"#,
            ),
            todo_write_script(
                "t3",
                r#"{"todos":[
                    {"content":"设计","status":"completed"},
                    {"content":"实现","status":"completed"},
                    {"content":"测试","status":"completed"}]}"#,
            ),
            text_then_end("全部完成。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let (mut session, todos) = p4_session(model.clone(), dir.path());
        let reason = session.run_turn("s-1", "完成三步任务", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        // 清单终态：全部 completed（共享句柄断言）。
        assert_eq!(todos.unfinished(), (0, 0));
        assert_eq!(todos.snapshot().len(), 3);

        // 事件流可观测状态迁移：3 次 todo_write 的 begin/end。
        let events = collect_events(&mut rx);
        let todo_begins = events
            .iter()
            .filter(|m| matches!(m, EventMsg::ToolCallBegin { tool, .. } if tool == "todo_write"))
            .count();
        assert_eq!(todo_begins, 3, "事件流应含 3 次 todo_write: {events:?}");
        // 全部完成后收工：无 steering 提醒。
        assert!(
            !events
                .iter()
                .any(|m| matches!(m, EventMsg::Warning { message } if message.contains("nudging"))),
            "清单全部完成不得 steering: {events:?}"
        );

        // 清单注入：第 2/3 轮请求的 system 尾部反映当轮清单快照。
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert!(seen[0].system.contains("(not a git repository)"));
        assert!(
            !seen[0].system.contains("<system-reminder>"),
            "首轮清单为空不注入"
        );
        assert!(
            seen[1].system.contains("1. [in_progress] 设计"),
            "第 2 轮注入首轮清单: {}",
            seen[1].system
        );
        assert!(
            seen[2].system.contains("1. [completed] 设计")
                && seen[2].system.contains("2. [in_progress] 实现"),
            "第 3 轮注入状态迁移后清单: {}",
            seen[2].system
        );
        // 前缀稳定：静态层恒为前缀。
        for req in seen.iter() {
            assert!(req.system.starts_with(crate::prompt::STATIC_LAYER));
        }
    }

    /// P4 验收：stop steering——清单有未完成项时模型想收工 → turn 继续并
    /// 注入提醒；连续 3 次（MAX_TODO_STEERINGS）后放行。
    #[tokio::test]
    async fn steering_continues_turn_until_limit() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            todo_write_script(
                "t1",
                r#"{"todos":[{"content":"未完成的活","status":"pending"}]}"#,
            ),
            // 之后每轮都想收工：连续 steering 3 次后放行。
            text_then_end("做完了。"),
            text_then_end("做完了。"),
            text_then_end("做完了。"),
            text_then_end("做完了。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let (mut session, _todos) = p4_session(model.clone(), dir.path());
        let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);

        // 轮次：1（todo）+ 1（首次收工）+ 3（steering）= 5 次采样。
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 5, "采样次数: 1 todo + 1 stop + 3 steering");
        // 第 5 次请求的历史里累计 3 条 steering 提醒。
        let nudges = seen[4]
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, wavecode_llm::ContentBlock::Text { text } if text.contains("unfinished items")))
            .count();
        assert_eq!(nudges, 3, "steering 提醒累计 3 条");
        // 事件流：3 次 steering Warning。
        let events = collect_events(&mut rx);
        let warnings = events
            .iter()
            .filter(|m| matches!(m, EventMsg::Warning { message } if message.contains("nudging")))
            .count();
        assert_eq!(warnings, 3, "steering Warning 3 次: {events:?}");
        // 前缀稳定：清单建立后未再变化，第 2 轮起各轮 system 字节相等。
        for w in seen[1..].windows(2) {
            assert_eq!(w[0].system, w[1].system, "清单不变时 system 须字节稳定");
        }
    }

    /// P4 验收：steering 后模型把清单更新为全部 completed → 不再 steering，
    /// 正常收工（连续计数之外的解除路径）。
    #[tokio::test]
    async fn steering_stops_once_list_completed() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = vec![
            todo_write_script(
                "t1",
                r#"{"todos":[{"content":"活","status":"in_progress"}]}"#,
            ),
            text_then_end("做完了。"), // 想收工 → steering #1
            todo_write_script("t2", r#"{"todos":[{"content":"活","status":"completed"}]}"#),
            text_then_end("全部完成。"), // 清单已清 → 正常收工
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
        let (mut session, _todos) = p4_session(model.clone(), dir.path());
        let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 4, "steering 1 次后清单完成即收工");
        let events = collect_events(&mut rx);
        let warnings = events
            .iter()
            .filter(|m| matches!(m, EventMsg::Warning { message } if message.contains("nudging")))
            .count();
        assert_eq!(warnings, 1, "仅 1 次 steering: {events:?}");
    }

    // ------------------------------------------------------------------
    // P6：记忆系统（memory_write 工具 / 审批挂接 / 跨会话召回 / 自动提取）
    // ------------------------------------------------------------------

    /// P6 测试夹具：memory_write 单轮调用脚本。
    fn memory_write_script(call_id: &str, category: &str, content: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseBegin {
                id: call_id.into(),
                name: "memory_write".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: format!(r#"{{"category":"{category}","content":"{content}"}}"#),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ]
    }

    /// P6 测试夹具：最小记忆配置（无指令记忆 / 索引，仅存储根）。
    fn p6_memory(store_root: &std::path::Path) -> Option<crate::memory::MemorySessionConfig> {
        Some(crate::memory::MemorySessionConfig {
            instruction_memory: String::new(),
            memory_index: String::new(),
            store_root: store_root.to_path_buf(),
        })
    }

    /// P6 验收：memory_write 审批挂接——default 模式下经 sandbox 非只读
    /// 默认策略给出 Ask（ApprovalRequested → ExecApproval 放行后才写入）。
    #[tokio::test]
    async fn memory_write_asks_in_default_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store_root = dir.path().join("memories");
        let scripts = vec![
            memory_write_script("t1", "user", "偏好紧凑回复"),
            text_then_end("已记住。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: Sandbox::without_rules(PermissionMode::Default),
            context: ContextConfig::default(),
            memory: p6_memory(&store_root),
            skills: None,
            hooks: None,
            rollout: None,
        });
        let gate = session.approval_handle();
        let signal = async {
            let mut rx = rx;
            let mut requested = None;
            while let Some(ev) = rx.recv().await {
                if let EventMsg::ApprovalRequested {
                    call_id,
                    kind,
                    detail,
                } = ev.msg
                {
                    requested = Some((kind, detail));
                    gate.decide(call_id, wavecode_protocol::ApprovalDecision::AllowOnce);
                }
            }
            requested
        };
        let (reason, requested) = tokio::join!(session.run_turn("s-1", "记住我的偏好", tx), signal);
        assert_eq!(reason.unwrap(), StopReason::Completed);
        let (kind, detail) = requested.expect("default 模式下 memory_write 应发审批请求");
        assert_eq!(kind, wavecode_protocol::ApprovalKind::Write);
        assert!(detail.contains("memory_write"));
        // 放行后实际写入：类别文件 + 索引。
        let store = crate::memory::MemoryStore::new(store_root);
        assert_eq!(
            store
                .read_category(crate::memory::MemoryCategory::User)
                .unwrap(),
            "- 偏好紧凑回复\n"
        );
        assert!(store.read_index().unwrap().contains("[user] 偏好紧凑回复"));
    }

    /// P6 验收：跨会话召回——会话 A 经 memory_write 写入条目；模拟会话 B
    /// 装配（启动时读索引 → 注入系统提示词槽位）：注入含索引条目，且
    /// 条目正文可按需加载。
    #[tokio::test]
    async fn cross_session_memory_recall() {
        let dir = tempfile::tempdir().unwrap();
        let store_root = dir.path().join("memories");
        // —— 会话 A：模型调用 memory_write 写入条目（bypass 免审批）——
        let scripts = vec![
            memory_write_script("t1", "project", "仓库用 pnpm 管理，不要引入 yarn"),
            text_then_end("已记录项目约定。"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session_a = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model,
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: p6_memory(&store_root),
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session_a.run_turn("s-1", "记住项目约定", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);

        // —— 会话 B 装配：启动时读索引（cli bootstrap 的同款路径）——
        let store = crate::memory::MemoryStore::new(store_root);
        let index = store.read_index().unwrap();
        assert!(
            index.contains("[project] 仓库用 pnpm 管理"),
            "索引应含条目: {index}"
        );
        let system = crate::prompt::build_system_prompt(dir.path(), "", "", &index, &[]).await;
        assert!(
            system.contains("# Persistent Memory Index"),
            "注入应含记忆索引段:\n{system}"
        );
        assert!(system.contains("[project] 仓库用 pnpm 管理"));
        // 条目正文按需加载（模型 read_file 的等价物）。
        let body = store
            .read_category(crate::memory::MemoryCategory::Project)
            .unwrap();
        assert!(body.contains("不要引入 yarn"), "条目正文可加载: {body}");
    }

    /// P6：自动提取——会话历史经提取子代理（mock 回放）提炼为带类别
    /// 标签的条目，解析后追加到存储（简化首版：纯追加式）。
    #[tokio::test]
    async fn memory_extraction_appends_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store_root = dir.path().join("memories");
        let scripts = vec![
            // 第 1 次采样：会话正文（建立历史）。
            text_then_end("好的，以后回复保持紧凑。另外这个项目用 pnpm。"),
            // 第 2 次采样：提取子代理的输出（约定线格式）。
            text_then_end("[user] 偏好紧凑回复\n[project] 仓库用 pnpm 管理"),
        ];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model,
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: p6_memory(&store_root),
            skills: None,
            hooks: None,
            rollout: None,
        });
        session.run_turn("s-1", "随便聊聊", tx).await.unwrap();

        let n = session.extract_memories().await.unwrap();
        assert_eq!(n, 2, "应提取 2 条");
        let store = crate::memory::MemoryStore::new(store_root);
        assert_eq!(
            store
                .read_category(crate::memory::MemoryCategory::User)
                .unwrap(),
            "- 偏好紧凑回复\n"
        );
        assert_eq!(
            store
                .read_category(crate::memory::MemoryCategory::Project)
                .unwrap(),
            "- 仓库用 pnpm 管理\n"
        );
        let index = store.read_index().unwrap();
        assert!(index.contains("[user]") && index.contains("[project]"));
    }

    /// P6：memory_write 参数校验（非法类别 / 空内容 → is_error 回灌，
    /// 不 panic、不写入）。
    #[tokio::test]
    async fn memory_write_validates_input() {
        use wavecode_tools::Tool as _;
        let dir = tempfile::tempdir().unwrap();
        let tool = crate::memory::MemoryWrite::new(dir.path().join("memories"));
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        let out = tool
            .execute(
                serde_json::json!({"category": "nope", "content": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error && out.content.contains("invalid category"));
        let out = tool
            .execute(
                serde_json::json!({"category": "user", "content": "  "}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error && out.content.contains("'content'"));
        // 未写入任何文件。
        let store = crate::memory::MemoryStore::new(dir.path().join("memories"));
        assert_eq!(store.read_index().unwrap(), "");
    }

    // —— P7：skills 与 hooks（SPEC §8 / §9 场景验收）——

    use wavecode_hooks::{HookDef, HookEngine, HookEventPoint};
    use wavecode_skills::{Skill, SkillContext, SkillMeta, SkillSet, SkillSource};

    /// P7 hook 命令构造（与 hooks crate 测试同款平台适配）：cmd 与 sh 都认
    /// `exit N`；stderr 输出分平台写法。stderr 断言用 ASCII（Windows cmd
    /// 按 GBK 输出非 ASCII，UTF-8 有损解码会替换）。
    fn p7_exit_cmd(code: u32, stderr: &str) -> String {
        if stderr.is_empty() {
            format!("exit {code}")
        } else if cfg!(windows) {
            format!("echo {stderr} 1>&2 & exit {code}")
        } else {
            format!("echo {stderr} 1>&2; exit {code}")
        }
    }

    /// 超时测试的"睡眠"命令（cmd 无 sleep，用 ping 占位）。
    fn p7_sleep_cmd() -> String {
        if cfg!(windows) {
            "ping -n 10 127.0.0.1 >nul".to_owned()
        } else {
            "sleep 10".to_owned()
        }
    }

    fn p7_hook_def(command: &str) -> HookDef {
        HookDef {
            matcher: None,
            command: command.to_owned(),
            timeout_ms: wavecode_hooks::DEFAULT_TIMEOUT_MS,
            once: false,
        }
    }

    fn p7_engine(entries: &[(HookEventPoint, HookDef)]) -> Arc<HookEngine> {
        let mut defs: std::collections::HashMap<HookEventPoint, Vec<HookDef>> =
            std::collections::HashMap::new();
        for (point, def) in entries {
            defs.entry(*point).or_default().push(def.clone());
        }
        Arc::new(HookEngine::new(defs))
    }

    fn p7_skill(name: &str, context: SkillContext, allowed: &[&str], body: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            // 直接以正斜杠字面量构造（join 在 Windows 用反斜杠，断言文本
            // 保持正斜杠形态——路径分隔符本身不在本测试语义内）。
            dir: std::path::PathBuf::from(format!("C:/skills/{name}")),
            source: SkillSource::Project,
            meta: SkillMeta {
                description: format!("{name} 描述"),
                when_to_use: Some("测试触发条件".to_owned()),
                allowed_tools: allowed.iter().map(|s| s.to_string()).collect(),
                context,
                user_invocable: true,
                argument_hint: None,
                paths: vec![],
            },
            body: body.to_owned(),
        }
    }

    fn p7_skill_set(skills: Vec<Skill>) -> Option<crate::skills::SkillSessionConfig> {
        let mut set = SkillSet::default();
        for skill in skills {
            set.add(skill);
        }
        Some(crate::skills::SkillSessionConfig { set: Arc::new(set) })
    }

    /// P7 会话构造：bypass 沙箱 + with_subagents（fork 派生面），可挂
    /// hooks / skills；模型用 CompactAwareMock（seen 记录全部采样请求，
    /// 供"回灌模型"断言）。
    fn p7_session(
        model: Arc<CompactAwareMock>,
        dir: &std::path::Path,
        hooks: Option<Arc<HookEngine>>,
        skills: Option<crate::skills::SkillSessionConfig>,
    ) -> Session {
        Session::with_subagents(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model,
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills,
            hooks,
            rollout: None,
        })
    }

    /// 采样请求历史的全文（text + tool_result + tool_use 摘要），
    /// 供"X 回灌模型出现在后续请求"断言。
    fn p7_history_text(req: &ChatRequest) -> String {
        req.messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| match b {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::ToolResult { content, .. } => content.clone(),
                ContentBlock::ToolUse { name, input, .. } => format!("[tool_use {name} {input}]"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// skill 工具调用脚本。
    fn p7_skill_tool_script(name: &str, args: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "skill".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: format!(r#"{{"name":"{name}","args":"{args}"}}"#),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ]
    }

    /// SPEC §8 验收：inline 展开——skill 工具触发展开正文（$ARGUMENTS
    /// 替换）并回灌模型（出现在后续采样请求历史）；清单注入（name +
    /// description + when_to_use）出现在系统提示词。
    #[tokio::test]
    async fn skill_tool_inline_expands_arguments_into_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let skills = p7_skill_set(vec![p7_skill(
            "fixit",
            SkillContext::Inline,
            &[],
            "修复 $ARGUMENTS（参考 ${WAVECODE_SKILL_DIR}/notes.md）",
        )]);
        let model = p3_mock(vec![
            Some(p7_skill_tool_script("fixit", "崩溃问题")),
            Some(text_then_end("已修复。")),
        ]);
        let (tx, _rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), None, skills);
        session.run_turn("s-1", "修一下", tx).await.unwrap();

        let seen = model.seen.lock().unwrap();
        assert!(seen.len() >= 2, "应至少两次采样: {}", seen.len());
        // 清单注入：name + description + when_to_use（SPEC §8.2 注入形态）。
        assert!(
            seen[0]
                .system
                .contains("- fixit: fixit 描述 (when: 测试触发条件)"),
            "清单应注入系统提示词:\n{}",
            seen[0].system
        );
        // inline 展开回灌：$ARGUMENTS 替换 + skill 目录变量替换。
        let history = p7_history_text(&seen[1]);
        assert!(
            history.contains("修复 崩溃问题"),
            "展开正文应回灌:\n{history}"
        );
        assert!(
            history.contains("C:/skills/fixit/notes.md"),
            "skill 目录变量应展开:\n{history}"
        );
    }

    /// SPEC §8 验收：fork 派生——skill 工具触发后台子代理（SubagentStarted
    /// 事件可见、ToolResult 回执 task id）；allowed-tools 按 registry 过滤
    /// 子代理工具面（子代理采样请求的 tools 恰为白名单）。
    #[tokio::test]
    async fn skill_tool_fork_spawns_subagent_with_filtered_registry() {
        let dir = tempfile::tempdir().unwrap();
        let skills = p7_skill_set(vec![p7_skill(
            "deepreview",
            SkillContext::Fork,
            &["read_file"],
            "评审 $ARGUMENTS",
        )]);
        let model = p3_mock(vec![
            Some(p7_skill_tool_script("deepreview", "src/")),
            Some(text_then_end("评审完成。")),
        ]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), None, skills);
        session.run_turn("s-1", "评审一下", tx).await.unwrap();

        // 后台子代理与父会话并发：等 SubagentCompleted 再断言（消除
        // "子代理尚未采样"的竞态），超时兜底防挂死。
        let mut events = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(ev) = rx.recv().await {
                let done = matches!(ev.msg, EventMsg::SubagentCompleted { .. });
                events.push(ev.msg);
                if done {
                    break;
                }
            }
        })
        .await
        .expect("5s 内应见 SubagentCompleted");
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::SubagentStarted { description, .. } if description == "skill: deepreview"
            )),
            "应见 skill fork 的 SubagentStarted: {events:?}"
        );
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::ToolCallEnd { ok: true, output, .. } if output.contains("task-")
            )),
            "skill 工具回执应含 task id: {events:?}"
        );
        // allowed-tools 过滤：子代理采样请求的 tools 恰为 ["read_file"]
        //（父会话请求带全量工具，按此特征定位子代理请求，与调度顺序无关）。
        let seen = model.seen.lock().unwrap();
        let child_req = seen
            .iter()
            .find(|req| {
                let mut names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
                names.sort_unstable();
                names == ["read_file"]
            })
            .expect("子代理请求的工具面应被 allowed-tools 过滤");
        // fork 指令：skill 正文（preamble）拼在子代理输入前部，args 进 prompt。
        let child_input = p7_history_text(child_req);
        assert!(child_input.contains("评审 src/"), "{child_input}");
    }

    /// SPEC §8 验收：`/name [args]` slash 直调（invoke_skill）——inline
    /// 展开正文作为 turn 输入（历史首条 user 消息含展开文本）。
    #[tokio::test]
    async fn slash_inline_skill_expands_as_turn_input() {
        let dir = tempfile::tempdir().unwrap();
        let skills = p7_skill_set(vec![p7_skill(
            "fixit",
            SkillContext::Inline,
            &[],
            "修复 $ARGUMENTS",
        )]);
        let model = p3_mock(vec![Some(text_then_end("done"))]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), None, skills);
        session
            .invoke_skill("s-1", "fixit", "崩溃问题", tx)
            .await
            .unwrap();

        {
            let seen = model.seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "inline slash 应驱动一轮 turn");
            assert!(
                p7_history_text(&seen[0]).contains("修复 崩溃问题"),
                "展开正文应为 turn 输入"
            );
        }
        let events = collect_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|m| matches!(m, EventMsg::TurnCompleted { .. })),
            "slash 交互应以 TurnCompleted 收尾: {events:?}"
        );
        // 未知 skill：Error + TurnCompleted，不发起采样。
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        session.invoke_skill("s-2", "nope", "", tx).await.unwrap();
        let events = collect_events(&mut rx);
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::Error { message, .. } if message.contains("unknown skill: nope")
            )),
            "{events:?}"
        );
        assert!(model.seen.lock().unwrap().len() == 1, "未知名不得发起采样");
    }

    /// SPEC §9 验收：PreToolUse 阻塞——退出码 2，工具不执行，stderr 回灌
    /// 模型（出现在后续采样请求历史）。
    #[tokio::test]
    async fn pre_tool_use_hook_blocks_and_stderr_reaches_model() {
        let dir = tempfile::tempdir().unwrap();
        let engine = p7_engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                matcher: Some("write_file".to_owned()),
                ..p7_hook_def(&p7_exit_cmd(2, "no-writes-today"))
            },
        )]);
        let model = p3_mock(vec![
            Some(write_file_script("blocked.txt", "x")),
            Some(text_then_end("被拦了。")),
        ]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
        session.run_turn("s-1", "写文件", tx).await.unwrap();

        assert!(
            !dir.path().join("blocked.txt").exists(),
            "阻塞的工具不得执行"
        );
        let events = collect_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|m| matches!(m, EventMsg::ToolCallEnd { ok: false, .. })),
            "阻塞应产生失败 ToolCallEnd: {events:?}"
        );
        let seen = model.seen.lock().unwrap();
        let history = p7_history_text(&seen[1]);
        assert!(
            history.contains("no-writes-today"),
            "stderr 应回灌模型:\n{history}"
        );
    }

    /// SPEC §9 验收：退出码 1——警告放行（Warning 事件可见，工具照常执行）。
    #[tokio::test]
    async fn pre_tool_use_hook_exit1_warns_and_allows() {
        let dir = tempfile::tempdir().unwrap();
        let engine = p7_engine(&[(
            HookEventPoint::PreToolUse,
            p7_hook_def(&p7_exit_cmd(1, "hook-oops")),
        )]);
        let model = p3_mock(vec![
            Some(write_file_script("ok.txt", "x")),
            Some(text_then_end("done")),
        ]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
        session.run_turn("s-1", "写文件", tx).await.unwrap();

        assert!(dir.path().join("ok.txt").exists(), "警告放行的工具应执行");
        let events = collect_events(&mut rx);
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::Warning { message } if message.contains("退出码 1") && message.contains("hook-oops")
            )),
            "退出码 1 应转 Warning 事件: {events:?}"
        );
    }

    /// SPEC §9 验收：超时强制 kill 记 warning（工具照常执行；hook 不拖死 turn）。
    #[tokio::test]
    async fn pre_tool_use_hook_timeout_killed_warns() {
        let dir = tempfile::tempdir().unwrap();
        let engine = p7_engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                timeout_ms: 200,
                ..p7_hook_def(&p7_sleep_cmd())
            },
        )]);
        let model = p3_mock(vec![
            Some(write_file_script("ok.txt", "x")),
            Some(text_then_end("done")),
        ]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
        session.run_turn("s-1", "写文件", tx).await.unwrap();

        assert!(
            dir.path().join("ok.txt").exists(),
            "超时警告放行的工具应执行"
        );
        let events = collect_events(&mut rx);
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::Warning { message } if message.contains("超时")
            )),
            "超时 kill 应转 Warning 事件: {events:?}"
        );
    }

    /// SPEC §9 验收：matcher 匹配——matcher 未命中的工具不触发 hook
    ///（无警告、正常执行）；命中的工具才触发。
    #[tokio::test]
    async fn pre_tool_use_hook_matcher_filters_tools() {
        let dir = tempfile::tempdir().unwrap();
        let engine = p7_engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                matcher: Some("shell".to_owned()),
                ..p7_hook_def(&p7_exit_cmd(2, "should-not-fire"))
            },
        )]);
        let model = p3_mock(vec![
            Some(write_file_script("ok.txt", "x")),
            Some(text_then_end("done")),
        ]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
        session.run_turn("s-1", "写文件", tx).await.unwrap();

        assert!(
            dir.path().join("ok.txt").exists(),
            "matcher 未命中时工具应正常执行"
        );
        let events = collect_events(&mut rx);
        assert!(
            !events.iter().any(|m| matches!(m, EventMsg::Warning { .. })),
            "matcher 未命中不得产生 hook 警告: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|m| matches!(m, EventMsg::ToolCallEnd { ok: true, .. })),
            "{events:?}"
        );
    }

    /// SPEC §9 / §5.2 验收：Stop hook 阻塞——stderr 作为 user 消息回灌模型
    /// 继续 turn；once 语义下第二次收尾放行（次序：先 todo steering 后
    /// Stop hook，本测试清单为空直接到 Stop hook）。
    #[tokio::test]
    async fn stop_hook_blocks_once_then_completes() {
        let dir = tempfile::tempdir().unwrap();
        let engine = p7_engine(&[(
            HookEventPoint::Stop,
            HookDef {
                once: true,
                ..p7_hook_def(&p7_exit_cmd(2, "goal-not-met"))
            },
        )]);
        let model = p3_mock(vec![
            Some(text_then_end("先收工。")),
            Some(text_then_end("补完了。")),
        ]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
        let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);

        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "Stop 阻塞应驱动额外一轮采样");
        let history = p7_history_text(&seen[1]);
        assert!(
            history.contains("goal-not-met"),
            "Stop hook stderr 应回灌模型:\n{history}"
        );
        drop(seen);
        let events = collect_events(&mut rx);
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::Warning { message } if message.contains("Stop hook blocked")
            )),
            "{events:?}"
        );
    }

    /// UserPromptSubmit hook 阻塞：输入不进历史、不发起采样，stderr 以
    /// Error 事件展示 + TurnCompleted 收尾（前端不悬挂）。
    #[tokio::test]
    async fn user_prompt_submit_hook_blocks_turn() {
        let dir = tempfile::tempdir().unwrap();
        let engine = p7_engine(&[(
            HookEventPoint::UserPromptSubmit,
            p7_hook_def(&p7_exit_cmd(2, "blocked-word")),
        )]);
        let model = p3_mock(vec![Some(text_then_end("不应到达"))]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
        session.run_turn("s-1", "敏感输入", tx).await.unwrap();

        assert!(
            model.seen.lock().unwrap().is_empty(),
            "阻塞的输入不得发起采样"
        );
        let events = collect_events(&mut rx);
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::Error { message, .. } if message.contains("blocked-word")
            )),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|m| matches!(m, EventMsg::TurnCompleted { .. })),
            "{events:?}"
        );
    }

    // —— P9：MCP 工具桥接（SPEC §10，真实 transport 未实现，mock client 验证） ——

    /// P9 测试夹具：假 MCP client——单工具 `echo`（回显 text 参数），
    /// 记录收到的 `(原始名, 输入)` 供断言。
    struct FakeMcpClient {
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait::async_trait]
    impl crate::mcp::McpClient for FakeMcpClient {
        async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolDef>, crate::mcp::McpError> {
            Ok(vec![crate::mcp::McpToolDef {
                name: "echo".into(),
                description: Some("Echo the input text back".into()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }),
            }])
        }

        async fn call_tool(
            &self,
            name: &str,
            input: serde_json::Value,
        ) -> Result<crate::mcp::McpToolOutput, crate::mcp::McpError> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_owned(), input.clone()));
            let text = input["text"].as_str().unwrap_or("");
            Ok(crate::mcp::McpToolOutput {
                content: format!("echo: {text}"),
                is_error: false,
            })
        }
    }

    /// P9 测试夹具：调用 `mcp__fake__echo` 的单轮脚本。
    fn mcp_echo_script(call_id: &str, text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseBegin {
                id: call_id.into(),
                name: "mcp__fake__echo".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: format!(r#"{{"text":"{text}"}}"#),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            },
        ]
    }

    /// P9 测试夹具：mock client 经 McpToolBridge 注册进 Registry
    ///（SPEC §10 命名注入点）。
    async fn p9_registry(client: Arc<FakeMcpClient>) -> wavecode_tools::Registry {
        let bridge = crate::mcp::McpToolBridge::new("fake", client);
        let mut registry = wavecode_tools::Registry::builtin();
        for tool in bridge.tools().await.unwrap() {
            registry.register(tool);
        }
        registry
    }

    /// P9 验收：mock McpClient 的工具经桥注册进 Registry（`mcp__fake__echo`
    /// 命名注入），经 turn 循环调用成功、结果回灌模型（第二轮采样请求中
    /// 可见 ToolResult），call_tool 收到的是 server 侧原始名。
    #[tokio::test]
    async fn mcp_bridged_tool_callable_in_turn() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(FakeMcpClient {
            calls: Mutex::new(vec![]),
        });
        let scripts = vec![mcp_echo_script("t1", "hi"), text_then_end("完成。")];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: p9_registry(client.clone()).await,
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let reason = session.run_turn("s-1", "调用 echo", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);

        // client 收到原始名（不含 mcp__ 前缀）与透传输入。
        assert_eq!(
            client.calls.lock().unwrap().as_slice(),
            &[("echo".to_owned(), serde_json::json!({"text": "hi"}))]
        );
        // 结果回灌：第二轮采样请求的历史中含 ToolResult "echo: hi"。
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let results: Vec<&str> = seen[1]
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                wavecode_llm::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, vec!["echo: hi"], "MCP 结果应回灌模型");
    }

    /// P9 验收：桥接工具走 sandbox 同一审批管道（非只读默认）——default
    /// 模式下调用前发 ApprovalRequested（detail 含 `mcp__fake__echo`），
    /// 放行后才实际调用 client。
    #[tokio::test]
    async fn mcp_bridged_tool_asks_in_default_mode() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(FakeMcpClient {
            calls: Mutex::new(vec![]),
        });
        let scripts = vec![mcp_echo_script("t1", "hi"), text_then_end("完成。")];
        let model = Arc::new(MockModel::new(scripts));
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: p9_registry(client.clone()).await,
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: Sandbox::without_rules(PermissionMode::Default),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        });
        let gate = session.approval_handle();
        let signal = async {
            let mut rx = rx;
            let mut requested = None;
            while let Some(ev) = rx.recv().await {
                if let EventMsg::ApprovalRequested {
                    call_id,
                    kind,
                    detail,
                } = ev.msg
                {
                    requested = Some((kind, detail));
                    gate.decide(call_id, wavecode_protocol::ApprovalDecision::AllowOnce);
                }
            }
            requested
        };
        let (reason, requested) = tokio::join!(session.run_turn("s-1", "调用 echo", tx), signal);
        assert_eq!(reason.unwrap(), StopReason::Completed);
        let (kind, detail) = requested.expect("default 模式下 MCP 工具应发审批请求");
        assert_eq!(kind, wavecode_protocol::ApprovalKind::Write);
        assert!(detail.contains("mcp__fake__echo"), "{detail}");
        // 放行后实际调用到达 client。
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    // ------------------------------------------------------------------
    // P10：会话持久化（rollout 写入 / replay 恢复 / 断点续跑 / 崩溃恢复）
    // 与长程硬化（压缩循环压力、泄漏粗检）
    // ------------------------------------------------------------------

    /// P10 测试夹具：注入临时根目录的 rollout 配置。
    fn p10_rollout(dir: &std::path::Path, thread_id: &str) -> Option<crate::rollout::RolloutConfig> {
        Some(crate::rollout::RolloutConfig {
            root: dir.join("threads"),
            thread_id: thread_id.to_owned(),
        })
    }

    /// P10 会话构造：bypass 沙箱 + 可挂 rollout（cwd 独立 tempdir，
    /// write_file 落点互不影响）。
    fn p10_session(
        model: Arc<dyn ChatModel>,
        rollout: Option<crate::rollout::RolloutConfig>,
    ) -> Session {
        Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model,
            registry: wavecode_tools::Registry::builtin(),
            cwd: tempfile::tempdir().unwrap().keep(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout,
        })
    }

    /// rollout 文件全部记录的序号清单（断言连续递增用）。
    fn p10_seqs(load: &crate::rollout::RolloutLoad) -> Vec<u64> {
        load.records.iter().map(|r| r.seq()).collect()
    }

    /// P10 验收锚点：rollout 写入 → replay 恢复 → 断点续跑。
    #[tokio::test]
    async fn rollout_records_turn_and_resume_continues() {
        let dir = tempfile::tempdir().unwrap();
        // —— 会话 A：一轮含工具调用的 turn，全程落 rollout ——
        let model_a = Arc::new(MockModel::new(vec![
            write_file_script("hello.txt", "hi"),
            text_then_end("已创建。"),
        ]));
        let (tx, _rx) = mpsc::channel::<Event>(64);
        let mut session_a = p10_session(model_a, p10_rollout(dir.path(), "thread-1"));
        let reason = session_a.run_turn("s-1", "创建 hello.txt", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let history_a = session_a.messages.clone();

        // rollout 文件：4 条消息记录（user / assistant tool_use /
        // tool_result user / assistant 文本），seq 从 1 连续递增。
        let path = dir.path().join("threads/thread-1.jsonl");
        let load = crate::rollout::load_rollout(&path).unwrap();
        assert!(load.warnings.is_empty(), "{:?}", load.warnings);
        assert_eq!(load.records.len(), 4);
        assert_eq!(p10_seqs(&load), vec![1, 2, 3, 4]);
        assert!(
            load.records
                .iter()
                .all(|r| matches!(r, crate::rollout::RolloutRecord::Message { .. }))
        );
        drop(session_a); // 丢弃 Session（模拟进程退出）

        // —— 会话 B：同 rollout 构造即 replay 恢复，历史与 A 一致 ——
        let model_b = Arc::new(MockModel::new(vec![text_then_end("续跑完成。")]));
        let (tx, _rx) = mpsc::channel::<Event>(64);
        let mut session_b = p10_session(model_b.clone(), p10_rollout(dir.path(), "thread-1"));
        assert_eq!(
            *session_b.messages, *history_a,
            "replay 恢复的历史应与退出前一致"
        );
        assert_eq!(
            wavecode_context::find_pairing_violations(&session_b.messages),
            Vec::<String>::new()
        );

        // —— 断点续跑：再跑一轮 turn，采样请求的历史 = 恢复历史 + 新输入 ——
        let reason = session_b.run_turn("s-2", "继续", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let seen = model_b.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].messages.len(), history_a.len() + 1);
        drop(seen);

        // rollout 续写：seq 接续不重号；第三次 replay 与会话 B 全量一致。
        let load = crate::rollout::load_rollout(&path).unwrap();
        assert_eq!(load.records.len(), 6);
        assert_eq!(p10_seqs(&load), vec![1, 2, 3, 4, 5, 6]);
        let session_c = p10_session(
            Arc::new(MockModel::new(vec![])),
            p10_rollout(dir.path(), "thread-1"),
        );
        assert_eq!(*session_c.messages, *session_b.messages);
    }

    /// P10 验收锚点：压缩记录落盘（承载压缩时点新历史）→ 压缩后 resume
    /// 恢复——压缩点之后原文 + 摘要即新历史（SPEC §16 / §5.2）。
    #[tokio::test]
    async fn rollout_compaction_record_and_resume_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let model = p3_mock(vec![Some(text_then_end("好的"))]);
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let mut session_a = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 100_000,
            max_output_tokens: 8192,
            model,
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig {
                thresholds: wavecode_context::Thresholds {
                    warning_margin: 200,
                    auto_compact_margin: 100,
                    blocking_margin: 10,
                },
                keep_recent: 2,
                summary_max_tokens: 500,
                estimate_chars_per_token: 4,
            },
            memory: None,
            skills: None,
            hooks: None,
            rollout: p10_rollout(dir.path(), "t-compact"),
        });
        // 上一 turn 结转的权威占用：首次 PreTurn 检查即过自动压缩线。
        session_a.usage_carry = Some(99_950);
        let reason = session_a.run_turn("s-1", "继续干活", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let events = collect_events(&mut rx);
        assert!(
            events.iter().any(|m| matches!(
                m,
                EventMsg::CompactStarted { trigger } if *trigger == wavecode_protocol::CompactTrigger::Auto
            )),
            "应以 Auto 触发压缩: {events:?}"
        );
        let history_a = session_a.messages.clone();

        // rollout 记录序：user 输入 → 压缩记录 → assistant 文本。
        let path = dir.path().join("threads/t-compact.jsonl");
        let load = crate::rollout::load_rollout(&path).unwrap();
        assert_eq!(load.records.len(), 3, "{:?}", load.records);
        let crate::rollout::RolloutRecord::Compaction {
            trigger,
            messages: recorded,
            ..
        } = &load.records[1]
        else {
            panic!("第二条应为压缩记录: {:?}", load.records)
        };
        assert_eq!(*trigger, wavecode_protocol::CompactTrigger::Auto);
        // 压缩记录承载压缩时点的新历史（= 会话当前历史的前缀）。
        assert_eq!(recorded.as_slice(), &history_a[..recorded.len()]);
        assert_eq!(p10_seqs(&load), vec![1, 2, 3]);

        // —— 压缩后 resume：replay 恢复 == 会话 A 当前历史；首条为摘要消息 ——
        let session_b = p10_session(
            Arc::new(MockModel::new(vec![])),
            p10_rollout(dir.path(), "t-compact"),
        );
        assert_eq!(*session_b.messages, *history_a);
        assert!(
            matches!(&session_b.messages[0].content[0], ContentBlock::Text { text } if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX)),
            "恢复历史首条应为摘要消息: {:?}",
            session_b.messages
        );
        assert_eq!(
            wavecode_context::find_pairing_violations(&session_b.messages),
            Vec::<String>::new()
        );
    }

    /// P10 验收锚点：崩溃恢复——写 rollout → 流中途中断（丢弃 Session 模拟
    /// 崩溃）→ replay 恢复 → 继续 turn，历史一致且配对完整。
    #[tokio::test]
    async fn rollout_crash_recovery_interrupt_then_resume() {
        let dir = tempfile::tempdir().unwrap();
        // turn 1：正常完成（write_file + 文本），rollout 落 4 条记录。
        let model_a = Arc::new(MockModel::new(vec![
            write_file_script("a.txt", "A"),
            text_then_end("完成。"),
        ]));
        let (tx, _rx) = mpsc::channel::<Event>(64);
        let mut session_a = p10_session(model_a, p10_rollout(dir.path(), "t-crash"));
        session_a.run_turn("s-1", "创建 a.txt", tx).await.unwrap();
        drop(session_a);

        // turn 2：恢复后流中途被中断（半截 tool_use）——中断路径合成配对
        // 结果落盘；随后直接丢弃 Session（模拟崩溃：无优雅关闭）。
        let gate = Arc::new(AtomicBool::new(false));
        let model_b = Arc::new(GatedModel {
            script: vec![
                StreamEvent::TextDelta { text: "再写".into() },
                StreamEvent::BlockEnd,
                StreamEvent::ToolUseBegin {
                    id: "t9".into(),
                    name: "write_file".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"path":"b.txt""#.into(),
                },
            ],
            gate: gate.clone(),
            seen: Mutex::new(vec![]),
        });
        let (tx, rx) = mpsc::channel::<Event>(64);
        let mut session_b = p10_session(model_b, p10_rollout(dir.path(), "t-crash"));
        let handle = session_b.interrupt_handle();
        let signal = async {
            let mut rx = rx;
            loop {
                let ev = rx.recv().await.unwrap();
                if matches!(&ev.msg, EventMsg::AgentMessageDelta { text } if text == "再写") {
                    break;
                }
            }
            handle.store(true, Ordering::SeqCst);
            gate.store(true, Ordering::SeqCst);
            rx
        };
        let (reason, mut rx) = tokio::join!(session_b.run_turn("s-2", "再写一个", tx), signal);
        assert_eq!(reason.unwrap(), StopReason::Interrupted);
        drop(rx);
        let history_b = session_b.messages.clone();
        // 半截 tool_use 已合成 is_error 配对结果（中断路径的既有纪律）。
        assert_eq!(
            wavecode_context::find_pairing_violations(&history_b),
            Vec::<String>::new()
        );
        drop(session_b); // 模拟崩溃：无 Shutdown、无提取，直接丢弃

        // —— 崩溃后 resume：replay 恢复历史与被中断时一致 ——
        let model_c = Arc::new(MockModel::new(vec![text_then_end("恢复后继续。")]));
        let (tx, _rx) = mpsc::channel::<Event>(64);
        let mut session_c = p10_session(model_c.clone(), p10_rollout(dir.path(), "t-crash"));
        assert_eq!(
            *session_c.messages, *history_b,
            "崩溃恢复的历史应与被中断时一致"
        );
        // 悬空 tool_use t9 的 is_error 配对结果在恢复历史中。
        let last = session_c.messages.last().unwrap();
        assert!(last.content.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t9")
        ));

        // —— 断点续跑：继续 turn 正常完成，采样请求携带恢复历史 ——
        let reason = session_c.run_turn("s-3", "继续", tx).await.unwrap();
        assert_eq!(reason, StopReason::Completed);
        let seen = model_c.seen.lock().unwrap();
        assert_eq!(seen[0].messages.len(), history_b.len() + 1);
        drop(seen);
        // rollout 全程 seq 连续（4 + 3 + 2 = 9 条记录）。
        let load = crate::rollout::load_rollout(&dir.path().join("threads/t-crash.jsonl")).unwrap();
        assert_eq!(load.records.len(), 9);
        assert_eq!(p10_seqs(&load), (1..=9).collect::<Vec<u64>>());
    }

    // —— P10 长程硬化：压缩循环压力测试（≥50 轮）+ 泄漏粗检 ——

    /// P10 泄漏粗检：计数分配器（统计活跃分配字节 = 累计 alloc − dealloc）。
    /// 精度边界（诚实声明）：这是"活跃分配字节"快照而非 RSS——RSS 受分配器
    /// 缓存与碎片影响，且无可移植读法（Windows 无 /proc）；tokio 任务数无
    /// 稳定 API；句柄泄漏无便携探测。故本断言只锁定"活跃内存不随轮次线性
    /// 增长"这一代理指标，RSS / 句柄 / 任务数级泄漏由人工长跑验收覆盖
    ///（scripts/acceptance/ecommerce.md）。
    struct CountingAlloc;

    static LIVE_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            // SAFETY: 透传系统分配器；layout 有效性由调用方（运行时）保证。
            unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            // SAFETY: 透传系统分配器；ptr/layout 与 alloc 配对由运行时保证。
            unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
        }
    }

    #[global_allocator]
    static P10_LEAK_CHECK_ALLOC: CountingAlloc = CountingAlloc;

    /// P10 压力 mock：采样恒回 99_950 input_tokens（过自动压缩线，每个
    /// turn 的 PreTurn 触发一次压缩）；摘要"引用前次摘要"——从历史首条的
    /// 上一轮摘要解析迭代号并 +1（模拟真实摘要的信息链传递），解析失败
    /// 产出 CHAIN-BROKEN 标记（断链在最终断言可见）。
    struct StressMock {
        summary_calls: Mutex<usize>,
    }

    /// 从上一轮摘要正文解析"第 N 轮迭代完成"的迭代号。
    fn p10_parse_round(summary: &str) -> Option<usize> {
        let start = summary.find("第 ")? + "第 ".len();
        let end = summary[start..].find(" 轮迭代完成")? + start;
        summary[start..end].parse().ok()
    }

    #[async_trait::async_trait]
    impl ChatModel for StressMock {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            if req.tools.is_empty() {
                // 摘要请求（ModelSummary 不带工具，与 p3 mock 同判定）。
                let prev_summary = req.messages.first().and_then(|m| {
                    m.content.iter().find_map(|b| match b {
                        ContentBlock::Text { text }
                            if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX) =>
                        {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                });
                let round = match &prev_summary {
                    None => Some(1usize),
                    Some(text) => p10_parse_round(text).map(|r| r + 1),
                };
                *self.summary_calls.lock().unwrap() += 1;
                let body = match round {
                    Some(round) => format!(
                        "## 目标\n搭建电商平台（GOAL-ANCHOR）。\n## 进展\n第 {round} 轮迭代完成（引用前次摘要：第 {} 轮）。\n## 关键决策\n首版 SQLite，零运维（DECISION-ANCHOR）。\n## 文件清单\ncrates/shop/src/cart.rs 已创建（FILE-ANCHOR）。\n## 待办\n第 {} 轮迭代（TODO-ANCHOR）。",
                        round.saturating_sub(1),
                        round + 1
                    ),
                    None => "CHAIN-BROKEN".to_owned(),
                };
                Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::TextDelta { text: body }),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: "end_turn".into(),
                        usage: Usage {
                            input_tokens: 100,
                            output_tokens: 20,
                        },
                    }),
                ])))
            } else {
                // 采样：纯文本终态 + 过自动线水位（下一 turn PreTurn 压缩）。
                Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::TextDelta {
                        text: "本轮完成。".into(),
                    }),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: "end_turn".into(),
                        usage: Usage {
                            input_tokens: 99_950,
                            output_tokens: 5,
                        },
                    }),
                ])))
            }
        }
    }

    /// P10 验收锚点（DEV-PLAN §0 总目标 2 的代理指标）：压缩循环压力
    /// 测试——mock 驱动 50 轮 turn，每轮 PreTurn 自动压缩一次；每轮断言
    /// 配对零违规；50 轮后五要素锚点链（目标/决策/文件清单/待办）在
    /// "摘要引用前次摘要"的传递下完整可追溯，历史条数有界。
    /// 附泄漏粗检：活跃分配字节增量有界（精度边界见 CountingAlloc 注释）。
    #[tokio::test]
    async fn compaction_loop_stress_50_rounds_no_pairing_violations() {
        const ROUNDS: usize = 50;
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(StressMock {
            summary_calls: Mutex::new(0),
        });
        let mut session = Session::new(SessionConfig {
            model_name: "mock".into(),
            context_window: 100_000,
            max_output_tokens: 8192,
            model: model.clone(),
            registry: wavecode_tools::Registry::builtin(),
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
            sandbox: bypass_sandbox(),
            context: ContextConfig {
                thresholds: wavecode_context::Thresholds {
                    warning_margin: 200,
                    auto_compact_margin: 100,
                    blocking_margin: 10,
                },
                keep_recent: 2,
                summary_max_tokens: 500,
                estimate_chars_per_token: 4,
            },
            memory: None,
            skills: None,
            hooks: None,
            // 压力测试同时压 rollout 记录面（每轮压缩记录 + 消息记录）。
            rollout: p10_rollout(dir.path(), "t-stress"),
        });
        // 种子水位：每个 turn 的首次 PreTurn 检查即触发自动压缩。
        session.usage_carry = Some(99_950);

        // 泄漏粗检基线：先跑 2 轮热身（惰性初始化 / 一次性分配不计入）。
        for i in 0..2 {
            let (tx, _rx) = mpsc::channel::<Event>(64);
            session
                .run_turn(&format!("s-{i}"), "继续迭代", tx)
                .await
                .unwrap();
        }
        let live_before = LIVE_BYTES.load(Ordering::Relaxed);

        for i in 2..ROUNDS {
            let (tx, _rx) = mpsc::channel::<Event>(64);
            session
                .run_turn(&format!("s-{i}"), "继续迭代", tx)
                .await
                .unwrap();
            assert_eq!(
                wavecode_context::find_pairing_violations(&session.messages),
                Vec::<String>::new(),
                "第 {i} 轮压缩后配对违规: {:?}",
                session.messages
            );
        }
        let live_after = LIVE_BYTES.load(Ordering::Relaxed);

        // 每轮恰一次压缩。
        assert_eq!(*model.summary_calls.lock().unwrap(), ROUNDS);
        // 历史有界：压缩稳态下条数不随轮次增长（摘要 + 保留尾 + 本轮收发）。
        assert!(
            session.messages.len() <= 6,
            "历史条数应有界: {}",
            session.messages.len()
        );
        // 无请求快照滞留：turn 结束后历史 Arc 唯一持有（泄漏的常见形态）。
        assert_eq!(Arc::strong_count(&session.messages), 1);

        // 五要素信息链：50 轮压缩后锚点仍可追溯，迭代号连续未断链。
        let ContentBlock::Text { text } = &session.messages[0].content[0] else {
            panic!("首条应为摘要文本消息: {:?}", session.messages)
        };
        assert!(
            text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX),
            "{text}"
        );
        assert!(!text.contains("CHAIN-BROKEN"), "摘要信息链断裂: {text}");
        for anchor in [
            "GOAL-ANCHOR",
            "DECISION-ANCHOR",
            "FILE-ANCHOR",
            "TODO-ANCHOR",
        ] {
            assert!(text.contains(anchor), "50 轮压缩后缺要素锚点「{anchor}」: {text}");
        }
        assert!(
            text.contains("第 50 轮迭代完成"),
            "迭代号应链式传递到第 50 轮: {text}"
        );

        // rollout 记录面同步受压：50 条压缩记录 + 每轮 2 条消息（首轮另
        // 有种子输入外的 user 消息……精确计数 = 50 压缩 + 100 消息）。
        let load = crate::rollout::load_rollout(&dir.path().join("threads/t-stress.jsonl")).unwrap();
        let compactions = load
            .records
            .iter()
            .filter(|r| matches!(r, crate::rollout::RolloutRecord::Compaction { .. }))
            .count();
        assert_eq!(compactions, ROUNDS);
        assert_eq!(load.records.len(), ROUNDS * 3);
        let seqs = p10_seqs(&load);
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "seq 全程连续");

        // 泄漏粗检（best-effort，精度边界见 CountingAlloc 注释）：活跃
        // 分配增量有界——稳态下每轮的分配应在 turn 结束时释放；阈值取
        // 宽裕常数以吸收并行测试的瞬时分配噪声。
        let delta = live_after.saturating_sub(live_before);
        assert!(
            delta < 16 * 1024 * 1024,
            "活跃分配增量 {delta} 字节超界（疑似随轮次增长的泄漏）"
        );
    }
}
