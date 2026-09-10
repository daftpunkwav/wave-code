//! P1 基础 turn：工具执行 / 只读并行批 / 中断安全 / ToolCtx 注入 / 检索工具。

use super::*;

#[tokio::test]
async fn turn_executes_tool_and_completes() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            StreamEvent::TextDelta {
                text: "好的，创建文件。".into(),
            },
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "write_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"hello.txt","content":"hi"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            },
        ],
        text_then_end("已创建。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let reason = session.run_turn("s-1", "创建 hello.txt", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    assert!(dir.path().join("hello.txt").exists());

    let mut msgs = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        msgs.push(ev.msg);
    }
    let kinds: Vec<String> = msgs
        .iter()
        .map(|m| {
            serde_json::to_value(m).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds.first().unwrap(), "turn_started");
    assert!(kinds.contains(&"tool_call_begin".to_string()));
    assert!(kinds.contains(&"tool_call_end".to_string()));
    assert!(kinds.contains(&"token_count".to_string()));
    assert_eq!(kinds.last().unwrap(), "turn_completed");

    // tool_result 回灌：第二次请求的消息里应含 tool_result 块
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let second = &seen[1];
    let has_tool_result = second.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, wavecode_llm::ContentBlock::ToolResult { .. }))
    });
    assert!(has_tool_result);
}

#[tokio::test]
async fn unknown_tool_returns_error_result_not_crash() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t9".into(),
                name: "no_such_tool".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: "{}".into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("工具不存在，换个方式。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let reason = session.run_turn("s-1", "试一下", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let mut tool_end_ok = None;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::ToolCallEnd { ok, .. } = ev.msg {
            tool_end_ok = Some(ok);
        }
    }
    assert_eq!(tool_end_ok, Some(false));
    // 第二次请求应含 is_error=true 的 ToolResult 回灌
    let seen = model.seen.lock().unwrap();
    let second = &seen[1];
    let has_err_result = second.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(
                b,
                wavecode_llm::ContentBlock::ToolResult { is_error: true, .. }
            )
        })
    });
    assert!(has_err_result);
}

#[tokio::test]
async fn read_only_tools_run_in_batch_results_ordered() {
    // 两个只读调用一批发出：结果顺序须与声明序一致
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "A").unwrap();
    std::fs::write(dir.path().join("b.txt"), "B").unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "read_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"a.txt"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::ToolUseBegin {
                id: "t2".into(),
                name: "read_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"b.txt"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("读完了。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    session.run_turn("s-1", "读两个文件", tx).await.unwrap();
    let seen = model.seen.lock().unwrap();
    let second = &seen[1];
    let results: Vec<&str> = second
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            wavecode_llm::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["A", "B"]);
}

#[tokio::test]
async fn interrupt_in_stream_keeps_tool_pairing() {
    // 流给到一半（半个 tool_use）时中断：中断在流消费循环内被捕获
    //（finish_interrupted 路径），部分结果保留入历史，悬空 tool_use
    // 必须有配对的 is_error ToolResult。
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(AtomicBool::new(false));
    let model = Arc::new(GatedModel {
        script: vec![
            StreamEvent::TextDelta {
                text: "先创建文件".into(),
            },
            // text 块先闭合（真实 SSE 形态），再开 tool_use 块
            StreamEvent::BlockEnd,
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "write_file".into(),
            },
            // 半个 input：之后无 BlockEnd / MessageComplete，流挂起
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"x.txt""#.into(),
            },
        ],
        gate: gate.clone(),
        seen: Mutex::new(vec![]),
    });
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let handle = session.interrupt_handle();
    // join! 同 task 顺序 poll：run_turn 第一轮 poll 即从入口推进到
    // tail 挂起（脚本事件全部立即就绪、send 均不阻塞），故 signal
    // 收到 AgentMessageDelta 时 InputDelta 必已入 cur_tool——此时
    // 置位无竞争。handle 触发 session 中断（T8 驱动模式同款路径），
    // gate 放行 mock 流尾部产出 sentinel。
    let signal = async {
        let mut rx = rx;
        loop {
            let ev = rx.recv().await.unwrap();
            if matches!(&ev.msg, EventMsg::AgentMessageDelta { text } if text == "先创建文件")
            {
                break;
            }
        }
        handle.store(true, Ordering::SeqCst);
        gate.store(true, Ordering::SeqCst);
        rx
    };
    let (reason, mut rx) = tokio::join!(session.run_turn("s-1", "干活", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Interrupted);
    // 中断于流消费循环内：无第二次采样请求
    assert_eq!(model.seen.lock().unwrap().len(), 1);
    // 未实际执行：半个 JSON 不触发 write_file
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());

    let mut saw_interrupted_complete = false;
    let mut saw_message_complete = false;
    while let Ok(ev) = rx.try_recv() {
        match ev.msg {
            EventMsg::TurnCompleted { stop_reason } => {
                saw_interrupted_complete = stop_reason == StopReason::Interrupted;
            }
            EventMsg::AgentMessageComplete { .. } => saw_message_complete = true,
            _ => {}
        }
    }
    assert!(saw_interrupted_complete);
    // 触发点锁定：流内捕获（finish_interrupted）在步骤 4 之前
    // return，不会发出 AgentMessageComplete
    assert!(!saw_message_complete);

    // 历史保留部分结果（同文件测试模块可读私有字段）：
    // assistant 的悬空 tool_use 与 is_error ToolResult 配对
    let assistant = session
        .messages
        .iter()
        .find(|m| m.role == wavecode_llm::Role::Assistant)
        .expect("部分 assistant 消息应入历史");
    assert!(
        assistant
            .content
            .iter()
            .any(|b| matches!(b, wavecode_llm::ContentBlock::ToolUse { id, .. } if id == "t1"))
    );
    // sentinel 未入历史：流内检查点在事件分发前 return
    let full_text: String = assistant
        .content
        .iter()
        .filter_map(|b| match b {
            wavecode_llm::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(full_text, "先创建文件");
    let last = session.messages.last().unwrap();
    assert_eq!(last.role, wavecode_llm::Role::User);
    assert!(last.content.iter().any(
        |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1")
    ));
}

#[tokio::test]
async fn invalid_tool_json_returns_error_result_not_execute() {
    // tool input JSON 解析失败：不实际执行，is_error 结果回灌且配对
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "write_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: "{not json".into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("参数 JSON 坏了，重来。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let reason = session.run_turn("s-1", "写文件", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    // 未实际执行：tempdir 内不产生任何文件
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    let mut tool_end_ok = None;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::ToolCallEnd { ok, .. } = ev.msg {
            tool_end_ok = Some(ok);
        }
    }
    assert_eq!(tool_end_ok, Some(false));
    // 第二次请求：assistant 的 tool_use 与 is_error ToolResult 配对回灌
    let seen = model.seen.lock().unwrap();
    let second = &seen[1];
    let has_tool_use = second.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, wavecode_llm::ContentBlock::ToolUse { id, .. } if id == "t1"))
    });
    assert!(has_tool_use);
    let has_err_pair = second.messages.iter().any(|m| {
        m.content.iter().any(
            |b| matches!(b, wavecode_llm::ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1"),
        )
    });
    assert!(has_err_pair);
}

#[tokio::test]
async fn max_tokens_warns_then_completes() {
    // max_tokens 终态：先 Warning（message 含 max_tokens）再按 Completed 收尾
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![vec![
        StreamEvent::TextDelta {
            text: "写到一半被截断".into(),
        },
        StreamEvent::MessageComplete {
            stop_reason: "max_tokens".into(),
            usage: Usage {
                input_tokens: 7,
                output_tokens: 8192,
            },
        },
    ]];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let reason = session.run_turn("s-1", "写长文", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let mut has_warning = false;
    let mut completed = None;
    while let Ok(ev) = rx.try_recv() {
        match ev.msg {
            EventMsg::Warning { message } => {
                assert!(message.contains("max_tokens"));
                has_warning = true;
            }
            EventMsg::TurnCompleted { stop_reason } => completed = Some(stop_reason),
            _ => {}
        }
    }
    assert!(has_warning);
    assert_eq!(completed, Some(StopReason::Completed));
}

#[tokio::test]
async fn interrupt_in_serial_tools_skips_resample() {
    // 中断落在串行工具执行段：剩余调用以 interrupted 收尾、结果完整
    // 回灌后，循环头检查点直接终结 turn——不发起第二次采样请求。
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![vec![
        StreamEvent::ToolUseBegin {
            id: "t1".into(),
            name: "write_file".into(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: r#"{"path":"a.txt","content":"A"}"#.into(),
        },
        StreamEvent::BlockEnd,
        StreamEvent::ToolUseBegin {
            id: "t2".into(),
            name: "write_file".into(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: r#"{"path":"b.txt","content":"B"}"#.into(),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "tool_use".into(),
            usage: Usage::default(),
        },
    ]];
    let model = Arc::new(MockModel::new(scripts));
    // 容量 1 channel 形成逐滴同步：begin t2 的 send 必须等 signal
    // 取走 begin t1 才能完成——保证 signal 在串行段 i=0 检查点前
    // 完成置位（无 race）。
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let handle = session.interrupt_handle();
    // 收到首个 ToolCallBegin 即置位（此时串行执行段尚未开始）；
    // 之后继续 drain 直到 channel 关闭（run_turn 结束 tx drop），
    // 否则容量 1 下后续 send 会因无人接收而卡住。
    let signal = async {
        let mut rx = rx;
        let mut stored = false;
        let mut saw_interrupted_complete = false;
        while let Some(ev) = rx.recv().await {
            match ev.msg {
                EventMsg::ToolCallBegin { .. } if !stored => {
                    handle.store(true, Ordering::SeqCst);
                    stored = true;
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
        tokio::join!(session.run_turn("s-1", "写两个文件", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Interrupted);
    // 循环头检查点：不发起第二次采样请求
    assert_eq!(model.seen.lock().unwrap().len(), 1);
    // 串行段检查点命中：两个 write_file 均未实际执行
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    assert!(saw_interrupted_complete);

    // 配对完整：末尾 user 消息按声明序含 t1/t2 两条 is_error ToolResult
    let last = session.messages.last().unwrap();
    assert_eq!(last.role, wavecode_llm::Role::User);
    let results: Vec<&str> = last
        .content
        .iter()
        .filter_map(|b| match b {
            wavecode_llm::ContentBlock::ToolResult {
                tool_use_id,
                is_error: true,
                ..
            } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["t1", "t2"]);
}

/// 异步延迟 mock 工具：execute 内 tokio sleep 让出 executor，
/// 用于验证只读批 join_all 的真实并行（编排层不再垫 spawn_blocking——
/// 内置工具已是真 async，并行性由 future 本身的让出语义保证）。
struct AsyncDelayTool {
    tool_name: &'static str,
    delay: std::time::Duration,
}

#[async_trait::async_trait]
impl wavecode_tools::Tool for AsyncDelayTool {
    fn name(&self) -> &str {
        self.tool_name
    }
    fn description(&self) -> &str {
        "read-only async-delay mock tool"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &wavecode_tools::ToolCtx,
    ) -> wavecode_tools::Result<ToolOutput> {
        tokio::time::sleep(self.delay).await;
        Ok(ToolOutput {
            content: self.tool_name.to_owned(),
            is_error: false,
        })
    }
}

#[tokio::test]
async fn read_only_tools_run_in_parallel() {
    // 两个 200ms 延迟工具同批只读：join_all 并发下总耗时 ≈200ms；
    // 串行 await 则 ≥400ms。阈值 350ms 消除调度抖动。
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "slow_tool".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: "{}".into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::ToolUseBegin {
                id: "t2".into(),
                name: "fast_tool".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: "{}".into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("并行读完了。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (mut registry, todos) = builtin_registry();
    registry.register(Arc::new(AsyncDelayTool {
        tool_name: "slow_tool",
        delay: std::time::Duration::from_millis(200),
    }));
    registry.register(Arc::new(AsyncDelayTool {
        tool_name: "fast_tool",
        delay: std::time::Duration::from_millis(200),
    }));
    let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new(
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build(),
    );
    let start = std::time::Instant::now();
    let reason = session.run_turn("s-1", "并行读", tx).await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(reason, StopReason::Completed);
    assert!(
        elapsed < std::time::Duration::from_millis(350),
        "只读工具未真并行：耗时 {elapsed:?} ≥ 350ms（串行应 ≥400ms）"
    );
    // 结果按声明序回灌（配对完整才发起第二轮采样）
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let results: Vec<&str> = seen[1]
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            wavecode_llm::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["slow_tool", "fast_tool"]);
}

/// 探针工具：记录执行时看到的 `ToolCtx.deny_env`，锁定
/// SessionConfig → ToolCtx 的透传接线。
struct CtxProbe {
    seen: Mutex<Option<Vec<String>>>,
}

#[async_trait::async_trait]
impl wavecode_tools::Tool for CtxProbe {
    fn name(&self) -> &str {
        "ctx_probe"
    }
    fn description(&self) -> &str {
        "records ToolCtx.deny_env"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        ctx: &wavecode_tools::ToolCtx,
    ) -> wavecode_tools::Result<ToolOutput> {
        *self.seen.lock().unwrap() = Some(ctx.deny_env.clone());
        Ok(ToolOutput {
            content: "ok".into(),
            is_error: false,
        })
    }
}

/// deny_env 接线（批 C）：SessionConfig.deny_env 须原样透传到
/// 工具执行时的 ToolCtx（shell 的 env 剔除依赖此通道）。
#[tokio::test]
async fn deny_env_flows_to_tool_ctx() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "ctx_probe".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: "{}".into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("探测完毕。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let probe = Arc::new(CtxProbe {
        seen: Mutex::new(None),
    });
    let (mut registry, todos) = builtin_registry();
    registry.register(probe.clone());
    let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new(
        SessionConfig::builder("mock", model, registry, dir.path().to_path_buf())
            .todos(todos)
            .deny_env(vec!["MINIMAX_KEY".to_owned()])
            .sandbox(bypass_sandbox())
            .build(),
    );
    let reason = session.run_turn("s-1", "探测", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(
        *probe.seen.lock().unwrap(),
        Some(vec!["MINIMAX_KEY".to_owned()]),
        "ToolCtx.deny_env 应透传 SessionConfig 的名单"
    );
}

#[tokio::test]
async fn turn_uses_grep_and_glob_via_registry() {
    // P1 新工具接线验证：模型经 Registry 调 grep + glob（同为只读，
    // 一个并行批），结果正确回灌；请求侧 ToolSpec 清单含两个新工具。
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.path().join("src/b.txt"), "hello\n").unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "grep".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"pattern":"fn main"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::ToolUseBegin {
                id: "t2".into(),
                name: "glob".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"pattern":"src/**/*.rs"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("检索完成。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let reason = session.run_turn("s-1", "找入口函数", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    // 请求侧 specs 含新工具
    let names: Vec<&str> = seen[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"grep") && names.contains(&"glob"));
    // 结果按声明序回灌：grep 带行号匹配，glob 列相对路径
    let results: Vec<&str> = seen[1]
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            wavecode_llm::ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].contains("src/a.rs:1:fn main() {}"));
    assert!(results[0].contains("[1 matches in 1 files]"));
    assert_eq!(results[1], "src/a.rs");
}
