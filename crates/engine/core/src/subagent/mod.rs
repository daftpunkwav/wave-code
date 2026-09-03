//! 子代理（subagents，P5，deepagents 核心能力之一，SPEC §5.3 / §11.2）。
//!
//! 形态：
//! - 子代理 = 独立 [`Session`]（隔离消息历史）跑自己的 turn 循环，输入为
//!   任务描述 + 任务指令（可选内置类型的系统前言）；运行在独立 tokio
//!   task（后台形态）或调用方工具执行内（同步形态）；
//! - 内置类型：[`SubagentType::GeneralPurpose`]（全工具）与
//!   [`SubagentType::Explore`]（只读工具——按 registry 过滤 `is_read_only`）；
//! - 完成 / 失败 / 停止时产出结构化结果 [`TaskResult`]（最终文本摘要 +
//!   状态 + token 用量）；后台形态的终态以 `<task-notification>` user 消息
//!   注入父会话下一 turn（注入点在 turn 循环头，见 session.rs）。
//!
//! 依赖矩阵取舍（tools 不能依赖 core）：`task` / `task_output` / `task_stop`
//! 三个工具需要驱动 core 的 Session，故工具实现放 core 侧（core 本就可实现
//! tools 的 [`Tool`] trait），经 [`Session::with_subagents`] 装配进父会话
//! registry——与 `todo_write` 的"共享状态句柄注入"先例同构，无新依赖边。
//!
//! 深度上限 1（防失控）：子代理的 Session 经 [`Session::new`] 构造，其
//! registry 由本模块单独装配（builtin 全集或只读子集），**不含** task 工具
//! ——子代理在工具面层面就无法再派生，上限由构造保证而非运行时检查。
//!
//! 事件可见性（择一注释）：新增 `SubagentStarted` / `SubagentCompleted`
//! 协议变体而非复用 Warning——起止语义清晰、前端（TUI P8）可专门渲染；
//! 子代理的中间过程（delta / 工具调用）不进父会话事件流（上下文隔离的
//! 同构），前端只见起点与终点。wire tag 已在 protocol 锁定测试登记。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use wavecode_llm::ChatModel;
use wavecode_protocol::{Event, EventMsg, StopReason, SubagentStatus};
use wavecode_tools::{Registry, Tool, ToolCtx, ToolOutput};

use crate::session::{Session, SessionConfig};

/// 子代理事件通道容量（事件只被驱动任务排干取终态，无人消费中间事件）。
const CHILD_EVENT_CHANNEL_CAPACITY: usize = 256;

/// task_stop 等待子代理到达终态的超时：子代理中断在安全点（流消费循环
/// 每个元素 / 工具迭代间）生效，正常毫秒级；超时兜底防挂起的工具执行
/// 拖死父会话 turn。
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// task_stop 等待终态的轮询间隔（轮询同时重武装中断标志，覆盖
/// "run_turn 入口清标志"的竞态窗口，见 [`SubagentManager::stop`]）。
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// explore 类型的子代理前言（拼在子代理 turn 输入前部）。
///
/// 取舍（YAGNI）：完整自定义系统提示词注入点（替换 system prompt）留待
/// 后续；内置类型的差异 = 工具集过滤（构造保证）+ 此前言（行为引导）。
const EXPLORE_PREAMBLE: &str = "\
You are an explore subagent: investigate the codebase and answer with findings. \
You only have read-only tools; do not attempt to modify anything.";

mod format;
mod manager;
mod task_output;
mod task_spawn;
mod task_stop;
mod types;

pub(super) use format::{format_notification, format_result, non_empty_summary, required_str};
pub use manager::SubagentManager;
pub(crate) use task_output::TaskOutputTool;
pub(crate) use task_spawn::TaskSpawn;
pub(crate) use task_stop::TaskStop;
pub(crate) use types::TaskSpec;
pub use types::{SubagentType, TaskResult, TaskState};

#[cfg(test)]
mod tests;
