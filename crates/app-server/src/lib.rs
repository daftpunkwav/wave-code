//! wavecode-app-server — 以 JSON-RPC 2.0 统一暴露 core 能力。
//!
//! 三种 transport（目标形态）：
//! - stdio（NDJSON）：Desktop / SDK 以子进程方式接入；
//! - WebSocket：Web UI 接入；
//! - 进程内双工通道：TUI 零 IPC 开销直连。
//!
//! M1 仅落地进程内 transport（[`InProcessClient`]）：Submission / Event
//! 经两条 mpsc 通道直传，零 JSON 序列化往返；JSON-RPC 编码层随
//! stdio / WebSocket transport 在后续里程碑引入。
//!
//! 另规划 `generate-ts`（后续里程碑落地）：从 [`wavecode_protocol`]
//! 类型导出 TypeScript schema，保证前端类型与协议永远一致。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use wavecode_core::{ApprovalGate, Session, SessionConfig};
use wavecode_protocol::{Event, Op, PermissionMode, Submission};

/// submission 通道容量（前端 → actor）。
const SUBMISSION_CHANNEL_CAPACITY: usize = 32;
/// event 通道容量（actor → 前端）。
const EVENT_CHANNEL_CAPACITY: usize = 256;
/// Shutdown / 客户端全部析构时，等待活动 turn 收尾的超时。
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// 进程内 transport 客户端句柄：Submission / Event 经两条 mpsc 通道
/// 与 core Session actor 直传，零 JSON 序列化往返。
///
/// 析构即结束会话：置中断标志并 abort actor 任务（见 `Drop` 实现）。
pub struct InProcessClient {
    submit_tx: mpsc::Sender<Submission>,
    event_rx: mpsc::Receiver<Event>,
    /// 中断标志共享句柄（与 actor 内克隆同源）；Drop 时置位。
    interrupt_handle: Arc<AtomicBool>,
    actor_handle: JoinHandle<()>,
}

impl InProcessClient {
    /// 启动 core Session actor 任务并返回客户端句柄。
    ///
    /// 须在 tokio runtime 上下文内调用（内部 `tokio::spawn`）。
    pub fn spawn(cfg: SessionConfig) -> Self {
        let (submit_tx, submission_rx) = mpsc::channel(SUBMISSION_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        // P5：生产路径的父会话具备子代理能力（task 工具注册进 registry；
        // 后台终态经 SubagentStarted/Completed 事件对前端可见）。
        let session = Session::with_subagents(cfg);
        // T7 解锁的驱动模式：驱动 turn 前克隆句柄，actor 在 select! 中
        // 经句柄置位，绕开 run_turn(&mut self) 的借用冲突。
        let interrupt_handle = session.interrupt_handle();
        // P2 同模式：审批共享槽与权限模式句柄（ExecApproval / SetPermissionMode
        // 的 in-turn select! 路由，§17.5 M3）。
        let approval_handle = session.approval_handle();
        let permission_mode_handle = session.permission_mode_handle();
        let actor_handle = tokio::spawn(actor_loop(
            session,
            submission_rx,
            event_tx,
            interrupt_handle.clone(),
            approval_handle,
            permission_mode_handle,
        ));
        Self {
            submit_tx,
            event_rx,
            interrupt_handle,
            actor_handle,
        }
    }

    /// 投递一次请求；actor 已退出（通道关闭）时返回错误。
    pub async fn submit(&self, sub: Submission) -> anyhow::Result<()> {
        self.submit_tx
            .send(sub)
            .await
            .map_err(|_| anyhow::anyhow!("session actor 已退出，submission 无法投递"))
    }

    /// 拉取下一事件；Shutdown 完成（actor 退出、通道关闭）后返回 None。
    pub async fn next_event(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }
}

impl Drop for InProcessClient {
    fn drop(&mut self) {
        // 中断标志只是并发 poll 窗口内的最佳努力：actor 若在 abort 生效前
        // 恰好到达安全点，可走优雅收尾；abort 立即取消兜底——模型流挂起
        //（无安全点可达）时 actor 任务也不泄漏。
        self.interrupt_handle.store(true, Ordering::SeqCst);
        self.actor_handle.abort();
    }
}

/// Session actor 主循环：串行驱动 turn；turn 期间经 select! 继续监听
/// submission 通道（响应 Interrupt / Shutdown / ExecApproval /
/// SetPermissionMode，UserInput / Compact 本地排队）。
/// submission 通道关闭（客户端全部析构）即退出；event 通道随本任务
/// 持有的 event_tx 析构而关闭，客户端 next_event 收 None。
async fn actor_loop(
    mut session: Session,
    mut submission_rx: mpsc::Receiver<Submission>,
    event_tx: mpsc::Sender<Event>,
    interrupt_handle: Arc<AtomicBool>,
    approval_handle: Arc<ApprovalGate>,
    permission_mode_handle: Arc<std::sync::Mutex<PermissionMode>>,
) {
    // turn 期间到达的 UserInput 本地排队，turn 结束后按序驱动，不丢请求。
    // 取舍：turn 期间 actor 持续 recv 抽干通道，pending 无界——M1 进程内
    // 可信客户端可接受；如需上限（不可信前端 / stdio transport）后续再议。
    let mut pending: VecDeque<Submission> = VecDeque::new();
    loop {
        let sub = match pending.pop_front() {
            Some(sub) => Some(sub),
            None => submission_rx.recv().await,
        };
        let Some(sub) = sub else {
            return;
        };
        match sub.op {
            Op::UserInput { text } => {
                // P6：in-turn Shutdown 时 run_turn 借用未释放，无法经 &Session
                // 调用提取入口——驱动 turn 前预取句柄（快照语义见 core 侧注释）。
                let mut extraction = session.memory_extraction_handle();
                // 事件由 run_turn 直接写入 event 通道（id 已在 run_turn 内回填）。
                let turn = session.run_turn(&sub.id, &text, event_tx.clone());
                tokio::pin!(turn);
                let shutdown = loop {
                    tokio::select! {
                        result = &mut turn => {
                            // run_turn 出错前已发 Error + TurnCompleted{Error}
                            //（core T7）：记 error 日志，actor 继续存活。
                            if let Err(e) = result {
                                tracing::error!(error = %e, "run_turn 失败，actor 继续存活");
                            }
                            break false;
                        }
                        maybe_sub = submission_rx.recv() => {
                            match maybe_sub {
                                Some(extra) => match extra.op {
                                    Op::UserInput { .. } => pending.push_back(extra),
                                    Op::Interrupt => {
                                        interrupt_handle.store(true, Ordering::SeqCst);
                                    }
                                    // P2：审批回填路由到 park 在 AwaitApproval 的
                                    // turn（共享槽按 call_id 键控；无等待者时
                                    // permit 留存，decide 先于 wait 也不丢）。
                                    Op::ExecApproval { call_id, decision } => {
                                        approval_handle.decide(call_id, decision);
                                    }
                                    // P2：turn 进行中切换权限模式，下一次
                                    // sandbox 判定即生效。
                                    Op::SetPermissionMode { mode } => {
                                        *permission_mode_handle
                                            .lock()
                                            .expect("mode 锁中毒即进程已有 panic") = mode;
                                    }
                                    // P3：turn 进行中的 /compact 与 UserInput
                                    // 同策略——排队到 turn 结束后执行（压缩会
                                    // 改写历史，turn 中途执行会破坏当轮快照）。
                                    Op::Compact => pending.push_back(extra),
                                    // P7：turn 进行中的 slash 直调同样排队
                                    //（skill 触发会改写历史 / 派生子代理）。
                                    Op::SlashCommand { .. } => pending.push_back(extra),
                                    Op::Shutdown => {
                                        interrupt_handle.store(true, Ordering::SeqCst);
                                        // 等 turn 收尾（至多 2s），然后退出。
                                        let _ = tokio::time::timeout(
                                            SHUTDOWN_DRAIN_TIMEOUT,
                                            &mut turn,
                                        )
                                        .await;
                                        break true;
                                    }
                                    // Op 标注 non_exhaustive：未来新增的 op 在 M1 忽略，warn 留痕。
                                    _ => {
                                        tracing::warn!(id = %extra.id, "忽略未知 op（M1 未实现）");
                                    }
                                },
                                None => {
                                    // 客户端全部析构，等价隐式 Shutdown。
                                    interrupt_handle.store(true, Ordering::SeqCst);
                                    let _ =
                                        tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut turn)
                                            .await;
                                    // P6：SessionEnd 触发记忆自动提取（同上）。
                                    if let Some(handle) = extraction.take() {
                                        handle.spawn();
                                    }
                                    return;
                                }
                            }
                        }
                    }
                };
                if shutdown {
                    // P6：SessionEnd 触发记忆自动提取（后台 detached，不阻塞退出）。
                    if let Some(handle) = extraction.take() {
                        handle.spawn();
                    }
                    return;
                }
            }
            // 无活动 turn 的 Interrupt：忽略（不发事件）。
            Op::Interrupt => {}
            // 无活动 turn 的迟到审批（turn 已结束 / 未在等审批）：忽略并
            // warn 留痕——共享槽以 call_id 键控，存入也不会被误消费，
            // 但回填一个无人等待的决策说明前后端时序已脱节。
            Op::ExecApproval { call_id, .. } => {
                tracing::warn!(id = %sub.id, %call_id, "忽略无等待者的审批回填");
            }
            // 无活动 turn 的 SetPermissionMode：直接生效（句柄共享，
            // 下一 turn 的 sandbox 判定即用新模式）。
            Op::SetPermissionMode { mode } => {
                *permission_mode_handle
                    .lock()
                    .expect("mode 锁中毒即进程已有 panic") = mode;
            }
            // 无活动 turn 的 Shutdown：直接退出。
            Op::Shutdown => {
                // P6：SessionEnd 触发记忆自动提取（后台 detached，不阻塞退出）。
                session.spawn_memory_extraction();
                return;
            }
            // P3：无活动 turn 的 /compact——立即压缩（事件以该 submission
            // 的 id 回填）；压缩失败已由 Session::compact 发 Error 事件，
            // 此处记日志即可，actor 继续存活。
            Op::Compact => {
                if let Err(e) = session.compact(&sub.id, event_tx.clone()).await {
                    tracing::error!(id = %sub.id, error = %e, "手动压缩失败");
                }
            }
            // P7：无活动 turn 的 slash 直调 skill（SPEC §8.2）：inline 驱动
            // 一轮 turn，fork 派生后台子代理；错误已由 Session::invoke_skill
            // 发 Error + TurnCompleted，Err 返回即引擎级失败，记日志存活。
            Op::SlashCommand { name, args } => {
                if let Err(e) = session
                    .invoke_skill(&sub.id, &name, &args, event_tx.clone())
                    .await
                {
                    tracing::error!(id = %sub.id, error = %e, "slash skill 触发失败");
                }
            }
            // Op 标注 non_exhaustive：未来新增的 op 在 M1 忽略，warn 留痕。
            _ => tracing::warn!(id = %sub.id, "忽略未知 op（M1 未实现）"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, stream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use wavecode_llm::{ChatModel, ChatRequest, StreamEvent, Usage};
    use wavecode_protocol::{Event, EventMsg, Op, Submission};

    struct MockModel {
        scripts: Vec<Vec<StreamEvent>>,
        calls: Mutex<u32>,
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
            let events = self.scripts[idx].clone();
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
        }
    }

    /// 门控 mock（与 core/src/session.rs 的 GatedModel 同构）：第一次调用
    /// 回放脚本后挂起，直到 `gate` 置位；尾部形态由 `repeat_tail` 决定：
    /// - false：gate 置位后流直接结束——FIFO 排队测试用来撑开 turn 窗口，
    ///   等测试把第二个 submission 送进 pending 后放流收尾；
    /// - true：gate 置位后每 1ms 产出一个 sentinel，流不结束——中断测试的
    ///   turn 只能靠 Interrupt 在安全点收尾；每个流元素都是中断检查点，
    ///   测试置位与 actor 处理 Interrupt 之间无竞态。
    ///
    /// 第二次调用起为普通脚本流（复用末个脚本，无门控尾部），验证中断 /
    /// 排队后的下一 turn 正常完成。
    struct GatedModel {
        scripts: Vec<Vec<StreamEvent>>,
        calls: Mutex<u32>,
        gate: Arc<AtomicBool>,
        repeat_tail: bool,
    }

    #[async_trait::async_trait]
    impl ChatModel for GatedModel {
        async fn stream(
            &self,
            _req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            let call_no = {
                let mut n = self.calls.lock().unwrap();
                let call_no = *n as usize;
                *n += 1;
                call_no
            };
            let idx = call_no.min(self.scripts.len().saturating_sub(1));
            let script = stream::iter(self.scripts[idx].clone().into_iter().map(Ok));
            if call_no > 0 {
                return Ok(Box::pin(script));
            }
            let gate = self.gate.clone();
            let repeat = self.repeat_tail;
            let tail = stream::unfold((), move |()| {
                let gate = gate.clone();
                async move {
                    while !gate.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    if !repeat {
                        return None;
                    }
                    // gate 置位后每 1ms 一个 sentinel：流不结束，turn 只能
                    // 被 Interrupt 收尾；每个流元素都先过中断检查点。
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    Some((
                        Ok(StreamEvent::TextDelta {
                            text: "tail-sentinel".into(),
                        }),
                        (),
                    ))
                }
            });
            Ok(Box::pin(script.chain(tail)))
        }
    }

    fn cfg_with(scripts: Vec<Vec<StreamEvent>>) -> wavecode_core::SessionConfig {
        cfg_with_model(Arc::new(MockModel {
            scripts,
            calls: Mutex::new(0),
        }))
    }

    fn cfg_with_model(model: Arc<dyn wavecode_llm::ChatModel>) -> wavecode_core::SessionConfig {
        cfg_with_sandbox(
            model,
            // 既有路由测试不涉审批：bypassPermissions 全放行，保持 M1 语义；
            // 审批路由由 P2 专项测试（default / plan 模式）锁定。
            wavecode_sandbox::Sandbox::without_rules(
                wavecode_protocol::PermissionMode::BypassPermissions,
            ),
        )
    }

    fn cfg_with_sandbox(
        model: Arc<dyn wavecode_llm::ChatModel>,
        sandbox: wavecode_sandbox::Sandbox,
    ) -> wavecode_core::SessionConfig {
        // tempdir 不能 drop——keep() 转持久路径、放弃自动删除（M1 测试可接受）。
        let cwd = tempfile::tempdir().unwrap().keep();
        wavecode_core::SessionConfig {
            model_name: "mock".into(),
            context_window: 200_000,
            max_output_tokens: 8192,
            model,
            registry: wavecode_tools::Registry::builtin(),
            cwd,
            deny_env: Vec::new(),
            sandbox,
            context: Default::default(),
            memory: None,
            skills: None,
            hooks: None,
            rollout: None,
        }
    }

    fn one_shot(text: &str) -> Vec<Vec<StreamEvent>> {
        vec![vec![
            StreamEvent::TextDelta { text: text.into() },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
        ]]
    }

    /// 消费事件直至 `pred` 命中并返回该事件；5s 超时兜底（回归时立即
    /// panic 给出明确信息，而不是挂到 CI 整体超时），事件流意外结束
    ///（actor 提前退出）同样立即 panic。
    async fn next_event_until(
        client: &mut InProcessClient,
        what: &str,
        mut pred: impl FnMut(&Event) -> bool,
    ) -> Event {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let Some(ev) = client.next_event().await else {
                    panic!("事件流意外结束（actor 提前退出）");
                };
                if pred(&ev) {
                    break ev;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("超时：5s 内未等到 {what}"))
    }

    #[tokio::test]
    async fn submit_user_input_streams_events_until_completed() {
        let mut client = InProcessClient::spawn(cfg_with(one_shot("pong")));
        client
            .submit(Submission {
                id: "s-1".into(),
                op: Op::UserInput {
                    text: "ping".into(),
                },
            })
            .await
            .unwrap();
        let mut saw_delta = false;
        let ev = next_event_until(&mut client, "turn 完成", |ev| {
            assert_eq!(ev.id, "s-1");
            if matches!(&ev.msg, EventMsg::AgentMessageDelta { text } if text.as_str() == "pong") {
                saw_delta = true;
            }
            matches!(&ev.msg, EventMsg::TurnCompleted { .. })
        })
        .await;
        assert!(saw_delta, "应收到 pong 文本增量");
        assert!(matches!(ev.msg, EventMsg::TurnCompleted { .. }));
    }

    #[tokio::test]
    async fn second_turn_after_first_works() {
        // interrupt 标志每 turn 自清：连续两个 turn 都正常完成
        let mut client = InProcessClient::spawn(cfg_with(one_shot("reply")));
        for i in 0..2 {
            client
                .submit(Submission {
                    id: format!("s-{i}"),
                    op: Op::UserInput { text: "go".into() },
                })
                .await
                .unwrap();
            let ev = next_event_until(&mut client, "turn 完成", |ev| {
                matches!(&ev.msg, EventMsg::TurnCompleted { .. })
            })
            .await;
            assert!(
                matches!(
                    ev.msg,
                    EventMsg::TurnCompleted {
                        stop_reason: wavecode_protocol::StopReason::Completed
                    }
                ),
                "turn {i} should complete"
            );
        }
    }

    #[tokio::test]
    async fn shutdown_ends_event_stream() {
        let mut client = InProcessClient::spawn(cfg_with(one_shot("x")));
        client
            .submit(Submission {
                id: "x".into(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        assert!(client.next_event().await.is_none());
    }

    /// 通道关闭语义锁定：actor 已 Shutdown 退出后再 submit 须返回 Err，
    /// 不得静默丢弃。
    #[tokio::test]
    async fn submit_after_shutdown_returns_err() {
        let mut client = InProcessClient::spawn(cfg_with(one_shot("x")));
        client
            .submit(Submission {
                id: "x".into(),
                op: Op::Shutdown,
            })
            .await
            .unwrap();
        // 事件流关闭（None）意味着 actor 任务已退出、submission 通道随之关闭。
        assert!(client.next_event().await.is_none());
        let result = client
            .submit(Submission {
                id: "late".into(),
                op: Op::UserInput { text: "hi".into() },
            })
            .await;
        assert!(result.is_err(), "actor 已退出时 submit 应返回 Err");
    }

    #[tokio::test]
    async fn interrupt_during_turn_completes_interrupted_then_next_turn_ok() {
        let gate = Arc::new(AtomicBool::new(false));
        let model = GatedModel {
            scripts: one_shot("first"),
            calls: Mutex::new(0),
            gate: gate.clone(),
            repeat_tail: true,
        };
        let mut client = InProcessClient::spawn(cfg_with_model(Arc::new(model)));
        client
            .submit(Submission {
                id: "s-1".into(),
                op: Op::UserInput { text: "go".into() },
            })
            .await
            .unwrap();

        // 等 turn 1 的脚本 delta：确认 turn 进行中（流挂起在门控尾部），
        // 再提交 Interrupt 并放流——gate 置位后尾部每 1ms 一个 sentinel，
        // actor 置位中断标志后，下一个 sentinel 先过检查点即收尾，无竞态。
        next_event_until(&mut client, "turn 1 的脚本 delta", |ev| {
            matches!(&ev.msg, EventMsg::AgentMessageDelta { text } if text.as_str() == "first")
        })
        .await;
        client
            .submit(Submission {
                id: "s-1".into(),
                op: Op::Interrupt,
            })
            .await
            .unwrap();
        gate.store(true, Ordering::SeqCst);
        let ev = next_event_until(&mut client, "TurnCompleted{Interrupted}", |ev| {
            matches!(&ev.msg, EventMsg::TurnCompleted { .. })
        })
        .await;
        assert_eq!(ev.id, "s-1");
        assert!(
            matches!(
                ev.msg,
                EventMsg::TurnCompleted {
                    stop_reason: wavecode_protocol::StopReason::Interrupted
                }
            ),
            "turn 1 应以 Interrupted 收尾"
        );

        // 中断标志每 turn 自清、actor 存活：第二个 turn 正常完成。
        client
            .submit(Submission {
                id: "s-2".into(),
                op: Op::UserInput { text: "go".into() },
            })
            .await
            .unwrap();
        let ev = next_event_until(&mut client, "turn 2 完成", |ev| {
            matches!(&ev.msg, EventMsg::TurnCompleted { .. })
        })
        .await;
        assert_eq!(ev.id, "s-2");
        assert!(
            matches!(
                ev.msg,
                EventMsg::TurnCompleted {
                    stop_reason: wavecode_protocol::StopReason::Completed
                }
            ),
            "中断后的下一 turn 应正常完成"
        );
    }

    #[tokio::test]
    async fn submissions_during_turn_are_queued_fifo() {
        let gate = Arc::new(AtomicBool::new(false));
        let model = GatedModel {
            scripts: one_shot("r1"),
            calls: Mutex::new(0),
            gate: gate.clone(),
            repeat_tail: false,
        };
        let mut client = InProcessClient::spawn(cfg_with_model(Arc::new(model)));
        client
            .submit(Submission {
                id: "s-1".into(),
                op: Op::UserInput { text: "go".into() },
            })
            .await
            .unwrap();

        // 等 turn 1 开始（流挂起在门控尾部）：s-2、s-3 都在 turn 1 期间提交，
        // actor 在 select! 中按到达序入 pending 队列，随后放流收尾。
        let ev = next_event_until(&mut client, "turn 1 开始", |ev| {
            matches!(&ev.msg, EventMsg::TurnStarted { .. })
        })
        .await;
        assert_eq!(ev.id, "s-1");
        for id in ["s-2", "s-3"] {
            client
                .submit(Submission {
                    id: id.into(),
                    op: Op::UserInput { text: "go".into() },
                })
                .await
                .unwrap();
        }
        gate.store(true, Ordering::SeqCst);

        // FIFO：turn 1 期间全部事件以 s-1 回填；s-1 的 TurnCompleted
        // 先于 pending 队首的 TurnStarted（LIFO 会相反）。
        next_event_until(&mut client, "s-1 完成", |ev| {
            assert_eq!(ev.id, "s-1", "turn 1 期间混入其他 submission 的事件");
            matches!(&ev.msg, EventMsg::TurnCompleted { .. })
        })
        .await;

        // 三元素保序（单元素队列分不清 FIFO/LIFO）：s-2 的 TurnStarted
        // 必须先于 s-3 的 TurnStarted。
        let ev = next_event_until(&mut client, "s-2 开始", |ev| {
            matches!(&ev.msg, EventMsg::TurnStarted { .. })
        })
        .await;
        assert_eq!(ev.id, "s-2", "s-1 完成后才轮到 pending 中的 s-2");

        // turn 2（pending 弹出驱动）正常完成，事件以 s-2 回填。
        let ev = next_event_until(&mut client, "s-2 完成", |ev| {
            assert_eq!(ev.id, "s-2", "turn 2 期间混入其他 submission 的事件");
            matches!(&ev.msg, EventMsg::TurnCompleted { .. })
        })
        .await;
        assert!(
            matches!(
                ev.msg,
                EventMsg::TurnCompleted {
                    stop_reason: wavecode_protocol::StopReason::Completed
                }
            ),
            "排队 turn 应正常完成"
        );
    }

    /// 单轮 write_file 调用 + 收尾文本的脚本（default 模式触发审批门）。
    fn write_file_scripts() -> Vec<Vec<StreamEvent>> {
        vec![
            vec![
                StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "write_file".into(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"path":"x.txt","content":"x"}"#.into(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                },
            ],
            vec![
                StreamEvent::TextDelta {
                    text: "done".into(),
                },
                StreamEvent::MessageComplete {
                    stop_reason: "end_turn".into(),
                    usage: Usage::default(),
                },
            ],
        ]
    }

    /// P2：ExecApproval 经 actor in-turn select! 路由——turn park 在
    /// AwaitApproval，协议回填放行后工具执行、turn 正常完成。
    #[tokio::test]
    async fn exec_approval_routed_to_waiting_turn() {
        let model = MockModel {
            scripts: write_file_scripts(),
            calls: Mutex::new(0),
        };
        let mut client = InProcessClient::spawn(cfg_with_sandbox(
            Arc::new(model),
            wavecode_sandbox::Sandbox::without_rules(wavecode_protocol::PermissionMode::Default),
        ));
        client
            .submit(Submission {
                id: "s-1".into(),
                op: Op::UserInput {
                    text: "写文件".into(),
                },
            })
            .await
            .unwrap();

        let ev = next_event_until(&mut client, "ApprovalRequested", |ev| {
            matches!(&ev.msg, EventMsg::ApprovalRequested { .. })
        })
        .await;
        let EventMsg::ApprovalRequested { call_id, .. } = ev.msg else {
            unreachable!("已按谓词命中 ApprovalRequested")
        };
        client
            .submit(Submission {
                id: "s-1".into(),
                op: Op::ExecApproval {
                    call_id,
                    decision: wavecode_protocol::ApprovalDecision::AllowOnce,
                },
            })
            .await
            .unwrap();

        let mut saw_ok_end = false;
        let ev = next_event_until(&mut client, "turn 完成", |ev| {
            if matches!(&ev.msg, EventMsg::ToolCallEnd { ok: true, .. }) {
                saw_ok_end = true;
            }
            matches!(&ev.msg, EventMsg::TurnCompleted { .. })
        })
        .await;
        assert!(saw_ok_end, "放行后工具应执行成功");
        assert!(
            matches!(
                ev.msg,
                EventMsg::TurnCompleted {
                    stop_reason: wavecode_protocol::StopReason::Completed
                }
            ),
            "审批放行的 turn 应正常完成"
        );
    }

    /// P3：Op::Compact 路由——无活动 turn 时立即压缩，CompactStarted{Manual}
    /// 与 CompactCompleted 以该 submission 的 id 回填（不经 TurnStarted/
    /// TurnCompleted——压缩不是 turn）。
    #[tokio::test]
    async fn compact_op_triggers_immediate_compaction() {
        let mut client =
            InProcessClient::spawn(cfg_with(one_shot("摘要：目标/进展/关键决策/文件清单/待办")));
        client
            .submit(Submission {
                id: "c-1".into(),
                op: Op::Compact,
            })
            .await
            .unwrap();
        let mut saw_started = false;
        let ev = next_event_until(&mut client, "CompactCompleted", |ev| {
            assert_eq!(ev.id, "c-1", "压缩事件应以 submission id 回填");
            if matches!(&ev.msg, EventMsg::CompactStarted { trigger } if *trigger == wavecode_protocol::CompactTrigger::Manual)
            {
                saw_started = true;
            }
            matches!(&ev.msg, EventMsg::CompactCompleted { .. })
        })
        .await;
        assert!(saw_started, "应先见 CompactStarted{{Manual}}");
        assert!(matches!(ev.msg, EventMsg::CompactCompleted { .. }));
    }

    /// P2：SetPermissionMode 路由——无活动 turn 时切换为 plan 立即生效，
    /// 随后 turn 的写工具被拦截（无审批请求、ok=false），turn 正常完成。
    #[tokio::test]
    async fn set_permission_mode_switches_to_plan_and_blocks_write() {
        let model = MockModel {
            scripts: write_file_scripts(),
            calls: Mutex::new(0),
        };
        let mut client = InProcessClient::spawn(cfg_with_sandbox(
            Arc::new(model),
            wavecode_sandbox::Sandbox::without_rules(wavecode_protocol::PermissionMode::Default),
        ));
        client
            .submit(Submission {
                id: "m".into(),
                op: Op::SetPermissionMode {
                    mode: wavecode_protocol::PermissionMode::Plan,
                },
            })
            .await
            .unwrap();
        client
            .submit(Submission {
                id: "s-1".into(),
                op: Op::UserInput {
                    text: "写文件".into(),
                },
            })
            .await
            .unwrap();

        let mut saw_fail_end = false;
        let ev = next_event_until(&mut client, "turn 完成", |ev| {
            assert!(
                !matches!(&ev.msg, EventMsg::ApprovalRequested { .. }),
                "plan 模式拦截不应发审批请求"
            );
            if matches!(&ev.msg, EventMsg::ToolCallEnd { ok: false, .. }) {
                saw_fail_end = true;
            }
            matches!(&ev.msg, EventMsg::TurnCompleted { .. })
        })
        .await;
        assert!(saw_fail_end, "plan 模式写工具应被拦截");
        assert!(
            matches!(
                ev.msg,
                EventMsg::TurnCompleted {
                    stop_reason: wavecode_protocol::StopReason::Completed
                }
            ),
            "plan 拦截的 turn 应正常完成"
        );
    }
}
