//! Tests for the MCP bridge: moved verbatim from the inline `tests` module
//! that used to close `bridge.rs`, so the implementation and its test suite
//! live in separate files.

use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Serializes env mutation in this binary against itself.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The stdio strip list covers sensitive-shaped parent variables and
/// leaves normal configuration visible (the transport applies config
/// `env` after the strip, so the list itself needs no exclusions).
#[test]
fn sensitive_env_strip_covers_secret_shapes_only() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("FOO_BRIDGE_API_KEY", "bridge-secret");
        std::env::set_var("FOO_BRIDGE_NORMAL", "visible");
    }
    let strip = sensitive_env_strip();
    unsafe {
        std::env::remove_var("FOO_BRIDGE_API_KEY");
        std::env::remove_var("FOO_BRIDGE_NORMAL");
    }
    assert!(
        strip.iter().any(|n| n == "FOO_BRIDGE_API_KEY"),
        "sensitive-shaped variable missing from the strip list: {strip:?}"
    );
    assert!(
        !strip.iter().any(|n| n == "FOO_BRIDGE_NORMAL"),
        "normal variable must not be stripped: {strip:?}"
    );
}

/// Scripted transport: pops queued rpc outcomes; each construction
/// bumps `connects` so tests can observe the healing count.
struct ScriptedRpc {
    outcomes: std::sync::Mutex<VecDeque<std::result::Result<serde_json::Value, McpError>>>,
}

#[async_trait::async_trait]
impl RpcClient for ScriptedRpc {
    async fn rpc(
        &self,
        _method: &str,
        _params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, McpError> {
        self.outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| Ok(serde_json::json!({})))
    }
}

/// One scripted connection: the nth connect (0 = initial) gets the
/// outcomes its factory slot returns, then empty results.
fn scripted_connect(
    make_outcomes: std::sync::Arc<
        dyn Fn(usize) -> Vec<std::result::Result<serde_json::Value, McpError>> + Send + Sync,
    >,
    connects: Arc<AtomicUsize>,
) -> ServerSpec {
    ServerSpec::Test(std::sync::Arc::new(move || {
        let outcomes = make_outcomes(connects.fetch_add(1, Ordering::SeqCst));
        Ok(Arc::new(ScriptedRpc {
            outcomes: std::sync::Mutex::new(VecDeque::from(outcomes)),
        }) as Arc<dyn RpcClient>)
    }))
}

#[tokio::test]
async fn transport_failure_heals_once_and_retries_idempotent_calls() {
    let connects = Arc::new(AtomicUsize::new(0));
    // First connection fails the call with a transport error; the
    // heal reconnects (connects: 2) and the retry answers.
    let make = |n: usize| {
        if n == 0 {
            vec![Err(McpError::Transport("child died".to_string()))]
        } else {
            vec![Ok(serde_json::json!({"fresh": true}))]
        }
    };
    let (client, _) = ResilientMcpClient::connect(scripted_connect(
        std::sync::Arc::new(make),
        connects.clone(),
    ))
    .await
    .unwrap();
    // tools/list is idempotent, so the healed retry may replay it.
    let answer = client
        .rpc("tools/list", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(answer["fresh"], true);
    assert_eq!(
        connects.load(Ordering::SeqCst),
        2,
        "one heal reconnects once"
    );
}

/// Consecutive transport failures back the heal off: inside the
/// cooldown window the second call refuses to reconnect; once the
/// window passes, the heal happens again.
#[tokio::test]
async fn reconnect_backs_off_while_the_cooldown_runs() {
    let connects = Arc::new(AtomicUsize::new(0));
    // Every connection fails its first three calls, so a call that
    // lands on the same connection keeps the failure streak alive;
    // the third connection finally answers.
    let make = move |n: usize| {
        if n <= 1 {
            vec![
                Err(McpError::Transport("down".to_string())),
                Err(McpError::Transport("down".to_string())),
                Err(McpError::Transport("down".to_string())),
            ]
        } else {
            vec![]
        }
    };
    let (client, _) = ResilientMcpClient::connect(scripted_connect(
        std::sync::Arc::new(make),
        connects.clone(),
    ))
    .await
    .unwrap();
    // First call: immediate heal (connects: 2), whose retry also fails.
    assert!(
        client
            .rpc("tools/list", serde_json::json!({}))
            .await
            .is_err()
    );
    assert_eq!(connects.load(Ordering::SeqCst), 2);
    // Second call inside the cooldown: refused without a reconnect.
    let error = client
        .rpc("tools/list", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, McpError::Transport(reason) if reason.contains("cooling down")),
        "the cooldown must gate the heal: {error}"
    );
    assert_eq!(
        connects.load(Ordering::SeqCst),
        2,
        "no reconnect inside the cooldown window"
    );
    // Once the window passes the heal happens again (connects: 3) and
    // the fresh connection answers.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let answer = client
        .rpc("tools/list", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(answer, serde_json::json!({}));
    assert_eq!(
        connects.load(Ordering::SeqCst),
        3,
        "the expired cooldown allows the heal"
    );
}

/// Cooldown doubles per consecutive failure and caps at the shift
/// bound (250ms base, 8s ceiling).
#[test]
fn reconnect_cooldown_doubles_and_caps() {
    assert_eq!(reconnect_cooldown(0), Duration::from_millis(250));
    assert_eq!(reconnect_cooldown(1), Duration::from_millis(500));
    assert_eq!(reconnect_cooldown(2), Duration::from_millis(1_000));
    assert_eq!(
        reconnect_cooldown(50),
        Duration::from_millis(250 * (1 << RECONNECT_COOLDOWN_MAX_SHIFTS))
    );
}

/// `tools/call` is never replayed after a transport failure: the server
/// may have already executed the tool, so a replay could run a write
/// twice. The connection still heals for later calls.
#[tokio::test]
async fn tools_call_is_not_replayed_after_transport_failure() {
    let connects = Arc::new(AtomicUsize::new(0));
    let make = |n: usize| {
        if n == 0 {
            vec![Err(McpError::Transport("child died".to_string()))]
        } else {
            vec![Ok(serde_json::json!({"fresh": true}))]
        }
    };
    let (client, _) = ResilientMcpClient::connect(scripted_connect(
        std::sync::Arc::new(make),
        connects.clone(),
    ))
    .await
    .unwrap();
    let error = client
        .rpc("tools/call", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, McpError::Transport(reason) if reason.contains("not replayed")),
        "the transport error must surface without a replay: {error}"
    );
    assert_eq!(
        connects.load(Ordering::SeqCst),
        2,
        "the heal still reconnects for later calls"
    );
}

#[tokio::test]
async fn protocol_failure_surfaces_without_healing() {
    let connects = Arc::new(AtomicUsize::new(0));
    let make = |_n: usize| vec![Err(McpError::Protocol("bad frame".to_string()))];
    let (client, _) = ResilientMcpClient::connect(scripted_connect(
        std::sync::Arc::new(make),
        connects.clone(),
    ))
    .await
    .unwrap();
    let error = client
        .rpc("tools/call", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Protocol(_)));
    assert_eq!(
        connects.load(Ordering::SeqCst),
        1,
        "protocol errors never heal"
    );
}

struct FakeClient {
    tools: Vec<McpToolDef>,
    resources: Vec<McpResourceDef>,
    prompts: Vec<McpPromptDef>,
    caps: ServerCaps,
    calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    fail_calls: bool,
}

impl FakeClient {
    fn new(tools: Vec<McpToolDef>) -> Self {
        Self {
            tools,
            resources: Vec::new(),
            prompts: Vec::new(),
            caps: ServerCaps::default(),
            calls: std::sync::Mutex::new(Vec::new()),
            fail_calls: false,
        }
    }
}

#[async_trait::async_trait]
impl McpClient for FakeClient {
    async fn list_tools(&self) -> std::result::Result<Vec<McpToolDef>, McpError> {
        Ok(self.tools.clone())
    }

    async fn call_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> std::result::Result<McpToolOutput, McpError> {
        self.calls.lock().unwrap().push((name.to_string(), input));
        if self.fail_calls {
            return Err(McpError::Transport("pipe broken".to_string()));
        }
        Ok(McpToolOutput {
            content: format!("ran {name}"),
            is_error: false,
        })
    }

    async fn list_prompts(&self) -> std::result::Result<Vec<McpPromptDef>, McpError> {
        if !self.caps.prompts {
            return Ok(vec![]);
        }
        Ok(self.prompts.clone())
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> std::result::Result<Vec<McpPromptMessage>, McpError> {
        if !self.caps.prompts {
            return Ok(vec![]);
        }
        self.calls
            .lock()
            .unwrap()
            .push(("prompts/get".to_string(), serde_json::json!(arguments)));
        Ok(vec![McpPromptMessage {
            role: "user".to_string(),
            text: format!("prompt {name} asks"),
        }])
    }

    async fn list_resources(&self) -> std::result::Result<Vec<McpResourceDef>, McpError> {
        if !self.caps.resources {
            return Ok(vec![]);
        }
        Ok(self.resources.clone())
    }

    async fn read_resource(
        &self,
        uri: &str,
    ) -> std::result::Result<Vec<McpResourceContent>, McpError> {
        if !self.caps.resources {
            return Ok(vec![]);
        }
        self.calls
            .lock()
            .unwrap()
            .push(("resources/read".to_string(), serde_json::json!(uri)));
        Ok(vec![McpResourceContent {
            uri: uri.to_string(),
            mime_type: Some("text/plain".to_string()),
            text: "hello".to_string(),
        }])
    }
}

fn def(name: &str) -> McpToolDef {
    McpToolDef {
        name: name.to_string(),
        description: Some(format!("does {name}")),
        input_schema: serde_json::json!({"type": "object"}),
        read_only_hint: false,
    }
}

fn ctx() -> ToolCtx {
    ToolCtx {
        cwd: std::path::PathBuf::from("/tmp"),
        deny_env: Vec::new(),
    }
}

#[test]
fn initialize_requires_a_protocol_version() {
    assert!(check_initialize(&serde_json::json!({"protocolVersion": "2024-11-05"})).is_ok());
    assert!(check_initialize(&serde_json::json!({})).is_err());
    assert!(check_initialize(&serde_json::json!({"protocolVersion": ""})).is_err());
    assert!(check_initialize(&serde_json::json!([])).is_err());
}

#[test]
fn tools_list_pages_parse_with_cursor() {
    let (defs, cursor) = parse_tools_list(&serde_json::json!({
        "tools": [
            {"name": "click", "description": "d", "inputSchema": {"type": "object"}},
            {"name": "", "description": "skipped"},
            {"description": "nameless skipped"},
            {"name": "scan", "annotations": {"readOnlyHint": true}},
        ],
        "nextCursor": "page2",
    }))
    .unwrap();
    assert_eq!(defs.len(), 2);
    assert_eq!(defs[0].name, "click");
    assert_eq!(defs[0].description.as_deref(), Some("d"));
    assert!(defs[1].read_only_hint);
    // Missing inputSchema defaults to a plain object schema.
    assert_eq!(defs[1].input_schema, serde_json::json!({"type": "object"}));
    assert_eq!(cursor.as_deref(), Some("page2"));
    let (defs, cursor) = parse_tools_list(&serde_json::json!({"tools": []})).unwrap();
    assert!(defs.is_empty());
    assert_eq!(cursor, None);
    assert!(parse_tools_list(&serde_json::json!({"tools": {}})).is_err());
    assert!(parse_tools_list(&serde_json::json!([])).is_err());
}

#[test]
fn tool_results_flatten_and_detect_errors() {
    let out = parse_tool_result(&serde_json::json!({
        "content": [
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "world"},
        ],
    }))
    .unwrap();
    assert_eq!(out.content, "hello\nworld");
    assert!(!out.is_error);
    let out = parse_tool_result(&serde_json::json!({
        "content": [{"type": "text", "text": "bad"}],
        "isError": true,
    }))
    .unwrap();
    assert!(out.is_error);
    let out = parse_tool_result(&serde_json::json!({
        "content": [{"type": "image", "data": "x"}],
    }))
    .unwrap();
    assert!(out.content.contains("non-text"));
    // JSON-RPC error shape (preserved verbatim by the decoder).
    assert!(parse_tool_result(&serde_json::json!({"code": -32601, "message": "nope"})).is_err());
    assert!(parse_tool_result(&serde_json::json!({"unexpected": 1})).is_err());
}

#[tokio::test]
async fn bridge_names_describes_and_forwards_calls() {
    let client = Arc::new(FakeClient::new(vec![def("click")]));
    let bridge =
        McpToolBridge::new("playwright", &def("click"), client.clone()).expect("valid name");
    assert_eq!(bridge.name(), "mcp__playwright__click");
    assert_eq!(bridge.description(), "does click");
    assert!(!bridge.is_read_only());
    let out = bridge
        .execute(serde_json::json!({"x": 1}), &ctx())
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("ran click"));
    // Server-side raw name (no prefix) reaches the client.
    assert_eq!(client.calls.lock().unwrap()[0].0, "click".to_string());
}

#[tokio::test]
async fn bridge_transport_failures_read_as_business_errors() {
    let client = Arc::new(FakeClient {
        fail_calls: true,
        ..FakeClient::new(vec![])
    });
    let bridge = McpToolBridge::new("srv", &def("go"), client).expect("valid name");
    let out = bridge.execute(serde_json::json!({}), &ctx()).await.unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("mcp__srv__go"));
}

#[test]
fn bridge_rejects_invalid_servers_and_distrusts_read_only_hint() {
    let client: Arc<dyn McpClient> = Arc::new(FakeClient::new(vec![]));
    assert!(McpToolBridge::new("a__b", &def("go"), client.clone()).is_none());
    let mut ro = def("scan");
    ro.description = None;
    ro.read_only_hint = true;
    let bridge = McpToolBridge::new("srv", &ro, client).expect("valid name");
    // The server's own readOnlyHint must not flip the bridge to
    // read-only: a lying server could otherwise bypass the approval
    // gate for writes.
    assert!(!bridge.is_read_only());
    assert!(bridge.description().contains("srv"));
}

fn raw(command: Option<&str>, url: Option<&str>) -> wavecode_config::McpServerRaw {
    wavecode_config::McpServerRaw {
        command: command.map(|s| s.to_string()),
        args: Vec::new(),
        env: HashMap::new(),
        url: url.map(|s| s.to_string()),
        headers: HashMap::new(),
        oauth_token_url: None,
        oauth_client_id: None,
        oauth_client_secret: None,
        oauth_scope: None,
    }
}

#[tokio::test]
async fn connect_all_skips_misconfigurations_without_spawning() {
    let registry = Arc::new(wavecode_tools::Registry::builtin());
    let report = connect_all(
        &[
            ("bad__name".to_string(), raw(Some("cmd"), None)),
            ("both".to_string(), raw(Some("cmd"), Some("http://x"))),
            ("neither".to_string(), raw(None, None)),
            (
                "web".to_string(),
                raw(None, Some("https://mcp.example.com")),
            ),
        ],
        &registry,
    )
    .await;
    assert_eq!(report.lines.len(), 4);
    assert!(
        report
            .lines
            .iter()
            .all(|l| l.contains("skipped") || l.contains("unavailable"))
    );
    // Concurrent connection keeps the report aligned with config order.
    assert!(report.lines[0].contains("bad__name"));
    assert!(report.lines[1].contains("both"));
    assert!(report.lines[2].contains("neither"));
    assert!(report.lines[3].contains("web"));
    assert!(report.warnings.len() == 4);
    assert!(registry.get("mcp__bad__name__x").is_none());
}

#[tokio::test]
async fn connect_all_empty_stays_empty() {
    let registry = Arc::new(wavecode_tools::Registry::builtin());
    let report = connect_all(&[], &registry).await;
    assert!(report.lines.is_empty());
    assert!(report.warnings.is_empty());
}

#[test]
fn server_caps_read_from_initialize_payload() {
    let caps = server_caps(&serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {"resources": {}, "prompts": {"listChanged": true}},
    }));
    assert!(caps.resources && caps.prompts);
    let tools_only = server_caps(&serde_json::json!({"capabilities": {"tools": {}}}));
    assert!(!tools_only.resources && !tools_only.prompts);
    assert_eq!(server_caps(&serde_json::json!({})), ServerCaps::default());
}

#[test]
fn resources_and_prompts_payloads_parse() {
    // resources/list: uri-less and empty-uri items skip; missing name
    // falls back to the uri; the cursor survives.
    let (defs, cursor) = parse_resources_list(&serde_json::json!({
        "resources": [
            {"uri": "file:///a.txt", "name": "a", "description": "first", "mimeType": "text/plain"},
            {"uri": "", "name": "skipped"},
            {"name": "no uri skipped"},
        ],
        "nextCursor": "r2",
    }))
    .unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].uri, "file:///a.txt");
    assert_eq!(defs[0].mime_type.as_deref(), Some("text/plain"));
    assert_eq!(cursor.as_deref(), Some("r2"));
    let (defs, cursor) =
        parse_resources_list(&serde_json::json!({"resources": [{"uri": "mem://x"}]})).unwrap();
    assert_eq!(defs[0].name, "mem://x");
    assert_eq!(cursor, None);
    assert!(parse_resources_list(&serde_json::json!({"resources": {}})).is_err());
    assert!(parse_resources_list(&serde_json::json!([])).is_err());

    // resources/read: text and blob entries; a text-less entry skips.
    let contents = parse_resource_contents(&serde_json::json!({
        "contents": [
            {"uri": "file:///a.txt", "mimeType": "text/plain", "text": "hello"},
            {"uri": "file:///b.bin", "blob": "aGVsbG8="},
            {"uri": "file:///empty.json"},
        ],
    }))
    .unwrap();
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0].text, "hello");
    assert!(contents[1].text.contains("omitted"));
    assert!(parse_resource_contents(&serde_json::json!({})).is_err());

    // prompts/list: arguments parse with their required flags.
    let (defs, cursor) = parse_prompts_list(&serde_json::json!({
        "prompts": [
            {"name": "review", "description": "code review", "arguments": [
                {"name": "path", "description": "file", "required": true},
                {"name": "lang"},
            ]},
            {"description": "nameless skipped"},
            {"name": ""},
        ],
    }))
    .unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].arguments.len(), 2);
    assert!(defs[0].arguments[0].required);
    assert!(!defs[0].arguments[1].required);
    assert_eq!(cursor, None);
    assert!(parse_prompts_list(&serde_json::json!([])).is_err());

    // prompts/get: role-bearing messages; non-text content degrades.
    let messages = parse_prompt_messages(&serde_json::json!({
        "messages": [
            {"role": "user", "content": {"type": "text", "text": "review this"}},
            {"role": "assistant", "content": {"type": "image", "data": "x"}},
            {"content": {"type": "text", "text": "no role skipped"}},
        ],
    }))
    .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].text, "review this");
    assert!(messages[1].text.contains("omitted"));
    assert!(parse_prompt_messages(&serde_json::json!({"messages": {}})).is_err());
}

#[test]
fn discovery_catalogs_embed_listings() {
    let resources = vec![McpResourceDef {
        uri: "file:///a.txt".to_string(),
        name: "a".to_string(),
        description: Some("first".to_string()),
        mime_type: Some("text/plain".to_string()),
    }];
    let catalog = resource_catalog("srv", &resources);
    assert!(catalog.contains("srv") && catalog.contains("file:///a.txt — a"));
    assert!(catalog.contains("text/plain"));
    assert!(resource_catalog("srv", &[]).contains("no resources"));

    let prompts = vec![McpPromptDef {
        name: "review".to_string(),
        description: Some("code review".to_string()),
        arguments: vec![crate::McpPromptArgument {
            name: "path".to_string(),
            description: None,
            required: true,
        }],
    }];
    let catalog = prompt_catalog("srv", &prompts);
    assert!(catalog.contains("review") && catalog.contains("path (required)"));
    assert!(prompt_catalog("srv", &[]).contains("no prompts"));
}

#[tokio::test]
async fn resource_bridge_reads_and_flags_bad_input() {
    let resources = vec![McpResourceDef {
        uri: "file:///a.txt".to_string(),
        name: "a".to_string(),
        description: None,
        mime_type: None,
    }];
    let client: Arc<dyn McpClient> = Arc::new(FakeClient {
        caps: ServerCaps {
            resources: true,
            prompts: false,
        },
        ..FakeClient::new(vec![])
    });
    let bridge = McpResourceBridge::new("srv", &resources, client).expect("valid name");
    assert_eq!(bridge.name(), "mcp__srv__read_resource");
    assert!(bridge.is_read_only());
    let out = bridge
        .execute(serde_json::json!({"uri": "file:///a.txt"}), &ctx())
        .await
        .unwrap();
    assert!(!out.is_error);
    assert_eq!(out.content, "hello");
    // A missing uri reads as a business error, not an implementation fault.
    let out = bridge.execute(serde_json::json!({}), &ctx()).await.unwrap();
    assert!(out.is_error && out.content.contains("uri"));
}

#[tokio::test]
async fn prompt_bridge_renders_and_drops_non_string_arguments() {
    let prompts = vec![McpPromptDef {
        name: "review".to_string(),
        description: None,
        arguments: vec![],
    }];
    let client: Arc<dyn McpClient> = Arc::new(FakeClient {
        caps: ServerCaps {
            resources: false,
            prompts: true,
        },
        ..FakeClient::new(vec![])
    });
    let bridge = McpPromptBridge::new("srv", &prompts, client).expect("valid name");
    assert_eq!(bridge.name(), "mcp__srv__get_prompt");
    assert!(bridge.is_read_only());
    let out = bridge
        .execute(
            serde_json::json!({"name": "review", "arguments": {"path": "x.rs", "n": 1}}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("user: prompt review asks"));
    let out = bridge.execute(serde_json::json!({}), &ctx()).await.unwrap();
    assert!(out.is_error && out.content.contains("name"));
}

#[tokio::test]
async fn bridge_server_registers_discovery_tools_only_when_capable() {
    let resources = vec![McpResourceDef {
        uri: "file:///a.txt".to_string(),
        name: "a".to_string(),
        description: None,
        mime_type: None,
    }];
    let prompts = vec![McpPromptDef {
        name: "review".to_string(),
        description: None,
        arguments: vec![],
    }];
    let capable = Arc::new(FakeClient {
        caps: ServerCaps {
            resources: true,
            prompts: true,
        },
        resources,
        prompts,
        ..FakeClient::new(vec![def("click")])
    });
    let registry = Arc::new(wavecode_tools::Registry::builtin());
    let count = bridge_server("srv", capable.clone(), capable.caps, &registry)
        .await
        .unwrap();
    // One tool plus the two discovery bridges.
    assert_eq!(count, 3);
    assert!(registry.get("mcp__srv__click").is_some());
    let reader = registry.get("mcp__srv__read_resource").expect("reader");
    assert!(reader.description().contains("file:///a.txt"));
    let out = reader
        .execute(serde_json::json!({"uri": "file:///a.txt"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out.content, "hello");
    let getter = registry.get("mcp__srv__get_prompt").expect("getter");
    let out = getter
        .execute(serde_json::json!({"name": "review"}), &ctx())
        .await
        .unwrap();
    assert!(out.content.contains("user:"));

    // Without the capabilities only the tool itself registers.
    let plain_registry = Arc::new(wavecode_tools::Registry::builtin());
    let plain = Arc::new(FakeClient::new(vec![def("click")]));
    let count = bridge_server("srv", plain, ServerCaps::default(), &plain_registry)
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(plain_registry.get("mcp__srv__read_resource").is_none());
    assert!(plain_registry.get("mcp__srv__get_prompt").is_none());
}

/// A server may list a tool named exactly like a discovery bridge
/// (`read_resource` / `get_prompt`). The registry replaces on register,
/// so a later discovery bridge would silently overwrite the server's own
/// tool; the explicit tool must win and the discovery surface stays out.
#[tokio::test]
async fn discovery_bridges_never_shadow_same_named_tools() {
    let resources = vec![McpResourceDef {
        uri: "file:///a.txt".to_string(),
        name: "a".to_string(),
        description: None,
        mime_type: None,
    }];
    let prompts = vec![McpPromptDef {
        name: "review".to_string(),
        description: None,
        arguments: vec![],
    }];
    let client = Arc::new(FakeClient {
        caps: ServerCaps {
            resources: true,
            prompts: true,
        },
        resources,
        prompts,
        ..FakeClient::new(vec![def("read_resource"), def("get_prompt")])
    });
    let registry = Arc::new(wavecode_tools::Registry::builtin());
    let count = bridge_server("srv", client.clone(), client.caps, &registry)
        .await
        .unwrap();
    // Two explicit tools only; neither discovery bridge registered.
    assert_eq!(count, 2);
    let out = registry
        .get("mcp__srv__read_resource")
        .expect("the server's own read_resource tool stays bridged")
        .execute(serde_json::json!({}), &ctx())
        .await
        .unwrap();
    assert!(
        out.content.contains("ran read_resource"),
        "the explicit tool answers, not the resource bridge: {}",
        out.content
    );
    let out = registry
        .get("mcp__srv__get_prompt")
        .expect("the server's own get_prompt tool stays bridged")
        .execute(serde_json::json!({}), &ctx())
        .await
        .unwrap();
    assert!(
        out.content.contains("ran get_prompt"),
        "the explicit tool answers, not the prompt bridge: {}",
        out.content
    );
}

/// Handler shape the HTTP tests reason in: JSON-RPC method plus the
/// `mcp-session-id` request header, answered with a status, extra
/// headers, and a UTF-8 body.
type StubHandler =
    Arc<dyn Fn(&str, Option<&str>) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

/// Serve one HTTP stub over the transport crate's shared test server:
/// the shared socket plumbing records requests; this adapter keeps the
/// method/session handler shape the tests below are written in.
async fn spawn_rpc_stub(handler: StubHandler) -> String {
    let server = transport_mcp::test_support::StubServer::spawn(Arc::new(move |request| {
        let session = request.headers.get("mcp-session-id").map(String::as_str);
        let (status, extra, body) = handler(&request.rpc_method(), session);
        // Echo the request's own id into any JSON-RPC body: the client
        // correlates responses by id, so a handler's hardcoded id never
        // matches once the client's id counter has moved past it.
        let request_id = serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|value| value.get("id").cloned());
        let mut body = body;
        if let (Some(request_id), Ok(mut value)) =
            (request_id, serde_json::from_str::<serde_json::Value>(&body))
            && let Some(object) = value.as_object_mut()
            && object.contains_key("id")
        {
            object.insert("id".to_string(), request_id);
            body = value.to_string();
        }
        (status, extra, body.into_bytes())
    }))
    .await;
    server.url("/mcp")
}

/// End-to-end session expiry over the local stub: the first
/// post-handshake request 404s, the client re-initializes exactly once,
/// and the retried listing succeeds with the fresh session.
#[tokio::test]
async fn http_client_reinitializes_once_on_session_404() {
    let initialize_hits = Arc::new(AtomicUsize::new(0));
    let counter = initialize_hits.clone();
    let handler: StubHandler = Arc::new(move |method, session| {
        let json_headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        match method {
            "initialize" => {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let mut headers = json_headers;
                headers.push(("mcp-session-id".to_owned(), format!("sess-{n}")));
                let body = serde_json::json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {"name": "stub", "version": "0"},
                    },
                })
                .to_string();
                (200, headers, body)
            }
            "notifications/initialized" => (202, vec![], String::new()),
            "tools/list" => {
                if session == Some("sess-2") {
                    let body = serde_json::json!({
                            "jsonrpc": "2.0", "id": 2,
                            "result": {"tools": [
                                {"name": "click", "description": "clicks", "inputSchema": {"type": "object"}},
                            ]},
                        })
                        .to_string();
                    (200, json_headers, body)
                } else {
                    (404, vec![], "session expired".to_owned())
                }
            }
            _ => (400, vec![], "unknown method".to_owned()),
        }
    });
    let url = spawn_rpc_stub(handler).await;
    let registry = Arc::new(wavecode_tools::Registry::builtin());
    let count = connect_http("web", &url, HashMap::new(), None, &registry)
        .await
        .expect("connect succeeds after the re-initialize");
    assert_eq!(count, 1, "the retried tools/list bridges one tool");
    assert_eq!(
        initialize_hits.load(Ordering::SeqCst),
        2,
        "exactly one re-initialize after the 404"
    );
    assert!(registry.get("mcp__web__click").is_some());
}
