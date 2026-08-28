//! 记忆自动提取（阶段 1b 拆分自 session/mod.rs，SPEC §7.2）：SessionEnd 挂接
//! 的可 awaiting 入口 + detached 后台任务 + 预捕获句柄。

use std::sync::Arc;
use wavecode_llm::Message;

/// 记忆自动提取句柄（P6）：包住一次提取所需的模型通道、历史快照与存储
/// 根，[`MemoryExtractionHandle::spawn`] 派生 detached 提取任务。
///
/// 获取时点：SessionEnd 挂接点（app-server actor 的 Shutdown / 客户端断开
/// 路径）经 [`Session::spawn_memory_extraction`] 现取——不在 turn 驱动前
/// 预取：预取会长期克隆历史 `Arc`，使 turn 内首次 `push_message` 的
/// `Arc::make_mut` 退化为整历史深克隆（O(1) 快照不变量被破坏）。现取
/// 语义下快照即会话终态历史（含被中断 turn 的部分结果）。
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
impl super::Session {
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
}
