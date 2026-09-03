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
use wavecode_protocol::{Event, EventMsg, Op, PermissionMode, Submission};

/// submission 通道容量（前端 → actor）。
const SUBMISSION_CHANNEL_CAPACITY: usize = 32;
/// event 通道容量（actor → 前端）。
const EVENT_CHANNEL_CAPACITY: usize = 256;
/// turn 期间本地排队（pending）的 submission 上限：溢出以该 submission
/// 的 id 回填 Error 事件显式拒绝（请求不静默丢弃——拒绝即确定的响应，
/// SPEC §4.2"请求不允许丢"的进程内形态）；控制类 op 不入队不受限。
const PENDING_QUEUE_CAPACITY: usize = 64;
/// Shutdown / 客户端全部析构时，等待活动 turn 收尾的超时。
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// 进程内 transport 客户端句柄：Submission / Event 经两条 mpsc 通道
/// 与 core Session actor 直传，零 JSON 序列化往返。
///
/// 析构即结束会话：置中断标志并 abort actor 任务（见 `Drop` 实现）。
mod actor;
mod client;

pub use client::InProcessClient;

#[cfg(test)]
mod tests;
