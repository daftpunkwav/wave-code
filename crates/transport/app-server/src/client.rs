//! 进程内 transport 客户端（阶段 5 拆分自 lib.rs）：spawn actor 循环驱动
//! Session，经 mpsc 双工通道提交 Submission 与接收 Event。

use super::actor::{ControlPlane, actor_loop};
use super::*;

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
            ControlPlane {
                event_tx,
                interrupt_handle: interrupt_handle.clone(),
                approval_handle,
                permission_mode_handle,
            },
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
