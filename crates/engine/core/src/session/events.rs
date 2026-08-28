//! turn 执行过程的事件投递与结果回灌 helper(拆分自 session/mod.rs):
//! emit / fail_turn / emit_hook_warnings 负责事件发射;output_or_err /
//! rejection_content 负责工具结果回灌;is_prompt_too_long 是 reactive
//! compact 的触发判定。

use tokio::sync::mpsc;
use wavecode_llm::LlmError;
use wavecode_tools::ToolOutput;
use wavecode_protocol::{Event, EventMsg, StopReason};

pub(super) fn output_or_err(out: wavecode_tools::Result<ToolOutput>) -> ToolOutput {
    match out {
        Ok(o) => o,
        Err(e) => ToolOutput {
            content: format!("tool execution failed: {e}"),
            is_error: true,
        },
    }
}

/// 识别 prompt_too_long 类错误（reactive compact 触发条件，SPEC §5.2）。
///
/// llm 在错误构造点（HTTP 非 2xx 响应与 SSE error 事件）统一分类为
/// [`LlmError::PromptTooLong`]（见 `classify_api_error`），core 直接枚举
/// 匹配，不做 kind / message 字符串嗅探。
pub(super) fn is_prompt_too_long(e: &LlmError) -> bool {
    matches!(e, LlmError::PromptTooLong { .. })
}

/// 用户拒绝审批的回灌文案：模型须能区分"被人拒绝"与"执行失败"；
/// 空原因补默认句，保证 reason 总是出现在回灌内容里。
pub(super) fn rejection_content(reason: &str) -> String {
    let reason = reason.trim();
    if reason.is_empty() {
        "rejected by user (no reason given)".to_owned()
    } else {
        format!("rejected by user: {reason}")
    }
}

/// 错误路径收尾：TurnStarted 已发出，emit Error + TurnCompleted{Error}
/// 防前端悬挂等待；错误本身仍以 Err 返回调用方。
pub(super) async fn fail_turn(events: &mpsc::Sender<Event>, submission_id: &str, message: String) {
    emit(
        events,
        submission_id,
        EventMsg::Error {
            message,
            recoverable: false,
        },
    )
    .await;
    emit(
        events,
        submission_id,
        EventMsg::TurnCompleted {
            stop_reason: StopReason::Error,
        },
    )
    .await;
}

/// 尽力投递事件：send 失败即 receiver 已关闭（前端断开），记 debug 日志
/// 并继续执行——M1 选择"继续"而非安全退出：历史一致性优先，事件流只是
/// 旁观通道，channel 满时 send 自然挂起形成背压，不会失败。
pub(super) async fn emit(events: &mpsc::Sender<Event>, submission_id: &str, msg: EventMsg) {
    let ev = Event {
        id: submission_id.to_owned(),
        msg,
    };
    if events.send(ev).await.is_err() {
        tracing::debug!("事件接收端已断开，继续执行 turn（后续事件不再投递）");
    }
}

/// P7：hook 警告统一转 Warning 事件（非零退出码 / 超时 kill / spawn 失败
/// 等对前端可见；SPEC §9"警告放行"的可观测面）。
pub(super) async fn emit_hook_warnings(
    events: &mpsc::Sender<Event>,
    submission_id: &str,
    warnings: &[String],
) {
    for message in warnings {
        emit(
            events,
            submission_id,
            EventMsg::Warning {
                message: message.clone(),
            },
        )
        .await;
    }
}

