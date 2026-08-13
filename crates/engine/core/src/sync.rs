//! 锁中毒的统一恢复策略（core 内单点决策）。
//!
//! ApprovalGate 的审批槽与 SubagentManager 的任务表 / 状态 / 通知 /
//! 事件汇 / 中断槽等 `std::sync::Mutex` 保护的都是单操作临界区
//!（一次 insert / take / 赋值 / drain），持锁期间 panic 不会留下半截
//! 不变量——中毒时取回守卫继续执行（panic 已沿原线程传播），不做
//! 二次 panic 级联。

/// 取锁守卫；锁中毒时恢复（`into_inner`）而非 panic 级联。
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
