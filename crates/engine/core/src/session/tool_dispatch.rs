//! 工具编排与审批（阶段 1b 拆分自 session/mod.rs）：execute_tool_calls 按
//! 只读并行/非只读串行过审批门（SPEC §11.1）；ApprovalGate 是审批反向通道
//! 共享槽（await_approval 需访问其私有 notify 字段，故同文件）。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;

use super::*;
use super::turn::RoundBlocks;
use tokio::sync::{Notify, mpsc};
use wavecode_llm::{ContentBlock, Message, Role};
use wavecode_protocol::{ApprovalDecision, Event, EventMsg, StopReason};
use wavecode_sandbox::Verdict;
use wavecode_tools::{ToolCtx, ToolOutput};

/// 审批反向通道的共享槽（§17.5 M3"复用 interrupt_handle 模式"）。
///
/// 驱动方（app-server actor）收到 `Op::ExecApproval` 时经
/// [`ApprovalGate::decide`] 存入决策并唤醒等待者；`run_turn` 在 AwaitApproval
/// 状态按 call_id 取走决策。以 call_id 为键：同一时刻只有一个待决调用
///（非只读串行段逐一审批），键控是为防迟到 / 错号决策被错误消费。
pub struct ApprovalGate {
    decisions: Mutex<HashMap<String, ApprovalDecision>>,
    notify: Notify,
}

impl ApprovalGate {
    /// 测试与 subagent 装配（共享槽注入前的独立槽）也需构造。
    pub(crate) fn new() -> Self {
        Self {
            decisions: Mutex::new(HashMap::new()),
            notify: Notify::new(),
        }
    }

    /// 存入一个审批决策并唤醒等待者（actor 路由 `Op::ExecApproval` 用）。
    /// `Notify::notify_one` 在无等待者时留存一个 permit，与取走决策后的
    /// 下一轮等待无丢失唤醒竞态。
    pub fn decide(&self, call_id: String, decision: ApprovalDecision) {
        crate::sync::lock(&self.decisions).insert(call_id, decision);
        self.notify.notify_one();
    }

    /// 按 call_id 取走决策（一次性消费）。
    fn take(&self, call_id: &str) -> Option<ApprovalDecision> {
        crate::sync::lock(&self.decisions).remove(call_id)
    }

    /// turn 开始时清空残留决策（与中断标志每 turn 自清同理：上一 turn
    /// 未被消费的迟到决策不得影响本轮）。
    pub(super) fn clear(&self) {
        crate::sync::lock(&self.decisions).clear();
    }
}

/// 审批等待的终局。
enum ApprovalWait {
    /// 收到决策（放行 / 拒绝）。
    Decision(ApprovalDecision),
    /// 等待期间被中断。
    Interrupted,
}

impl super::Session {
    /// AwaitApproval（SPEC §5.1）：park 等待驱动方经共享槽回填决策；
    /// 等待中中断标志同样生效（审批等待也是中断安全点）。
    async fn await_approval(&self, call_id: &str) -> ApprovalWait {
        loop {
            // 先取决策再查中断：决策已到达按决策走（与工具执行前的中断
            // 检查点同序——中断不撤销已就绪的结果）。
            if let Some(decision) = self.approval_gate.take(call_id) {
                return ApprovalWait::Decision(decision);
            }
            if self.interrupted.load(Ordering::SeqCst) {
                return ApprovalWait::Interrupted;
            }
            tokio::select! {
                // 正常路径：actor 存决策时 notify_one 即时唤醒（permit 留存，
                // 与 take 无丢失唤醒竞态）。
                _ = self.approval_gate.notify.notified() => {}
                // 兜底轮询：裸 interrupt_handle 驱动形态（无人戳 gate，如
                // 单测直接 run_turn）也能在一个间隔内观察到中断。
                _ = tokio::time::sleep(APPROVAL_POLL_INTERVAL) => {}
            }
        }
    }

    /// 流内中断收尾：部分结果保留入历史；悬空 tool_use 合成结果保持配对。
    pub(super) async fn finish_interrupted(
        &mut self,
        events: &mpsc::Sender<Event>,
        submission_id: &str,
        mut round: RoundBlocks,
    ) -> StopReason {
        let blocks = round.finish();
        if !blocks.is_empty() {
            self.push_message(Message {
                role: Role::Assistant,
                content: blocks,
            });
            self.push_pairing_results(round.preset_results, "interrupted by user");
        }
        emit(
            events,
            submission_id,
            EventMsg::TurnCompleted {
                stop_reason: StopReason::Interrupted,
            },
        )
        .await;
        tracing::debug!("turn 被中断，历史保留部分结果");
        StopReason::Interrupted
    }

    /// 为历史末尾 assistant 消息中的 tool_use 块补齐 ToolResult（user 消息），
    /// 保持 Anthropic tool_use/tool_result 配对约束：优先用预置结果
    ///（invalid json），其余合成 `fallback_content` 的 is_error 结果；
    /// 顺序与 tool_use 声明序一致。
    pub(super) fn push_pairing_results(
        &mut self,
        mut presets: Vec<ContentBlock>,
        fallback_content: &str,
    ) {
        let Some(last) = self.messages.last() else {
            return;
        };
        if last.role != Role::Assistant {
            return;
        }
        let mut results = Vec::new();
        for block in &last.content {
            if let ContentBlock::ToolUse { id, .. } = block {
                let preset = presets
                    .iter()
                    .position(
                        |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
                    )
                    .map(|i| presets.remove(i));
                results.push(preset.unwrap_or(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: fallback_content.to_owned(),
                    is_error: true,
                }));
            }
        }
        if !results.is_empty() {
            self.push_message(Message {
                role: Role::User,
                content: results,
            });
        }
    }

    /// 步骤 5：取历史末尾 assistant 消息的 tool_use 块，编排执行并返回
    /// ToolResult 块数组（顺序与模型声明序一致）。
    ///
    /// 执行分组：批内 `is_read_only()` 的调用 `join_all` 并行（保序），
    /// 非只读串行。事件序：先按声明序发出全部 ToolCallBegin，执行结束后
    /// 按声明序发出全部 ToolCallEnd——并行批内无法逐调用穿插
    /// begin/execute/end，统一前置/后置是最简单且保序的形态。
    pub(super) async fn execute_tool_calls(
        &self,
        events: &mpsc::Sender<Event>,
        submission_id: &str,
        mut preset_results: Vec<ContentBlock>,
        tool_ctx: &ToolCtx,
    ) -> Vec<ContentBlock> {
        // 声明序的调用清单：`(id, name, input)`。
        let calls: Vec<(String, String, serde_json::Value)> = self
            .messages
            .last()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        for (id, name, input) in &calls {
            emit(
                events,
                submission_id,
                EventMsg::ToolCallBegin {
                    call_id: id.clone(),
                    tool: name.clone(),
                    input: input.clone(),
                },
            )
            .await;
        }

        // 结果槽位（按声明序）；invalid-json 预置结果与 unknown-tool 先填，
        // 均不实际执行、不中断 turn。`executed` 标记实际执行过的调用
        //（PostToolUse hook 只对实际执行触发——预置 / 拦截 / 审批拒绝不算）。
        let mut slots: Vec<Option<ToolOutput>> = calls.iter().map(|_| None).collect();
        let mut executed = vec![false; calls.len()];
        for (i, (id, name, input)) in calls.iter().enumerate() {
            let preset = preset_results
                .iter()
                .position(
                    |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
                )
                .map(|j| preset_results.remove(j));
            if let Some(ContentBlock::ToolResult {
                content, is_error, ..
            }) = preset
            {
                slots[i] = Some(ToolOutput { content, is_error });
            } else if self.cfg.registry.get(name).is_none() {
                slots[i] = Some(ToolOutput {
                    content: format!("unknown tool: {name}"),
                    is_error: true,
                });
                continue;
            }
            if slots[i].is_some() {
                continue;
            }
            // P7：skill 激活的工具面白名单（allowed-tools，SPEC §8.2）——
            // 在 hook 与审批之前拦截：名单外工具直接 is_error 回灌，不实际
            // 执行（skill 工具自身也受限：白名单不含 `skill` 时激活后不能
            // 再触发其他 skill，首版语义见 skills 模块注释）。
            if !self.cfg.registry.allowlist().is_allowed(name) {
                slots[i] = Some(ToolOutput {
                    content: format!(
                        "tool `{name}` is not in the active skill's allowed-tools; the call was \
                         blocked (no changes were made)"
                    ),
                    is_error: true,
                });
                continue;
            }
            // P7：PreToolUse hook（可阻塞，SPEC §9 / §11.1 管道顺序：查找
            // → PreToolUse → 审批 → execute）。退出码 2 阻塞：stderr 以
            // is_error ToolResult 回灌模型，工具不执行、不进审批。
            if let Some(engine) = &self.cfg.hooks {
                let report = engine
                    .run(
                        wavecode_hooks::HookEventPoint::PreToolUse,
                        &wavecode_hooks::HookInput {
                            cwd: &tool_ctx.cwd,
                            tool_name: Some(name),
                            tool_input: Some(input),
                            tool_output: None,
                        },
                    )
                    .await;
                emit_hook_warnings(events, submission_id, &report.warnings).await;
                if let wavecode_hooks::HookVerdict::Block(stderr) = report.verdict {
                    let reason = if stderr.is_empty() {
                        "(hook 未给出原因)".to_owned()
                    } else {
                        stderr
                    };
                    slots[i] = Some(ToolOutput {
                        content: format!("blocked by PreToolUse hook:\n{reason}"),
                        is_error: true,
                    });
                    continue;
                }
            }
        }

        // 只读调用 join_all 并行（保序）。内置工具 execute 已为真 async
        //（tokio::fs / tokio::process；grep/glob 内部包 spawn_blocking 自理
        // 阻塞遍历），编排层直接 await 即得真实并行，无需再垫 spawn_blocking。
        // 知情延后：批内重排为"全部只读并行 → 非只读串行"，如 [R1, W1, R2]
        // 实际执行 R1∥R2 → W1——R2 读到 W1 写入前的内容；
        // 后续考虑按连续只读段分组以贴近声明执行序。
        // 只读但标记 destructive 的工具不进并行批：破坏性调用一律走串行段
        // 过审批门（P2，SPEC §12"破坏性工具默认需审批"）。
        let read_only: Vec<usize> = (0..calls.len())
            .filter(|&i| {
                slots[i].is_none()
                    && self
                        .cfg
                        .registry
                        .get(&calls[i].1)
                        .is_some_and(|t| t.is_read_only() && !t.is_destructive())
            })
            .collect();
        let ro_futures: Vec<_> = read_only
            .iter()
            .map(|&i| {
                // slots[i] 为空即工具存在（unknown 已预填），此处 expect 不会触发。
                let tool = self.cfg.registry.get(&calls[i].1).expect("tool 已判定存在");
                let input = calls[i].2.clone();
                let ctx = tool_ctx.clone();
                async move { tool.execute(input, &ctx).await }
            })
            .collect();
        let ro_outputs = futures::future::join_all(ro_futures).await;
        for (&i, out) in read_only.iter().zip(ro_outputs) {
            slots[i] = Some(output_or_err(out));
            executed[i] = true;
        }

        // 非只读（及只读但破坏性）串行（execute 同为真 async，直接 await）。
        for i in 0..calls.len() {
            if slots[i].is_none() {
                // 安全点：串行迭代间检查中断；剩余调用以 interrupted 结果
                // 收尾（不 break——ToolResult 必须与全部 tool_use 配对）。
                if self.interrupted.load(Ordering::SeqCst) {
                    slots[i] = Some(ToolOutput {
                        content: "interrupted by user".to_owned(),
                        is_error: true,
                    });
                    continue;
                }
                let tool = self.cfg.registry.get(&calls[i].1).expect("tool 已判定存在");
                // P2 审批门（SPEC §5.1 AwaitApproval）：执行前经 sandbox 判定。
                // Deny 不实际执行，reason 以 is_error 结果回灌模型；Ask 发
                // ApprovalRequested 事件并 park 等待 ExecApproval 回填。
                let verdict = self.cfg.sandbox.decide(
                    &calls[i].1,
                    &calls[i].2,
                    tool.is_read_only(),
                    tool.is_destructive(),
                );
                match verdict {
                    Verdict::Allow => {}
                    Verdict::Deny { reason } => {
                        slots[i] = Some(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                        continue;
                    }
                    Verdict::Ask { kind, detail } => {
                        emit(
                            events,
                            submission_id,
                            EventMsg::ApprovalRequested {
                                call_id: calls[i].0.clone(),
                                kind,
                                detail,
                            },
                        )
                        .await;
                        match self.await_approval(&calls[i].0).await {
                            ApprovalWait::Decision(ApprovalDecision::AllowOnce) => {}
                            ApprovalWait::Decision(ApprovalDecision::AllowAlways) => {
                                // 始终放行：派生会话级精确 allow 规则，后续
                                // 同形态调用免审批（见 Sandbox::allow_always
                                // 语义注释）。输入缺 command/path 无法派生时
                                // 退化为单次放行，warn 留痕不静默吞掉差异。
                                match self.cfg.sandbox.allow_always(&calls[i].1, &calls[i].2) {
                                    Some(rule) => {
                                        tracing::info!(%rule, "allow_always: 已写入会话级放行规则")
                                    }
                                    None => tracing::warn!(
                                        "allow_always 无法从输入派生规则，按 allow_once 处理"
                                    ),
                                }
                            }
                            ApprovalWait::Decision(ApprovalDecision::Deny { reason }) => {
                                slots[i] = Some(ToolOutput {
                                    content: rejection_content(&reason),
                                    is_error: true,
                                });
                                continue;
                            }
                            ApprovalWait::Interrupted => {
                                // 与串行段中断检查点同形态：以 interrupted
                                // 结果收尾，后续调用在迭代头检查点同样收尾。
                                slots[i] = Some(ToolOutput {
                                    content: "interrupted by user".to_owned(),
                                    is_error: true,
                                });
                                continue;
                            }
                            // ApprovalDecision 标注 non_exhaustive：未来新增
                            // 决策变体按拒绝处理（安全默认），warn 留痕。
                            ApprovalWait::Decision(_) => {
                                tracing::warn!("未知审批决策变体，按拒绝处理");
                                slots[i] = Some(ToolOutput {
                                    content: "rejected: unsupported approval decision".to_owned(),
                                    is_error: true,
                                });
                                continue;
                            }
                        }
                    }
                }
                let out = tool.execute(calls[i].2.clone(), tool_ctx).await;
                slots[i] = Some(output_or_err(out));
                executed[i] = true;
            }
        }

        // ToolCallEnd + ToolResult 组装（声明序）。P7：PostToolUse hook
        //（不可阻塞，SPEC §9）挂在实际执行之后、ToolCallEnd 之前——只对
        // 实际执行的调用触发（预置 / 拦截 / 审批拒绝不算执行）。
        let mut results = Vec::with_capacity(calls.len());
        for (i, (id, name, input)) in calls.iter().enumerate() {
            let out = slots[i].take().expect("每个槽位必有结果");
            if executed[i]
                && let Some(engine) = &self.cfg.hooks
            {
                let report = engine
                    .run(
                        wavecode_hooks::HookEventPoint::PostToolUse,
                        &wavecode_hooks::HookInput {
                            cwd: &tool_ctx.cwd,
                            tool_name: Some(name),
                            tool_input: Some(input),
                            tool_output: Some(&out.content),
                        },
                    )
                    .await;
                emit_hook_warnings(events, submission_id, &report.warnings).await;
            }
            tracing::debug!(call_id = %id, ok = !out.is_error, "工具调用完成");
            emit(
                events,
                submission_id,
                EventMsg::ToolCallEnd {
                    call_id: id.clone(),
                    ok: !out.is_error,
                    output: out
                        .content
                        .chars()
                        .take(TOOL_OUTPUT_EVENT_MAX_CHARS)
                        .collect(),
                },
            )
            .await;
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: out.content,
                is_error: out.is_error,
            });
        }
        results
    }
}

/// ToolCallEnd 事件回显工具输出的字符上限（回灌模型的 ToolResult 不截断）。
const TOOL_OUTPUT_EVENT_MAX_CHARS: usize = 2000;

/// 如单测直接 run_turn）兜底，最坏多等一个间隔。
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(25);
