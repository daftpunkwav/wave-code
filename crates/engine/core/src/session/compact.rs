//! 上下文压缩管线（阶段 1b 拆分自 session/mod.rs）：手动 / 自动 / 阻塞三类
//! 触发共用一条管线（SPEC §6）；check_budget 是 PreTurn 三级阈值。

use super::*;
use tokio::sync::mpsc;
use wavecode_context::{BudgetLevel, ModelSummary};
use wavecode_llm::{ContentBlock, Message, Role};
use wavecode_protocol::{CompactTrigger, Event, EventMsg};

impl super::Session {
    pub async fn compact(
        &mut self,
        submission_id: &str,
        events: mpsc::Sender<Event>,
    ) -> anyhow::Result<u64> {
        match self
            .compact_with_trigger(&events, submission_id, CompactTrigger::Manual)
            .await
        {
            Ok(summary_tokens) => Ok(summary_tokens),
            Err(e) => {
                emit(
                    &events,
                    submission_id,
                    EventMsg::Error {
                        message: format!("上下文压缩失败: {e:#}"),
                        recoverable: true,
                    },
                )
                .await;
                Err(e)
            }
        }
    }

    /// 压缩管线统一入口（三类触发 + 手动共用，SPEC §6 "触发管线只有一条"）：
    /// 发 CompactStarted → 模型摘要压缩（normalize 保证配对完整）→ 替换历史
    /// → 发 CompactCompleted{summary_tokens}。返回摘要 token 估算。
    pub(super) async fn compact_with_trigger(
        &mut self,
        events: &mpsc::Sender<Event>,
        submission_id: &str,
        trigger: CompactTrigger,
    ) -> anyhow::Result<u64> {
        // P7：PreCompact hook（不可阻塞，压缩前留档；SPEC §9）。
        if let Some(engine) = &self.cfg.hooks {
            let report = engine
                .run(
                    wavecode_hooks::HookEventPoint::PreCompact,
                    &wavecode_hooks::HookInput {
                        cwd: &self.cfg.cwd,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    },
                )
                .await;
            emit_hook_warnings(events, submission_id, &report.warnings).await;
        }
        emit(events, submission_id, EventMsg::CompactStarted { trigger }).await;
        let strategy = ModelSummary::new(self.cfg.model.clone(), self.cfg.model_name.clone());
        let outcome =
            wavecode_context::compact_history(&self.messages, &strategy, &self.cfg.context).await?;
        let summary_tokens = wavecode_context::estimate_tokens(
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: outcome.summary.clone(),
                }],
            }],
            self.cfg.context.estimate_chars_per_token,
        );
        self.messages = Arc::new(outcome.messages);
        // P10：压缩记录落盘——承载压缩后的完整新历史（replay 遇此记录
        // 即重置历史，恢复语义见 rollout 模块注释）。
        if let Some(recorder) = &mut self.recorder {
            recorder.record_compaction(trigger, summary_tokens, &self.messages);
        }
        // 压缩后 usage_carry 以新历史的估算重置：下次采样回传权威 usage 前，
        // PreTurn 阈值判断用这个估算（误差边界见 estimate_tokens 注释）。
        self.usage_carry = Some(
            wavecode_context::estimate_tokens(
                &self.messages,
                self.cfg.context.estimate_chars_per_token,
            ) + wavecode_context::SYSTEM_OVERHEAD_TOKENS,
        );
        emit(
            events,
            submission_id,
            EventMsg::CompactCompleted { summary_tokens },
        )
        .await;
        // P7：PostCompact hook（不可阻塞，压缩后留档；SPEC §9）。
        if let Some(engine) = &self.cfg.hooks {
            let report = engine
                .run(
                    wavecode_hooks::HookEventPoint::PostCompact,
                    &wavecode_hooks::HookInput {
                        cwd: &self.cfg.cwd,
                        tool_name: None,
                        tool_input: None,
                        tool_output: None,
                    },
                )
                .await;
            emit_hook_warnings(events, submission_id, &report.warnings).await;
        }
        tracing::debug!(?trigger, summary_tokens, "上下文压缩完成");
        Ok(summary_tokens)
    }

    /// PreTurn 预算检查（SPEC §6 三级阈值）：
    /// - 警告线：发 Warning（每 turn 至多一次，`warned` 去重）；
    /// - 自动压缩线 / 阻塞线：触发压缩（阻塞 = 强制先压缩再采样）。
    ///
    /// 每 turn 至多自动压缩一次（`compacted` 去重）：压缩后水位仍超标
    ///（极端：最近 N 条原文本身就逼近窗口）时不反复压缩空转——阻塞线
    /// 改发 Warning 放行，reactive compact（prompt_too_long 重试）是兜底。
    pub(super) async fn check_budget(
        &mut self,
        events: &mpsc::Sender<Event>,
        submission_id: &str,
        used: u64,
        warned: &mut bool,
        compacted: &mut bool,
    ) -> anyhow::Result<()> {
        match self
            .cfg
            .context
            .thresholds
            .check(used, self.cfg.context_window)
        {
            BudgetLevel::Ok => {}
            BudgetLevel::Warning => {
                if !*warned {
                    *warned = true;
                    emit(
                        events,
                        submission_id,
                        EventMsg::Warning {
                            message: format!(
                                "context near limit: {used}/{} tokens used",
                                self.cfg.context_window
                            ),
                        },
                    )
                    .await;
                }
            }
            level @ (BudgetLevel::AutoCompact | BudgetLevel::Blocking) => {
                if *compacted {
                    // 本 turn 已压缩过仍超标：不再空转，警告后放行（兜底见上）。
                    if !*warned {
                        *warned = true;
                        emit(
                            events,
                            submission_id,
                            EventMsg::Warning {
                                message: format!(
                                    "context still near limit after compaction: {used}/{} tokens",
                                    self.cfg.context_window
                                ),
                            },
                        )
                        .await;
                    }
                    return Ok(());
                }
                *compacted = true;
                let trigger = match level {
                    BudgetLevel::AutoCompact => CompactTrigger::Auto,
                    _ => CompactTrigger::Blocking,
                };
                // 压缩失败：自动线降级为警告放行（仍低于阻塞线，可继续）；
                // 阻塞线无法再安全采样，错误上抛由调用方收尾。
                if let Err(e) = self
                    .compact_with_trigger(events, submission_id, trigger)
                    .await
                {
                    if trigger == CompactTrigger::Blocking {
                        return Err(e);
                    }
                    emit(
                        events,
                        submission_id,
                        EventMsg::Warning {
                            message: format!("auto compaction failed, continuing: {e:#}"),
                        },
                    )
                    .await;
                }
            }
        }
        Ok(())
    }
}
