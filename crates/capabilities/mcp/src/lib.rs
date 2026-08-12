//! wavecode-mcp — Model Context Protocol 双向支持（P9 接口边界）。
//!
//! - 客户端：stdio 与 streamable-http（含 OAuth）transport，外部工具以
//!   `mcp__{server}__{tool}` 命名空间注入工具注册表；
//! - 服务端：把 WaveCode 自身能力暴露为 MCP server，供其他 agent /
//!   IDE 调用。
//!
//! **P9 范围（诚实声明）**：本 crate 只定义接口边界——[`McpClient`] /
//! [`McpServerHandler`] trait、工具与 prompt 的数据类型、命名约定、
//! [`McpServerConfig`] 配置类型。**不实现真实 transport**：stdio /
//! streamable-http 连接为后续迭代（实现时对齐 rmcp 官方 crate 的能力面，
//! 本 trait 面按 MCP 协议的 `tools/list`、`tools/call`、`prompts/list`
//! 方法设计，避免对接时返工；resources / 通知等其余协议面留待需要时
//! 扩展）。工具桥接（MCP 工具 → wavecode `Tool`）与配置解析的编排在
//! core 侧（core→mcp 边由 SPEC §3 矩阵允许；mcp 自身无 workspace 内
//! 依赖）。

use std::collections::HashMap;

use serde_json::Value;

/// MCP 工具注入注册表时的命名前缀（SPEC §10）：完整工具名为
/// `mcp__{server}__{tool}`，与内置工具天然不冲突。
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// server 名与 tool 名之间的分隔符（[`parse_tool_name`] 按首个分隔符
/// 拆分——tool 名自身含 `__` 不影响往返，server 名则不得含 `__`，
/// 由装配层（core 配置转换）校验）。
pub const NAME_SEPARATOR: &str = "__";

/// 拼接注册表工具名：`mcp__{server}__{tool}`。
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server}{NAME_SEPARATOR}{tool}")
}

/// 拆分注册表工具名 → `(server, tool)`；非 `mcp__` 前缀、缺分隔符或
/// 任一段为空均返回 `None`。
pub fn parse_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(MCP_TOOL_PREFIX)?;
    let (server, tool) = rest.split_once(NAME_SEPARATOR)?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// MCP server 暴露的工具定义（对齐协议 `tools/list` 结果项）。
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    /// server 侧原始工具名（**不含** `mcp__` 前缀；`call_tool` 用此名）。
    pub name: String,
    /// 能力描述（协议中可选；桥接层缺省时给回退文案）。
    pub description: Option<String>,
    /// 参数的 JSON Schema（`inputSchema`，注入采样请求）。
    pub input_schema: Value,
}

/// MCP 工具调用结果（对齐协议 `tools/call` 结果）。
///
/// 首版为扁平文本形态：协议的 content 块数组（text / image / resource）
/// 在真实 transport 落地时再结构化，桥接层届时把多块拼接进 `content`。
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolOutput {
    /// 人类可读输出（回灌模型）。
    pub content: String,
    /// 业务失败标记（协议 `isError`；与 transport/协议级故障区分——后者
    /// 走 [`McpError`]）。
    pub is_error: bool,
}

/// MCP prompt 的参数定义（协议 `prompts/list` 的 `arguments` 项）。
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptArgument {
    /// 参数名。
    pub name: String,
    /// 参数描述（可选）。
    pub description: Option<String>,
    /// 是否必填。
    pub required: bool,
}

/// MCP server 暴露的 prompt 定义（对齐协议 `prompts/list` 结果项）。
///
/// SPEC §10 要求 prompt 自动转换为 inline skill：转换需 `prompts/get`
/// 拉取内容，依赖真实 transport，随后续迭代在 core 侧接线（skills crate
/// 已落地 `SkillSource::Mcp` 来源占位）。
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptDef {
    /// prompt 名。
    pub name: String,
    /// 描述（可选）。
    pub description: Option<String>,
    /// 参数清单。
    pub arguments: Vec<McpPromptArgument>,
}

/// MCP 客户端错误。
///
/// 仅 transport / 协议级故障走 `Err`；工具业务失败由 server 经
/// [`McpToolOutput::is_error`] 表达，实现不得把业务失败包装为 `Err`
///（与 wavecode `Tool::execute` 的错误契约对齐）。
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// transport 层故障（连接断开、进程退出、HTTP 错误、超时等）。
    #[error("MCP transport 错误: {0}")]
    Transport(String),
    /// 协议层故障（初始化握手失败、响应无法解析、server 返回 JSON-RPC
    /// 错误等）。
    #[error("MCP 协议错误: {0}")]
    Protocol(String),
}

/// MCP 客户端：与一个外部 MCP server 的会话面。
///
/// 方法对齐协议能力面（`tools/list` / `tools/call` / `prompts/list`）；
/// 对象安全 + Send + Sync，桥接层以 `Arc<dyn McpClient>` 持有。
///
/// **实现状态**：真实实现（stdio spawn 子进程 / streamable-http，经
/// rmcp crate）为后续迭代；P9 的验证面是 mock 实现（见 core 侧
/// `McpToolBridge` 的测试）。连接失败的指数退避重连（SPEC §10）属
/// transport 实现细节，不进 trait 面。
#[async_trait::async_trait]
pub trait McpClient: Send + Sync {
    /// 列出 server 暴露的全部工具（`tools/list`）。
    async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError>;
    /// 调用工具（`tools/call`；`name` 为 server 侧原始名，不含
    /// `mcp__{server}__` 前缀——前缀只是注册表命名空间）。
    async fn call_tool(&self, name: &str, input: Value) -> Result<McpToolOutput, McpError>;
    /// 列出 server 暴露的 prompt（`prompts/list`）。可选能力：server
    /// 不支持 prompts 时默认实现返回空清单。
    async fn list_prompts(&self) -> Result<Vec<McpPromptDef>, McpError> {
        Ok(vec![])
    }
}

/// MCP server 配置（SPEC §13 `[mcp_servers.<name>]` 段的两种形态）。
///
/// 与 config crate 原始表的转换在 core 侧（config 无 workspace 内依赖，
/// 只做原始解析；二选一校验见 core 的 `servers_from_config`）。
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerConfig {
    /// stdio transport：spawn 子进程，经标准输入输出收发 JSON-RPC。
    Stdio {
        /// 可执行命令（如 `npx`）。
        command: String,
        /// 命令参数。
        args: Vec<String>,
        /// 追加注入子进程的环境变量（在继承环境之上覆盖）。
        env: HashMap<String, String>,
    },
    /// streamable-http transport（含 OAuth 2.0 + PKCE，SPEC §10）。
    Http {
        /// server endpoint URL。
        url: String,
        /// 追加的请求头（如 Authorization 占位；OAuth 落地前手工配置）。
        headers: HashMap<String, String>,
    },
}

impl McpServerConfig {
    /// transport 类型名（`/mcp` 状态展示与诊断用）。
    pub fn transport_kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }

    /// 单行摘要（`/mcp` 展示面）：stdio 为 command + args 拼接，http 为
    /// URL。env / headers 不展示（可能含敏感值）。
    pub fn summary(&self) -> String {
        match self {
            Self::Stdio { command, args, .. } => {
                let mut line = command.clone();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                format!("stdio: {line}")
            }
            Self::Http { url, .. } => format!("http: {url}"),
        }
    }
}

/// MCP 服务端占位 trait（SPEC §10 服务端：`wavecode mcp serve` 经 stdio
/// 暴露 WaveCode 工具集与会话能力，供 IDE / 其他 agent 调用）。
///
/// **实现状态**：P10 之后实现——届时把 Registry 的工具面适配为本 trait
/// 的实现，再经 rmcp 的 server 端暴露；鉴权（默认仅本机、client 白名单）
/// 在 serve 装配层完成，不进 trait 面。方法面与 [`McpClient`] 镜像是
/// 刻意的：同一协议的两个方向。
#[async_trait::async_trait]
pub trait McpServerHandler: Send + Sync {
    /// 回应 `tools/list`。
    async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError>;
    /// 回应 `tools/call`。
    async fn call_tool(&self, name: &str, input: Value) -> Result<McpToolOutput, McpError>;
    /// 回应 `prompts/list`（可选能力，默认空清单）。
    async fn list_prompts(&self) -> Result<Vec<McpPromptDef>, McpError> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命名拼接与拆分往返；tool 名含 `__` 不破坏往返（按首个分隔符拆）。
    #[test]
    fn tool_name_roundtrip() {
        assert_eq!(tool_name("playwright", "click"), "mcp__playwright__click");
        assert_eq!(
            parse_tool_name("mcp__playwright__click"),
            Some(("playwright", "click"))
        );
        assert_eq!(
            parse_tool_name("mcp__srv__a__b"),
            Some(("srv", "a__b")),
            "tool 段含分隔符时按首个分隔符拆分，server 段不受影响"
        );
    }

    /// 拆分拒绝非法形态：无前缀 / 缺分隔符 / server 或 tool 段为空。
    #[test]
    fn parse_tool_name_rejects_invalid() {
        assert_eq!(parse_tool_name("read_file"), None);
        assert_eq!(parse_tool_name("mcp__noserver"), None);
        assert_eq!(parse_tool_name("mcp____tool"), None);
        assert_eq!(parse_tool_name("mcp__srv__"), None);
        assert_eq!(parse_tool_name(""), None);
    }

    /// 配置类型：transport 类型名与单行摘要（env / headers 不进摘要）。
    #[test]
    fn server_config_kind_and_summary() {
        let stdio = McpServerConfig::Stdio {
            command: "npx".into(),
            args: vec!["@playwright/mcp@latest".into()],
            env: HashMap::from([("SECRET".into(), "x".into())]),
        };
        assert_eq!(stdio.transport_kind(), "stdio");
        assert_eq!(stdio.summary(), "stdio: npx @playwright/mcp@latest");
        assert!(!stdio.summary().contains("SECRET"));

        let http = McpServerConfig::Http {
            url: "https://mcp.example.com/sse".into(),
            headers: HashMap::from([("Authorization".into(), "Bearer x".into())]),
        };
        assert_eq!(http.transport_kind(), "http");
        assert_eq!(http.summary(), "http: https://mcp.example.com/sse");
        assert!(!http.summary().contains("Bearer"));
    }
}
