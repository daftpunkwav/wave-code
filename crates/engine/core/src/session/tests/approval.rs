//! P2 审批：allow/deny golden / plan 模式 / 审批等待中断与超时 / 工具轮熔断。

use super::*;

/// P2 golden：审批放行——ApprovalRequested 事件 → ExecApproval 回填
/// AllowOnce → 工具实际执行成功，非 is_error 结果回灌模型。
#[tokio::test]
async fn approval_allow_executes_tool_golden() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        write_file_script("hello.txt", "hi"),
        text_then_end("已创建。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .build()
    });
    let gate = session.approval_handle();
    // 收到 ApprovalRequested 即回填放行决策（模拟前端 / actor 路由）；
    // 继续 drain 到通道关闭，顺带记录 ToolCallEnd。
    let signal = async {
        let mut rx = rx;
        let mut requested = None;
        let mut saw_ok_end = false;
        while let Some(ev) = rx.recv().await {
            match ev.msg {
                EventMsg::ApprovalRequested {
                    call_id,
                    kind,
                    detail,
                } => {
                    requested = Some((call_id.clone(), kind, detail));
                    gate.decide(call_id, wavecode_protocol::ApprovalDecision::AllowOnce);
                }
                EventMsg::ToolCallEnd { call_id, ok, .. } if call_id == "t1" => {
                    saw_ok_end = ok;
                }
                _ => {}
            }
        }
        (requested, saw_ok_end)
    };
    let (reason, (requested, saw_ok_end)) =
        tokio::join!(session.run_turn("s-1", "创建 hello.txt", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Completed);
    // 审批事件：call_id 关联、kind=Write、detail 含工具与路径
    let (call_id, kind, detail) = requested.expect("应发出 ApprovalRequested");
    assert_eq!(call_id, "t1");
    assert_eq!(kind, wavecode_protocol::ApprovalKind::Write);
    assert!(detail.contains("write_file") && detail.contains("hello.txt"));
    // 放行后实际执行
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hi"
    );
    // 第二轮请求：非 is_error 的 ToolResult 回灌（配对 t1）
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let ok_result = seen[1].messages.iter().any(|m| {
        m.content.iter().any(
            |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "t1"),
        )
    });
    assert!(ok_result, "放行结果应回灌: {:?}", seen[1].messages);
    // 事件流含 ToolCallEnd ok=true
    assert!(saw_ok_end);
}

/// P2 golden：审批拒绝——工具不执行，is_error 结果回灌且拒绝原因
/// 出现在后续请求消息里。
#[tokio::test]
async fn approval_deny_skips_execution_and_feeds_reason() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        write_file_script("nope.txt", "x"),
        text_then_end("明白了，不写。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .build()
    });
    let gate = session.approval_handle();
    let signal = async {
        let mut rx = rx;
        let mut saw_fail_end = false;
        while let Some(ev) = rx.recv().await {
            match ev.msg {
                EventMsg::ApprovalRequested { call_id, .. } => {
                    gate.decide(
                        call_id,
                        wavecode_protocol::ApprovalDecision::Deny {
                            reason: "目录受保护，不要写".into(),
                        },
                    );
                }
                EventMsg::ToolCallEnd { ok, .. } => saw_fail_end = !ok,
                _ => {}
            }
        }
        saw_fail_end
    };
    let (reason, saw_fail_end) = tokio::join!(session.run_turn("s-1", "创建 nope.txt", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Completed);
    // 未实际执行
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    assert!(saw_fail_end, "ToolCallEnd 应 ok=false");
    // 拒绝原因回灌模型：第二轮请求含 is_error ToolResult 且带原因原文
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let fed = seen[1].messages.iter().any(|m| {
        m.content.iter().any(
            |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: true, content } if tool_use_id == "t1" && content.contains("目录受保护，不要写")),
        )
    });
    assert!(fed, "拒绝原因应回灌: {:?}", seen[1].messages);
}

/// P2：plan 模式拦截——写工具被 Deny（不发 ApprovalRequested、不执行），
/// 拒绝原因回灌模型。
#[tokio::test]
async fn plan_mode_denies_write_tool_without_approval_request() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        write_file_script("plan.txt", "x"),
        text_then_end("plan 模式下只规划。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(Sandbox::without_rules(PermissionMode::Plan))
            .build()
    });
    let reason = session.run_turn("s-1", "创建 plan.txt", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    // 不执行、不发审批请求
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    let mut saw_approval_request = false;
    let mut saw_fail_end = false;
    while let Ok(ev) = rx.try_recv() {
        match ev.msg {
            EventMsg::ApprovalRequested { .. } => saw_approval_request = true,
            EventMsg::ToolCallEnd { ok, .. } => saw_fail_end = !ok,
            _ => {}
        }
    }
    assert!(!saw_approval_request, "plan 模式拦截不应发审批请求");
    assert!(saw_fail_end, "ToolCallEnd 应 ok=false");
    // 拒绝原因（plan mode）回灌模型
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let fed = seen[1].messages.iter().any(|m| {
        m.content.iter().any(
            |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { is_error: true, content, .. } if content.contains("plan mode")),
        )
    });
    assert!(fed, "plan 拦截原因应回灌: {:?}", seen[1].messages);
}

/// P2：审批等待中中断——park 在 AwaitApproval 时 interrupt 生效，
/// 悬空 tool_use 以 interrupted 结果配对收尾，不发起第二次采样。
///（驱动方只置中断标志、不戳审批槽：走 APPROVAL_POLL_INTERVAL 兜底路径。）
#[tokio::test]
async fn interrupt_during_approval_wait_completes_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![write_file_script("x.txt", "x")];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .build()
    });
    let interrupt = session.interrupt_handle();
    let signal = async {
        let mut rx = rx;
        let mut saw_interrupted_complete = false;
        while let Some(ev) = rx.recv().await {
            match ev.msg {
                // 审批请求出现即中断（不回填决策：等待中的中断路径）
                EventMsg::ApprovalRequested { .. } => {
                    interrupt.store(true, Ordering::SeqCst);
                }
                EventMsg::TurnCompleted { stop_reason } => {
                    saw_interrupted_complete = stop_reason == StopReason::Interrupted;
                }
                _ => {}
            }
        }
        saw_interrupted_complete
    };
    let (reason, saw_interrupted_complete) =
        tokio::join!(session.run_turn("s-1", "写文件", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Interrupted);
    assert!(saw_interrupted_complete);
    // 不执行、不再采样
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    assert_eq!(model.seen.lock().unwrap().len(), 1);
    // 配对完整：末尾 user 消息含 t1 的 is_error ToolResult
    let last = session.messages.last().unwrap();
    assert_eq!(last.role, wavecode_llm::Role::User);
    assert!(last.content.iter().any(
        |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1")
    ));
}

/// 中断路径的权威占用结转：审批等待中被中断的 turn 已完成一轮采样
///（usage 权威值 20+3），中断收尾必须把真实占用写入 `usage_carry` 并发
/// TokenCount——不得丢弃后让下一 turn 用过期 carry / 估算低估占用。
#[tokio::test]
async fn interrupted_turn_carries_usage_to_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let script = vec![
        StreamEvent::ToolUseBegin {
            id: "t1".into(),
            name: "write_file".into(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: r#"{"path":"x.txt","content":"x"}"#.into(),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "tool_use".into(),
            usage: Usage {
                input_tokens: 20,
                output_tokens: 3,
            },
        },
    ];
    let model = Arc::new(MockModel::new(vec![script]));
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .build()
    });
    let interrupt = session.interrupt_handle();
    let signal = async {
        let mut rx = rx;
        let mut saw_interrupted_complete = false;
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            match ev.msg {
                // 审批请求出现即中断（不回填决策：等待中的中断路径）
                EventMsg::ApprovalRequested { .. } => {
                    interrupt.store(true, Ordering::SeqCst);
                }
                EventMsg::TurnCompleted { stop_reason } => {
                    saw_interrupted_complete = stop_reason == StopReason::Interrupted;
                }
                other => events.push(other),
            }
        }
        (saw_interrupted_complete, events)
    };
    let (reason, (saw_interrupted_complete, events)) =
        tokio::join!(session.run_turn("s-1", "写文件", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Interrupted);
    assert!(saw_interrupted_complete);
    // 真实占用已结转，供下一 turn 的 PreTurn 预算检查使用
    assert_eq!(session.usage_carry, Some(23));
    // TokenCount 事件对前端可见（与正常完成路径同形态）
    assert!(
        events
            .iter()
            .any(|m| matches!(m, EventMsg::TokenCount { used: 23, .. }))
    );
}

/// P2：审批等待超时——无人回填决策（前端崩溃 / 弹窗丢失）时按拒绝收尾
///（is_error 回灌、工具不执行），turn 以 Completed 结束而非永久 park；
/// Warning 事件对前端可见。
#[tokio::test]
async fn approval_wait_timeout_rejects_and_completes() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![write_file_script("x.txt", "x"), text_then_end("继续。")];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .approval_wait_timeout(std::time::Duration::from_millis(50))
            .build()
    });
    let reason = session.run_turn("s-1", "写文件", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    // 工具未执行
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    // 超时后 turn 继续：发生了第二次采样
    assert_eq!(model.seen.lock().unwrap().len(), 2);
    // 超时拒绝以 is_error 回灌
    let fed = model.seen.lock().unwrap()[1].messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b,
                wavecode_llm::ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.contains("timed out"))
        })
    });
    assert!(
        fed,
        "超时拒绝应回灌: {:?}",
        model.seen.lock().unwrap()[1].messages
    );
    // Warning 事件可见
    let mut saw_warning = false;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::Warning { message } = ev.msg
            && message.contains("timed out")
        {
            saw_warning = true;
        }
    }
    assert!(saw_warning);
}

/// 工具轮数上限（失控熔断）：模型无限 tool_use 时达 `max_tool_rounds`
/// 即不再采样，turn 以 Completed 收尾；历史配对完整，Warning 可见。
#[tokio::test]
async fn tool_round_cap_stops_runaway_tool_loop() {
    struct LoopModel {
        calls: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl ChatModel for LoopModel {
        async fn stream(
            &self,
            _req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            *self.calls.lock().unwrap() += 1;
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamEvent::ToolUseBegin {
                    id: "t1".into(),
                    name: "read_file".into(),
                }),
                Ok(StreamEvent::ToolUseInputDelta {
                    partial_json: r#"{"path":"nope.txt"}"#.into(),
                }),
                Ok(StreamEvent::BlockEnd),
                Ok(StreamEvent::MessageComplete {
                    stop_reason: "tool_use".into(),
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                }),
            ])))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(LoopModel {
        calls: Mutex::new(0),
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .max_tool_rounds(3)
            .build()
    });
    let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(*model.calls.lock().unwrap(), 3, "达上限后不再采样");
    // 历史配对完整（每轮 tool_use 均有结果回灌）
    assert_eq!(
        wavecode_context::find_pairing_violations(&session.messages),
        Vec::<String>::new()
    );
    let mut saw_cap_warning = false;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::Warning { message } = ev.msg
            && message.contains("tool round limit")
        {
            saw_cap_warning = true;
        }
    }
    assert!(saw_cap_warning);
}

/// 采样错误出口的占用结转：`stream()` 返回非 prompt_too_long 类错误
///（overloaded / 鉴权 / stall 超时等）时，本 turn 已完成轮次的真实占用
/// 同样结转并发 TokenCount——不被通用错误路径丢弃。
#[tokio::test]
async fn sampling_error_carries_usage_from_completed_rounds() {
    struct FailOnSecondModel {
        calls: Mutex<u32>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for FailOnSecondModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>,
            >,
        > {
            self.seen.lock().unwrap().push(req);
            let n = *self.calls.lock().unwrap();
            *self.calls.lock().unwrap() += 1;
            if n == 0 {
                // 首轮正常：tool_use + 权威 usage(10,2)
                Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::ToolUseBegin {
                        id: "t1".into(),
                        name: "read_file".into(),
                    }),
                    Ok(StreamEvent::ToolUseInputDelta {
                        partial_json: r#"{"path":"a.txt"}"#.into(),
                    }),
                    Ok(StreamEvent::BlockEnd),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: "tool_use".into(),
                        usage: Usage {
                            input_tokens: 10,
                            output_tokens: 2,
                        },
                    }),
                ])))
            } else {
                // 第二轮采样直接失败（非 prompt_too_long，不触发 compact）
                Err(LlmError::Api {
                    kind: "overloaded_error".into(),
                    message: "overloaded".into(),
                })
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(FailOnSecondModel {
        calls: Mutex::new(0),
        seen: Mutex::new(vec![]),
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let result = session.run_turn("s-1", "干活", tx).await;
    assert!(result.is_err(), "采样错误应以 Err 收尾");
    // 首轮已消耗的权威占用(10+2)不因通用错误出口丢弃
    assert_eq!(session.usage_carry, Some(12));
    let mut saw_token_count = false;
    let mut saw_error = false;
    while let Ok(ev) = rx.try_recv() {
        match ev.msg {
            EventMsg::TokenCount { used: 12, .. } => saw_token_count = true,
            EventMsg::Error { .. } => saw_error = true,
            _ => {}
        }
    }
    assert!(saw_token_count, "结转应发 TokenCount");
    assert!(saw_error, "错误事件可见");
}

/// `max_tool_rounds(0)` 回归：首轮循环头即熔断、未发生任何采样——turn
/// 以 Completed 收尾且不 panic（终态不假设"必有采样轮"）；模型零调用、
/// 不发 TokenCount（无权威占用可结转，下一 turn 沿用既有 carry / 估算）。
#[tokio::test]
async fn tool_round_cap_zero_completes_without_sampling() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(MockModel::new(vec![]));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .max_tool_rounds(0)
            .build()
    });
    let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(*model.calls.lock().unwrap(), 0, "上限 0 = 不发起采样");
    assert_eq!(
        wavecode_context::find_pairing_violations(&session.messages),
        Vec::<String>::new()
    );
    assert_eq!(session.usage_carry, None, "无采样轮则不结转占用");
    let mut saw_cap_warning = false;
    let mut saw_token_count = false;
    while let Ok(ev) = rx.try_recv() {
        match ev.msg {
            EventMsg::Warning { message } if message.contains("tool round limit") => {
                saw_cap_warning = true;
            }
            EventMsg::TokenCount { .. } => saw_token_count = true,
            _ => {}
        }
    }
    assert!(saw_cap_warning);
    assert!(!saw_token_count, "无采样轮不应发 TokenCount");
}

// ------------------------------------------------------------------
