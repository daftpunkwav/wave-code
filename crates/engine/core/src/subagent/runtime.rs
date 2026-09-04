use super::*;

/// 后台任务的跟踪槽（任务表的值；id / 描述 / 类型由任务表键与
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
    /// 终态到达通知：finish_child 先置状态再 notify_one。`Notify` 的
    /// permit 留存语义保证"先 notify 后 wait"不丢失唤醒——stop 等待
    /// 终态无需轮询状态。
    notify: tokio::sync::Notify,
}

/// 后台终态通知队列上限：通知在父会话 turn 循环头消费，长时间无 turn
/// （交互空闲）时后台任务完成会在队列积压。超限丢弃最旧并以 warn 留痕
/// ——通知是终态的便利回注，权威终态在任务表槽位（task_output 可查），
/// 丢弃不丢结果。
const MAX_NOTIFICATIONS: usize = 64;

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
    /// 子代理审批槽：默认自建兜底槽（事件汇未挂接的无头形态，审批请求
    /// 无人能应答，fail-fast 拒绝落此槽）；`set_approval_gate` 注入父会话
    /// 槽后，审批经父事件流冒泡、由前端经 `Op::ExecApproval` 回填。
    approval_gate: Mutex<Arc<crate::session::ApprovalGate>>,
}

/// 子代理运行时：派生 / 跟踪 / 停止子代理，收集后台终态通知。
///
/// 由 [`Session::with_subagents`] 创建并注册三个工具；父会话 turn 循环头
/// 经 [`SubagentRuntime::drain_notifications`] 取走待注入通知。
pub struct SubagentRuntime {
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

impl SubagentRuntime {
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
                approval_gate: Mutex::new(Arc::new(crate::session::ApprovalGate::new())),
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
        *crate::sync::lock(&self.event_sink) = Some((events, submission_id.to_owned()));
    }

    /// 注入父会话审批槽（`Session::with_subagents` 在父 Session 构造后
    /// 调用）：子代理 Session 共享该槽，实现审批冒泡——子代理的
    /// `ApprovalRequested` 经父事件流到达前端，决策经 `Op::ExecApproval`
    /// 落到共享槽由子代理取走。
    pub fn set_approval_gate(&self, gate: Arc<crate::session::ApprovalGate>) {
        *crate::sync::lock(&self.deps.approval_gate) = gate;
    }

    /// 取走全部待注入通知（turn 循环头调用，一次性消费）。
    pub fn drain_notifications(&self) -> Vec<String> {
        std::mem::take(&mut *crate::sync::lock(&self.notifications))
    }

    /// 派生后台子代理：登记跟踪槽后在独立 tokio task 中运行，立即返回 id。
    pub fn spawn_background(self: &Arc<Self>, spec: TaskSpec) -> String {
        let id = self.alloc_id();
        let slot = Arc::new(TaskSlot {
            stop_requested: AtomicBool::new(false),
            interrupt: Mutex::new(None),
            state: Mutex::new(TaskState::Running),
            notify: tokio::sync::Notify::new(),
        });
        crate::sync::lock(&self.tasks).insert(id.clone(), slot.clone());
        let mgr = self.clone();
        let driver = self.clone();
        let driver_id = id.clone();
        let driver_slot = slot.clone();
        let fail_id = id.clone();
        let fail_spec = spec.clone();
        let fail_slot = slot.clone();
        tokio::spawn(async move {
            // 双层 spawn 做 panic 隔离：驱动任务的 panic 由内层
            // JoinHandle::await 以 JoinError 呈现，不会静默丢失——原实现
            // handle 直接 drop，panic 后 slot 永久 Running（task_output 恒
            // running、stop 轮询超时、无任何日志），任务表条目永不回收。
            let task = tokio::spawn(async move {
                driver.drive_child(driver_id, spec, Some(driver_slot)).await;
            });
            if task.await.is_err() {
                // 正常路径 drive_child 自行 finish_child；panic 路径由
                // 外层补齐：slot 置 Failed + 排队 task-notification +
                // SubagentCompleted 事件，父会话可感知异常终态。
                let result = TaskResult {
                    status: SubagentStatus::Failed,
                    summary: "subagent task panicked (driver task aborted)".to_owned(),
                    tokens_used: None,
                };
                mgr.finish_child(fail_id, &fail_spec, &fail_slot, result)
                    .await;
            }
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
        let slot = crate::sync::lock(&self.tasks).get(task_id).cloned()?;
        Some(crate::sync::lock(&slot.state).clone())
    }

    /// 停止后台子代理（task_stop）：置停止标志 + 中断句柄，等待终态
    ///（超时兜底返回当时的 Running 状态）。未知 id 返回 None。
    ///
    /// 等待体是 [`TaskSlot::notify`] 与周期 tick 的 select：终态到达经
    /// notify 即时唤醒（finish_child 先置状态再 notify_one，permit 留存
    /// 无丢失唤醒），不再依赖轮询检测终态；tick 唯一职责是周期重武装
    /// 中断标志——`run_turn` 入口会清一次中断标志，stop 恰好落在"句柄
    /// 已装配、turn 未开始"的窗口时单次置位会被抹掉，重武装保证窗口内
    /// 置位最终生效。
    pub async fn stop(&self, task_id: &str) -> Option<TaskState> {
        let slot = crate::sync::lock(&self.tasks).get(task_id).cloned()?;
        slot.stop_requested.store(true, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + STOP_WAIT_TIMEOUT;
        loop {
            if let Some(handle) = crate::sync::lock(&slot.interrupt).as_ref() {
                handle.store(true, Ordering::SeqCst);
            }
            let state = crate::sync::lock(&slot.state).clone();
            if matches!(state, TaskState::Finished(_)) {
                return Some(state);
            }
            let waited = tokio::time::timeout_at(deadline, async {
                tokio::select! {
                    _ = slot.notify.notified() => {}
                    _ = tokio::time::sleep(STOP_POLL_INTERVAL) => {}
                }
            })
            .await;
            if waited.is_err() {
                // 超时兜底：子代理仍卡在不可中断点（如挂起的工具执行），
                // 返回当时的 Running 状态。
                return Some(crate::sync::lock(&slot.state).clone());
            }
            // 通知唤醒（终态已登记）或 tick 到期（重武装）：回循环头
            // 复查状态与 deadline。
        }
    }

    /// 分配任务 id（`task-N`，1 起单调递增）。
    fn alloc_id(&self) -> String {
        format!("task-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// 发出子代理事件；返回是否真正发出（事件汇未挂接 = `false`——
    /// 无 turn 期间无旁观方，审批类事件无人能应答须由调用方 fail-fast）。
    async fn try_emit_event(&self, msg: EventMsg) -> bool {
        let sink = crate::sync::lock(&self.event_sink).clone();
        match sink {
            Some((tx, submission_id)) => {
                let ev = Event {
                    id: submission_id,
                    msg,
                };
                if tx.send(ev).await.is_err() {
                    tracing::debug!("父会话事件接收端已断开，子代理事件丢弃");
                    return false;
                }
                true
            }
            None => false,
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
    pub(in crate::subagent) fn child_config(&self, spec: &TaskSpec) -> SessionConfig {
        let (registry, todos) = match spec.subagent_type {
            SubagentType::GeneralPurpose => {
                // GP 子代理持全新的独立任务清单（与父会话 todo 隔离）：
                // 子代理的 planning 不读写父清单；registry 内 todo_write
                // 与该清单同源（builtin_with_todos 配对返回）。
                let (base, todos) = Registry::builtin_with_todos();
                let registry = match &spec.allowed_tools {
                    Some(names) => base.name_subset(names),
                    None => base,
                };
                (registry, todos)
            }
            SubagentType::Explore => {
                let base = Registry::builtin().read_only_subset();
                let registry = match &spec.allowed_tools {
                    Some(names) => base.name_subset(names),
                    None => base,
                };
                // explore 无 todo_write；空清单仅占位，与工具面无关。
                (registry, wavecode_tools::TodoStore::default())
            }
        };
        let builder = SessionConfig::builder(
            self.deps.model_name.clone(),
            self.deps.model.clone(),
            registry,
            self.deps.cwd.clone(),
        )
        .context_window(self.deps.context_window)
        .max_output_tokens(self.deps.max_output_tokens)
        .deny_env(self.deps.deny_env.clone())
        .sandbox(self.deps.sandbox.clone())
        .context(self.deps.context.clone())
        .todos(todos)
        .allowlist(wavecode_tools::ToolAllowlist::default());
        // 审批槽：共享（父槽注入后冒泡；未注入时自建兜底槽承接 fail-fast）。
        builder
            .approval_gate(crate::sync::lock(&self.deps.approval_gate).clone())
            .build()
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
        let _ = self
            .try_emit_event(EventMsg::SubagentStarted {
                task_id: task_id.clone(),
                subagent_type: spec.subagent_type.as_str().to_owned(),
                description: spec.description.clone(),
            })
            .await;

        // 子代理经 Session::new 构造：无 task 工具（深度上限 1），无通知
        // 注入路径（subagents 字段为 None）。
        let mut session = Session::new(self.child_config(&spec));
        if let Some(slot) = &slot {
            *crate::sync::lock(&slot.interrupt) = Some(session.interrupt_handle());
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
        // 审批冒泡（drain 侧）：子代理的 ApprovalRequested 转发到父事件流，
        // 前端经同一 Op::ExecApproval 回填共享槽（子代理 Session 共享父
        // gate，call_id 键控父子不冲突）。事件汇未挂接 = 无人能应答——
        // 直接以拒绝落槽 fail-fast，子代理不 park 挂死（同步形态曾挂死
        // 整个父会话）。
        let mgr = self.clone();
        let gate = crate::sync::lock(&self.deps.approval_gate).clone();
        let drain = async {
            while let Some(ev) = child_rx.recv().await {
                match ev.msg {
                    EventMsg::AgentMessageComplete { text } => last_text = text,
                    EventMsg::TokenCount { used, .. } => tokens_used = Some(used),
                    EventMsg::ApprovalRequested {
                        call_id,
                        kind,
                        detail,
                    } => {
                        let forwarded = mgr
                            .try_emit_event(EventMsg::ApprovalRequested {
                                call_id: call_id.clone(),
                                kind,
                                detail,
                            })
                            .await;
                        if !forwarded {
                            gate.decide(
                                call_id,
                                wavecode_protocol::ApprovalDecision::Deny {
                                    reason: "subagent has no event sink attached: approval cannot be answered"
                                        .to_owned(),
                                },
                            );
                        }
                    }
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
                self.try_emit_event(EventMsg::SubagentCompleted {
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
        *crate::sync::lock(&slot.state) = TaskState::Finished(result.clone());
        slot.notify.notify_one();
        push_notification(
            &mut crate::sync::lock(&self.notifications),
            format_notification(&task_id, spec.subagent_type, &spec.description, &result),
        );
        self.try_emit_event(EventMsg::SubagentCompleted {
            task_id,
            status: result.status,
            summary: result.summary.clone(),
        })
        .await;
        result
    }
}

/// 通知入队（带上限）：超限丢弃最旧并以 warn 留痕（见
/// [`MAX_NOTIFICATIONS`] 的取舍注释）。
fn push_notification(queue: &mut Vec<String>, note: String) {
    queue.push(note);
    let mut dropped = 0usize;
    while queue.len() > MAX_NOTIFICATIONS {
        queue.remove(0);
        dropped += 1;
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            cap = MAX_NOTIFICATIONS,
            "子代理通知队列溢出，丢弃最旧通知（终态仍可经 task_output 查询）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 通知队列上限：超限丢弃最旧、长度封顶（回归 MAX_NOTIFICATIONS）。
    #[test]
    fn notification_queue_is_capped_dropping_oldest() {
        let mut q = Vec::new();
        for i in 0..MAX_NOTIFICATIONS + 3 {
            push_notification(&mut q, format!("n{i}"));
        }
        assert_eq!(q.len(), MAX_NOTIFICATIONS);
        assert_eq!(q.first().map(String::as_str), Some("n3"), "最旧三条被丢弃");
        let newest = format!("n{}", MAX_NOTIFICATIONS + 2);
        assert_eq!(q.last().map(String::as_str), Some(newest.as_str()));
    }
}
