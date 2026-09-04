//! P4 规划系统：todo 生命周期 / stop steering 上限。

use super::*;

// —— P4 规划系统（todo_write / 清单注入 / stop steering）——

/// todo_write 调用脚本（一轮：声明 + 输入 + 终态 tool_use）。
fn todo_write_script(id: &str, todos_json: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseBegin {
            id: id.into(),
            name: "todo_write".into(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: todos_json.into(),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "tool_use".into(),
            usage: Usage::default(),
        },
    ]
}

/// P4 会话构造：bypass 沙箱（todo_write 本就各模式免审批），返回
/// session 与清单句柄（测试经句柄断言共享状态迁移）。
fn p4_session(
    model: Arc<MockModel>,
    dir: &std::path::Path,
) -> (Session, wavecode_tools::TodoStore) {
    let (registry, todos) = builtin_registry();
    (
        Session::new(
            SessionConfig::builder("mock", model, registry, dir.to_path_buf())
                .todos(todos.clone())
                .sandbox(bypass_sandbox())
                .build(),
        ),
        todos,
    )
}

/// P4 验收：mock 长任务 golden——模型先 todo_write 建立清单 → 逐步执行
/// 并更新状态（事件流可观测状态迁移）→ 全部完成 → 收工（无 steering）。
#[tokio::test]
async fn todo_golden_task_lifecycle_observable() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        todo_write_script(
            "t1",
            r#"{"todos":[
                {"content":"设计","status":"in_progress"},
                {"content":"实现","status":"pending"},
                {"content":"测试","status":"pending"}]}"#,
        ),
        todo_write_script(
            "t2",
            r#"{"todos":[
                {"content":"设计","status":"completed"},
                {"content":"实现","status":"in_progress"},
                {"content":"测试","status":"pending"}]}"#,
        ),
        todo_write_script(
            "t3",
            r#"{"todos":[
                {"content":"设计","status":"completed"},
                {"content":"实现","status":"completed"},
                {"content":"测试","status":"completed"}]}"#,
        ),
        text_then_end("全部完成。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let (mut session, todos) = p4_session(model.clone(), dir.path());
    let reason = session.run_turn("s-1", "完成三步任务", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    // 清单终态：全部 completed（共享句柄断言）。
    assert_eq!(todos.unfinished(), (0, 0));
    assert_eq!(todos.snapshot().len(), 3);

    // 事件流可观测状态迁移：3 次 todo_write 的 begin/end。
    let events = collect_events(&mut rx);
    let todo_begins = events
        .iter()
        .filter(|m| matches!(m, EventMsg::ToolCallBegin { tool, .. } if tool == "todo_write"))
        .count();
    assert_eq!(todo_begins, 3, "事件流应含 3 次 todo_write: {events:?}");
    // 全部完成后收工：无 steering 提醒。
    assert!(
        !events
            .iter()
            .any(|m| matches!(m, EventMsg::Warning { message } if message.contains("nudging"))),
        "清单全部完成不得 steering: {events:?}"
    );

    // 清单注入：第 2/3 轮请求的 system 尾部反映当轮清单快照。
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert!(seen[0].system.contains("(not a git repository)"));
    assert!(
        !seen[0].system.contains("<system-reminder>"),
        "首轮清单为空不注入"
    );
    assert!(
        seen[1].system.contains("1. [in_progress] 设计"),
        "第 2 轮注入首轮清单: {}",
        seen[1].system
    );
    assert!(
        seen[2].system.contains("1. [completed] 设计")
            && seen[2].system.contains("2. [in_progress] 实现"),
        "第 3 轮注入状态迁移后清单: {}",
        seen[2].system
    );
    // 前缀稳定：静态层恒为前缀。
    for req in seen.iter() {
        assert!(req.system.starts_with(crate::prompt::STATIC_LAYER));
    }
}

/// P4 验收：stop steering——清单有未完成项时模型想收工 → turn 继续并
/// 注入提醒；连续 3 次（MAX_TODO_STEERINGS）后放行。
#[tokio::test]
async fn steering_continues_turn_until_limit() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        todo_write_script(
            "t1",
            r#"{"todos":[{"content":"未完成的活","status":"pending"}]}"#,
        ),
        // 之后每轮都想收工：连续 steering 3 次后放行。
        text_then_end("做完了。"),
        text_then_end("做完了。"),
        text_then_end("做完了。"),
        text_then_end("做完了。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let (mut session, _todos) = p4_session(model.clone(), dir.path());
    let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);

    // 轮次：1（todo）+ 1（首次收工）+ 3（steering）= 5 次采样。
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 5, "采样次数: 1 todo + 1 stop + 3 steering");
    // 第 5 次请求的历史里累计 3 条 steering 提醒。
    let nudges = seen[4]
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, wavecode_llm::ContentBlock::Text { text } if text.contains("unfinished items")))
        .count();
    assert_eq!(nudges, 3, "steering 提醒累计 3 条");
    // 事件流：3 次 steering Warning。
    let events = collect_events(&mut rx);
    let warnings = events
        .iter()
        .filter(|m| matches!(m, EventMsg::Warning { message } if message.contains("nudging")))
        .count();
    assert_eq!(warnings, 3, "steering Warning 3 次: {events:?}");
    // 前缀稳定：清单建立后未再变化，第 2 轮起各轮 system 字节相等。
    for w in seen[1..].windows(2) {
        assert_eq!(w[0].system, w[1].system, "清单不变时 system 须字节稳定");
    }
}

/// P4 验收：steering 后模型把清单更新为全部 completed → 不再 steering，
/// 正常收工（连续计数之外的解除路径）。
#[tokio::test]
async fn steering_stops_once_list_completed() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        todo_write_script(
            "t1",
            r#"{"todos":[{"content":"活","status":"in_progress"}]}"#,
        ),
        text_then_end("做完了。"), // 想收工 → steering #1
        todo_write_script("t2", r#"{"todos":[{"content":"活","status":"completed"}]}"#),
        text_then_end("全部完成。"), // 清单已清 → 正常收工
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let (mut session, _todos) = p4_session(model.clone(), dir.path());
    let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "steering 1 次后清单完成即收工");
    let events = collect_events(&mut rx);
    let warnings = events
        .iter()
        .filter(|m| matches!(m, EventMsg::Warning { message } if message.contains("nudging")))
        .count();
    assert_eq!(warnings, 1, "仅 1 次 steering: {events:?}");
}
