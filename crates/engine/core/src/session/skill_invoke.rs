//! slash 直调 skill（阶段 1b 拆分自 session/mod.rs，SPEC §8.2）：inline 展开
//! 驱动一轮 turn / fork 派生后台子代理。

use super::*;
use tokio::sync::mpsc;
use wavecode_protocol::{Event, EventMsg, StopReason};

impl super::Session {
    pub async fn invoke_skill(
        &mut self,
        submission_id: &str,
        name: &str,
        args: &str,
        events: mpsc::Sender<Event>,
    ) -> anyhow::Result<()> {
        let finish = |events: mpsc::Sender<Event>| async move {
            emit(
                &events,
                submission_id,
                EventMsg::TurnCompleted {
                    stop_reason: StopReason::Completed,
                },
            )
            .await;
        };
        let fail = |events: mpsc::Sender<Event>, message: String| async move {
            emit(
                &events,
                submission_id,
                EventMsg::Error {
                    message,
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
        };
        let Some(skills) = &self.cfg.skills else {
            fail(events, "skills 不可用（启动时未发现任何 skill）".to_owned()).await;
            return Ok(());
        };
        match crate::skills::plan_invocation(&skills.set, name, args, true) {
            Err(reason) => {
                fail(events, reason).await;
            }
            Ok(crate::skills::SkillInvocation::Inline(expanded)) => {
                let allowed = self
                    .cfg
                    .skills
                    .as_ref()
                    .and_then(|s| s.set.get(name))
                    .filter(|skill| !skill.meta.allowed_tools.is_empty())
                    .map(|skill| skill.meta.allowed_tools.clone());
                // inline：展开正文作为 turn 输入（历史里可见完整展开——
                // slash 直调的透明性）；run_turn 自身发 TurnCompleted。
                self.run_turn_inner(submission_id, &expanded, events, allowed)
                    .await?;
            }
            Ok(crate::skills::SkillInvocation::Fork(spec)) => {
                let Some(mgr) = &self.subagents else {
                    fail(
                        events,
                        format!("skill `{name}` 需要子代理能力（context: fork），当前会话不可用"),
                    )
                    .await;
                    return Ok(());
                };
                // 事件汇挂到本 submission：SubagentStarted/Completed 随本次
                // slash 交互可见（中间过程不进父会话，同 P5 纪律）。
                mgr.set_event_sink(events.clone(), submission_id);
                mgr.spawn_background(spec);
                finish(events).await;
            }
        }
        Ok(())
    }
}
