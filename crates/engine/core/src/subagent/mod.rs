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
mod tests {
    use super::*;
    use futures::stream;
    use wavecode_llm::{ChatRequest, StreamEvent, Usage};
    use wavecode_protocol::PermissionMode;

    /// 脚本化 mock（与 session.rs 测试的 MockModel 同构）：按调用次数回放。
    struct MockModel {
        calls: Mutex<u32>,
        scripts: Vec<Vec<StreamEvent>>,
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
            Ok(Box::pin(stream::iter(
                self.scripts[idx].clone().into_iter().map(Ok),
            )))
        }
    }

    fn text_end(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta { text: text.into() },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                },
            },
        ]
    }

    fn parent_config(model: Arc<dyn ChatModel>) -> SessionConfig {
        let cwd = tempfile::tempdir().unwrap().keep();
        SessionConfig::builder("mock", model, Registry::builtin(), cwd)
            .sandbox(wavecode_sandbox::Sandbox::without_rules(
                PermissionMode::BypassPermissions,
            ))
            .build()
    }

    fn spec(prompt: &str, subagent_type: SubagentType) -> TaskSpec {
        TaskSpec {
            description: "测试任务".into(),
            prompt: prompt.into(),
            subagent_type,
            preamble: None,
            allowed_tools: None,
        }
    }

    /// 深度上限 1（构造保证）：两类子代理的 registry 均无 task 工具；
    /// explore 只保留只读工具。
    #[test]
    fn child_registry_has_no_task_tools() {
        let model = Arc::new(MockModel {
            calls: Mutex::new(0),
            scripts: vec![],
        });
        let mgr = SubagentManager::from_config(&parent_config(model));
        for t in [SubagentType::GeneralPurpose, SubagentType::Explore] {
            let cfg = mgr.child_config(&spec("x", t));
            for name in ["task", "task_output", "task_stop"] {
                assert!(
                    cfg.registry.get(name).is_none(),
                    "{t:?} 子代理不得有 {name} 工具（深度上限 1）"
                );
            }
        }
        let explore = mgr.child_config(&spec("x", SubagentType::Explore));
        assert!(explore.registry.get("grep").is_some());
        assert!(explore.registry.get("read_file").is_some());
        for name in ["write_file", "edit_file", "shell", "todo_write"] {
            assert!(
                explore.registry.get(name).is_none(),
                "explore 子代理不得有 {name}"
            );
        }
        // general-purpose 全工具（对照）。
        let general = mgr.child_config(&spec("x", SubagentType::GeneralPurpose));
        assert!(general.registry.get("write_file").is_some());
        assert!(general.registry.get("shell").is_some());
    }

    /// 后台派生 → 终态可查询、通知可取走（且一次性消费）。
    #[tokio::test]
    async fn background_task_completes_and_notifies() {
        let model = Arc::new(MockModel {
            calls: Mutex::new(0),
            scripts: vec![text_end("调查结论：一切正常")],
        });
        let mgr = SubagentManager::from_config(&parent_config(model));
        let id = mgr.spawn_background(spec("调查一下", SubagentType::Explore));
        assert_eq!(id, "task-1");
        // 等待终态（mock 即时完成，轮询兜底）。
        let state = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(TaskState::Finished(r)) = mgr.query(&id) {
                    break r;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("子代理应在 5s 内完成");
        assert_eq!(state.status, SubagentStatus::Completed);
        assert!(state.summary.contains("调查结论"));
        assert!(state.tokens_used.is_some());

        let notes = mgr.drain_notifications();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("<task-notification>"));
        assert!(notes[0].contains("task-1"));
        assert!(notes[0].contains("completed"));
        assert!(notes[0].contains("调查结论"));
        // 一次性消费。
        assert!(mgr.drain_notifications().is_empty());
    }

    /// 同步派生：结果直接返回，不进任务表、无通知。
    #[tokio::test]
    async fn sync_task_returns_result_directly() {
        let model = Arc::new(MockModel {
            calls: Mutex::new(0),
            scripts: vec![text_end("同步结果")],
        });
        let mgr = SubagentManager::from_config(&parent_config(model));
        let result = mgr
            .run_sync(spec("干活", SubagentType::GeneralPurpose))
            .await;
        assert_eq!(result.status, SubagentStatus::Completed);
        assert!(result.summary.contains("同步结果"));
        assert!(mgr.query("task-1").is_none(), "同步任务不进任务表");
        assert!(mgr.drain_notifications().is_empty(), "同步任务不发通知");
    }

    /// 三个工具的参数校验与未知 id 的错误形态（is_error 回灌，不 panic）。
    #[tokio::test]
    async fn tools_validate_input_and_unknown_ids() {
        let model = Arc::new(MockModel {
            calls: Mutex::new(0),
            scripts: vec![],
        });
        let mgr = SubagentManager::from_config(&parent_config(model));
        let ctx = ToolCtx {
            cwd: std::path::PathBuf::from("."),
            deny_env: Vec::new(),
        };
        let spawn = TaskSpawn::new(mgr.clone());
        // 缺 description / prompt。
        assert!(spawn.execute(json!({}), &ctx).await.unwrap().is_error);
        // 非法 subagent_type。
        let out = spawn
            .execute(
                json!({"description": "d", "prompt": "p", "subagent_type": "nope"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("invalid subagent_type"));

        let output = TaskOutputTool::new(mgr.clone());
        let out = output
            .execute(json!({"task_id": "task-99"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("unknown task id"));

        let stop = TaskStop::new(mgr.clone());
        assert!(
            stop.execute(json!({"task_id": "task-99"}), &ctx)
                .await
                .unwrap()
                .is_error
        );
    }

    // ------------------------------------------------------------------
    // 集成测试：mock model 驱动父会话 turn 全流程（P5 验收）
    // ------------------------------------------------------------------

    /// 请求全文扁平化（text + tool_use 标识 + tool_result 内容），供断言。
    fn request_text(req: &ChatRequest) -> String {
        req.messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        wavecode_llm::ContentBlock::Text { text } => text.clone(),
                        wavecode_llm::ContentBlock::ToolUse { id, name, .. } => {
                            format!("tool_use:{name}:{id}")
                        }
                        wavecode_llm::ContentBlock::ToolResult { content, .. } => {
                            format!("tool_result:{content}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 首条消息（turn 输入）的文本：父子请求路由判据。
    fn first_text(req: &ChatRequest) -> String {
        req.messages
            .first()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        wavecode_llm::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 父会话 turn 输入（parallel / stop 两个集成测试的路由锚点）。
    const PARENT_INPUT: &str = "开始任务";

    /// 并行测试 mock：父子共用一个 ChatModel（与生产形态一致，子代理继承
    /// 父会话模型 Arc），按首条消息文本路由——
    /// - 含 `child-task-`：子代理请求（round1 中间过程：文本 + list_dir
    ///   工具调用；round2 终态文本）；两个子代理都进入采样后才放行脚本
    ///   （并行证明：串行执行会在 5s 超时后才放行且 overlap 标志不置位）；
    /// - 否则：父会话请求，按调用序回放脚本；第 2 次采样（task_output 轮）
    ///   等待外部 gate（测试在观察到两个 SubagentCompleted 后置位，保证
    ///   task_output 查询时子代理已终态——无竞态）。
    struct ParallelModel {
        parent_scripts: Vec<Vec<StreamEvent>>,
        parent_calls: Mutex<u32>,
        gate: Arc<AtomicBool>,
        child_started: AtomicUsize,
        child_overlap: AtomicBool,
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for ParallelModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            self.seen.lock().unwrap().push(req.clone());
            let ft = first_text(&req);
            if ft.contains("child-task-") {
                let n = self.child_started.fetch_add(1, Ordering::SeqCst) + 1;
                if n >= 2 {
                    self.child_overlap.store(true, Ordering::SeqCst);
                }
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while self.child_started.load(Ordering::SeqCst) < 2
                    && tokio::time::Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let suffix = if ft.contains("child-task-A") {
                    "A"
                } else {
                    "B"
                };
                let round2 = req.messages.iter().any(|m| {
                    m.content
                        .iter()
                        .any(|b| matches!(b, wavecode_llm::ContentBlock::ToolResult { .. }))
                });
                let script = if round2 {
                    text_end(&format!("result-{suffix}"))
                } else {
                    vec![
                        StreamEvent::TextDelta {
                            text: format!("child-{suffix}-intermediate"),
                        },
                        StreamEvent::ToolUseBegin {
                            id: format!("ct-{suffix}"),
                            name: "list_dir".into(),
                        },
                        StreamEvent::ToolUseInputDelta {
                            partial_json: r#"{"path":"."}"#.into(),
                        },
                        StreamEvent::BlockEnd,
                        StreamEvent::MessageComplete {
                            stop_reason: "tool_use".into(),
                            usage: Usage::default(),
                        },
                    ]
                };
                return Ok(Box::pin(stream::iter(script.into_iter().map(Ok))));
            }
            let call = {
                let mut n = self.parent_calls.lock().unwrap();
                let c = *n as usize;
                *n += 1;
                c
            };
            if call == 1 {
                while !self.gate.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            let idx = call.min(self.parent_scripts.len().saturating_sub(1));
            Ok(Box::pin(stream::iter(
                self.parent_scripts[idx].clone().into_iter().map(Ok),
            )))
        }
    }

    fn tool_use(id: &str, name: &str, input_json: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseBegin {
                id: id.into(),
                name: name.into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: input_json.into(),
            },
            StreamEvent::BlockEnd,
        ]
    }

    /// P5 验收主测试：父会话派生 2 个后台子代理并行执行，两者结果经
    /// `<task-notification>` 注入与 task_output 查询正确回注父会话；父会话
    /// 历史不含子代理中间消息（上下文隔离）。
    #[tokio::test]
    async fn parallel_background_subagents_isolated_and_reinjected() {
        let gate = Arc::new(AtomicBool::new(false));
        let parent_scripts = vec![
            // call0：派生两个后台子代理（A=explore，B=general-purpose）。
            {
                let mut v = tool_use(
                    "p1",
                    "task",
                    r#"{"description":"调查认证模块","prompt":"child-task-A","subagent_type":"explore","run_in_background":true}"#,
                );
                v.extend(tool_use(
                    "p2",
                    "task",
                    r#"{"description":"调查日志模块","prompt":"child-task-B","run_in_background":true}"#,
                ));
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            // call1（gate 后）：查询两个任务的结果。
            {
                let mut v = tool_use("p3", "task_output", r#"{"task_id":"task-1"}"#);
                v.extend(tool_use("p4", "task_output", r#"{"task_id":"task-2"}"#));
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            text_end("父会话收尾"),
        ];
        let model = Arc::new(ParallelModel {
            parent_scripts,
            parent_calls: Mutex::new(0),
            gate: gate.clone(),
            child_started: AtomicUsize::new(0),
            child_overlap: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
        });
        let mut session = Session::with_subagents(parent_config(model.clone()));
        let (tx, mut rx) = mpsc::channel::<Event>(512);
        let turn = tokio::spawn(async move { session.run_turn("s-1", PARENT_INPUT, tx).await });

        // 事件观察：两个 SubagentCompleted 到齐后放行父会话 task_output 轮。
        let mut started = 0usize;
        let mut completed_statuses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let ev = tokio::time::timeout(deadline - tokio::time::Instant::now(), rx.recv())
                .await
                .expect("超时：事件流停滞")
                .expect("事件流意外结束");
            match ev.msg {
                EventMsg::SubagentStarted { .. } => started += 1,
                EventMsg::SubagentCompleted { status, .. } => {
                    completed_statuses.push(status);
                    if completed_statuses.len() == 2 {
                        gate.store(true, Ordering::SeqCst);
                    }
                }
                EventMsg::TurnCompleted { .. } => break,
                _ => {}
            }
        }
        let reason = turn.await.expect("turn 任务 panic").expect("turn 应成功");
        assert_eq!(reason, StopReason::Completed);
        assert_eq!(started, 2, "应见 2 个 SubagentStarted");
        assert_eq!(
            completed_statuses,
            vec![SubagentStatus::Completed, SubagentStatus::Completed],
            "两个子代理应并行完成"
        );
        assert!(
            model.child_overlap.load(Ordering::SeqCst),
            "两个子代理应真正并行（同时处于采样中）"
        );

        let seen = model.seen.lock().unwrap();
        // 两个子代理各自独立跑了 turn（隔离的 Session）。
        assert!(seen.iter().any(|r| first_text(r).contains("child-task-A")));
        assert!(seen.iter().any(|r| first_text(r).contains("child-task-B")));
        let parents: Vec<&ChatRequest> = seen
            .iter()
            .filter(|r| first_text(r) == PARENT_INPUT)
            .collect();
        assert_eq!(parents.len(), 3, "父会话应有 3 轮采样");

        // 时序说明：call0 工具结果回灌后循环头的 drain 在子代理完成前执行
        //（call1 采样被 gate 挡住），两条通知在 call2 前的循环头注入；
        // task_output 结果（call1 的 tool_result）同样落在 call2 的请求里。
        // p1 仅含派生回执（其文案提到 <task-notification>，不是真通知）。
        let p2 = request_text(parents[2]);
        assert_eq!(
            p2.matches("<task-notification>\nBackground task").count(),
            2,
            "两条后台终态通知应在循环头注入（派生回执的文案提及不含此主体）: {p2}"
        );
        assert!(p2.contains("result-A") && p2.contains("result-B"));
        assert_eq!(
            p2.matches("status: completed").count(),
            4,
            "通知与 task_output 各报告一次终态: {p2}"
        );

        // 上下文隔离：父会话任何一轮请求都不含子代理中间消息
        //（中间文本 / 子代理 tool_use id / 子代理工具结果）。
        for r in &parents {
            let t = request_text(r);
            for needle in [
                "child-A-intermediate",
                "child-B-intermediate",
                "ct-A",
                "ct-B",
            ] {
                assert!(
                    !t.contains(needle),
                    "父会话历史泄漏了子代理中间过程 {needle}"
                );
            }
        }
    }

    /// stop 测试 mock：含 `infinite-task` 的子代理请求返回无限流
    ///（每 1ms 一个 delta，只能被中断收尾）；父会话按调用序回放脚本。
    struct StopModel {
        parent_scripts: Vec<Vec<StreamEvent>>,
        parent_calls: Mutex<u32>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for StopModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            self.seen.lock().unwrap().push(req.clone());
            if first_text(&req).contains("infinite-task") {
                let endless = stream::unfold((), |()| async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    Some((
                        Ok(StreamEvent::TextDelta {
                            text: "tick".into(),
                        }),
                        (),
                    ))
                });
                return Ok(Box::pin(endless));
            }
            let call = {
                let mut n = self.parent_calls.lock().unwrap();
                let c = *n as usize;
                *n += 1;
                c
            };
            let idx = call.min(self.parent_scripts.len().saturating_sub(1));
            Ok(Box::pin(stream::iter(
                self.parent_scripts[idx].clone().into_iter().map(Ok),
            )))
        }
    }

    /// P5 验收：后台子代理可被 task_stop 停止，父会话收到停止状态
    ///（SubagentCompleted{Stopped} 事件 + task_output / 通知回注）。
    /// 无 gate：task_stop 自身等待子代理终态（轮询重武装中断标志），
    /// 无论 stop 落在子代理启动前还是运行中，结果都是确定的 Stopped。
    #[tokio::test]
    async fn background_subagent_can_be_stopped() {
        let parent_scripts = vec![
            // call0：派生后台子代理（无限任务）。
            {
                let mut v = tool_use(
                    "p1",
                    "task",
                    r#"{"description":"无限任务","prompt":"infinite-task","run_in_background":true}"#,
                );
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            // call1：停止它。
            {
                let mut v = tool_use("p2", "task_stop", r#"{"task_id":"task-1"}"#);
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            // call2：查询终态。
            {
                let mut v = tool_use("p3", "task_output", r#"{"task_id":"task-1"}"#);
                v.push(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            },
            text_end("收尾"),
        ];
        let model = Arc::new(StopModel {
            parent_scripts,
            parent_calls: Mutex::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let mut session = Session::with_subagents(parent_config(model.clone()));
        let (tx, mut rx) = mpsc::channel::<Event>(512);
        let turn = tokio::spawn(async move { session.run_turn("s-1", PARENT_INPUT, tx).await });

        let mut stopped_event = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let ev = tokio::time::timeout(deadline - tokio::time::Instant::now(), rx.recv())
                .await
                .expect("超时：事件流停滞")
                .expect("事件流意外结束");
            match ev.msg {
                EventMsg::SubagentCompleted { status, .. } => {
                    assert_eq!(status, SubagentStatus::Stopped, "子代理应以 Stopped 收尾");
                    stopped_event = true;
                }
                EventMsg::TurnCompleted { .. } => break,
                _ => {}
            }
        }
        let reason = turn.await.expect("turn 任务 panic").expect("turn 应成功");
        assert_eq!(reason, StopReason::Completed);
        assert!(stopped_event, "应见 SubagentCompleted{{Stopped}}");

        let seen = model.seen.lock().unwrap();
        let parents: Vec<&ChatRequest> = seen
            .iter()
            .filter(|r| first_text(r) == PARENT_INPUT)
            .collect();
        assert_eq!(parents.len(), 4, "父会话应有 4 轮采样");
        // call2 的请求：task_stop 结果 + 停止通知（循环头注入）。
        let p2 = request_text(parents[2]);
        assert!(p2.contains("task-1 stopped"), "task_stop 结果回灌: {p2}");
        assert!(p2.contains("status: stopped"));
        assert!(
            p2.contains("<task-notification>") && p2.matches("stopped").count() >= 2,
            "停止状态应同时经通知注入: {p2}"
        );
        // call3 的请求：task_output 查询到停止终态。
        let p3 = request_text(parents[3]);
        assert!(p3.contains("status: stopped"), "task_output 终态回灌: {p3}");
    }
}
