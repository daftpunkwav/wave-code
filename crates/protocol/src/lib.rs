//! wavecode-protocol — 前后端协议的唯一事实源。
//!
//! 定义 `Submission { id, op: Op }`（前端 → core 的请求）与
//! `Event { id, msg: EventMsg }`（core → 前端的事件流），id 用于关联一次
//! 请求与其全部后续事件。`Op` / `EventMsg` 标注 `#[non_exhaustive]`，
//! 保证协议向后兼容地演进。
//!
//! 所有 Rust crate 与前端（TUI / Web / Desktop）共享此协议；
//! TypeScript 侧类型由 `wavecode app-server generate-ts`（规划，后续
//! 里程碑落地）从此处导出。

use serde::{Deserialize, Serialize};

/// 前端 → core 的一次请求；id 由前端生成（uuid），关联后续全部事件
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Submission {
    pub id: String,
    pub op: Op,
}

/// 前端 → core 的操作（`Submission.op` 的负载；线上格式由 serde tag 锁定）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Op {
    UserInput {
        text: String,
    },
    Interrupt,
    /// 审批回填（SPEC §12）：对应进行中的 `ApprovalRequested`，按 call_id 关联。
    ExecApproval {
        call_id: String,
        decision: ApprovalDecision,
    },
    /// 切换会话级权限模式（SPEC §12）。
    SetPermissionMode {
        mode: PermissionMode,
    },
    /// 立即压缩上下文（`/compact`，SPEC §6）：无活动 turn 时直接执行；
    /// turn 进行中由驱动方排队到 turn 结束后执行。
    Compact,
    /// slash 直调 skill（`/name [args]`，SPEC §4.1 / §8.2，P7 落地）：
    /// 无活动 turn 时直接执行；turn 进行中排队到 turn 结束后执行。
    SlashCommand {
        /// skill 名。
        name: String,
        /// 调用参数（`$ARGUMENTS` 展开来源；可空串）。
        args: String,
    },
    Shutdown,
}

/// 权限模式（SPEC §12；线型与 config 的 `permission_mode` 字符串一致）。
///
/// 放 protocol 而非 sandbox：它是 `Op::SetPermissionMode` 的线型，与
/// `StopReason` 同例；sandbox 只做判定逻辑并依赖本类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermissionMode {
    /// 写 / 执行 / 破坏性工具逐次审批（命中 allow 规则免审批）。
    #[serde(rename = "default")]
    Default,
    /// 仅只读工具可用；非只读工具直接拒绝回灌模型。
    #[serde(rename = "plan")]
    Plan,
    /// 文件编辑（write_file / edit_file）自动放行，shell 等仍审批。
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// 全放行（deny 规则仍生效）。
    // TODO(P2 后续)：进入此模式需输入确认短语（SPEC §12），前端交互待 TUI 落地。
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl PermissionMode {
    /// 解析 config / 前端传入的模式字符串（与 serde 线型同名）。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "plan" => Some(Self::Plan),
            "acceptEdits" => Some(Self::AcceptEdits),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }

    /// 线型字符串（config 与展示共用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 审批决策（`Op::ExecApproval.decision` 的负载；线上格式由 serde tag 锁定）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApprovalDecision {
    /// 本次放行（仅当前这一次工具调用）。
    AllowOnce,
    /// 始终放行并写入 allow 规则。P2 占位：core 当前按 AllowOnce 处理，
    /// 规则持久化待配置分层（§17.5 M3）落地后接线。
    AllowAlways,
    /// 拒绝；reason 回灌模型（可空串，core 会补默认文案）。
    Deny { reason: String },
}

/// 审批类别（`EventMsg::ApprovalRequested.kind`），供前端选择展示形态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApprovalKind {
    /// shell 命令执行。
    Exec,
    /// 文件写入 / 编辑。
    Write,
}

/// core → 前端的事件；id 回填对应 Submission.id
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: String,
    pub msg: EventMsg,
}

/// core → 前端的事件消息（`Event.msg` 的负载；线上格式由 serde tag 锁定）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventMsg {
    TurnStarted {
        turn_id: String,
    },
    AgentMessageDelta {
        text: String,
    },
    AgentMessageComplete {
        text: String,
    },
    ToolCallBegin {
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    ToolCallEnd {
        call_id: String,
        ok: bool,
        output: String,
    },
    /// 破坏性 / 非只读工具需人工审批（SPEC §12）：前端展示 detail 后以
    /// `Op::ExecApproval`（同 call_id）回填决策。
    ApprovalRequested {
        call_id: String,
        kind: ApprovalKind,
        detail: String,
    },
    TokenCount {
        used: u64,
        window: u64,
    },
    /// 上下文压缩开始（SPEC §6；`trigger` 区分三类触发与手动 `/compact`）。
    CompactStarted {
        trigger: CompactTrigger,
    },
    /// 上下文压缩完成；`summary_tokens` 为摘要消息的 token 估算。
    CompactCompleted {
        summary_tokens: u64,
    },
    /// 子代理派生（P5，SPEC §5.3）：task 工具派生子代理时发出
    ///（后台与同步形态都发）；子代理的中间过程不进父会话事件流
    ///（上下文隔离的同构），前端只见起点与终点。
    SubagentStarted {
        task_id: String,
        subagent_type: String,
        description: String,
    },
    /// 子代理到达终态（完成 / 失败 / 停止）；`summary` 为最终文本摘要
    ///（失败时为错误摘要），与回注父会话的 `<task-notification>` 同源。
    SubagentCompleted {
        task_id: String,
        status: SubagentStatus,
        summary: String,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
        recoverable: bool,
    },
    TurnCompleted {
        stop_reason: StopReason,
    },
}

/// 子代理终态（`EventMsg::SubagentCompleted.status`，P5）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubagentStatus {
    /// 正常完成（turn 以 end_turn 等终态收尾）。
    Completed,
    /// 失败（turn 出错，summary 为错误摘要）。
    Failed,
    /// 被 task_stop 停止（turn 以 Interrupted 收尾）。
    Stopped,
}

/// 压缩触发来源（`EventMsg::CompactStarted.trigger`）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactTrigger {
    /// 自动压缩线（PreTurn 预算检查，SPEC §6）。
    Auto,
    /// 阻塞线（强制先压缩再采样）。
    Blocking,
    /// reactive compact：`prompt_too_long` 类错误触发的压缩重试（SPEC §5.2）。
    Reactive,
    /// 手动 `/compact`（Op::Compact）。
    Manual,
}

/// turn 结束原因（`EventMsg::TurnCompleted.stop_reason`）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    Completed,
    Interrupted,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_roundtrip() {
        let sub = Submission {
            id: "s-1".into(),
            op: Op::UserInput {
                text: "hello".into(),
            },
        };
        let json = serde_json::to_string(&sub).unwrap();
        assert_eq!(
            json,
            r#"{"id":"s-1","op":{"type":"user_input","text":"hello"}}"#
        );
        assert_eq!(serde_json::from_str::<Submission>(&json).unwrap(), sub);
    }

    #[test]
    fn event_roundtrip_tool_call() {
        let ev = Event {
            id: "s-1".into(),
            msg: EventMsg::ToolCallBegin {
                call_id: "c1".into(),
                tool: "read_file".into(),
                input: serde_json::json!({"path": "a.txt"}),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }

    #[test]
    fn wire_type_tags_locked() {
        // 锁死协议线上格式：每个变体的精确 "type" tag（SPEC §4.1）。
        // 新增变体必须在表中登记；改动既有 tag 即破坏线上兼容。
        let op_cases: [(Op, &str); 7] = [
            (Op::UserInput { text: "t".into() }, "user_input"),
            (Op::Interrupt, "interrupt"),
            (
                Op::ExecApproval {
                    call_id: "c".into(),
                    decision: ApprovalDecision::AllowOnce,
                },
                "exec_approval",
            ),
            (
                Op::SetPermissionMode {
                    mode: PermissionMode::Default,
                },
                "set_permission_mode",
            ),
            (Op::Compact, "compact"),
            (
                Op::SlashCommand {
                    name: "commit".into(),
                    args: "a".into(),
                },
                "slash_command",
            ),
            (Op::Shutdown, "shutdown"),
        ];
        for (op, tag) in op_cases {
            let json = serde_json::to_string(&op).unwrap();
            assert!(
                json.contains(&format!(r#""type":"{tag}""#)),
                "Op::{tag} 的 type tag 漂移: {json}"
            );
        }

        let event_cases: [(EventMsg, &str); 14] = [
            (
                EventMsg::TurnStarted {
                    turn_id: "t1".into(),
                },
                "turn_started",
            ),
            (
                EventMsg::AgentMessageDelta { text: "t".into() },
                "agent_message_delta",
            ),
            (
                EventMsg::AgentMessageComplete { text: "t".into() },
                "agent_message_complete",
            ),
            (
                EventMsg::ToolCallBegin {
                    call_id: "c".into(),
                    tool: "read_file".into(),
                    input: serde_json::json!({}),
                },
                "tool_call_begin",
            ),
            (
                EventMsg::ToolCallEnd {
                    call_id: "c".into(),
                    ok: true,
                    output: "o".into(),
                },
                "tool_call_end",
            ),
            (
                EventMsg::ApprovalRequested {
                    call_id: "c".into(),
                    kind: ApprovalKind::Write,
                    detail: "d".into(),
                },
                "approval_requested",
            ),
            (EventMsg::TokenCount { used: 1, window: 2 }, "token_count"),
            (
                EventMsg::CompactStarted {
                    trigger: CompactTrigger::Auto,
                },
                "compact_started",
            ),
            (
                EventMsg::CompactCompleted { summary_tokens: 7 },
                "compact_completed",
            ),
            (
                EventMsg::SubagentStarted {
                    task_id: "task-1".into(),
                    subagent_type: "explore".into(),
                    description: "d".into(),
                },
                "subagent_started",
            ),
            (
                EventMsg::SubagentCompleted {
                    task_id: "task-1".into(),
                    status: SubagentStatus::Completed,
                    summary: "s".into(),
                },
                "subagent_completed",
            ),
            (
                EventMsg::Warning {
                    message: "w".into(),
                },
                "warning",
            ),
            (
                EventMsg::Error {
                    message: "e".into(),
                    recoverable: true,
                },
                "error",
            ),
            (
                EventMsg::TurnCompleted {
                    stop_reason: StopReason::Completed,
                },
                "turn_completed",
            ),
        ];
        for (msg, tag) in event_cases {
            let json = serde_json::to_string(&msg).unwrap();
            assert!(
                json.contains(&format!(r#""type":"{tag}""#)),
                "EventMsg::{tag} 的 type tag 漂移: {json}"
            );
        }

        // StopReason snake_case 全量锁定
        let reason_cases: [(StopReason, &str); 3] = [
            (StopReason::Completed, "completed"),
            (StopReason::Interrupted, "interrupted"),
            (StopReason::Error, "error"),
        ];
        for (reason, tag) in reason_cases {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!(r#""{tag}""#), "StopReason tag 漂移");
        }

        // PermissionMode 线型锁定（与 config `permission_mode` 字符串一致，
        // 注意 acceptEdits / bypassPermissions 是 camelCase 而非 snake_case）。
        let mode_cases: [(PermissionMode, &str); 4] = [
            (PermissionMode::Default, "default"),
            (PermissionMode::Plan, "plan"),
            (PermissionMode::AcceptEdits, "acceptEdits"),
            (PermissionMode::BypassPermissions, "bypassPermissions"),
        ];
        for (mode, tag) in mode_cases {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!(r#""{tag}""#), "PermissionMode tag 漂移");
            assert_eq!(PermissionMode::parse(tag), Some(mode));
            assert_eq!(mode.to_string(), tag);
        }
        assert_eq!(PermissionMode::parse("Default"), None);

        // ApprovalDecision / ApprovalKind 线型锁定
        let decision_cases: [(ApprovalDecision, &str); 3] = [
            (ApprovalDecision::AllowOnce, r#"{"type":"allow_once"}"#),
            (ApprovalDecision::AllowAlways, r#"{"type":"allow_always"}"#),
            (
                ApprovalDecision::Deny { reason: "r".into() },
                r#"{"type":"deny","reason":"r"}"#,
            ),
        ];
        for (decision, json) in decision_cases {
            assert_eq!(
                serde_json::to_string(&decision).unwrap(),
                json,
                "ApprovalDecision 线型漂移"
            );
        }
        let kind_cases: [(ApprovalKind, &str); 2] =
            [(ApprovalKind::Exec, "exec"), (ApprovalKind::Write, "write")];
        for (kind, tag) in kind_cases {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!(r#""{tag}""#), "ApprovalKind tag 漂移");
        }

        // CompactTrigger 线型锁定（P3）
        let trigger_cases: [(CompactTrigger, &str); 4] = [
            (CompactTrigger::Auto, "auto"),
            (CompactTrigger::Blocking, "blocking"),
            (CompactTrigger::Reactive, "reactive"),
            (CompactTrigger::Manual, "manual"),
        ];
        for (trigger, tag) in trigger_cases {
            let json = serde_json::to_string(&trigger).unwrap();
            assert_eq!(json, format!(r#""{tag}""#), "CompactTrigger tag 漂移");
        }

        // SubagentStatus 线型锁定（P5）
        let status_cases: [(SubagentStatus, &str); 3] = [
            (SubagentStatus::Completed, "completed"),
            (SubagentStatus::Failed, "failed"),
            (SubagentStatus::Stopped, "stopped"),
        ];
        for (status, tag) in status_cases {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!(r#""{tag}""#), "SubagentStatus tag 漂移");
        }
    }

    #[test]
    fn turn_completed_wire_format_locked() {
        // 锁死协议线上格式：type tag 与嵌套 stop_reason 的精确 JSON 表示
        let ev = Event {
            id: "s-1".into(),
            msg: EventMsg::TurnCompleted {
                stop_reason: StopReason::Completed,
            },
        };
        let msg_json = serde_json::to_string(&ev.msg).unwrap();
        assert_eq!(
            msg_json,
            r#"{"type":"turn_completed","stop_reason":"completed"}"#
        );
        // 包进 Event 后 msg 字段内的嵌套结构不变
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            json,
            r#"{"id":"s-1","msg":{"type":"turn_completed","stop_reason":"completed"}}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }
}
