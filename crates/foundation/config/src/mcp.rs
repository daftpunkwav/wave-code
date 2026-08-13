//! MCP server 原始配置（阶段 5 拆分自 lib.rs）：[mcp_servers] 段。

use super::*;

/// 单个 MCP server 的原始配置（SPEC §13 `[mcp_servers.<name>]` 段字段，
/// P9）。stdio 形态填 `command`（+ 可选 `args` / `env`），http 形态填
/// `url`（+ 可选 `headers`）；两种形态的二选一校验不在本层（config 无
/// workspace 内依赖，同 hooks 的原始解析纪律）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerRaw {
    /// stdio transport 的可执行命令（stdio 形态必填）。
    pub command: Option<String>,
    /// 命令参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 追加注入子进程的环境变量。
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// streamable-http endpoint（http 形态必填）。
    pub url: Option<String>,
    /// 追加的请求头。
    #[serde(default)]
    pub headers: HashMap<String, String>,
}
