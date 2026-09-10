//! P6 记忆：memory_write 审批 / 跨会话召回 / 自动提取 / 输入校验。

use super::*;

// ------------------------------------------------------------------
// P6：记忆系统（memory_write 工具 / 审批挂接 / 跨会话召回 / 自动提取）
// ------------------------------------------------------------------

/// P6 测试夹具：memory_write 单轮调用脚本。
fn memory_write_script(call_id: &str, category: &str, content: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseBegin {
            id: call_id.into(),
            name: "memory_write".into(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: format!(r#"{{"category":"{category}","content":"{content}"}}"#),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "tool_use".into(),
            usage: Usage::default(),
        },
    ]
}

/// P6 测试夹具：最小记忆配置（无指令记忆 / 索引，仅存储根）。
fn p6_memory(store_root: &std::path::Path) -> Option<crate::memory::MemorySessionConfig> {
    Some(crate::memory::MemorySessionConfig {
        instruction_memory: String::new(),
        memory_index: String::new(),
        store_root: store_root.to_path_buf(),
    })
}

/// P6 验收：memory_write 审批挂接——default 模式下经 sandbox 非只读
/// 默认策略给出 Ask（ApprovalRequested → ExecApproval 放行后才写入）。
#[tokio::test]
async fn memory_write_asks_in_default_mode() {
    let dir = tempfile::tempdir().unwrap();
    let store_root = dir.path().join("memories");
    let scripts = vec![
        memory_write_script("t1", "user", "偏好紧凑回复"),
        text_then_end("已记住。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .memory(p6_memory(&store_root))
            .build()
    });
    let gate = session.approval_handle();
    let signal = async {
        let mut rx = rx;
        let mut requested = None;
        while let Some(ev) = rx.recv().await {
            if let EventMsg::ApprovalRequested {
                call_id,
                kind,
                detail,
            } = ev.msg
            {
                requested = Some((kind, detail));
                gate.decide(call_id, wavecode_protocol::ApprovalDecision::AllowOnce);
            }
        }
        requested
    };
    let (reason, requested) = tokio::join!(session.run_turn("s-1", "记住我的偏好", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Completed);
    let (kind, detail) = requested.expect("default 模式下 memory_write 应发审批请求");
    assert_eq!(kind, wavecode_protocol::ApprovalKind::Write);
    assert!(detail.contains("memory_write"));
    // 放行后实际写入：类别文件 + 索引。
    let store = wavecode_memory::MemoryStore::new(store_root);
    assert_eq!(
        store
            .read_category(wavecode_memory::MemoryCategory::User)
            .unwrap(),
        "- 偏好紧凑回复\n"
    );
    assert!(store.read_index().unwrap().contains("[user] 偏好紧凑回复"));
}

/// P6 验收：跨会话召回——会话 A 经 memory_write 写入条目；模拟会话 B
/// 装配（启动时读索引 → 注入系统提示词槽位）：注入含索引条目，且
/// 条目正文可按需加载。
#[tokio::test]
async fn cross_session_memory_recall() {
    let dir = tempfile::tempdir().unwrap();
    let store_root = dir.path().join("memories");
    // —— 会话 A：模型调用 memory_write 写入条目（bypass 免审批）——
    let scripts = vec![
        memory_write_script("t1", "project", "仓库用 pnpm 管理，不要引入 yarn"),
        text_then_end("已记录项目约定。"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session_a = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model, registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .memory(p6_memory(&store_root))
            .build()
    });
    let reason = session_a.run_turn("s-1", "记住项目约定", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);

    // —— 会话 B 装配：启动时读索引（cli bootstrap 的同款路径）——
    let store = wavecode_memory::MemoryStore::new(store_root);
    let index = store.read_index().unwrap();
    assert!(
        index.contains("[project] 仓库用 pnpm 管理"),
        "索引应含条目: {index}"
    );
    let system = crate::prompt::build_system_prompt(dir.path(), "", "", &index, &[]).await;
    assert!(
        system.contains("# Persistent Memory Index"),
        "注入应含记忆索引段:\n{system}"
    );
    assert!(system.contains("[project] 仓库用 pnpm 管理"));
    // 条目正文按需加载（模型 read_file 的等价物）。
    let body = store
        .read_category(wavecode_memory::MemoryCategory::Project)
        .unwrap();
    assert!(body.contains("不要引入 yarn"), "条目正文可加载: {body}");
}

/// P6：自动提取——会话历史经提取子代理（mock 回放）提炼为带类别
/// 标签的条目，解析后追加到存储（简化首版：纯追加式）。
#[tokio::test]
async fn memory_extraction_appends_entries() {
    let dir = tempfile::tempdir().unwrap();
    let store_root = dir.path().join("memories");
    let scripts = vec![
        // 第 1 次采样：会话正文（建立历史）。
        text_then_end("好的，以后回复保持紧凑。另外这个项目用 pnpm。"),
        // 第 2 次采样：提取子代理的输出（约定线格式）。
        text_then_end("[user] 偏好紧凑回复\n[project] 仓库用 pnpm 管理"),
    ];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model, registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .memory(p6_memory(&store_root))
            .build()
    });
    session.run_turn("s-1", "随便聊聊", tx).await.unwrap();

    let n = session.extract_memories().await.unwrap();
    assert_eq!(n, 2, "应提取 2 条");
    let store = wavecode_memory::MemoryStore::new(store_root);
    assert_eq!(
        store
            .read_category(wavecode_memory::MemoryCategory::User)
            .unwrap(),
        "- 偏好紧凑回复\n"
    );
    assert_eq!(
        store
            .read_category(wavecode_memory::MemoryCategory::Project)
            .unwrap(),
        "- 仓库用 pnpm 管理\n"
    );
    let index = store.read_index().unwrap();
    assert!(index.contains("[user]") && index.contains("[project]"));
}

/// P6：memory_write 参数校验（非法类别 / 空内容 → is_error 回灌，
/// 不 panic、不写入）。
#[tokio::test]
async fn memory_write_validates_input() {
    use wavecode_tools::Tool as _;
    let dir = tempfile::tempdir().unwrap();
    let tool = crate::memory::MemoryWrite::new(dir.path().join("memories"));
    let ctx = ToolCtx {
        cwd: dir.path().to_path_buf(),
        deny_env: Vec::new(),
    };
    let out = tool
        .execute(
            serde_json::json!({"category": "nope", "content": "x"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("invalid category"));
    let out = tool
        .execute(
            serde_json::json!({"category": "user", "content": "  "}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.is_error && out.content.contains("'content'"));
    // 未写入任何文件。
    let store = wavecode_memory::MemoryStore::new(dir.path().join("memories"));
    assert_eq!(store.read_index().unwrap(), "");
}
