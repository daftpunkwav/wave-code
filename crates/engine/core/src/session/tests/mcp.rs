//! P9 MCP 工具桥接（FakeMcpClient 验证桥接工具面与审批归类）。

use super::*;

// —— P9：MCP 工具桥接（SPEC §10，真实 transport 未实现，mock client 验证） ——

/// P9 测试夹具：假 MCP client——单工具 `echo`（回显 text 参数），
/// 记录收到的 `(原始名, 输入)` 供断言。
struct FakeMcpClient {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
}

#[async_trait::async_trait]
impl crate::mcp::McpClient for FakeMcpClient {
    async fn list_tools(&self) -> Result<Vec<crate::mcp::McpToolDef>, crate::mcp::McpError> {
        Ok(vec![crate::mcp::McpToolDef {
            name: "echo".into(),
            description: Some("Echo the input text back".into()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
        }])
    }

    async fn call_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<crate::mcp::McpToolOutput, crate::mcp::McpError> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_owned(), input.clone()));
        let text = input["text"].as_str().unwrap_or("");
        Ok(crate::mcp::McpToolOutput {
            content: format!("echo: {text}"),
            is_error: false,
        })
    }
}

/// P9 测试夹具：调用 `mcp__fake__echo` 的单轮脚本。
fn mcp_echo_script(call_id: &str, text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseBegin {
            id: call_id.into(),
            name: "mcp__fake__echo".into(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: format!(r#"{{"text":"{text}"}}"#),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "tool_use".into(),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        },
    ]
}

/// P9 测试夹具：mock client 经 McpToolBridge 注册进 Registry
///（SPEC §10 命名注入点）；返回与 `todo_write` 同源的 TodoStore。
async fn p9_registry(
    client: Arc<FakeMcpClient>,
) -> (wavecode_tools::Registry, wavecode_tools::TodoStore) {
    let bridge = crate::mcp::McpToolBridge::new("fake", client);
    let (mut registry, todos) = builtin_registry();
    for tool in bridge.tools().await.unwrap() {
        registry.register(tool);
    }
    (registry, todos)
}

/// P9 验收：mock McpClient 的工具经桥注册进 Registry（`mcp__fake__echo`
/// 命名注入），经 turn 循环调用成功、结果回灌模型（第二轮采样请求中
/// 可见 ToolResult），call_tool 收到的是 server 侧原始名。
#[tokio::test]
async fn mcp_bridged_tool_callable_in_turn() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeMcpClient {
        calls: Mutex::new(vec![]),
    });
    let scripts = vec![mcp_echo_script("t1", "hi"), text_then_end("完成。")];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = p9_registry(client.clone()).await;
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .build()
    });
    let reason = session.run_turn("s-1", "调用 echo", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);

    // client 收到原始名（不含 mcp__ 前缀）与透传输入。
    assert_eq!(
        client.calls.lock().unwrap().as_slice(),
        &[("echo".to_owned(), serde_json::json!({"text": "hi"}))]
    );
    // 结果回灌：第二轮采样请求的历史中含 ToolResult "echo: hi"。
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
    assert_eq!(results, vec!["echo: hi"], "MCP 结果应回灌模型");
}

/// P9 验收：桥接工具走 sandbox 同一审批管道（非只读默认）——default
/// 模式下调用前发 ApprovalRequested（detail 含 `mcp__fake__echo`），
/// 放行后才实际调用 client。
#[tokio::test]
async fn mcp_bridged_tool_asks_in_default_mode() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeMcpClient {
        calls: Mutex::new(vec![]),
    });
    let scripts = vec![mcp_echo_script("t1", "hi"), text_then_end("完成。")];
    let model = Arc::new(MockModel::new(scripts));
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    let mut session = Session::new({
        let (registry, todos) = p9_registry(client.clone()).await;
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
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
    let (reason, requested) = tokio::join!(session.run_turn("s-1", "调用 echo", tx), signal);
    assert_eq!(reason.unwrap(), StopReason::Completed);
    let (kind, detail) = requested.expect("default 模式下 MCP 工具应发审批请求");
    assert_eq!(kind, wavecode_protocol::ApprovalKind::Write);
    assert!(detail.contains("mcp__fake__echo"), "{detail}");
    // 放行后实际调用到达 client。
    assert_eq!(client.calls.lock().unwrap().len(), 1);
}
