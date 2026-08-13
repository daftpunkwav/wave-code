//! actor 循环（阶段 5 拆分自 lib.rs）：事件分发 / 审批路由 / 中断 / 退出。

use super::*;

/// Session actor 主循环：串行驱动 turn；turn 期间经 select! 继续监听
/// submission 通道（响应 Interrupt / Shutdown / ExecApproval /
/// SetPermissionMode，UserInput / Compact / SlashCommand 本地排队，队列
/// 有界——溢出经 [`queue_or_reject`] 显式拒绝）。
/// submission 通道关闭（客户端全部析构）即退出；event 通道随本任务
/// 持有的 event_tx 析构而关闭，客户端 next_event 收 None。
pub(super) async fn actor_loop(
    mut session: Session,
    mut submission_rx: mpsc::Receiver<Submission>,
    event_tx: mpsc::Sender<Event>,
    interrupt_handle: Arc<AtomicBool>,
    approval_handle: Arc<ApprovalGate>,
    permission_mode_handle: Arc<std::sync::Mutex<PermissionMode>>,
) {
    // turn 期间到达的 UserInput 本地排队，turn 结束后按序驱动，不丢请求。
    // pending 有界（PENDING_QUEUE_CAPACITY）：溢出显式拒绝而非背压挂起——
    // 背压会把队列头部后面的 Interrupt 也堵在通道里（用户无法中断），
    // 拒绝让客户端立即得到确定响应。
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
                                    // 可排队 op（UserInput / Compact /
                                    // SlashCommand）：队列满时显式拒绝。
                                    Op::UserInput { .. } | Op::Compact | Op::SlashCommand { .. } => {
                                        queue_or_reject(&mut pending, extra, &event_tx).await
                                    }
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
                                        *lock_mode(&permission_mode_handle) = mode;
                                    }
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
                *lock_mode(&permission_mode_handle) = mode;
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

/// 权限模式句柄锁的统一恢复策略（本 crate 单点决策）：临界区是单次
/// 赋值，持锁期间 panic 不会留下半截不变量——中毒时取回守卫继续执行
///（panic 已沿原线程传播），不做二次 panic 级联。
fn lock_mode(m: &std::sync::Mutex<PermissionMode>) -> std::sync::MutexGuard<'_, PermissionMode> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// turn 期间到达的可排队 op（UserInput / Compact / SlashCommand）入
/// pending 队尾，turn 结束后按序驱动（FIFO）。
///
/// 排队而非立即执行的原因：Compact 会改写历史、SlashCommand 可能改写
/// 历史 / 派生子代理，turn 中途执行会破坏当轮快照；UserInput 即新 turn。
///
/// 队列满（[`PENDING_QUEUE_CAPACITY`]）时以该 submission 的 id 回填
/// Error 事件显式拒绝（recoverable）——请求不静默丢弃：拒绝即确定的
/// 响应（SPEC §4.2"请求不允许丢"的进程内形态；stdio/WS transport 落地
/// 时同语义映射为 JSON-RPC 错误响应）。控制类 op（Interrupt /
/// ExecApproval / SetPermissionMode / Shutdown）不入队，不受此限。
async fn queue_or_reject(
    pending: &mut VecDeque<Submission>,
    sub: Submission,
    event_tx: &mpsc::Sender<Event>,
) {
    if pending.len() >= PENDING_QUEUE_CAPACITY {
        tracing::warn!(id = %sub.id, "pending 队列已满，显式拒绝排队请求");
        let ev = Event {
            id: sub.id,
            msg: EventMsg::Error {
                message: format!("请求队列已满（{PENDING_QUEUE_CAPACITY} 条待处理），请稍后重试"),
                recoverable: true,
            },
        };
        // send 失败即接收端已断开：与 core emit 同策略，继续执行不中断。
        let _ = event_tx.send(ev).await;
    } else {
        pending.push_back(sub);
    }
}
