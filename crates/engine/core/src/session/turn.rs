//! turn 状态机执行器（阶段 1b 拆分自 session/mod.rs）：TurnRunner 收编
//! turn 级跨轮状态；push_message 是历史追加的唯一入口（rollout 落盘 + 写时复制）。

use super::*;
use futures::StreamExt;
use tokio::sync::mpsc;
use wavecode_llm::{ChatRequest, ContentBlock, Message, Role, StreamEvent};
use wavecode_protocol::{Event, EventMsg, StopReason};
use wavecode_tools::ToolCtx;

/// turn 执行器：收编 turn 级跨轮状态（原 `run_turn_inner` 的局部变量），
/// 把"采样→工具编排→续写/steering→收尾"状态机从 Session 方法迁入独立类型。
///
/// 持有 `&mut Session`，经字段路径访问共享状态（messages/interrupted/
/// approval_gate/cfg）。`round`（每轮采样缓冲，`finish` 已 take）与
/// `budget_warned`/`budget_compacted` 为 `run` 方法局部、不入字段——防跨轮
/// 残留污染（SEC-002 / B-NEW-1）。`events`/`submission_id` 作 `run` 参数，
/// 避免与 `session` 字段形成跨 await 的 disjoint borrow 冲突。
pub(super) struct TurnRunner<'a> {
    session: &'a mut Session,
    tool_ctx: ToolCtx,
    /// 末轮 input_tokens；每轮采样后赋值，break 路径 expect 安全。
    last_input_tokens: Option<u64>,
    /// 各轮 output_tokens 累计。
    total_output_tokens: u64,
    /// max_tokens 续写连续计数（成功即清零 / 达上限熔断）。
    continuations: u32,
    /// reactive compact 连续失败计数（采样成功即清零 / 达上限熔断）。
    reactive_compacts: u32,
    /// todo steering 连续提醒计数（模型再次 tool_use 即清零）。
    todo_steerings: u32,
    /// Stop hook 连续阻塞计数（上限后放行，防死循环）。
    stop_hook_blocks: u32,
}

impl<'a> TurnRunner<'a> {
    pub(super) fn new(session: &'a mut Session) -> Self {
        let tool_ctx = ToolCtx {
            cwd: session.cfg.cwd.clone(),
            deny_env: session.cfg.deny_env.clone(),
        };
        Self {
            session,
            tool_ctx,
            last_input_tokens: None,
            total_output_tokens: 0,
            continuations: 0,
            reactive_compacts: 0,
            todo_steerings: 0,
            stop_hook_blocks: 0,
        }
    }

    pub(super) async fn run(
        mut self,
        submission_id: &str,
        text: &str,
        events: mpsc::Sender<Event>,
        allowed_tools: Option<Vec<String>>,
    ) -> anyhow::Result<StopReason> {
        // P7：turn 级工具面白名单每 turn 入口清零后按需设置（上一 turn
        // 的 skill 激活不得泄漏进本轮）。
        self.session
            .cfg
            .registry
            .allowlist()
            .set(allowed_tools.map(|names| names.into_iter().collect()));
        // SEC-002 单点不变量：中断标志 / 审批槽每 turn 自清，仅此一处——
        // 不得挪入任何可能被多次调用的 helper（用户中断被吞 / 已批准决策丢失）。
        self.session.interrupted.store(false, Ordering::SeqCst);
        self.session.approval_gate.clear();
        // P5：子代理事件汇挂接——SubagentStarted/Completed 以本 turn 的
        // submission_id 回填。
        if let Some(mgr) = &self.session.subagents {
            mgr.set_event_sink(events.clone(), submission_id);
        }

        // 步骤 1：用户消息入历史，发出 TurnStarted。
        // P7：UserPromptSubmit hook（可阻塞）：阻塞时输入不进历史、不发起
        // 采样，stderr 以 Error 事件展示（TurnStarted 未发，补 TurnCompleted
        // 防前端悬挂）。
        if let Some(engine) = &self.session.cfg.hooks {
            let report = engine
                .run(
                    wavecode_hooks::HookEventPoint::UserPromptSubmit,
                    &wavecode_hooks::HookInput {
                        cwd: &self.session.cfg.cwd,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    },
                )
                .await;
            emit_hook_warnings(&events, submission_id, &report.warnings).await;
            if let wavecode_hooks::HookVerdict::Block(stderr) = report.verdict {
                let reason = if stderr.is_empty() {
                    "(hook 未给出原因)".to_owned()
                } else {
                    stderr
                };
                emit(
                    &events,
                    submission_id,
                    EventMsg::Error {
                        message: format!("输入被 UserPromptSubmit hook 拦截: {reason}"),
                        recoverable: true,
                    },
                )
                .await;
                emit(
                    &events,
                    submission_id,
                    EventMsg::TurnCompleted {
                        stop_reason: StopReason::Completed,
                    },
                )
                .await;
                return Ok(StopReason::Completed);
            }
        }
        let turn_id = uuid::Uuid::new_v4().to_string();
        self.session.push_message(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        });
        emit(
            &events,
            submission_id,
            EventMsg::TurnStarted {
                turn_id: turn_id.clone(),
            },
        )
        .await;
        tracing::debug!(turn_id = %turn_id, submission_id, "turn 开始");

        // 上下文占用估算：末轮 input_tokens + 各轮 output_tokens 累计。
        let mut budget_warned = false;
        let mut budget_compacted = false;

        let stop_reason = loop {
            // 安全点①：循环头检查中断。触发场景：步骤 5 串行工具段中断后
            // 回到循环头——结果消息已完整回灌（配对完整），不再发起多余采样。
            if self.session.interrupted.load(Ordering::SeqCst) {
                emit(
                    &events,
                    submission_id,
                    EventMsg::TurnCompleted {
                        stop_reason: StopReason::Interrupted,
                    },
                )
                .await;
                return Ok(StopReason::Interrupted);
            }

            // —— P5：后台子代理终态通知注入（<task-notification>）——
            if let Some(mgr) = &self.session.subagents {
                for note in mgr.drain_notifications() {
                    self.session.push_message(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text { text: note }],
                    });
                }
            }

            // —— PreTurn 预算检查（P3，三级阈值）——
            let used = match self.last_input_tokens {
                Some(input) => input + self.total_output_tokens,
                None => self.session.usage_carry.unwrap_or_else(|| {
                    wavecode_context::estimate_tokens(
                        &self.session.messages,
                        self.session.cfg.context.estimate_chars_per_token,
                    ) + wavecode_context::SYSTEM_OVERHEAD_TOKENS
                }),
            };
            if let Err(e) = self
                .session
                .check_budget(
                    &events,
                    submission_id,
                    used,
                    &mut budget_warned,
                    &mut budget_compacted,
                )
                .await
            {
                fail_turn(&events, submission_id, format!("{e:#}")).await;
                return Err(e);
            }

            // —— 步骤 2：组装请求，发起流式采样 ——
            let (instruction_memory, memory_index) = match &self.session.cfg.memory {
                Some(mem) => (mem.instruction_memory.as_str(), mem.memory_index.as_str()),
                None => ("", ""),
            };
            let system = crate::prompt::build_system_prompt(
                &self.session.cfg.cwd,
                instruction_memory,
                &self.session.skills_catalog,
                memory_index,
                &self.session.cfg.registry.todos().snapshot(),
            )
            .await;
            let req = ChatRequest {
                model: self.session.cfg.model_name.clone(),
                system,
                messages: self.session.messages.clone(),
                tools: self.session.cfg.registry.specs(),
                max_tokens: self.session.cfg.max_output_tokens,
            };
            let mut stream = match self.session.cfg.model.stream(req).await {
                Ok(s) => {
                    self.reactive_compacts = 0; // 采样成功：连续失败计数清零
                    s
                }
                Err(e) => {
                    // reactive compact：prompt_too_long 压缩后重试，连续 3 次熔断。
                    if is_prompt_too_long(&e) {
                        self.reactive_compacts += 1;
                        if self.reactive_compacts >= MAX_REACTIVE_COMPACT_RETRIES {
                            fail_turn(
                                &events,
                                submission_id,
                                format!(
                                    "prompt_too_long 连续 {} 次，压缩重试熔断: {e}",
                                    self.reactive_compacts
                                ),
                            )
                            .await;
                            return Err(e.into());
                        }
                        tracing::warn!(
                            attempt = self.reactive_compacts,
                            "prompt_too_long，压缩后以压缩历史重试"
                        );
                        match self
                            .session
                            .compact_with_trigger(&events, submission_id, CompactTrigger::Reactive)
                            .await
                        {
                            Ok(_) => continue,
                            Err(ce) => {
                                fail_turn(
                                    &events,
                                    submission_id,
                                    format!("reactive compact 失败: {ce:#}"),
                                )
                                .await;
                                return Err(ce);
                            }
                        }
                    }
                    fail_turn(&events, submission_id, e.to_string()).await;
                    return Err(e.into());
                }
            };

            // —— 步骤 3：消费流，累计内容块与终态 ——
            let mut round = RoundBlocks::default();
            let mut stop_reason = String::new();
            let mut round_input_tokens = 0u64;
            let mut round_output_tokens = 0u64;
            while let Some(item) = stream.next().await {
                // 安全点②：流消费循环内检查中断，历史保留部分结果。
                if self.session.interrupted.load(Ordering::SeqCst) {
                    return Ok(self
                        .session
                        .finish_interrupted(&events, submission_id, round)
                        .await);
                }
                let event = match item {
                    Ok(ev) => ev,
                    Err(e) => {
                        fail_turn(&events, submission_id, e.to_string()).await;
                        return Err(e.into());
                    }
                };
                match event {
                    StreamEvent::TextDelta { text } => {
                        emit(
                            &events,
                            submission_id,
                            EventMsg::AgentMessageDelta { text: text.clone() },
                        )
                        .await;
                        round.cur_text.push_str(&text);
                    }
                    StreamEvent::ToolUseBegin { id, name } => {
                        round.close_open();
                        round.cur_tool = Some((id, name, String::new()));
                    }
                    StreamEvent::ToolUseInputDelta { partial_json } => {
                        if let Some((_, _, buf)) = round.cur_tool.as_mut() {
                            buf.push_str(&partial_json);
                        }
                    }
                    StreamEvent::BlockEnd => round.close_open(),
                    StreamEvent::MessageComplete {
                        stop_reason: sr,
                        usage,
                    } => {
                        if !sr.is_empty() {
                            stop_reason = sr;
                        }
                        round_input_tokens = usage.input_tokens;
                        round_output_tokens = usage.output_tokens;
                    }
                }
            }
            self.last_input_tokens = Some(round_input_tokens);
            self.total_output_tokens += round_output_tokens;

            // —— 步骤 4：组装 assistant 消息入历史 ——
            let blocks = round.finish();
            let full_text: String = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let has_tool_use = blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
            // 空响应不入历史（Anthropic 拒绝空 content）。
            if !blocks.is_empty() {
                self.session.push_message(Message {
                    role: Role::Assistant,
                    content: blocks,
                });
            }
            emit(
                &events,
                submission_id,
                EventMsg::AgentMessageComplete { text: full_text },
            )
            .await;

            if !has_tool_use {
                // max_tokens 续写：截断后以续写提示继续，最多 MAX_CONTINUATIONS 次。
                if stop_reason == "max_tokens" && self.continuations < MAX_CONTINUATIONS {
                    self.continuations += 1;
                    emit(
                        &events,
                        submission_id,
                        EventMsg::Warning {
                            message: format!(
                                "output truncated at max_tokens; continuing ({}/{MAX_CONTINUATIONS})",
                                self.continuations
                            ),
                        },
                    )
                    .await;
                    self.session.push_message(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: CONTINUATION_PROMPT.to_owned(),
                        }],
                    });
                    continue;
                }
                // P4 stop steering：终态无 tool_use 且清单仍有未完成项时注入提醒。
                let (pending, in_progress) = self.session.cfg.registry.todos().unfinished();
                if pending + in_progress > 0 && self.todo_steerings < MAX_TODO_STEERINGS {
                    self.todo_steerings += 1;
                    emit(
                        &events,
                        submission_id,
                        EventMsg::Warning {
                            message: format!(
                                "todo list has {} unfinished item(s); nudging the model to continue ({}/{MAX_TODO_STEERINGS})",
                                pending + in_progress, self.todo_steerings
                            ),
                        },
                    )
                    .await;
                    let reminder = format!(
                        "{TODO_STEERING_PROMPT}\nCurrent task list:\n{}",
                        wavecode_tools::format_todos(&self.session.cfg.registry.todos().snapshot())
                    );
                    self.session.push_message(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text { text: reminder }],
                    });
                    continue;
                }
                // P7 Stop hook（可阻塞）：与 todo steering 次序择一——都通过后
                // 由 Stop hook 外部门禁最后把关。阻塞时 stderr 回灌模型继续。
                if let Some(engine) = self.session.cfg.hooks.clone() {
                    let report = engine
                        .run(
                            wavecode_hooks::HookEventPoint::Stop,
                            &wavecode_hooks::HookInput {
                                cwd: &self.session.cfg.cwd,
                                tool_name: None,
                                tool_input: None,
                                tool_output: None,
                            },
                        )
                        .await;
                    emit_hook_warnings(&events, submission_id, &report.warnings).await;
                    if let wavecode_hooks::HookVerdict::Block(stderr) = report.verdict
                        && self.stop_hook_blocks < MAX_STOP_HOOK_BLOCKS
                    {
                        self.stop_hook_blocks += 1;
                        emit(
                            &events,
                            submission_id,
                            EventMsg::Warning {
                                message: format!(
                                    "Stop hook blocked turn completion ({}/{MAX_STOP_HOOK_BLOCKS})",
                                    self.stop_hook_blocks
                                ),
                            },
                        )
                        .await;
                        let reason = if stderr.is_empty() {
                            "(hook 未给出原因)".to_owned()
                        } else {
                            stderr
                        };
                        self.session.push_message(Message {
                            role: Role::User,
                            content: vec![ContentBlock::Text {
                                text: format!("A Stop hook blocked turn completion:\n{reason}"),
                            }],
                        });
                        continue;
                    }
                }
                break stop_reason;
            }
            // 模型再次发起 tool_use：steering 连续计数清零。
            self.todo_steerings = 0;

            // 安全点③：工具执行前检查中断。为悬空 tool_use 合成 interrupted
            // 结果保持配对。
            if self.session.interrupted.load(Ordering::SeqCst) {
                self.session
                    .push_pairing_results(round.preset_results, "interrupted by user");
                emit(
                    &events,
                    submission_id,
                    EventMsg::TurnCompleted {
                        stop_reason: StopReason::Interrupted,
                    },
                )
                .await;
                return Ok(StopReason::Interrupted);
            }

            // —— 步骤 5：工具编排执行，结果作为一个 user 消息回灌 ——
            let results = self
                .session
                .execute_tool_calls(&events, submission_id, round.preset_results, &self.tool_ctx)
                .await;
            self.session.push_message(Message {
                role: Role::User,
                content: results,
            });
        };

        // —— 步骤 6：终态 ——
        let last_input_tokens = self.last_input_tokens.expect("每轮采样后必先赋值才 break");
        if stop_reason == "max_tokens" {
            emit(
                &events,
                submission_id,
                EventMsg::Warning {
                    message: "output truncated: max_tokens reached".into(),
                },
            )
            .await;
        }
        // 权威占用跨 turn 结转。
        self.session.usage_carry = Some(last_input_tokens + self.total_output_tokens);
        emit(
            &events,
            submission_id,
            EventMsg::TokenCount {
                used: last_input_tokens + self.total_output_tokens,
                window: self.session.cfg.context_window,
            },
        )
        .await;
        emit(
            &events,
            submission_id,
            EventMsg::TurnCompleted {
                stop_reason: StopReason::Completed,
            },
        )
        .await;
        tracing::debug!(turn_id = %turn_id, %stop_reason, "turn 结束");
        Ok(StopReason::Completed)
    }
}

/// execute 的 Err（io 故障等实现级错误）同样转为 is_error ToolResult
/// 回灌，不中断 turn。
impl super::Session {
    pub(super) fn push_message(&mut self, msg: Message) {
        if let Some(recorder) = &mut self.recorder {
            recorder.record_message(&msg);
        }
        Arc::make_mut(&mut self.messages).push(msg);
    }
}
