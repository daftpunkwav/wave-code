//! 事件渲染：exec 与 REPL 共用。
//!
//! 两种输出：
//! - [`render_jsonl`]：事件 → 单行 JSON（`exec --json` 的 stdout 契约）；
//! - [`HumanRenderer`]：人类可读渲染状态机（delta 进缓冲、complete/中断时
//!   经 markdown 一次渲染；工具行/告警着色；等待动画帧由 tick 驱动）。
//!
//! 中断路径（M1-T7 审查结论）：`TurnCompleted{Interrupted}` 之前没有
//! `AgentMessageComplete` 与 `TokenCount`，渲染状态机不得假设每个 turn
//! 都有 TokenCount；Error 事件后 turn 也可能直接结束，渲染不得 panic。

use std::io::{self, Write};

use wavecode_protocol::{Event, StopReason};

/// 工具调用输入摘要的字符上限。
const TOOL_INPUT_MAX_CHARS: usize = 80;
/// 工具失败输出摘要的字符上限。
const TOOL_OUTPUT_MAX_CHARS: usize = 200;

/// 是否为需剥离的控制字符：C0（保留 `\n` / `\t`）、DEL、C1（U+0080–U+009F）。
mod human;
mod jsonl;
mod sanitize;
mod theme;

pub use human::HumanRenderer;
#[cfg(test)]
pub(crate) use human::{human_task_begin, human_tool_begin};
pub use jsonl::render_jsonl;
#[cfg(test)]
pub(crate) use sanitize::sanitize_terminal;
#[cfg(test)]
use std::borrow::Cow;
#[cfg(test)]
pub(crate) use theme::truncate_chars;

#[cfg(test)]
mod tests {
    use super::*;

    /// 去掉 ANSI 序列，便于断言可见文本（测试辅助）
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // 跳过 CSI：ESC [ ... 终字节 @-~
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('@'..='~').contains(&c2) {
                            break;
                        }
                    }
                    continue;
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn jsonl_one_event_per_line() {
        let ev = wavecode_protocol::Event {
            id: "s-1".into(),
            msg: wavecode_protocol::EventMsg::AgentMessageDelta { text: "hi".into() },
        };
        let line = render_jsonl(&ev);
        assert!(line.starts_with(r#"{"id":"s-1","msg":{"type":"agent_message_delta""#));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn human_render_tool_call_truncates_input() {
        let long = "x".repeat(200);
        let s = human_tool_begin("write_file", &serde_json::json!({"content": long}));
        // T5 起输出带 ANSI 样式：strip 后断言可见字符数
        assert!(strip(&s).chars().count() <= 100);
    }

    /// 多字节 UTF-8 输入按字符截断（非字节）：不切断码点、不 panic，
    /// 结果在上限内且以 `…` 收尾。
    #[test]
    fn truncate_multibyte_utf8_by_chars() {
        // CJK（3 字节码点）。
        let t = truncate_chars(&"汉".repeat(200), 80);
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
        // emoji（4 字节码点）与 CJK 混合。
        let t = truncate_chars(&"🦀汉".repeat(100), 80);
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
        // 短于上限原样返回。
        assert_eq!(truncate_chars("短", 80), "短");
    }

    /// P3：压缩事件渲染为弱化提示行（开始 / 完成带摘要 token 数）。
    #[test]
    fn compact_events_render_dim_lines() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::CompactStarted {
            trigger: wavecode_protocol::CompactTrigger::Manual,
        }))
        .unwrap();
        r.handle(&ev(M::CompactCompleted { summary_tokens: 42 }))
            .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("⟳ 正在压缩上下文"), "缺少开始行: {out:?}");
        assert!(
            out.contains("✓ 上下文已压缩（摘要 42 tokens）"),
            "缺少完成行: {out:?}"
        );
    }

    /// P2：审批请求渲染为黄色提示行（detail 经 sanitize 防终端注入）。
    #[test]
    fn approval_requested_renders_warning_line() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = wavecode_protocol::Event {
            id: "s-1".into(),
            msg: wavecode_protocol::EventMsg::ApprovalRequested {
                call_id: "c1".into(),
                kind: wavecode_protocol::ApprovalKind::Exec,
                detail: "shell: rm -rf build/\x1b[2J".into(),
            },
        };
        r.handle(&ev).unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(
            out.contains("⚠ 审批请求（执行命令）"),
            "应有审批提示行: {out:?}"
        );
        assert!(out.contains("shell: rm -rf build/"));
        assert!(!out.contains("\x1b[2J"), "注入序列应被剥离: {out:?}");
    }

    /// 中断路径（M1-T7 审查结论）：TurnCompleted{Interrupted} 前没有
    /// AgentMessageComplete / TokenCount —— 渲染不得假设有 tokens 行，
    /// 且须打印（已中断）标记、先补换行（delta 是裸 print!）。
    #[test]
    fn interrupted_turn_renders_without_tokens() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t-1".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta { text: "hi".into() }))
            .unwrap();
        r.handle(&ev(M::TurnCompleted {
            stop_reason: wavecode_protocol::StopReason::Interrupted,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("（已中断）"), "缺少中断标记: {out:?}");
        assert!(
            !out.contains("tokens:"),
            "无 TokenCount 不应打印 tokens 行: {out:?}"
        );
        // delta 之后必须先补换行再输出标记。
        assert!(out.contains("hi\n"), "TurnCompleted 前未补换行: {out:?}");
    }

    /// 正常路径：tokens 行取本 turn 最近一次 TokenCount。
    #[test]
    fn completed_turn_prints_latest_token_count() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t-1".into(),
        }))
        .unwrap();
        r.handle(&ev(M::TokenCount {
            used: 100,
            window: 200_000,
        }))
        .unwrap();
        r.handle(&ev(M::TokenCount {
            used: 120,
            window: 200_000,
        }))
        .unwrap();
        r.handle(&ev(M::TurnCompleted {
            stop_reason: wavecode_protocol::StopReason::Completed,
        }))
        .unwrap();
        let out = String::from_utf8_lossy(&r.out).into_owned();
        assert!(
            out.contains("tokens: 120/200000"),
            "tokens 行应取最近一次: {out:?}"
        );
        assert!(!out.contains("（已中断）"));
        // 下一个 turn 开始前状态已清理：无 TokenCount 的 turn 不残留上一 turn 数据。
        r.handle(&ev(M::TurnStarted {
            turn_id: "t-2".into(),
        }))
        .unwrap();
        r.handle(&ev(M::TurnCompleted {
            stop_reason: wavecode_protocol::StopReason::Completed,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert_eq!(out.matches("tokens:").count(), 1, "tokens 行残留: {out:?}");
    }

    /// 工具失败输出截断到 200 字符以内并带 ✗ 前缀；成功不输出。
    #[test]
    fn tool_call_end_failure_truncates_output() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::ToolCallEnd {
            call_id: "c1".into(),
            ok: true,
            output: "fine".into(),
        }))
        .unwrap();
        let long = "e".repeat(300);
        r.handle(&ev(M::ToolCallEnd {
            call_id: "c1".into(),
            ok: false,
            output: long,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        // T5 起 ✗ 行带红色样式：strip 后再断言
        let line = strip(out.lines().next().unwrap_or(""));
        assert!(line.starts_with('✗'), "失败输出应带 ✗ 前缀: {out:?}");
        // ✗(3字节) + 空格 + 199字符 + …(3字节) = 206 字节上限，且不含完整 300 字符。
        assert!(line.len() <= 206, "输出未截断: {} 字节", line.len());
        assert!(!out.contains(&"e".repeat(300)));
    }

    /// sanitize 单元面：CSI / OSC / BEL 剥离；正常中文 / emoji / 换行 /
    /// 制表符不受影响；无控制字符时零拷贝借用。
    #[test]
    fn sanitize_strips_control_sequences() {
        // CSI：清屏、带参数的颜色序列。
        assert_eq!(sanitize_terminal("a\x1b[2Jb"), "ab");
        assert_eq!(sanitize_terminal("\x1b[1;31m红\x1b[0m"), "红");
        // OSC：52 写剪贴板（BEL 与 ESC \ 两种终止形态）。
        assert_eq!(sanitize_terminal("x\x1b]52;;cGF5bG9hZA==\x07y"), "xy");
        assert_eq!(sanitize_terminal("x\x1b]0;title\x1b\\y"), "xy");
        // 孤立 BEL 与其他 C0（\n \t 除外）。
        assert_eq!(sanitize_terminal("p\x07q\x08r"), "pqr");
        // C1 控制字符（U+0080–U+009F，UTF-8 双字节形态）：字符本身剥离，
        // 其后参数字节按普通文本留存（终端已无法解释为序列）。
        assert_eq!(sanitize_terminal("a\u{9b}1;31mb"), "a1;31mb");
        // 正常文本不受影响；无控制字符走 Borrowed 零拷贝路径。
        let s = "正常中文🦀\n换行\t制表符";
        let sanitized = sanitize_terminal(s);
        assert_eq!(sanitized, s);
        assert!(matches!(sanitized, Cow::Borrowed(_)), "应零拷贝借用");
    }

    /// 渲染路径面：模型 / 工具来源文本中的控制序列不得进终端输出——
    /// delta、工具失败摘要、Warning / Error 四处应用点逐一锁定。
    #[test]
    fn render_strips_escape_sequences() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::AgentMessageDelta {
            text: "hi\x1b[2J\x07".into(),
        }))
        .unwrap();
        r.handle(&ev(M::ToolCallEnd {
            call_id: "c1".into(),
            ok: false,
            output: "boom\x1b]52;;cGF5bG9hZA==\x07".into(),
        }))
        .unwrap();
        r.handle(&ev(M::Warning {
            message: "warn\x1b[2J".into(),
        }))
        .unwrap();
        r.handle(&ev(M::Error {
            message: "err\x07".into(),
            recoverable: false,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        // T5 起渲染自行注入样式（含 ESC）：改为断言"注入的载荷"未进入输出
        assert!(!out.contains("\x1b[2J"), "注入的 CSI 进入输出: {out:?}");
        assert!(
            !out.contains("cGF5bG9hZA=="),
            "注入的 OSC 载荷进入输出: {out:?}"
        );
        assert!(!out.contains('\x07'), "输出含 BEL: {out:?}");
        assert!(out.contains("hi"), "正常文本被误剥: {out:?}");
        assert!(out.contains("boom"), "正常文本被误剥: {out:?}");
    }

    /// P4：todo_write 渲染清单状态符号与状态迁移标注（pending→completed 等）。
    #[test]
    fn todo_write_renders_status_migration() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |todos: serde_json::Value| wavecode_protocol::Event {
            id: "s-1".into(),
            msg: wavecode_protocol::EventMsg::ToolCallBegin {
                call_id: "c".into(),
                tool: "todo_write".into(),
                input: serde_json::json!({"todos": todos}),
            },
        };
        r.handle(&ev(serde_json::json!([
            {"content": "设计", "status": "in_progress"},
            {"content": "实现", "status": "pending"}
        ])))
        .unwrap();
        r.handle(&ev(serde_json::json!([
            {"content": "设计", "status": "completed"},
            {"content": "实现", "status": "pending"}
        ])))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("▸ todo_write"));
        assert!(out.contains("▸ 设计"), "首轮 in_progress 符号: {out:?}");
        assert!(out.contains("☐ 实现"), "pending 符号: {out:?}");
        assert!(
            out.contains("✓ 设计") && out.contains("（in_progress → completed）"),
            "状态迁移标注: {out:?}"
        );
    }

    /// P5：task 工具行展示子代理类型与描述（后台形态附标记）；子代理
    /// 起止事件渲染为弱化提示行。
    #[test]
    fn task_tool_and_subagent_events_render() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::ToolCallBegin {
            call_id: "c".into(),
            tool: "task".into(),
            input: serde_json::json!({
                "description": "调查认证模块",
                "prompt": "…",
                "subagent_type": "explore",
                "run_in_background": true
            }),
        }))
        .unwrap();
        r.handle(&ev(M::SubagentStarted {
            task_id: "task-1".into(),
            subagent_type: "explore".into(),
            description: "调查认证模块".into(),
        }))
        .unwrap();
        r.handle(&ev(M::SubagentCompleted {
            task_id: "task-1".into(),
            status: wavecode_protocol::SubagentStatus::Completed,
            summary: "结论".into(),
        }))
        .unwrap();
        let out = strip(&String::from_utf8(r.out).unwrap());
        assert!(out.contains("▸ task"), "task 工具行: {out:?}");
        assert!(out.contains("explore") && out.contains("调查认证模块"));
        assert!(out.contains("（后台）"), "后台标记: {out:?}");
        assert!(
            out.contains("⏚ 子代理 task-1 启动（explore）调查认证模块"),
            "启动行: {out:?}"
        );
        assert!(out.contains("✓ 子代理 task-1 完成"), "完成行: {out:?}");
        // 默认类型与无后台标记的回退形态。
        let line = human_task_begin(&serde_json::json!({"description": "d"}));
        let line = strip(&line);
        assert!(line.contains("general-purpose"));
        assert!(!line.contains("（后台）"));
    }

    /// T5：delta 进缓冲不直接输出；complete 时一次性经 markdown 渲染
    ///（记号被渲染掉、注入样式）。
    #[test]
    fn delta_is_buffered_until_complete_then_rendered_markdown() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta {
            text: "**你好**".into(),
        }))
        .unwrap();
        assert!(r.out.is_empty(), "delta 不得直接输出");
        r.handle(&ev(M::AgentMessageComplete {
            text: "**你好**".into(),
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("你好"));
        assert!(!out.contains("**"), "markdown 记号应被渲染掉：{out}");
        assert!(out.contains("\x1b["), "应有样式：{out}");
    }

    /// T5：中断 turn 的残余缓冲照常渲染，附（已中断）且无 tokens 行。
    #[test]
    fn interrupted_turn_renders_residual_buffer() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta {
            text: "写了一半".into(),
        }))
        .unwrap();
        r.handle(&ev(M::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("写了一半"), "中断残余应渲染：{out}");
        assert!(out.contains("（已中断）"));
        assert!(!out.contains("tokens:"), "中断无 tokens 行");
    }

    /// T5：工具开始/失败行着色（`▸ 工具名` 同色段、`✗` 红）。
    #[test]
    fn tool_lines_are_colored() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.handle(&ev(M::ToolCallBegin {
            call_id: "1".into(),
            tool: "write_file".into(),
            input: serde_json::json!({"path":"a.txt"}),
        }))
        .unwrap();
        r.handle(&ev(M::ToolCallEnd {
            call_id: "1".into(),
            ok: false,
            output: "permission denied".into(),
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        assert!(out.contains("▸ write_file"));
        assert!(out.contains("✗ permission denied"));
        assert!(out.contains("\x1b["), "工具行应着色：{out}");
    }

    /// T5：非 animate 时 tick_frame 为 no-op；turn 内处于等待模型状态。
    #[test]
    fn tick_frame_noop_when_not_animate() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        assert!(r.is_waiting_on_model());
        r.tick_frame().unwrap();
        assert!(r.out.is_empty(), "非 animate 时 tick 为 no-op");
    }

    /// animate=true 时 Complete 先清波形指示再渲染消息（flush_message 内聚清除）。
    #[test]
    fn complete_clears_indicator_before_rendering() {
        let mut r = HumanRenderer::new(Vec::new(), true);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.tick_frame().unwrap(); // 波形上屏（indicator_on = true）
        r.handle(&ev(M::AgentMessageDelta {
            text: "正文".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageComplete {
            text: "正文".into(),
        }))
        .unwrap();
        let out = String::from_utf8(r.out).unwrap();
        let clear = out.find("\x1b[K").expect("应有清行序列：{out}");
        let msg = out.find("正文").expect("消息应渲染：{out}");
        assert!(clear < msg, "消息应在波形清除之后输出：{out:?}");
    }

    /// 交错时序回归锁：delta → 工具行 → delta → complete，输出保持
    /// 前半 → 工具行 → 后半 的可读顺序（工具行前先把半截消息渲染掉）。
    #[test]
    fn interleaved_delta_tool_delta_keeps_order() {
        let mut r = HumanRenderer::new(Vec::new(), false);
        let ev = |msg: wavecode_protocol::EventMsg| wavecode_protocol::Event {
            id: "s-1".into(),
            msg,
        };
        use wavecode_protocol::EventMsg as M;
        r.handle(&ev(M::TurnStarted {
            turn_id: "t".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta {
            text: "前半".into(),
        }))
        .unwrap();
        r.handle(&ev(M::ToolCallBegin {
            call_id: "1".into(),
            tool: "read_file".into(),
            input: serde_json::json!({}),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageDelta {
            text: "后半".into(),
        }))
        .unwrap();
        r.handle(&ev(M::AgentMessageComplete {
            text: "后半".into(),
        }))
        .unwrap();
        let out = strip(&String::from_utf8(r.out).unwrap());
        let first = out.find("前半").expect("前半应输出：{out}");
        let tool = out.find("▸ read_file").expect("工具行应输出：{out}");
        let second = out.find("后半").expect("后半应输出：{out}");
        assert!(first < tool && tool < second, "时序错乱：{out}");
    }
}
