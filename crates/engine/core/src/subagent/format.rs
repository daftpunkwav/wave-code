use super::*;

/// 摘要空串兜底（中断 / 畸形流可能没有最终文本）。
pub(crate) fn non_empty_summary(text: String, fallback: &str) -> String {
    if text.trim().is_empty() {
        fallback.to_owned()
    } else {
        text
    }
}

/// 子代理终态词（非 exhaustive 协议枚举的兜底映射）。
pub(crate) fn status_label(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Completed => "completed",
        SubagentStatus::Failed => "failed",
        SubagentStatus::Stopped => "stopped",
        // SubagentStatus 标注 non_exhaustive：未来变体按未知词展示。
        _ => "unknown",
    }
}

/// `<task-notification>` 注入文本（SPEC §5.3）：后台子代理终态以 user
/// 消息注入父会话下一 turn。
pub(crate) fn format_notification(
    task_id: &str,
    subagent_type: SubagentType,
    description: &str,
    result: &TaskResult,
) -> String {
    format!(
        "<task-notification>\nBackground task {task_id} ({}) finished with status: {}.\nDescription: {description}\nResult:\n{}\n</task-notification>",
        subagent_type.as_str(),
        status_label(result.status),
        result.summary,
    )
}

/// task_output / 同步 task 的结果文本（状态 + token 用量 + 摘要）。
pub(crate) fn format_result(result: &TaskResult) -> String {
    let mut out = format!("status: {}", status_label(result.status));
    if let Some(tokens) = result.tokens_used {
        out.push_str(&format!("\ntokens used: {tokens}"));
    }
    out.push_str(&format!("\nresult:\n{}", result.summary));
    out
}

/// 取必填字符串参数；缺失 / 空串返回错误文案（回灌模型自我纠正）。
pub(crate) fn required_str<'a>(
    input: &'a Value,
    key: &str,
) -> std::result::Result<&'a str, String> {
    match input.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(format!(
            "missing or invalid parameter '{key}' (non-empty string required)"
        )),
    }
}
