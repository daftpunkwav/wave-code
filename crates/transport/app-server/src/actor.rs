//! actor 循环（阶段 5 拆分自 lib.rs）：事件分发 / 审批路由 / 中断 / 退出。

use super::*;
use std::future::Future;

/// 会话生命周期 hook 执行（SessionStart / SessionEnd，P7 收口 actor）：
/// 警告经 Warning 事件进事件流，前端可见；id 为合成值（无对应 Submission）。
async fn run_session_lifecycle(
    session: &Session,
    point: wavecode_core::hooks::HookEventPoint,
    event_tx: &mpsc::Sender<Event>,
) {
    for warning in session.run_lifecycle_hook(point).await {
        let _ = event_tx
            .send(Event {
                id: "session-lifecycle".to_owned(),
                msg: EventMsg::Warning { message: warning },
            })
            .await;
    }
}

/// actor 控制面句柄束：事件通道 + 三类共享控制句柄。turn 驱动与空闲期
/// 操作驱动共用；打包为单值以收敛 op 路由的参数面（原 `drive_idle_operation`
/// 8 参数触发 clippy::too_many_arguments，且与 in-turn 双份分发）。
pub(super) struct ControlPlane {
    pub(super) event_tx: mpsc::Sender<Event>,
    pub(super) interrupt_handle: Arc<AtomicBool>,
    pub(super) approval_handle: Arc<ApprovalGate>,
    pub(super) permission_mode_handle: Arc<std::sync::Mutex<PermissionMode>>,
}

/// [`route_extra`] 的路由结果：`Handled` 表示已处理、继续 select；
/// `Shutdown` 表示须等当前操作收尾后退出（drain 等待与 SessionEnd 由
/// 调用方负责——turn future / idle op 各自 pin 在调用方作用域）。
enum Routing {
    Handled,
    Shutdown,
}

/// in-turn 与 idle 两态共用的 submission 路由（原双份 match 收口单点）：
/// 可排队 op（UserInput / Compact / SlashCommand / MemoryList）入 pending
///（满时经 [`queue_or_reject`] 显式拒绝）；控制类 op 即时生效；Shutdown 置
/// 中断标志后交调用方收尾。Op 标注 non_exhaustive：未知 op warn 留痕。
async fn route_extra(
    extra: Submission,
    pending: &mut VecDeque<Submission>,
    cp: &ControlPlane,
) -> Routing {
    match extra.op {
        Op::UserInput { .. } | Op::Compact | Op::SlashCommand { .. } | Op::MemoryList => {
            queue_or_reject(pending, extra, &cp.event_tx).await
        }
        Op::Interrupt => cp.interrupt_handle.store(true, Ordering::SeqCst),
        // P2：审批回填路由到 park 在 AwaitApproval 的操作（共享槽按
        // call_id 键控；无等待者时 permit 留存，decide 先于 wait 也不丢）。
        Op::ExecApproval { call_id, decision } => cp.approval_handle.decide(call_id, decision),
        // P2：权限模式切换，下一次 sandbox 判定即生效。
        Op::SetPermissionMode { mode } => {
            *lock_mode(&cp.permission_mode_handle) = mode;
        }
        Op::Shutdown => {
            cp.interrupt_handle.store(true, Ordering::SeqCst);
            return Routing::Shutdown;
        }
        // Op 标注 non_exhaustive：未来新增的 op 在 M1 忽略，warn 留痕。
        _ => tracing::warn!(id = %extra.id, "忽略未知 op（M1 未实现）"),
    }
    Routing::Handled
}

/// Session actor 主循环：串行驱动 turn；turn 期间经 select! 继续监听
/// submission 通道（控制类 op 经 [`route_extra`] 即时生效，可排队 op 本地
/// 排队，队列有界——溢出显式拒绝）。
/// submission 通道关闭（客户端全部析构）即退出；event 通道随 [`ControlPlane`]
/// 析构而关闭，客户端 next_event 收 None。
pub(super) async fn actor_loop(
    mut session: Session,
    mut submission_rx: mpsc::Receiver<Submission>,
    cp: ControlPlane,
) {
    // P7：SessionStart hook（SPEC §9）——原由各前端在 spawn 前自行触发，
    // 现收口 actor（与记忆提取同点）：前端只经协议面交互，能力等价。
    // 警告经 Warning 事件进事件流（合成 id，无对应 Submission）。
    run_session_lifecycle(
        &session,
        wavecode_core::hooks::HookEventPoint::SessionStart,
        &cp.event_tx,
    )
    .await;

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
            // submission 通道关闭（客户端全部析构）：尽力补 SessionEnd 与
            // 记忆提取（InProcessClient 析构的 abort 兜底可能抢先生效，
            // 此时跑不到——尽力而为语义，与提取一致）。
            run_session_lifecycle(
                &session,
                wavecode_core::hooks::HookEventPoint::SessionEnd,
                &cp.event_tx,
            )
            .await;
            session.spawn_memory_extraction();
            return;
        };
        match sub.op {
            Op::UserInput { text } => {
                // 提取不再在驱动 turn 前预取句柄：预取会长期克隆历史
                // `Arc`，使本 turn 首次 push_message 的 `Arc::make_mut`
                // 退化为整历史深克隆（违背 O(1) 快照不变量）。改为
                // Shutdown 时 turn future 出借用后再提取（拿终态历史）。
                let shutdown = {
                    let session = &mut session;
                    // 事件由 run_turn 直接写入 event 通道（id 已在 run_turn 内回填）。
                    let turn = session.run_turn(&sub.id, &text, cp.event_tx.clone());
                    tokio::pin!(turn);
                    loop {
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
                                // 控制/排队 op 路由与空闲态共用单点（route_extra）；
                                // Shutdown / 通道关闭在此等 turn 收尾（至多 2s）后退出。
                                let routing = match maybe_sub {
                                    Some(extra) => route_extra(extra, &mut pending, &cp).await,
                                    None => {
                                        // 客户端全部析构，等价隐式 Shutdown。
                                        cp.interrupt_handle.store(true, Ordering::SeqCst);
                                        Routing::Shutdown
                                    }
                                };
                                if matches!(routing, Routing::Shutdown) {
                                    let _ = tokio::time::timeout(
                                        SHUTDOWN_DRAIN_TIMEOUT,
                                        &mut turn,
                                    )
                                    .await;
                                    break true;
                                }
                            }
                        }
                    }
                };
                if shutdown {
                    // P7：SessionEnd hook + P6：SessionEnd 触发记忆自动提取
                    //（后台 detached，不阻塞退出）。此刻 turn future 已出
                    // 借用——提取经 spawn_memory_extraction 现取句柄，快照
                    // 即会话终态历史。
                    run_session_lifecycle(
                        &session,
                        wavecode_core::hooks::HookEventPoint::SessionEnd,
                        &cp.event_tx,
                    )
                    .await;
                    session.spawn_memory_extraction();
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
                *lock_mode(&cp.permission_mode_handle) = mode;
            }
            // 无活动 turn 的 Shutdown：直接退出。
            Op::Shutdown => {
                run_session_lifecycle(
                    &session,
                    wavecode_core::hooks::HookEventPoint::SessionEnd,
                    &cp.event_tx,
                )
                .await;
                // P6：SessionEnd 触发记忆自动提取（后台 detached，不阻塞退出）。
                session.spawn_memory_extraction();
                return;
            }
            // `/memory` 读取面（Op::MemoryList）：索引内容回包（非 turn
            // 操作，无 TurnStarted/Completed 包裹）；读取失败发 recoverable
            // Error；无记忆装配回 path=None（前端展示"能力不可用"）。
            Op::MemoryList => {
                let msg = match session.memory_index() {
                    Some((path, Ok(content))) => EventMsg::MemoryIndex {
                        path: Some(path.display().to_string()),
                        content,
                    },
                    Some((_, Err(e))) => EventMsg::Error {
                        message: format!("读取记忆索引失败：{e}"),
                        recoverable: true,
                    },
                    None => EventMsg::MemoryIndex {
                        path: None,
                        content: String::new(),
                    },
                };
                // send 失败即前端断开：与 core emit 同策略，继续循环。
                let _ = cp
                    .event_tx
                    .send(Event {
                        id: sub.id.clone(),
                        msg,
                    })
                    .await;
            }
            // P3：无活动 turn 的 /compact——立即压缩（事件以该 submission
            // 的 id 回填）；压缩失败已由 Session::compact 发 Error 事件，
            // 此处记日志即可，actor 继续存活。
            // 驱动期间经 select! 继续监听 submission（与 in-turn 同纪律）：
            // 直接 `.await` 会让 Interrupt / Shutdown / 审批回填堵在通道里
            // 直到操作自然结束（compact 是一次 LLM 调用、slash 直调是一轮
            // 完整 turn），中断盲区即此处。
            Op::Compact => {
                let op = session.compact(&sub.id, cp.event_tx.clone());
                if drive_idle_operation(op, &sub.id, &mut submission_rx, &mut pending, &cp).await {
                    run_session_lifecycle(
                        &session,
                        wavecode_core::hooks::HookEventPoint::SessionEnd,
                        &cp.event_tx,
                    )
                    .await;
                    session.spawn_memory_extraction();
                    return;
                }
            }
            // P7：无活动 turn 的 slash 直调 skill（SPEC §8.2）：inline 驱动
            // 一轮 turn，fork 派生后台子代理；错误已由 Session::invoke_skill
            // 发 Error + TurnCompleted，Err 返回即引擎级失败，记日志存活。
            Op::SlashCommand { name, args } => {
                let op = session.invoke_skill(&sub.id, &name, &args, cp.event_tx.clone());
                if drive_idle_operation(op, &sub.id, &mut submission_rx, &mut pending, &cp).await {
                    run_session_lifecycle(
                        &session,
                        wavecode_core::hooks::HookEventPoint::SessionEnd,
                        &cp.event_tx,
                    )
                    .await;
                    session.spawn_memory_extraction();
                    return;
                }
            }
            // Op 标注 non_exhaustive：未来新增的 op 在 M1 忽略，warn 留痕。
            _ => tracing::warn!(id = %sub.id, "忽略未知 op（M1 未实现）"),
        }
    }
}

/// 空闲期长操作（idle Compact / SlashCommand）的驱动：与 in-turn 同纪律，
/// `select!` 持续监听 submission——控制类 op 即时生效（Interrupt 置中断
/// 标志 / ExecApproval 回填 / SetPermissionMode 切换 / Shutdown 置标志并
/// 等操作收尾），可排队 op（UserInput / Compact / SlashCommand）入 pending。
///
/// 操作自身的可中断性：slash 直调是完整 turn（内部有中断安全点）；
/// compact 无内部安全点，Interrupt 在其中只保证即时置位、下一次 turn
/// 生效，操作自然完成后 actor 继续。返回 `true` 表示应退出（Shutdown /
/// 客户端全部析构），此时操作已收尾，调用方负责 SessionEnd 提取。
async fn drive_idle_operation<F, T>(
    op: F,
    id: &str,
    submission_rx: &mut mpsc::Receiver<Submission>,
    pending: &mut VecDeque<Submission>,
    cp: &ControlPlane,
) -> bool
where
    F: Future<Output = anyhow::Result<T>>,
{
    tokio::pin!(op);
    loop {
        tokio::select! {
            result = &mut op => {
                if let Err(e) = result {
                    tracing::error!(id = %id, error = %e, "空闲期操作失败，actor 继续存活");
                }
                return false;
            }
            maybe_sub = submission_rx.recv() => {
                // 控制/排队 op 路由与 in-turn 共用单点（route_extra）；
                // Shutdown / 通道关闭在此等操作收尾（至多 2s）后退出。
                let routing = match maybe_sub {
                    Some(extra) => route_extra(extra, pending, cp).await,
                    None => {
                        // 客户端全部析构，等价隐式 Shutdown。
                        cp.interrupt_handle.store(true, Ordering::SeqCst);
                        Routing::Shutdown
                    }
                };
                if matches!(routing, Routing::Shutdown) {
                    let _ =
                        tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut op).await;
                    return true;
                }
            }
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
