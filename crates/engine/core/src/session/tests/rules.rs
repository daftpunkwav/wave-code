//! 阶段 1a-0 特征化安全网：deny/allow 规则在 run_turn 编排层的行为基线（SEC-001/002）。

use super::*;

// —— 特征化测试（阶段 1a-0 安全网，SEC-001/002）：锁定 deny/allow 规则在
// run_turn 编排层的现有行为基线。此前这些路径零集成覆盖（既有 run_turn
// 测试全用 bypass_sandbox 无规则）。TurnRunner 抽取与 session 拆分须保持。

/// V1（SEC-001）：只读工具（read_file）整体跳过 sandbox.decide()——deny
/// 规则对只读工具不生效（既存行为）。修复此 deny-on-read 缺口属独立安全
/// 增强，不混入重构；本测试锁定现状以防重构静默改变。
#[tokio::test]
async fn deny_rule_does_not_block_readonly_tool() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
    std::fs::write(dir.path().join("secrets/key.pem"), "TOPSECRET").unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "read_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"secrets/key.pem"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("已读。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(
                Sandbox::new(
                    PermissionMode::Default,
                    &[],
                    &["File(secrets/**)".to_string()],
                )
                .unwrap(),
            )
            .build()
    });
    let reason = session
        .run_turn("s-1", "读 secrets/key.pem", tx)
        .await
        .unwrap();
    assert_eq!(reason, StopReason::Completed);
    let mut tool_ok = None;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::ToolCallEnd { ok, .. } = ev.msg {
            tool_ok = Some(ok);
        }
    }
    assert_eq!(
        tool_ok,
        Some(true),
        "只读工具应跳过 decide 直接执行（SEC-001）"
    );
}

/// V2：非只读工具（write_file）经 sandbox.decide()，deny 规则生效——
/// 工具不执行，reason 以 is_error 回灌模型，文件未创建。
#[tokio::test]
async fn deny_rule_blocks_nonreadonly_tool() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "write_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"data.secret","content":"x"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("被拒了。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(
                Sandbox::new(
                    PermissionMode::Default,
                    &[],
                    &["File(*.secret)".to_string()],
                )
                .unwrap(),
            )
            .build()
    });
    let reason = session.run_turn("s-1", "写 data.secret", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let mut tool_ok = None;
    while let Ok(ev) = rx.try_recv() {
        if let EventMsg::ToolCallEnd { ok, .. } = ev.msg {
            tool_ok = Some(ok);
        }
    }
    assert_eq!(tool_ok, Some(false), "write_file 应被 deny 规则拒绝");
    assert!(
        !dir.path().join("data.secret").exists(),
        "被拒工具不应创建文件"
    );
}

/// V3：default 模式下非只读工具命中 allow 规则 → Allow（免审批直接执行），
/// 不发 ApprovalRequested 事件（default 无规则时非只读会 Ask 卡住）。
#[tokio::test]
async fn allow_rule_skips_approval_for_nonreadonly() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            StreamEvent::ToolUseBegin {
                id: "t1".into(),
                name: "write_file".into(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"out.txt","content":"hi"}"#.into(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ],
        text_then_end("已写。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(Sandbox::new(PermissionMode::Default, &["File(*)".to_string()], &[]).unwrap())
            .build()
    });
    let reason = session.run_turn("s-1", "写 out.txt", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);
    let mut saw_approval = false;
    let mut tool_ok = None;
    while let Ok(ev) = rx.try_recv() {
        match ev.msg {
            EventMsg::ApprovalRequested { .. } => saw_approval = true,
            EventMsg::ToolCallEnd { ok, .. } => tool_ok = Some(ok),
            _ => {}
        }
    }
    assert!(
        !saw_approval,
        "allow 规则命中应免审批，不发 ApprovalRequested"
    );
    assert_eq!(tool_ok, Some(true), "write_file 应直接执行成功");
    assert!(dir.path().join("out.txt").exists(), "文件应被创建");
}
