//! MCP 预留接口的 core 侧编排（P9，SPEC §10）。
//!
//! mcp crate 无 workspace 内依赖（SPEC §3 依赖矩阵），凡涉及 config /
//! tools 的能力都落在 core（core→mcp 边由矩阵允许）：
//!
//! - [`servers_from_config`]：config 的 `[mcp_servers.<name>]` 原始表 →
//!   [`NamedMcpServer`]（stdio / http 二选一校验；非法条目警告跳过，
//!   单点坏配置不炸装配——同 skills 发现纪律）。首版仅解析 + 持有
//!   （`/mcp` 展示面），连接留待真实 transport 落地；
//! - [`McpToolBridge`]：把一个 `Arc<dyn McpClient>` 的每个工具包装成
//!   wavecode `Tool`（名字加 `mcp__{server}__` 前缀），可注册进
//!   Registry——即 SPEC §10 的"命名注入注册表"注入点。
//!
//! **审批挂接**（对齐 sandbox 既有方式，无特判）：桥接工具声明
//! `is_read_only() = false`（MCP 工具的副作用面本 crate 无法静态判定，
//! 取保守默认），sandbox `decide()` 的模式默认策略即给出 default 模式
//! `Ask` / plan 模式 `Deny` / bypassPermissions 放行——与 write_file /
//! memory_write 同一审批管道（ApprovalRequested → ExecApproval）。
//!
//! **prompt → inline skill 转换（占位）**：SPEC §10"server 暴露的 prompt
//! 自动转换为 inline skill"。真实转换需 `prompts/get` 拉取 prompt 内容，
//! 依赖真实 transport；落地路径已固定——`McpClient::list_prompts` →
//! 每个 [`McpPromptDef`] 转为 `skills::Skill`（`source =
//! SkillSource::Mcp`（P9 已落地枚举占位，优先级最高），body =
//! prompts/get 产物文本，arguments 映射调用参数），并入 skills 装配的
//! 技能集走既有 inline 展开管道。skills 与 mcp 两 crate 无相互依赖，
//! 转换函数届时写在本模块。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use wavecode_tools::{Tool, ToolCtx, ToolOutput};

// mcp crate 的公开面经 core 再导出：cli 装配层（bootstrap 解析持有、
// `/mcp` 命令）不新增 cli→mcp 依赖边（同 memory 模块的再导出纪律，
// SPEC §3 矩阵 cli 行无 mcp）。
pub use wavecode_mcp::{
    MCP_TOOL_PREFIX, McpClient, McpError, McpPromptArgument, McpPromptDef, McpServerConfig,
    McpServerHandler, McpToolDef, McpToolOutput, NAME_SEPARATOR, parse_tool_name, tool_name,
};

/// 一个已校验的 MCP server 配置（配置表中的 `<name>` + 转换产物）。
#[derive(Debug, Clone, PartialEq)]
pub struct NamedMcpServer {
    /// server 名（`[mcp_servers.<name>]` 的键；工具命名空间的一段，
    /// 校验保证非空且不含 `__`）。
    pub name: String,
    /// transport 配置（stdio / http）。
    pub config: McpServerConfig,
}

/// config 原始表 → 已校验 server 清单：逐条目校验，非法条目转为警告
/// 跳过（不炸整体装配）。返回 `(servers, warnings)`，servers 按名排序
/// 输出稳定。
///
/// 校验规则：
/// - server 名非空且不含 `__`（命名注入的往返性，见 mcp crate
///   [`parse_tool_name`] 注释）；
/// - `command` / `url` 恰填其一（stdio / http 二选一），command 为空串
///   视同未填。
pub fn servers_from_config(
    raw: &HashMap<String, wavecode_config::McpServerRaw>,
) -> (Vec<NamedMcpServer>, Vec<String>) {
    let mut servers = Vec::new();
    let mut warnings = Vec::new();
    // 按名排序遍历，警告与清单输出稳定。
    let mut entries: Vec<_> = raw.iter().collect();
    entries.sort_by_key(|(name, _)| name.as_str());
    for (name, entry) in entries {
        match convert_entry(name, entry) {
            Ok(server) => servers.push(server),
            Err(reason) => {
                warnings.push(format!("MCP server `{name}` 配置无效（已跳过）：{reason}"))
            }
        }
    }
    (servers, warnings)
}

/// 单条目校验转换（[`servers_from_config`] 的逐条面）。
fn convert_entry(
    name: &str,
    entry: &wavecode_config::McpServerRaw,
) -> Result<NamedMcpServer, String> {
    if name.is_empty() || name.contains(NAME_SEPARATOR) {
        return Err(format!(
            "server 名 {name:?} 非法（非空且不得含 `{NAME_SEPARATOR}`——命名注入 `mcp__{{server}}__{{tool}}` 的拆分要求）"
        ));
    }
    let command = entry.command.as_deref().filter(|c| !c.trim().is_empty());
    let url = entry.url.as_deref().filter(|u| !u.trim().is_empty());
    let config = match (command, url) {
        (Some(command), None) => McpServerConfig::Stdio {
            command: command.to_owned(),
            args: entry.args.clone(),
            env: entry.env.clone(),
        },
        (None, Some(url)) => McpServerConfig::Http {
            url: url.to_owned(),
            headers: entry.headers.clone(),
        },
        (Some(_), Some(_)) => {
            return Err(
                "`command` 与 `url` 只能填其一（stdio / http 两种 transport 形态）".to_owned(),
            );
        }
        (None, None) => {
            return Err("缺少 `command`（stdio）或 `url`（http）字段".to_owned());
        }
    };
    Ok(NamedMcpServer {
        name: name.to_owned(),
        config,
    })
}

/// `/mcp` 状态行（cli REPL 与 TUI 共用同一渲染面）。
///
/// 首版状态恒为"未连接（transport 未实现）"——诚实展示，不伪造在线
/// 状态；真实 transport 落地后此处改为按连接状态机渲染。
pub fn server_status_line(server: &NamedMcpServer) -> String {
    format!(
        "{} — {} — 未连接（transport 未实现）",
        server.name,
        server.config.summary()
    )
}

/// MCP 工具桥（SPEC §10"外部工具以 `mcp__{server}__{tool}` 命名注入
/// 注册表"）：把一个 [`McpClient`] 的工具清单逐个包装为 wavecode
/// [`Tool`]，经 [`McpToolBridge::tools`] 取出后注册进 Registry。
///
/// 桥接工具共享同一个 client（`Arc`）：同一 server 的全部调用走同一
/// 连接会话。
pub struct McpToolBridge {
    server: String,
    client: Arc<dyn McpClient>,
}

impl McpToolBridge {
    /// 以 server 名与已连接 client 构造。`server` 须与注册时
    /// [`servers_from_config`] 的校验同名（本桥不重复校验——它只负责
    /// 命名拼接，非法名只会得到拆不回去的工具名，不影响其他工具）。
    pub fn new(server: impl Into<String>, client: Arc<dyn McpClient>) -> Self {
        Self {
            server: server.into(),
            client,
        }
    }

    /// 拉取 server 工具清单（`tools/list`）并逐个包装为 [`Tool`]。
    /// 失败（transport / 协议级）整体返回 Err——半个工具面不可用比
    /// 静默缺失更诚实（调用方转警告处理）。
    pub async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let defs = self.client.list_tools().await?;
        Ok(defs
            .into_iter()
            .map(|def| {
                Arc::new(BridgedTool {
                    client: self.client.clone(),
                    remote_name: def.name.clone(),
                    full_name: tool_name(&self.server, &def.name),
                    // 描述缺省给回退文案（协议中可选；空描述对模型不友好）。
                    description: def.description.unwrap_or_else(|| {
                        format!(
                            "MCP tool `{}` from server `{}` (no description provided)",
                            def.name, self.server
                        )
                    }),
                    input_schema: def.input_schema,
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

/// 单个 MCP 工具的 [`Tool`] 包装（[`McpToolBridge`] 产物）。
struct BridgedTool {
    client: Arc<dyn McpClient>,
    /// server 侧原始名（`call_tool` 用）。
    remote_name: String,
    /// 注册表名（`mcp__{server}__{tool}`）。
    full_name: String,
    description: String,
    input_schema: Value,
}

#[async_trait::async_trait]
impl Tool for BridgedTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn is_read_only(&self) -> bool {
        // MCP 工具的副作用面无法静态判定，取保守默认：非只读 → 进串行
        // 段过 sandbox 审批门（模块注释）。只读标注能力待真实 transport
        // 落地后按 server 通告的 annotations 细化。
        false
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> wavecode_tools::Result<ToolOutput> {
        match self.client.call_tool(&self.remote_name, input).await {
            // 业务结果（含 server 侧的 is_error）原样回灌模型。
            Ok(out) => Ok(ToolOutput {
                content: out.content,
                is_error: out.is_error,
            }),
            // transport / 协议级故障以 is_error 回灌模型（可自我纠正或
            // 换工具重试），不走 Err——Err 留给工具实现级故障，MCP 连接
            // 故障对会话而言是可呈现的业务事件。
            Err(e) => Ok(ToolOutput {
                content: format!("MCP call failed: {e}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_stdio() -> wavecode_config::McpServerRaw {
        wavecode_config::McpServerRaw {
            command: Some("npx".into()),
            args: vec!["@playwright/mcp@latest".into()],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
        }
    }

    /// 配置转换：stdio / http 两种形态；清单按名排序；警告为空。
    #[test]
    fn servers_from_config_converts_both_forms() {
        let mut http = raw_stdio();
        http.command = None;
        http.url = Some("https://mcp.example.com/sse".into());
        let raw = HashMap::from([
            ("z-http".to_owned(), http),
            ("a-stdio".to_owned(), raw_stdio()),
        ]);
        let (servers, warnings) = servers_from_config(&raw);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["a-stdio", "z-http"],
            "按名排序输出稳定"
        );
        assert_eq!(
            servers[0].config,
            McpServerConfig::Stdio {
                command: "npx".into(),
                args: vec!["@playwright/mcp@latest".into()],
                env: HashMap::new(),
            }
        );
        assert_eq!(servers[1].config.transport_kind(), "http");
    }

    /// 配置转换校验：command/url 双填、双缺、空串 command、非法 server
    /// 名——逐条警告跳过，不影响合法条目。
    #[test]
    fn servers_from_config_skips_invalid_entries_with_warnings() {
        let mut both = raw_stdio();
        both.url = Some("https://x".into());
        let mut neither = raw_stdio();
        neither.command = None;
        let mut empty_cmd = raw_stdio();
        empty_cmd.command = Some("  ".into());
        let raw = HashMap::from([
            ("both".to_owned(), both),
            ("neither".to_owned(), neither),
            ("empty".to_owned(), empty_cmd),
            ("bad__name".to_owned(), raw_stdio()),
            ("".to_owned(), raw_stdio()),
            ("ok".to_owned(), raw_stdio()),
        ]);
        let (servers, warnings) = servers_from_config(&raw);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "ok");
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.contains("已跳过")));
    }

    /// 状态行：首版恒为"未连接（transport 未实现）"——诚实展示。
    #[test]
    fn status_line_is_honestly_disconnected() {
        let server = NamedMcpServer {
            name: "playwright".into(),
            config: McpServerConfig::Stdio {
                command: "npx".into(),
                args: vec!["@playwright/mcp@latest".into()],
                env: HashMap::new(),
            },
        };
        let line = server_status_line(&server);
        assert!(line.contains("playwright"));
        assert!(line.contains("stdio: npx @playwright/mcp@latest"));
        assert!(line.contains("未连接（transport 未实现）"));
    }

    /// 测试用假 client：单工具 `ping`（无描述 → 回退文案），记录调用。
    struct FakeClient {
        calls: std::sync::Mutex<Vec<(String, Value)>>,
    }

    #[async_trait::async_trait]
    impl McpClient for FakeClient {
        async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
            Ok(vec![McpToolDef {
                name: "ping".into(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
            }])
        }

        async fn call_tool(&self, name: &str, input: Value) -> Result<McpToolOutput, McpError> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_owned(), input.clone()));
            if name == "boom" {
                return Err(McpError::Transport("连接断开".into()));
            }
            Ok(McpToolOutput {
                content: format!("pong: {input}"),
                is_error: false,
            })
        }
    }

    /// 桥接面：命名注入前缀、非只读默认（审批管道）、描述回退、
    /// execute 转发原始名与输入。
    #[tokio::test]
    async fn bridge_wraps_client_tools() {
        let client = Arc::new(FakeClient {
            calls: std::sync::Mutex::new(vec![]),
        });
        let bridge = McpToolBridge::new("srv", client.clone());
        let tools = bridge.tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name(), "mcp__srv__ping");
        assert!(!tool.is_read_only(), "MCP 工具保守默认非只读 → 审批管道");
        assert!(
            tool.description().contains("no description provided"),
            "描述缺省回退：{}",
            tool.description()
        );
        // execute 转发 server 侧原始名与输入，结果原样回灌。
        let ctx = ToolCtx {
            cwd: std::path::PathBuf::from("/tmp"),
            deny_env: vec![],
        };
        let out = tool
            .execute(serde_json::json!({"x": 1}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, r#"pong: {"x":1}"#);
        assert_eq!(
            client.calls.lock().unwrap().as_slice(),
            &[("ping".to_owned(), serde_json::json!({"x": 1}))],
            "call_tool 收到的是原始名（不含 mcp__ 前缀）"
        );
    }
}
