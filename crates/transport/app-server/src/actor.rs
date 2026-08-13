//! actor 循环（阶段 5 拆分自 lib.rs）：事件分发 / 审批路由 / 中断 / 退出。

use super::*;

/// Session actor 主循环：串行驱动 turn；turn 期间经 select! 继续监听
/// submission 通道（响应 Interrupt / Shutdown / ExecApproval /
/// SetPermissionMode，UserInput / Compact 本地排队）。
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
