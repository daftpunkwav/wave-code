//! P3 上下文管线：三级阈值 / 自动与阻塞压缩 / reactive compact / max_tokens 续写 / /compact。

use super::*;

// P3：上下文管线（PreTurn 三级阈值 / reactive compact / 续写 / /compact）
// ------------------------------------------------------------------

/// P3 配置：window=100_000，margin 200/100/10 → 三线 99800/99900/99990；
/// 估算路径（~2k 开销定额）远低于警告线，threshold 测试经 usage_carry
/// 种子精确控制水位；keep_recent=2 便于断言压缩后形态。
fn p3_session(model: Arc<CompactAwareMock>) -> Session {
    Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder(
            "mock",
            model,
            registry,
            // tempdir 转持久路径放弃自动删除（与 app-server 测试同例）。
            tempfile::tempdir().unwrap().keep(),
        )
        .todos(todos)
        .context_window(100_000)
        .sandbox(bypass_sandbox())
        .context(ContextConfig {
            thresholds: wavecode_context::Thresholds {
                warning_margin: 200,
                auto_compact_margin: 100,
                blocking_margin: 10,
            },
            keep_recent: 2,
            summary_max_tokens: 500,
            estimate_chars_per_token: 4,
        })
        .build()
    })
}

/// 阈值边界：used=99850 过警告线（99800）未及自动线（99900）——发一次
/// Warning（"context near limit"），不压缩；两轮循环头检查只发一次。
#[tokio::test]
async fn preturn_warning_line_emits_warning_once_no_compact() {
    let model = p3_mock(vec![
        Some(vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "read_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"a.txt"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage {
                    input_tokens: 99850,
                    output_tokens: 5,
                },
            },
        ]),
        Some(text_then_end("读完了")),
    ]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = p3_session(model.clone());
    // 上一 turn 结转的权威占用：首次 PreTurn 检查即过警告线
    session.usage_carry = Some(99850);
    let reason = session.run_turn("s-1", "读文件", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let events = collect_events(&mut rx);
    let warnings: Vec<&EventMsg> = events
        .iter()
        .filter(|m| matches!(m, EventMsg::Warning { .. }))
        .collect();
    assert_eq!(warnings.len(), 1, "警告每 turn 至多一次: {events:?}");
    let EventMsg::Warning { message } = warnings[0] else {
        unreachable!()
    };
    assert!(message.contains("context near limit"));
    assert!(
        !events
            .iter()
            .any(|m| matches!(m, EventMsg::CompactStarted { .. })),
        "警告线不得触发压缩"
    );
}

/// 自动压缩线：used=99950 过自动线（99900）——PreTurn 触发压缩，
/// CompactStarted{Auto} → CompactCompleted，随后采样请求的历史
/// 首条为摘要消息；压缩后配对完整。
#[tokio::test]
async fn preturn_auto_line_compacts_before_sampling() {
    let model = p3_mock(vec![Some(text_then_end("好的"))]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = p3_session(model.clone());
    session.usage_carry = Some(99950);
    let reason = session.run_turn("s-1", "继续干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);

    let events = collect_events(&mut rx);
    let started = events.iter().find_map(|m| match m {
        EventMsg::CompactStarted { trigger } => Some(*trigger),
        _ => None,
    });
    assert_eq!(
        started,
        Some(wavecode_protocol::CompactTrigger::Auto),
        "应以 Auto 触发压缩: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|m| matches!(m, EventMsg::CompactCompleted { .. }))
    );

    // 请求序：摘要（tools 空）→ 采样（历史首条为摘要消息）
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].tools.is_empty(), "首个请求应为摘要调用");
    let first = &seen[1].messages[0];
    assert!(
        matches!(&first.content[0], wavecode_llm::ContentBlock::Text { text } if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX)),
        "采样请求历史首条应为摘要消息: {:?}",
        seen[1].messages
    );
    // 摘要消息逐项含五要素（验收锚点：信息保留率）
    let wavecode_llm::ContentBlock::Text { text } = &first.content[0] else {
        unreachable!()
    };
    for element in ["目标", "进展", "关键决策", "文件清单", "待办"] {
        assert!(text.contains(element), "摘要缺要素「{element}」");
    }
    assert_eq!(
        wavecode_context::find_pairing_violations(&session.messages),
        Vec::<String>::new(),
        "压缩后历史配对须完整"
    );
}

/// 阻塞线：used=99995 过阻塞线（99990）——强制先压缩（Blocking 触发）再采样。
#[tokio::test]
async fn preturn_blocking_line_forces_compact_with_blocking_trigger() {
    let model = p3_mock(vec![Some(text_then_end("好的"))]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = p3_session(model.clone());
    session.usage_carry = Some(99995);
    let reason = session.run_turn("s-1", "继续干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let events = collect_events(&mut rx);
    let started = events.iter().find_map(|m| match m {
        EventMsg::CompactStarted { trigger } => Some(*trigger),
        _ => None,
    });
    assert_eq!(
        started,
        Some(wavecode_protocol::CompactTrigger::Blocking),
        "阻塞线应以 Blocking 触发: {events:?}"
    );
    // 压缩先于采样完成
    let seen = model.seen.lock().unwrap();
    assert!(seen[0].tools.is_empty() && !seen[1].tools.is_empty());
}

/// reactive compact：首次采样 prompt_too_long → 压缩（Reactive）→
/// 以压缩历史重试成功，turn 正常完成。
#[tokio::test]
async fn reactive_compact_recovers_from_prompt_too_long() {
    let model = p3_mock(vec![None, Some(text_then_end("压缩后重试成功"))]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = p3_session(model.clone());
    let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);

    let events = collect_events(&mut rx);
    let started = events.iter().find_map(|m| match m {
        EventMsg::CompactStarted { trigger } => Some(*trigger),
        _ => None,
    });
    assert_eq!(
        started,
        Some(wavecode_protocol::CompactTrigger::Reactive),
        "prompt_too_long 应以 Reactive 触发: {events:?}"
    );

    // 请求序：采样（失败）→ 摘要 → 采样（压缩历史重试）
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(!seen[0].tools.is_empty() && seen[1].tools.is_empty());
    let retry_first = &seen[2].messages[0];
    assert!(
        matches!(&retry_first.content[0], wavecode_llm::ContentBlock::Text { text } if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX)),
        "重试应以压缩历史发起"
    );
}

/// reactive compact 熔断：连续 3 次 prompt_too_long → 熔断上报
///（Error + TurnCompleted{Error} + run_turn 返回 Err），期间压缩 2 次。
#[tokio::test]
async fn reactive_compact_circuit_breaks_after_three() {
    let model = p3_mock(vec![None]); // 队列复用末项：永远 prompt_too_long
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = p3_session(model.clone());
    let result = session.run_turn("s-1", "干活", tx).await;
    assert!(result.is_err(), "熔断后应上抛错误");

    let events = collect_events(&mut rx);
    let compacts = events
        .iter()
        .filter(|m| {
            matches!(m, EventMsg::CompactStarted { trigger } if *trigger == wavecode_protocol::CompactTrigger::Reactive)
        })
        .count();
    assert_eq!(compacts, 2, "3 次采样失败之间压缩 2 次: {events:?}");
    let error = events.iter().find_map(|m| match m {
        EventMsg::Error { message, .. } => Some(message.clone()),
        _ => None,
    });
    assert!(error.unwrap().contains("熔断"), "熔断须上报: {events:?}");
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::TurnCompleted {
                stop_reason: StopReason::Error
            }
        )),
        "熔断 turn 应以 Error 收尾: {events:?}"
    );
    // 采样 3 次 + 摘要 2 次
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 5);
    assert_eq!(
        seen.iter().filter(|r| !r.tools.is_empty()).count(),
        3,
        "采样恰 3 次（熔断阈值）"
    );
}

/// max_output_tokens 续写：max_tokens 截断后以续写提示继续，
/// 第二次截断再续一次，第三次正常结束——续写请求恰 2 次。
#[tokio::test]
async fn max_tokens_continues_up_to_twice() {
    let truncated = |text: &str| {
        Some(vec![
            StreamEvent::TextDelta { text: text.into() },
            StreamEvent::MessageComplete {
                stop_reason: "max_tokens".into(),
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 8192,
                },
            },
        ])
    };
    let model = p3_mock(vec![
        truncated("前半"),
        truncated("中段"),
        Some(text_then_end("收尾")),
    ]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = p3_session(model.clone());
    let reason = session.run_turn("s-1", "写长文", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);

    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "初始 + 2 次续写");
    // 续写请求的历史末尾是续写提示（user 文本）
    for req in &seen[1..] {
        let last = req.messages.last().unwrap();
        assert!(
            matches!(&last.content[0], wavecode_llm::ContentBlock::Text { text } if text == CONTINUATION_PROMPT),
            "续写请求应以续写提示结尾: {:?}",
            req.messages
        );
    }
    // 两次续写警告，无"放弃"警告
    let events = collect_events(&mut rx);
    let warnings: Vec<String> = events
        .iter()
        .filter_map(|m| match m {
            EventMsg::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|w| w.contains("continuing")));
}

/// 续写熔断：连续 max_tokens 达上限后放弃——发 "max_tokens reached"
/// 警告并按 Completed 收尾，采样恰 3 次（初始 + 2 续写）。
#[tokio::test]
async fn max_tokens_gives_up_after_two_continuations() {
    let model = p3_mock(vec![Some(vec![
        StreamEvent::TextDelta {
            text: "永远写不完".into(),
        },
        StreamEvent::MessageComplete {
            stop_reason: "max_tokens".into(),
            usage: Usage {
                input_tokens: 7,
                output_tokens: 8192,
            },
        },
    ])]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = p3_session(model.clone());
    let reason = session.run_turn("s-1", "写长文", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    assert_eq!(model.seen.lock().unwrap().len(), 3, "初始 + 2 次续写后放弃");
    let events = collect_events(&mut rx);
    let last_warning = events.iter().rev().find_map(|m| match m {
        EventMsg::Warning { message } => Some(message.clone()),
        _ => None,
    });
    assert!(
        last_warning.unwrap().contains("max_tokens reached"),
        "放弃时须警告: {events:?}"
    );
}

/// `/compact`（Session::compact）：无论阈值立即压缩，CompactStarted
/// {Manual} → CompactCompleted{summary_tokens}，历史首条为摘要消息。
#[tokio::test]
async fn manual_compact_via_session_method() {
    let model = p3_mock(vec![Some(text_then_end("unused"))]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = p3_session(model.clone());
    let summary_tokens = session.compact("s-manual", tx).await.unwrap();
    assert!(summary_tokens > 0);

    let events = collect_events(&mut rx);
    let started = events.iter().find_map(|m| match m {
        EventMsg::CompactStarted { trigger } => Some(*trigger),
        _ => None,
    });
    assert_eq!(started, Some(wavecode_protocol::CompactTrigger::Manual));
    assert!(events.iter().any(
        |m| matches!(m, EventMsg::CompactCompleted { summary_tokens: t } if *t == summary_tokens)
    ));
    let first = &session.messages[0];
    assert!(
        matches!(&first.content[0], wavecode_llm::ContentBlock::Text { text } if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX))
    );
    assert_eq!(
        wavecode_context::find_pairing_violations(&session.messages),
        Vec::<String>::new()
    );
}
