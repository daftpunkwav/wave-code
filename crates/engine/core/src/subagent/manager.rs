use super::*;

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
    pub(in crate::subagent) fn child_config(&self, spec: &TaskSpec) -> SessionConfig {
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
