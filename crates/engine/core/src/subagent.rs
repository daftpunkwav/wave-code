//! 子代理（subagents，P5，deepagents 核心能力之一，SPEC §5.3 / §11.2）。
//!
//! 形态：
//! - 子代理 = 独立 [`Session`]（隔离消息历史）跑自己的 turn 循环，输入为
//!   任务描述 + 任务指令（可选内置类型的系统前言）；运行在独立 tokio
//!   task（后台形态）或调用方工具执行内（同步形态）；
//! - 内置类型：[`SubagentType::GeneralPurpose`]（全工具）与
//!   [`SubagentType::Explore`]（只读工具——按 registry 过滤 `is_read_only`）；
//! - 完成 / 失败 / 停止时产出结构化结果 [`TaskResult`]（最终文本摘要 +
//!   状态 + token 用量）；后台形态的终态以 `<task-notification>` user 消息
//!   注入父会话下一 turn（注入点在 turn 循环头，见 session.rs）。
//!
//! 依赖矩阵取舍（tools 不能依赖 core）：`task` / `task_output` / `task_stop`
//! 三个工具需要驱动 core 的 Session，故工具实现放 core 侧（core 本就可实现
//! tools 的 [`Tool`] trait），经 [`Session::with_subagents`] 装配进父会话
//! registry——与 `todo_write` 的"共享状态句柄注入"先例同构，无新依赖边。
//!
//! 深度上限 1（防失控）：子代理的 Session 经 [`Session::new`] 构造，其
//! registry 由本模块单独装配（builtin 全集或只读子集），**不含** task 工具
//! ——子代理在工具面层面就无法再派生，上限由构造保证而非运行时检查。
//!
//! 事件可见性（择一注释）：新增 `SubagentStarted` / `SubagentCompleted`
//! 协议变体而非复用 Warning——起止语义清晰、前端（TUI P8）可专门渲染；
//! 子代理的中间过程（delta / 工具调用）不进父会话事件流（上下文隔离的
//! 同构），前端只见起点与终点。wire tag 已在 protocol 锁定测试登记。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use wavecode_llm::ChatModel;
use wavecode_protocol::{Event, EventMsg, StopReason, SubagentStatus};
use wavecode_tools::{Registry, Tool, ToolCtx, ToolOutput};

use crate::session::{Session, SessionConfig};

/// 子代理事件通道容量（事件只被驱动任务排干取终态，无人消费中间事件）。
const CHILD_EVENT_CHANNEL_CAPACITY: usize = 256;

/// task_stop 等待子代理到达终态的超时：子代理中断在安全点（流消费循环
/// 每个元素 / 工具迭代间）生效，正常毫秒级；超时兜底防挂起的工具执行
/// 拖死父会话 turn。
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// task_stop 等待终态的轮询间隔（轮询同时重武装中断标志，覆盖
/// "run_turn 入口清标志"的竞态窗口，见 [`SubagentManager::stop`]）。
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// explore 类型的子代理前言（拼在子代理 turn 输入前部）。
///
/// 取舍（YAGNI）：完整自定义系统提示词注入点（替换 system prompt）留待
/// 后续；内置类型的差异 = 工具集过滤（构造保证）+ 此前言（行为引导）。
const EXPLORE_PREAMBLE: &str = "\
You are an explore subagent: investigate the codebase and answer with findings. \
You only have read-only tools; do not attempt to modify anything.";

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
    fn parse(raw: &str) -> Option<Self> {
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
    fn preamble(self) -> Option<&'static str> {
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

/// 后台任务的跟踪槽（Manager 任务表的值；id / 描述 / 类型由任务表键与
/// 驱动闭包持有的 spec 承载，不重复存放）。
struct TaskSlot {
    /// 停止请求标志：先于中断句柄存在（驱动任务尚未装配）的 stop 也能
    /// 被驱动任务在启动前观察到（见 drive_child 的启动前检查）。
    stop_requested: AtomicBool,
    /// 子代理 Session 的中断句柄（驱动任务装配后可用；复用
    /// [`Session::interrupt_handle`] 模式）。
    interrupt: Mutex<Option<Arc<AtomicBool>>>,
    /// 当前状态。
    state: Mutex<TaskState>,
}

/// 派生子代理所需的父会话配置快照（Arc 共享模型通道、继承 sandbox / cwd）。
///
/// sandbox 为 `Clone` 即共享（模式句柄同一份 Arc）：子代理继承父会话权限
/// 模式，turn 中的模式切换对子代理的下一次判定同样生效。
struct SpawnDeps {
    model: Arc<dyn ChatModel>,
    model_name: String,
    context_window: u64,
    max_output_tokens: u32,
    cwd: PathBuf,
    deny_env: Vec<String>,
    sandbox: wavecode_sandbox::Sandbox,
    context: wavecode_context::ContextConfig,
}

/// 子代理管理器：派生 / 跟踪 / 停止子代理，收集后台终态通知。
///
/// 由 [`Session::with_subagents`] 创建并注册三个工具；父会话 turn 循环头
/// 经 [`SubagentManager::drain_notifications`] 取走待注入通知。
pub struct SubagentManager {
    deps: SpawnDeps,
    /// 后台任务表（id → 跟踪槽）。
    tasks: Mutex<HashMap<String, Arc<TaskSlot>>>,
    /// 待注入父会话的后台终态通知（`<task-notification>` 文本）。
    notifications: Mutex<Vec<String>>,
    /// 父会话事件汇（turn 入口挂接；子代理起止事件以父 turn 的
    /// submission_id 回填）。无 turn 期间完成的后台任务以最近一次的
    /// 汇发出——事件是旁观通道，通知才是结果回注的正路。
    event_sink: Mutex<Option<(mpsc::Sender<Event>, String)>>,
    /// 任务 id 分配器（`task-N`，1 起单调递增）。
    next_id: AtomicUsize,
}

impl SubagentManager {
    /// 从父会话配置快照创建（`Session::with_subagents` 的装配入口）。
    pub fn from_config(cfg: &SessionConfig) -> Arc<Self> {
        Arc::new(Self {
            deps: SpawnDeps {
                model: cfg.model.clone(),
                model_name: cfg.model_name.clone(),
                context_window: cfg.context_window,
                max_output_tokens: cfg.max_output_tokens,
                cwd: cfg.cwd.clone(),
                deny_env: cfg.deny_env.clone(),
                sandbox: cfg.sandbox.clone(),
                context: cfg.context.clone(),
            },
            tasks: Mutex::new(HashMap::new()),
            notifications: Mutex::new(Vec::new()),
            event_sink: Mutex::new(None),
            next_id: AtomicUsize::new(0),
        })
    }

    /// 挂接父会话事件汇（`run_turn` 入口调用；子代理起止事件以该 turn 的
    /// submission_id 回填）。
    pub fn set_event_sink(&self, events: mpsc::Sender<Event>, submission_id: &str) {
        *self
            .event_sink
            .lock()
            .expect("事件汇锁中毒即进程已有 panic") = Some((events, submission_id.to_owned()));
    }

    /// 取走全部待注入通知（turn 循环头调用，一次性消费）。
    pub fn drain_notifications(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .notifications
                .lock()
                .expect("通知锁中毒即进程已有 panic"),
        )
    }

    /// 派生后台子代理：登记跟踪槽后在独立 tokio task 中运行，立即返回 id。
    pub fn spawn_background(self: &Arc<Self>, spec: TaskSpec) -> String {
        let id = self.alloc_id();
        let slot = Arc::new(TaskSlot {
            stop_requested: AtomicBool::new(false),
            interrupt: Mutex::new(None),
            state: Mutex::new(TaskState::Running),
        });
        self.tasks
            .lock()
            .expect("任务表锁中毒即进程已有 panic")
            .insert(id.clone(), slot.clone());
        let mgr = self.clone();
        let child_id = id.clone();
        tokio::spawn(async move {
            mgr.drive_child(child_id, spec, Some(slot)).await;
        });
        id
    }

    /// 派生同步子代理：在调用方（task 工具 execute）内运行至终态并返回
    /// 结构化结果。同步形态不进任务表——结果直接作为 ToolResult 回灌，
    /// task_output / task_stop 找不到同步任务的 id 是预期行为。
    pub async fn run_sync(self: &Arc<Self>, spec: TaskSpec) -> TaskResult {
        let id = self.alloc_id();
        self.drive_child(id, spec, None).await
    }

    /// 查询任务状态（task_output；未知 id 返回 None）。
    pub fn query(&self, task_id: &str) -> Option<TaskState> {
        let slot = self
            .tasks
            .lock()
            .expect("任务表锁中毒即进程已有 panic")
            .get(task_id)
            .cloned()?;
        Some(
            slot.state
                .lock()
                .expect("状态锁中毒即进程已有 panic")
                .clone(),
        )
    }

    /// 停止后台子代理（task_stop）：置停止标志 + 中断句柄，轮询等待终态
    ///（超时兜底返回当时的 Running 状态）。
    ///
    /// 轮询中重武装中断标志：`run_turn` 入口会清一次中断标志，stop 恰好
    /// 落在"句柄已装配、turn 未开始"的窗口时单次置位会被抹掉；重武装
    /// 保证窗口内置位最终生效。未知 id 返回 None。
    pub async fn stop(&self, task_id: &str) -> Option<TaskState> {
        let slot = self
            .tasks
            .lock()
            .expect("任务表锁中毒即进程已有 panic")
            .get(task_id)
            .cloned()?;
        slot.stop_requested.store(true, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + STOP_WAIT_TIMEOUT;
        loop {
            let state = slot
                .state
                .lock()
                .expect("状态锁中毒即进程已有 panic")
                .clone();
            if matches!(state, TaskState::Finished(_)) || tokio::time::Instant::now() >= deadline {
                return Some(state);
            }
            if let Some(handle) = slot
                .interrupt
                .lock()
                .expect("中断槽锁中毒即进程已有 panic")
                .as_ref()
            {
                handle.store(true, Ordering::SeqCst);
            }
            tokio::time::sleep(STOP_POLL_INTERVAL).await;
        }
    }

    /// 分配任务 id（`task-N`，1 起单调递增）。
    fn alloc_id(&self) -> String {
        format!("task-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// 发出子代理事件（事件汇未挂接时静默丢弃——无 turn 期间无旁观方）。
    async fn emit_event(&self, msg: EventMsg) {
        let sink = self
            .event_sink
            .lock()
            .expect("事件汇锁中毒即进程已有 panic")
            .clone();
        if let Some((tx, submission_id)) = sink {
            let ev = Event {
                id: submission_id,
                msg,
            };
            if tx.send(ev).await.is_err() {
                tracing::debug!("父会话事件接收端已断开，子代理事件丢弃");
            }
        }
    }

    /// 装配子代理 SessionConfig（深度上限 1 的构造保证点）：registry 按
    /// 类型取 builtin 全集 / 只读子集，均不含 task 工具；P7 起
    /// `allowed_tools` 白名单在此基础上再按名过滤（skill fork 工具面）。
    /// sandbox / cwd / deny_env / context 继承父会话（sandbox 克隆即共享
    /// 模式句柄）。memory / skills / hooks / rollout 不继承：隔离上下文中
    /// 不挂持久记忆写入面（自动提取由父会话在 SessionEnd 统一做）、不挂
    /// skill 触发面与 hook 面（hook 是会话级用户配置，子代理不重复触发）、
    /// 不写 rollout（P10：持久化以父会话为单位，子代理的中间过程本就是
    /// 隔离上下文，恢复父会话时不需要子代理历史）。
    fn child_config(&self, spec: &TaskSpec) -> SessionConfig {
        let base = match spec.subagent_type {
            SubagentType::GeneralPurpose => Registry::builtin(),
            SubagentType::Explore => Registry::builtin().read_only_subset(),
        };
        let registry = match &spec.allowed_tools {
            Some(names) => base.name_subset(names),
            None => base,
        };
        SessionConfig {
            model_name: self.deps.model_name.clone(),
            context_window: self.deps.context_window,
            max_output_tokens: self.deps.max_output_tokens,
            model: self.deps.model.clone(),
            registry,
            cwd: self.deps.cwd.clone(),
            deny_env: self.deps.deny_env.clone(),
            sandbox: self.deps.sandbox.clone(),
            context: self.deps.context.clone(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        }
    }

    /// 子代理驱动：建 Session 跑一轮 turn 至终态，产出结构化结果；
    /// 后台形态（slot 为 Some）登记状态、发通知，同步形态只返回结果。
    /// 两种形态都发 SubagentStarted / SubagentCompleted 事件。
    async fn drive_child(
        self: &Arc<Self>,
        task_id: String,
        spec: TaskSpec,
        slot: Option<Arc<TaskSlot>>,
    ) -> TaskResult {
        self.emit_event(EventMsg::SubagentStarted {
            task_id: task_id.clone(),
            subagent_type: spec.subagent_type.as_str().to_owned(),
            description: spec.description.clone(),
        })
        .await;

        // 子代理经 Session::new 构造：无 task 工具（深度上限 1），无通知
        // 注入路径（subagents 字段为 None）。
        let mut session = Session::new(self.child_config(&spec));
        if let Some(slot) = &slot {
            *slot.interrupt.lock().expect("中断槽锁中毒即进程已有 panic") =
                Some(session.interrupt_handle());
            // 启动前已请求停止：不进入 turn 直接以 Stopped 收尾——
            // run_turn 入口会清中断标志，此前置位会被抹掉。
            if slot.stop_requested.load(Ordering::SeqCst) {
                let result = TaskResult {
                    status: SubagentStatus::Stopped,
                    summary: "(stopped before the subagent started)".to_owned(),
                    tokens_used: None,
                };
                return self.finish_child(task_id, &spec, slot, result).await;
            }
        }

        // 子代理事件只排干取终态（最终文本 / token 用量），中间过程不进
        // 父会话（上下文隔离的同构）；转发子代理增量事件留 P8 再议。
        let input = match spec
            .preamble
            .as_deref()
            .or_else(|| spec.subagent_type.preamble())
        {
            Some(preamble) => format!("{preamble}\n\n{}", spec.prompt),
            None => spec.prompt.clone(),
        };
        let (child_tx, mut child_rx) = mpsc::channel::<Event>(CHILD_EVENT_CHANNEL_CAPACITY);
        let mut last_text = String::new();
        let mut tokens_used: Option<u64> = None;
        let drain = async {
            while let Some(ev) = child_rx.recv().await {
                match ev.msg {
                    EventMsg::AgentMessageComplete { text } => last_text = text,
                    EventMsg::TokenCount { used, .. } => tokens_used = Some(used),
                    _ => {}
                }
            }
        };
        let (turn_result, ()) = futures::join!(session.run_turn(&task_id, &input, child_tx), drain);

        let result = match turn_result {
            Ok(StopReason::Interrupted) => TaskResult {
                status: SubagentStatus::Stopped,
                summary: non_empty_summary(last_text, "(stopped before producing output)"),
                tokens_used,
            },
            Ok(_) => TaskResult {
                status: SubagentStatus::Completed,
                summary: non_empty_summary(last_text, "(subagent produced no text output)"),
                tokens_used,
            },
            Err(e) => TaskResult {
                status: SubagentStatus::Failed,
                summary: format!("subagent turn failed: {e:#}"),
                tokens_used,
            },
        };
        match slot {
            Some(slot) => self.finish_child(task_id, &spec, &slot, result).await,
            None => {
                self.emit_event(EventMsg::SubagentCompleted {
                    task_id,
                    status: result.status,
                    summary: result.summary.clone(),
                })
                .await;
                result
            }
        }
    }

    /// 后台子代理收尾：登记终态、排队 `<task-notification>`、发
    /// SubagentCompleted 事件。通知与事件同源（同一份 TaskResult）。
    async fn finish_child(
        &self,
        task_id: String,
        spec: &TaskSpec,
        slot: &Arc<TaskSlot>,
        result: TaskResult,
    ) -> TaskResult {
        *slot.state.lock().expect("状态锁中毒即进程已有 panic") =
            TaskState::Finished(result.clone());
        self.notifications
            .lock()
            .expect("通知锁中毒即进程已有 panic")
            .push(format_notification(
                &task_id,
                spec.subagent_type,
                &spec.description,
                &result,
            ));
        self.emit_event(EventMsg::SubagentCompleted {
            task_id,
            status: result.status,
            summary: result.summary.clone(),
        })
        .await;
        result
    }
}

/// 摘要空串兜底（中断 / 畸形流可能没有最终文本）。
fn non_empty_summary(text: String, fallback: &str) -> String {
    if text.trim().is_empty() {
        fallback.to_owned()
    } else {
        text
    }
}

/// 子代理终态词（非 exhaustive 协议枚举的兜底映射）。
fn status_label(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Completed => "completed",
        SubagentStatus::Failed => "failed",
        SubagentStatus::Stopped => "stopped",
        // SubagentStatus 标注 non_exhaustive：未来变体按未知词展示。
        _ => "unknown",
    }
}

/// `<task-notification>` 注入文本（SPEC §5.3）：后台子代理终态以 user
/// 消息注入父会话下一 turn。
fn format_notification(
    task_id: &str,
    subagent_type: SubagentType,
    description: &str,
    result: &TaskResult,
) -> String {
    format!(
        "<task-notification>\nBackground task {task_id} ({}) finished with status: {}.\nDescription: {description}\nResult:\n{}\n</task-notification>",
        subagent_type.as_str(),
        status_label(result.status),
        result.summary,
    )
}

/// task_output / 同步 task 的结果文本（状态 + token 用量 + 摘要）。
fn format_result(result: &TaskResult) -> String {
    let mut out = format!("status: {}", status_label(result.status));
    if let Some(tokens) = result.tokens_used {
        out.push_str(&format!("\ntokens used: {tokens}"));
    }
    out.push_str(&format!("\nresult:\n{}", result.summary));
    out
}

/// 取必填字符串参数；缺失 / 空串返回错误文案（回灌模型自我纠正）。
fn required_str<'a>(input: &'a Value, key: &str) -> std::result::Result<&'a str, String> {
    match input.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(format!(
            "missing or invalid parameter '{key}' (non-empty string required)"
        )),
    }
}

/// `task` 工具：派生子代理（同步等待或后台运行）。
pub struct TaskSpawn {
    manager: Arc<SubagentManager>,
}

impl TaskSpawn {
    /// 以 Manager 共享句柄构造（`Session::with_subagents` 装配）。
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
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
            let id = self.manager.spawn_background(spec);
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
        let result = self.manager.run_sync(spec).await;
        Ok(ToolOutput {
            is_error: result.status == SubagentStatus::Failed,
            content: format_result(&result),
        })
    }
}

/// `task_output` 工具：查询后台子代理结果。
pub struct TaskOutputTool {
    manager: Arc<SubagentManager>,
}

impl TaskOutputTool {
    /// 以 Manager 共享句柄构造（`Session::with_subagents` 装配）。
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
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
        match self.manager.query(&task_id) {
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

/// `task_stop` 工具：停止后台子代理。
pub struct TaskStop {
    manager: Arc<SubagentManager>,
}

impl TaskStop {
    /// 以 Manager 共享句柄构造（`Session::with_subagents` 装配）。
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
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
        match self.manager.stop(&task_id).await {
            None => err(format!("unknown task id: {task_id}")),
            Some(TaskState::Finished(result)) => Ok(ToolOutput {
                content: format!("{task_id} stopped.\n{}", format_result(&result)),
                is_error: false,
            }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use wavecode_llm::{ChatRequest, StreamEvent, Usage};
    use wavecode_protocol::PermissionMode;

    /// 脚本化 mock（与 session.rs 测试的 MockModel 同构）：按调用次数回放。
    struct MockModel {
        calls: Mutex<u32>,
        scripts: Vec<Vec<StreamEvent>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for MockModel {
        async fn stream(
            &self,
            _req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            let mut n = self.calls.lock().unwrap();
            let idx = (*n as usize).min(self.scripts.len().saturating_sub(1));
            *n += 1;
            Ok(Box::pin(stream::iter(
                self.scripts[idx].clone().into_iter().map(Ok),
            )))
        }
    }

    fn text_end(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta { text: text.into() },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                },
            },
        ]
    }

    fn parent_config(model: Arc<dyn ChatModel>) -> SessionConfig {
        let cwd = tempfile::tempdir().unwrap().keep();
        SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model,
            registry: Registry::builtin(),
            cwd,
            deny_env: Vec::new(),
            sandbox: wavecode_sandbox::Sandbox::without_rules(PermissionMode::BypassPermissions),
            context: Default::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        }
    }

    fn spec(prompt: &str, subagent_type: SubagentType) -> TaskSpec {
        TaskSpec {
            description: "测试任务".into(),
            prompt: prompt.into(),
            subagent_type,
            preamble: None,
            allowed_tools: None,
        }
    }

    /// 深度上限 1（构造保证）：两类子代理的 registry 均无 task 工具；
    /// explore 只保留只读工具。
    #[test]
    fn child_registry_has_no_task_tools() {
        let model = Arc::new(MockModel {
            calls: Mutex::new(0),
            scripts: vec![],
        });
        let mgr = SubagentManager::from_config(&parent_config(model));
        for t in [SubagentType::GeneralPurpose, SubagentType::Explore] {
            let cfg = mgr.child_config(&spec("x", t));
            for name in ["task", "task_output", "task_stop"] {
                assert!(
                    cfg.registry.get(name).is_none(),
                    "{t:?} 子代理不得有 {name} 工具（深度上限 1）"
                );
            }
        }
        let explore = mgr.child_config(&spec("x", SubagentType::Explore));
        assert!(explore.registry.get("grep").is_some());
        assert!(explore.registry.get("read_file").is_some());
        for name in ["write_file", "edit_file", "shell", "todo_write"] {
            assert!(
                explore.registry.get(name).is_none(),
                "explore 子代理不得有 {name}"
            );
        }
        // general-purpose 全工具（对照）。
        let general = mgr.child_config(&spec("x", SubagentType::GeneralPurpose));
        assert!(general.registry.get("write_file").is_some());
        assert!(general.registry.get("shell").is_some());
    }

    /// 后台派生 → 终态可查询、通知可取走（且一次性消费）。
    #[tokio::test]
    async fn background_task_completes_and_notifies() {
        let model = Arc::new(MockModel {
            calls: Mutex::new(0),
            scripts: vec![text_end("调查结论：一切正常")],
        });
        let mgr = SubagentManager::from_config(&parent_config(model));
        let id = mgr.spawn_background(spec("调查一下", SubagentType::Explore));
        assert_eq!(id, "task-1");
        // 等待终态（mock 即时完成，轮询兜底）。
        let state = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(TaskState::Finished(r)) = mgr.query(&id) {
                    break r;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("子代理应在 5s 内完成");
        assert_eq!(state.status, SubagentStatus::Completed);
        assert!(state.summary.contains("调查结论"));
        assert!(state.tokens_used.is_some());

        let notes = mgr.drain_notifications();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("<task-notification>"));
        assert!(notes[0].contains("task-1"));
        assert!(notes[0].contains("completed"));
        assert!(notes[0].contains("调查结论"));
        // 一次性消费。
        assert!(mgr.drain_notifications().is_empty());
    }

    /// 同步派生：结果直接返回，不进任务表、无通知。
    #[tokio::test]
    async fn sync_task_returns_result_directly() {
        let model = Arc::new(MockModel {
            calls: Mutex::new(0),
            scripts: vec![text_end("同步结果")],
        });
        let mgr = SubagentManager::from_config(&parent_config(model));
        let result = mgr
            .run_sync(spec("干活", SubagentType::GeneralPurpose))
            .await;
        assert_eq!(result.status, SubagentStatus::Completed);
        assert!(result.summary.contains("同步结果"));
        assert!(mgr.query("task-1").is_none(), "同步任务不进任务表");
        assert!(mgr.drain_notifications().is_empty(), "同步任务不发通知");
    }

    /// 三个工具的参数校验与未知 id 的错误形态（is_error 回灌，不 panic）。
    #[tokio::test]
    async fn tools_validate_input_and_unknown_ids() {
        let model = Arc::new(MockModel {
            calls: Mutex::new(0),
            scripts: vec![],
        });
        let mgr = SubagentManager::from_config(&parent_config(model));
        let ctx = ToolCtx {
            cwd: std::path::PathBuf::from("."),
            deny_env: Vec::new(),
        };
        let spawn = TaskSpawn::new(mgr.clone());
        // 缺 description / prompt。
        assert!(spawn.execute(json!({}), &ctx).await.unwrap().is_error);
        // 非法 subagent_type。
        let out = spawn
            .execute(
                json!({"description": "d", "prompt": "p", "subagent_type": "nope"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("invalid subagent_type"));

        let output = TaskOutputTool::new(mgr.clone());
        let out = output
            .execute(json!({"task_id": "task-99"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("unknown task id"));

        let stop = TaskStop::new(mgr.clone());
        assert!(
            stop.execute(json!({"task_id": "task-99"}), &ctx)
                .await
                .unwrap()
                .is_error
        );
    }

    // ------------------------------------------------------------------
    // 集成测试：mock model 驱动父会话 turn 全流程（P5 验收）
    // ------------------------------------------------------------------

    /// 请求全文扁平化（text + tool_use 标识 + tool_result 内容），供断言。
    fn request_text(req: &ChatRequest) -> String {
        req.messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        wavecode_llm::ContentBlock::Text { text } => text.clone(),
                        wavecode_llm::ContentBlock::ToolUse { id, name, .. } => {
                            format!("tool_use:{name}:{id}")
                        }
                        wavecode_llm::ContentBlock::ToolResult { content, .. } => {
                            format!("tool_result:{content}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 首条消息（turn 输入）的文本：父子请求路由判据。
    fn first_text(req: &ChatRequest) -> String {
        req.messages
            .first()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        wavecode_llm::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 父会话 turn 输入（parallel / stop 两个集成测试的路由锚点）。
    const PARENT_INPUT: &str = "开始任务";

    /// 并行测试 mock：父子共用一个 ChatModel（与生产形态一致，子代理继承
    /// 父会话模型 Arc），按首条消息文本路由——
    /// - 含 `child-task-`：子代理请求（round1 中间过程：文本 + list_dir
    ///   工具调用；round2 终态文本）；两个子代理都进入采样后才放行脚本
    ///   （并行证明：串行执行会在 5s 超时后才放行且 overlap 标志不置位）；
    /// - 否则：父会话请求，按调用序回放脚本；第 2 次采样（task_output 轮）
    ///   等待外部 gate（测试在观察到两个 SubagentCompleted 后置位，保证
    ///   task_output 查询时子代理已终态——无竞态）。
    struct ParallelModel {
        parent_scripts: Vec<Vec<StreamEvent>>,
        parent_calls: Mutex<u32>,
        gate: Arc<AtomicBool>,
        child_started: AtomicUsize,
        child_overlap: AtomicBool,
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for ParallelModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            self.seen.lock().unwrap().push(req.clone());
            let ft = first_text(&req);
            if ft.contains("child-task-") {
                let n = self.child_started.fetch_add(1, Ordering::SeqCst) + 1;
                if n >= 2 {
                    self.child_overlap.store(true, Ordering::SeqCst);
                }
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while self.child_started.load(Ordering::SeqCst) < 2
                    && tokio::time::Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let suffix = if ft.contains("child-task-A") {
                    "A"
                } else {
                    "B"
                };
                let round2 = req.messages.iter().any(|m| {
                    m.content
                        .iter()
                        .any(|b| matches!(b, wavecode_llm::ContentBlock::ToolResult { .. }))
                });
                let script = if round2 {
                    text_end(&format!("result-{suffix}"))
                } else {
                    vec![
                        StreamEvent::TextDelta {
                            text: format!("child-{suffix}-intermediate"),
                        },
                        StreamEvent::ToolUseBegin {
                            id: format!("ct-{suffix}"),
                            name: "list_dir".into(),
                        },
                        StreamEvent::ToolUseInputDelta {
                            partial_json: r#"{"path":"."}"#.into(),
                        },
                        StreamEvent::BlockEnd,
                        StreamEvent::MessageComplete {
                            stop_reason: "tool_use".into(),
                            usage: Usage::default(),
                        },
                    ]
                };
                return Ok(Box::pin(stream::iter(script.into_iter().map(Ok))));
            }
            let call = {
                let mut n = self.parent_calls.lock().unwrap();
                let c = *n as usize;
                *n += 1;
                c
            };
            if call == 1 {
                while !self.gate.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            let idx = call.min(self.parent_scripts.len().saturating_sub(1));
            Ok(Box::pin(stream::iter(
                self.parent_scripts[idx].clone().into_iter().map(Ok),
            )))
        }
    }

    fn tool_use(id: &str, name: &str, input_json: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseBegin {
                id: id.into(),
                name: name.into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: input_json.into(),
            },
            StreamEvent::BlockEnd,
        ]
    }

    /// P5 验收主测试：父会话派生 2 个后台子代理并行执行，两者结果经
    /// `<task-notification>` 注入与 task_output 查询正确回注父会话；父会话
    /// 历史不含子代理中间消息（上下文隔离）。
    #[tokio::test]
    async fn parallel_background_subagents_isolated_and_reinjected() {
        let gate = Arc::new(AtomicBool::new(false));
        let parent_scripts = vec![
            // call0：派生两个后台子代理（A=explore，B=general-purpose）。
            {
                let mut v = tool_use(
                    "p1",
                    "task",
                    r#"{"description":"调查认证模块","prompt":"child-task-A","subagent_type":"explore","run_in_background":true}"#,
                );
                v.extend(tool_use(
                    "p2",
                    "task",
                    r#"{"description":"调查日志模块","prompt":"child-task-B","run_in_background":true}"#,
                ));
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            // call1（gate 后）：查询两个任务的结果。
            {
                let mut v = tool_use("p3", "task_output", r#"{"task_id":"task-1"}"#);
                v.extend(tool_use("p4", "task_output", r#"{"task_id":"task-2"}"#));
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            text_end("父会话收尾"),
        ];
        let model = Arc::new(ParallelModel {
            parent_scripts,
            parent_calls: Mutex::new(0),
            gate: gate.clone(),
            child_started: AtomicUsize::new(0),
            child_overlap: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
        });
        let mut session = Session::with_subagents(parent_config(model.clone()));
        let (tx, mut rx) = mpsc::channel::<Event>(512);
        let turn = tokio::spawn(async move { session.run_turn("s-1", PARENT_INPUT, tx).await });

        // 事件观察：两个 SubagentCompleted 到齐后放行父会话 task_output 轮。
        let mut started = 0usize;
        let mut completed_statuses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let ev = tokio::time::timeout(deadline - tokio::time::Instant::now(), rx.recv())
                .await
                .expect("超时：事件流停滞")
                .expect("事件流意外结束");
            match ev.msg {
                EventMsg::SubagentStarted { .. } => started += 1,
                EventMsg::SubagentCompleted { status, .. } => {
                    completed_statuses.push(status);
                    if completed_statuses.len() == 2 {
                        gate.store(true, Ordering::SeqCst);
                    }
                }
                EventMsg::TurnCompleted { .. } => break,
                _ => {}
            }
        }
        let reason = turn.await.expect("turn 任务 panic").expect("turn 应成功");
        assert_eq!(reason, StopReason::Completed);
        assert_eq!(started, 2, "应见 2 个 SubagentStarted");
        assert_eq!(
            completed_statuses,
            vec![SubagentStatus::Completed, SubagentStatus::Completed],
            "两个子代理应并行完成"
        );
        assert!(
            model.child_overlap.load(Ordering::SeqCst),
            "两个子代理应真正并行（同时处于采样中）"
        );

        let seen = model.seen.lock().unwrap();
        // 两个子代理各自独立跑了 turn（隔离的 Session）。
        assert!(seen.iter().any(|r| first_text(r).contains("child-task-A")));
        assert!(seen.iter().any(|r| first_text(r).contains("child-task-B")));
        let parents: Vec<&ChatRequest> = seen
            .iter()
            .filter(|r| first_text(r) == PARENT_INPUT)
            .collect();
        assert_eq!(parents.len(), 3, "父会话应有 3 轮采样");

        // 时序说明：call0 工具结果回灌后循环头的 drain 在子代理完成前执行
        //（call1 采样被 gate 挡住），两条通知在 call2 前的循环头注入；
        // task_output 结果（call1 的 tool_result）同样落在 call2 的请求里。
        // p1 仅含派生回执（其文案提到 <task-notification>，不是真通知）。
        let p2 = request_text(parents[2]);
        assert_eq!(
            p2.matches("<task-notification>\nBackground task").count(),
            2,
            "两条后台终态通知应在循环头注入（派生回执的文案提及不含此主体）: {p2}"
        );
        assert!(p2.contains("result-A") && p2.contains("result-B"));
        assert_eq!(
            p2.matches("status: completed").count(),
            4,
            "通知与 task_output 各报告一次终态: {p2}"
        );

        // 上下文隔离：父会话任何一轮请求都不含子代理中间消息
        //（中间文本 / 子代理 tool_use id / 子代理工具结果）。
        for r in &parents {
            let t = request_text(r);
            for needle in [
                "child-A-intermediate",
                "child-B-intermediate",
                "ct-A",
                "ct-B",
            ] {
                assert!(
                    !t.contains(needle),
                    "父会话历史泄漏了子代理中间过程 {needle}"
                );
            }
        }
    }

    /// stop 测试 mock：含 `infinite-task` 的子代理请求返回无限流
    ///（每 1ms 一个 delta，只能被中断收尾）；父会话按调用序回放脚本。
    struct StopModel {
        parent_scripts: Vec<Vec<StreamEvent>>,
        parent_calls: Mutex<u32>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for StopModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            self.seen.lock().unwrap().push(req.clone());
            if first_text(&req).contains("infinite-task") {
                let endless = stream::unfold((), |()| async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    Some((
                        Ok(StreamEvent::TextDelta {
                            text: "tick".into(),
                        }),
                        (),
                    ))
                });
                return Ok(Box::pin(endless));
            }
            let call = {
                let mut n = self.parent_calls.lock().unwrap();
                let c = *n as usize;
                *n += 1;
                c
            };
            let idx = call.min(self.parent_scripts.len().saturating_sub(1));
            Ok(Box::pin(stream::iter(
                self.parent_scripts[idx].clone().into_iter().map(Ok),
            )))
        }
    }

    /// P5 验收：后台子代理可被 task_stop 停止，父会话收到停止状态
    ///（SubagentCompleted{Stopped} 事件 + task_output / 通知回注）。
    /// 无 gate：task_stop 自身等待子代理终态（轮询重武装中断标志），
    /// 无论 stop 落在子代理启动前还是运行中，结果都是确定的 Stopped。
    #[tokio::test]
    async fn background_subagent_can_be_stopped() {
        let parent_scripts = vec![
            // call0：派生后台子代理（无限任务）。
            {
                let mut v = tool_use(
                    "p1",
                    "task",
                    r#"{"description":"无限任务","prompt":"infinite-task","run_in_background":true}"#,
                );
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            // call1：停止它。
            {
                let mut v = tool_use("p2", "task_stop", r#"{"task_id":"task-1"}"#);
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            // call2：查询终态。
            {
                let mut v = tool_use("p3", "task_output", r#"{"task_id":"task-1"}"#);
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            text_end("收尾"),
        ];
        let model = Arc::new(StopModel {
            parent_scripts,
            parent_calls: Mutex::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let mut session = Session::with_subagents(parent_config(model.clone()));
        let (tx, mut rx) = mpsc::channel::<Event>(512);
        let turn = tokio::spawn(async move { session.run_turn("s-1", PARENT_INPUT, tx).await });

        let mut stopped_event = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let ev = tokio::time::timeout(deadline - tokio::time::Instant::now(), rx.recv())
                .await
                .expect("超时：事件流停滞")
                .expect("事件流意外结束");
            match ev.msg {
                EventMsg::SubagentCompleted { status, .. } => {
                    assert_eq!(status, SubagentStatus::Stopped, "子代理应以 Stopped 收尾");
                    stopped_event = true;
                }
                EventMsg::TurnCompleted { .. } => break,
                _ => {}
            }
        }
        let reason = turn.await.expect("turn 任务 panic").expect("turn 应成功");
        assert_eq!(reason, StopReason::Completed);
        assert!(stopped_event, "应见 SubagentCompleted{{Stopped}}");

        let seen = model.seen.lock().unwrap();
        let parents: Vec<&ChatRequest> = seen
            .iter()
            .filter(|r| first_text(r) == PARENT_INPUT)
            .collect();
        assert_eq!(parents.len(), 4, "父会话应有 4 轮采样");
        // call2 的请求：task_stop 结果 + 停止通知（循环头注入）。
        let p2 = request_text(parents[2]);
        assert!(p2.contains("task-1 stopped"), "task_stop 结果回灌: {p2}");
        assert!(p2.contains("status: stopped"));
        assert!(
            p2.contains("<task-notification>") && p2.matches("stopped").count() >= 2,
            "停止状态应同时经通知注入: {p2}"
        );
        // call3 的请求：task_output 查询到停止终态。
        let p3 = request_text(parents[3]);
        assert!(p3.contains("status: stopped"), "task_output 终态回灌: {p3}");
    }
}
