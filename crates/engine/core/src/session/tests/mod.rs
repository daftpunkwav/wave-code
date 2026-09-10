//! session 集成测试：共享 mock 基建（脚本化模型 / 注册表 / bypass 沙箱）
//! 与按里程碑场景拆分的子模块（自 3282 行单文件测试墙拆分，SPEC §18
//! "内联测试墙约定"）。测试体未做任何改写，仅按场景归组搬迁。

use super::turn::CONTINUATION_PROMPT;
use super::*;
use futures::StreamExt;
use futures::stream;
use std::sync::{Arc, Mutex};
use wavecode_context::ContextConfig;
use wavecode_llm::{ChatModel, ChatRequest, ContentBlock, LlmError, StreamEvent, Usage};
use wavecode_protocol::{Event, EventMsg, StopReason};
use wavecode_sandbox::Sandbox;
use wavecode_tools::{ToolCtx, ToolOutput};

/// 完整内置注册表 + 同源 TodoStore（Registry 不再持有会话状态）。
fn builtin_registry() -> (wavecode_tools::Registry, wavecode_tools::TodoStore) {
    wavecode_tools::Registry::builtin_with_todos()
}

/// 脚本化 mock：按调用次数返回预排事件序列
struct MockModel {
    calls: Mutex<u32>,
    scripts: Vec<Vec<StreamEvent>>,
    /// 记录每次请求，供断言 tool_result 回灌
    seen: Mutex<Vec<ChatRequest>>,
}

impl MockModel {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            calls: Mutex::new(0),
            scripts,
            seen: Mutex::new(vec![]),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for MockModel {
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> wavecode_llm::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>>,
    > {
        self.seen.lock().unwrap().push(req);
        let mut n = self.calls.lock().unwrap();
        let idx = (*n as usize).min(self.scripts.len().saturating_sub(1));
        *n += 1;
        let events = self.scripts[idx].clone();
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

/// 中断测试专用 mock：回放脚本后挂起，直到 `gate` 置位才再产出
/// 一个 sentinel 事件并结束流——run_turn 流循环的 next() 收到它时
/// 循环内中断检查点真正触发（覆盖 finish_interrupted）；
/// sentinel 本身不会被分发处理（检查点在事件分发之前 return）。
struct GatedModel {
    script: Vec<StreamEvent>,
    gate: Arc<AtomicBool>,
    seen: Mutex<Vec<ChatRequest>>,
}

#[async_trait::async_trait]
impl ChatModel for GatedModel {
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> wavecode_llm::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>>,
    > {
        self.seen.lock().unwrap().push(req);
        let script = self.script.clone();
        let gate = self.gate.clone();
        let tail = stream::once(async move {
            while !gate.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Ok(StreamEvent::TextDelta {
                text: "tail-sentinel".into(),
            })
        });
        Ok(Box::pin(
            stream::iter(script.into_iter().map(Ok)).chain(tail),
        ))
    }
}

fn text_then_end(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta { text: text.into() },
        StreamEvent::MessageComplete {
            stop_reason: "end_turn".into(),
            usage: Usage {
                input_tokens: 20,
                output_tokens: 3,
            },
        },
    ]
}

/// 既有编排测试不涉审批：bypassPermissions 全放行，保持 P1 语义；
/// 审批行为由 P2 专项测试（default / plan 模式）锁定。
fn bypass_sandbox() -> Sandbox {
    Sandbox::without_rules(PermissionMode::BypassPermissions)
}

/// P2 测试夹具：单轮 write_file 调用脚本（default 模式下触发审批门）。
fn write_file_script(path: &str, content: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseBegin {
            id: "t1".into(),
            name: "write_file".into(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: format!(r#"{{"path":"{path}","content":"{content}"}}"#),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "tool_use".into(),
            usage: Usage::default(),
        },
    ]
}

/// P3 测试夹具：识别摘要请求（ModelSummary 不带工具，tools 为空）回放
/// 摘要脚本；采样请求按 `sampling` 队列逐次回放——`None` 表示该次
/// 返回 prompt_too_long 类错误（reactive compact 触发条件）。
struct CompactAwareMock {
    sampling: Mutex<Vec<Option<Vec<StreamEvent>>>>,
    summary_script: Vec<StreamEvent>,
    seen: Mutex<Vec<ChatRequest>>,
}

#[async_trait::async_trait]
impl ChatModel for CompactAwareMock {
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> wavecode_llm::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>>,
    > {
        self.seen.lock().unwrap().push(req.clone());
        if req.tools.is_empty() {
            // 摘要请求：回放五要素摘要脚本
            return Ok(Box::pin(stream::iter(
                self.summary_script.clone().into_iter().map(Ok),
            )));
        }
        let mut q = self.sampling.lock().unwrap();
        let next = if q.len() > 1 {
            q.remove(0)
        } else {
            q[0].clone()
        };
        match next {
            Some(events) => Ok(Box::pin(stream::iter(events.into_iter().map(Ok)))),
            None => Err(LlmError::PromptTooLong {
                message: "prompt is too long: 210000 tokens > 200000 maximum".into(),
            }),
        }
    }
}

fn p3_mock(sampling: Vec<Option<Vec<StreamEvent>>>) -> Arc<CompactAwareMock> {
    Arc::new(CompactAwareMock {
        sampling: Mutex::new(sampling),
        summary_script: summary_script(),
        seen: Mutex::new(vec![]),
    })
}

/// 五要素摘要脚本（正文逐项含目标/进展/关键决策/文件清单/待办）。
fn summary_script() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            text: "## 目标\nT\n## 进展\nP\n## 关键决策\nD\n## 文件清单\nF\n## 待办\nN".into(),
        },
        StreamEvent::MessageComplete {
            stop_reason: "end_turn".into(),
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
            },
        },
    ]
}

fn collect_events(rx: &mut mpsc::Receiver<Event>) -> Vec<EventMsg> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev.msg);
    }
    out
}

/// P10 测试夹具：注入临时根目录的 rollout 配置。
fn p10_rollout(dir: &std::path::Path, thread_id: &str) -> Option<crate::rollout::RolloutConfig> {
    Some(crate::rollout::RolloutConfig {
        root: dir.join("threads"),
        thread_id: thread_id.to_owned(),
    })
}

/// rollout 文件全部记录的序号清单（断言连续递增用）。
fn p10_seqs(load: &crate::rollout::RolloutLoad) -> Vec<u64> {
    load.records.iter().map(|r| r.seq()).collect()
}

// 测试按里程碑场景分子模块（自 3282 行单文件测试墙拆分，SPEC §18
// "内联测试墙约定"；测试体未改写，仅按场景归组搬迁）。
mod approval;
mod basic;
mod compact;
mod mcp;
mod memory;
mod planning;
mod rollout;
mod rules;
mod skills_hooks;
mod stress;
