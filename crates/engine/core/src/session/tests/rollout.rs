//! P10 会话持久化：rollout 写入 / replay 恢复 / 断点续跑 / 崩溃恢复。

use super::*;

// ------------------------------------------------------------------
// P10：会话持久化（rollout 写入 / replay 恢复 / 断点续跑 / 崩溃恢复；
// 长程硬化见 stress.rs）
// ------------------------------------------------------------------

/// P10 会话构造：bypass 沙箱 + 可挂 rollout（cwd 独立 tempdir，
/// write_file 落点互不影响）。
fn p10_session(
    model: Arc<dyn ChatModel>,
    rollout: Option<crate::rollout::RolloutConfig>,
) -> Session {
    Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model, registry, tempfile::tempdir().unwrap().keep())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .rollout(rollout)
            .build()
    })
}

/// P10 验收锚点：rollout 写入 → replay 恢复 → 断点续跑。
#[tokio::test]
async fn rollout_records_turn_and_resume_continues() {
    let dir = tempfile::tempdir().unwrap();
    // —— 会话 A：一轮含工具调用的 turn，全程落 rollout ——
    let model_a = Arc::new(MockModel::new(vec![
        write_file_script("hello.txt", "hi"),
        text_then_end("已创建。"),
    ]));
    let (tx, _rx) = mpsc::channel::<Event>(64);
    let mut session_a = p10_session(model_a, p10_rollout(dir.path(), "thread-1"));
    let reason = session_a
        .run_turn("s-1", "创建 hello.txt", tx)
        .await
        .unwrap();
    assert_eq!(reason, StopReason::Completed);
    let history_a = session_a.messages.clone();

    // rollout 文件：4 条消息记录（user / assistant tool_use /
    // tool_result user / assistant 文本），seq 从 1 连续递增。
    let path = dir.path().join("threads/thread-1.jsonl");
    let load = crate::rollout::load_rollout(&path).unwrap();
    assert!(load.warnings.is_empty(), "{:?}", load.warnings);
    assert_eq!(load.records.len(), 4);
    assert_eq!(p10_seqs(&load), vec![1, 2, 3, 4]);
    assert!(
        load.records
            .iter()
            .all(|r| matches!(r, crate::rollout::RolloutRecord::Message { .. }))
    );
    drop(session_a); // 丢弃 Session（模拟进程退出）

    // —— 会话 B：同 rollout 构造即 replay 恢复，历史与 A 一致 ——
    let model_b = Arc::new(MockModel::new(vec![text_then_end("续跑完成。")]));
    let (tx, _rx) = mpsc::channel::<Event>(64);
    let mut session_b = p10_session(model_b.clone(), p10_rollout(dir.path(), "thread-1"));
    assert_eq!(
        *session_b.messages, *history_a,
        "replay 恢复的历史应与退出前一致"
    );
    assert_eq!(
        wavecode_context::find_pairing_violations(&session_b.messages),
        Vec::<String>::new()
    );

    // —— 断点续跑：再跑一轮 turn，采样请求的历史 = 恢复历史 + 新输入 ——
    let reason = session_b.run_turn("s-2", "继续", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let seen = model_b.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].messages.len(), history_a.len() + 1);
    drop(seen);

    // rollout 续写：seq 接续不重号；第三次 replay 与会话 B 全量一致。
    let load = crate::rollout::load_rollout(&path).unwrap();
    assert_eq!(load.records.len(), 6);
    assert_eq!(p10_seqs(&load), vec![1, 2, 3, 4, 5, 6]);
    let session_c = p10_session(
        Arc::new(MockModel::new(vec![])),
        p10_rollout(dir.path(), "thread-1"),
    );
    assert_eq!(*session_c.messages, *session_b.messages);
}

/// P10 验收锚点：压缩记录落盘（承载压缩时点新历史）→ 压缩后 resume
/// 恢复——压缩点之后原文 + 摘要即新历史（SPEC §16 / §5.2）。
#[tokio::test]
async fn rollout_compaction_record_and_resume_after_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let model = p3_mock(vec![Some(text_then_end("好的"))]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session_a = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model, registry, dir.path().to_path_buf())
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
            .rollout(p10_rollout(dir.path(), "t-compact"))
            .build()
    });
    // 上一 turn 结转的权威占用：首次 PreTurn 检查即过自动压缩线。
    session_a.usage_carry = Some(99_950);
    let reason = session_a.run_turn("s-1", "继续干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let events = collect_events(&mut rx);
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::CompactStarted { trigger } if *trigger == wavecode_protocol::CompactTrigger::Auto
        )),
        "应以 Auto 触发压缩: {events:?}"
    );
    let history_a = session_a.messages.clone();

    // rollout 记录序：user 输入 → 压缩记录 → assistant 文本。
    let path = dir.path().join("threads/t-compact.jsonl");
    let load = crate::rollout::load_rollout(&path).unwrap();
    assert_eq!(load.records.len(), 3, "{:?}", load.records);
    let crate::rollout::RolloutRecord::Compaction {
        trigger,
        messages: recorded,
        ..
    } = &load.records[1]
    else {
        panic!("第二条应为压缩记录: {:?}", load.records)
    };
    assert_eq!(*trigger, wavecode_protocol::CompactTrigger::Auto);
    // 压缩记录承载压缩时点的新历史（= 会话当前历史的前缀）。
    assert_eq!(recorded.as_slice(), &history_a[..recorded.len()]);
    assert_eq!(p10_seqs(&load), vec![1, 2, 3]);

    // —— 压缩后 resume：replay 恢复 == 会话 A 当前历史；首条为摘要消息 ——
    let session_b = p10_session(
        Arc::new(MockModel::new(vec![])),
        p10_rollout(dir.path(), "t-compact"),
    );
    assert_eq!(*session_b.messages, *history_a);
    assert!(
        matches!(&session_b.messages[0].content[0], ContentBlock::Text { text } if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX)),
        "恢复历史首条应为摘要消息: {:?}",
        session_b.messages
    );
    assert_eq!(
        wavecode_context::find_pairing_violations(&session_b.messages),
        Vec::<String>::new()
    );
}

/// P10 验收锚点：崩溃恢复——写 rollout → 流中途中断（丢弃 Session 模拟
/// 崩溃）→ replay 恢复 → 继续 turn，历史一致且配对完整。
#[tokio::test]
async fn rollout_crash_recovery_interrupt_then_resume() {
    let dir = tempfile::tempdir().unwrap();
    // turn 1：正常完成（write_file + 文本），rollout 落 4 条记录。
    let model_a = Arc::new(MockModel::new(vec![
        write_file_script("a.txt", "A"),
        text_then_end("完成。"),
    ]));
    let (tx, _rx) = mpsc::channel::<Event>(64);
    let mut session_a = p10_session(model_a, p10_rollout(dir.path(), "t-crash"));
    session_a.run_turn("s-1", "创建 a.txt", tx).await.unwrap();
    drop(session_a);

    // turn 2：恢复后流中途被中断（半截 tool_use）——中断路径合成配对
    // 结果落盘；随后直接丢弃 Session（模拟崩溃：无优雅关闭）。
    let gate = Arc::new(AtomicBool::new(false));
    let model_b = Arc::new(GatedModel {
        script: vec![
            StreamEvent::TextDelta {
                text: "再写".into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::ToolUseBegin {
                id: "t9".into(),
                name: "write_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"b.txt""#.into(),
            },
        ],
        gate: gate.clone(),
        seen: Mutex::new(vec![]),
    });
    let (tx, rx) = mpsc::channel::<Event>(64);
    let mut session_b = p10_session(model_b, p10_rollout(dir.path(), "t-crash"));
    let handle = session_b.interrupt_handle();
    let signal = async {
        let mut rx = rx;
        loop {
            let ev = rx.recv().await.unwrap();
            if matches!(&ev.msg, EventMsg::AgentMessageDelta { text } if text == "再写") {
                break;
            }
        }
        handle.store(true, Ordering::SeqCst);
        gate.store(true, Ordering::SeqCst);
        rx
    };
    let (reason, rx) = tokio::join!(session_b.run_turn("s-2", "再写一个", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Interrupted);
    drop(rx);
    let history_b = session_b.messages.clone();
    // 半截 tool_use 已合成 is_error 配对结果（中断路径的既有纪律）。
    assert_eq!(
        wavecode_context::find_pairing_violations(&history_b),
        Vec::<String>::new()
    );
    drop(session_b); // 模拟崩溃：无 Shutdown、无提取，直接丢弃

    // —— 崩溃后 resume：replay 恢复历史与被中断时一致 ——
    let model_c = Arc::new(MockModel::new(vec![text_then_end("恢复后继续。")]));
    let (tx, _rx) = mpsc::channel::<Event>(64);
    let mut session_c = p10_session(model_c.clone(), p10_rollout(dir.path(), "t-crash"));
    assert_eq!(
        *session_c.messages, *history_b,
        "崩溃恢复的历史应与被中断时一致"
    );
    // 悬空 tool_use t9 的 is_error 配对结果在恢复历史中。
    let last = session_c.messages.last().unwrap();
    assert!(last.content.iter().any(
        |b| matches!(b, ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t9")
    ));

    // —— 断点续跑：继续 turn 正常完成，采样请求携带恢复历史 ——
    let reason = session_c.run_turn("s-3", "继续", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let seen = model_c.seen.lock().unwrap();
    assert_eq!(seen[0].messages.len(), history_b.len() + 1);
    drop(seen);
    // rollout 全程 seq 连续（4 + 3 + 2 = 9 条记录）。
    let load = crate::rollout::load_rollout(&dir.path().join("threads/t-crash.jsonl")).unwrap();
    assert_eq!(load.records.len(), 9);
    assert_eq!(p10_seqs(&load), (1..=9).collect::<Vec<u64>>());
}
