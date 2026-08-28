//! wavecode-config — TOML 配置加载与 provider 解析。
//!
//! M1 阶段实现：加载用户级 `~/.wavecode/config.toml`，解析 `model` /
//! `model_provider` / `model_providers` 配置段，并解析当前 provider 的
//! api key（优先级：`env_key` 指向的环境变量 > 内联 `api_key`）。
//!
//! 规划中的分层合并（CLI 参数 > 项目级 `.wavecode/config.toml` > 用户级 >
//! 内置默认值）与 `profiles` 等能力将在后续里程碑落地；`mcp_servers`
//! 原始解析已于 P9 落地（SPEC §10/§13）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod hooks;
mod mcp;
mod provider;

/// 顶层配置。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    pub model: String,
    pub model_provider: String,
    #[serde(default)]
    pub model_providers: HashMap<String, ProviderConfig>,
    /// 权限模式（SPEC §12 四档字符串：default / plan / acceptEdits /
    /// bypassPermissions）；缺省 None，由装配层回退 default。
    /// P2 仅落地此单字段；profiles / projects 覆盖等分层合并留后续里程碑。
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// hooks 配置（SPEC §9）：`[hooks.<EventPoint>]` 表或表数组。
    /// config 层只做原始解析（config 无 workspace 内依赖，SPEC §3 矩阵）；
    /// 事件点合法性校验与执行语义由 core 经 hooks crate 落地。
    #[serde(default)]
    pub hooks: HashMap<String, HookRuleSet>,
    /// MCP server 配置（SPEC §10/§13，P9）：`[mcp_servers.<name>]` 表。
    /// config 层只做原始解析；stdio（command）与 http（url）二选一的
    /// 合法性校验由 core 经 mcp crate 转换时完成。
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerRaw>,
}

/// 用户主目录：`USERPROFILE`（Windows）优先，兜底 `HOME`；两者皆未
/// 设置时返回 `None`（调用方显式降级依赖 home 的能力，不猜相对路径）。
/// 全 workspace 单点定义——memory / core / cli 经本函数取，不再各自
/// `var_os` 一份。
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

pub use hooks::{ConfigError, HookRule, HookRuleSet};
pub use mcp::McpServerRaw;
pub use provider::{ProviderConfig, ProviderKind};

impl Config {
    /// 加载用户级 `~/.wavecode/config.toml`；不存在返回 [`ConfigError::NotFound`]。
    ///
    /// home 目录取 `USERPROFILE`（Windows），兜底 `HOME`；两者皆未设置时
    /// 按相对路径查找，实际效果等同于 NotFound。
    pub fn load() -> Result<Self, ConfigError> {
        // home 未设置时取空串 → 相对路径查找，实际效果等同于 NotFound
        //（与既有行为一致；调用方可先经 [`home_dir`] 显式探测）。
        let home = home_dir().unwrap_or_default();
        Self::load_from(&Path::new(&home).join(".wavecode").join("config.toml"))
    }

    /// 从指定路径加载配置。
    ///
    /// 文件不存在（或不可读）→ [`ConfigError::NotFound`]；TOML 解析失败 →
    /// [`ConfigError::Parse`]。
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        // M1 约定：读取失败（含权限问题）与文件不存在一样按 NotFound 处理，
        // 错误信息中带路径，足以定位问题。
        let content =
            std::fs::read_to_string(path).map_err(|_| ConfigError::NotFound(path.to_path_buf()))?;
        Ok(toml::from_str(&content)?)
    }

    /// 解析当前 provider；api key 优先级：`env_key` 指向的环境变量 > 内联 `api_key`
    ///（环境变量存在但为空串时视为未设置）。
    pub fn resolve_provider(&self) -> Result<(&ProviderConfig, String), ConfigError> {
        let provider = self
            .model_providers
            .get(&self.model_provider)
            .ok_or_else(|| ConfigError::MissingProvider(self.model_provider.clone()))?;

        let key = provider
            .env_key
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            // `export KEY=`（空串）不算有效 key：回落内联 api_key，
            // 而非带空 key 发请求。
            .filter(|k| !k.is_empty())
            .or_else(|| provider.api_key.clone())
            .ok_or_else(|| ConfigError::MissingApiKey(self.model_provider.clone()))?;

        Ok((provider, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML_OK: &str = r#"
model = "MiniMax-M3"
model_provider = "minimax"

[model_providers.minimax]
type = "anthropic"
base_url = "https://api.minimaxi.com/anthropic"
env_key = "TEST_KEY"
"#;

    #[test]
    fn parses_minimax_config() {
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        assert_eq!(cfg.model, "MiniMax-M3");
        assert_eq!(cfg.model_provider, "minimax");
        let prov = &cfg.model_providers["minimax"];
        assert_eq!(prov.kind, ProviderKind::Anthropic);
        assert_eq!(prov.base_url, "https://api.minimaxi.com/anthropic");
        assert_eq!(prov.context_window(), 200_000);
        assert_eq!(prov.max_output_tokens(), 8192);
    }

    /// permission_mode（P2）：可选字段——缺失为 None，配置了原样保留
    ///（合法性由装配层校验并回退 default，config 层不拒绝未知字符串）。
    #[test]
    fn permission_mode_is_optional() {
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        assert_eq!(cfg.permission_mode, None);
        let cfg: Config = toml::from_str(&TOML_OK.replacen(
            "model = \"MiniMax-M3\"",
            "model = \"MiniMax-M3\"\npermission_mode = \"plan\"",
            1,
        ))
        .unwrap();
        assert_eq!(cfg.permission_mode, Some("plan".to_owned()));
    }

    /// hooks 配置（P7，SPEC §9）：单表与表数组两种形态；缺省为空表。
    /// 事件点合法性不在 config 层校验（无 workspace 内依赖，由 core 转换时校验）。
    #[test]
    fn hooks_parse_single_and_array_forms() {
        let toml = format!(
            r#"{TOML_OK}
[hooks.PreToolUse]
matcher = "shell"
command = "./scripts/check.sh"
timeout_ms = 10000
once = true

[[hooks.PostToolUse]]
command = "cargo fmt"

[[hooks.PostToolUse]]
matcher = "write_file|edit_file"
command = "cargo clippy"
"#
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        let pre = cfg.hooks["PreToolUse"].rules();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].matcher.as_deref(), Some("shell"));
        assert_eq!(pre[0].command, "./scripts/check.sh");
        assert_eq!(pre[0].timeout_ms, Some(10000));
        assert_eq!(pre[0].once, Some(true));
        let post = cfg.hooks["PostToolUse"].rules();
        assert_eq!(post.len(), 2);
        assert_eq!(post[1].matcher.as_deref(), Some("write_file|edit_file"));
        assert_eq!(post[0].timeout_ms, None);
        // 缺省为空表。
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        assert!(cfg.hooks.is_empty());
    }

    /// mcp_servers 配置（P9，SPEC §13）：stdio（command+args+env）与
    /// http（url+headers）两种形态；缺省为空表；二选一校验不在本层。
    #[test]
    fn mcp_servers_parse_stdio_and_http_forms() {
        let toml = format!(
            r#"{TOML_OK}
[mcp_servers.playwright]
command = "npx"
args = ["@playwright/mcp@latest"]

[mcp_servers.local]
command = "python"
args = ["server.py"]
env = {{ API_TOKEN = "x" }}

[mcp_servers.remote]
url = "https://mcp.example.com/sse"
headers = {{ Authorization = "Bearer t" }}
"#
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        let pw = &cfg.mcp_servers["playwright"];
        assert_eq!(pw.command.as_deref(), Some("npx"));
        assert_eq!(pw.args, vec!["@playwright/mcp@latest"]);
        assert!(pw.env.is_empty() && pw.url.is_none() && pw.headers.is_empty());
        let local = &cfg.mcp_servers["local"];
        assert_eq!(local.env["API_TOKEN"], "x");
        let remote = &cfg.mcp_servers["remote"];
        assert_eq!(remote.url.as_deref(), Some("https://mcp.example.com/sse"));
        assert_eq!(remote.headers["Authorization"], "Bearer t");
        assert!(remote.command.is_none() && remote.args.is_empty());
        // 缺省为空表。
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    // TEST_KEY 是进程级状态，依赖它的测试需互斥执行。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn missing_api_key_is_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        // env 未设置且无内联 api_key → MissingApiKey
        unsafe { std::env::remove_var("TEST_KEY") };
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        assert!(matches!(
            cfg.resolve_provider(),
            Err(ConfigError::MissingApiKey(_))
        ));
    }

    #[test]
    fn env_key_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("TEST_KEY", "k-from-env") };
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = Some("k-inline".into());
        let (_p, key) = cfg.resolve_provider().unwrap();
        assert_eq!(key, "k-from-env");
        unsafe { std::env::remove_var("TEST_KEY") };
    }

    #[test]
    fn inline_key_fallback() {
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().env_key = None;
        cfg.model_providers.get_mut("minimax").unwrap().api_key = Some("k-inline".into());
        let (_p, key) = cfg.resolve_provider().unwrap();
        assert_eq!(key, "k-inline");
    }

    #[test]
    fn empty_env_key_falls_back_to_inline() {
        let _guard = ENV_LOCK.lock().unwrap();
        // export KEY=（空串）：视为未设置，回落内联 api_key
        unsafe { std::env::set_var("TEST_KEY", "") };
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = Some("k-inline".into());
        let (_p, key) = cfg.resolve_provider().unwrap();
        assert_eq!(key, "k-inline");
        // 空 env 且无内联 api_key → MissingApiKey
        cfg.model_providers.get_mut("minimax").unwrap().api_key = None;
        assert!(matches!(
            cfg.resolve_provider(),
            Err(ConfigError::MissingApiKey(_))
        ));
        unsafe { std::env::remove_var("TEST_KEY") };
    }

    #[test]
    fn malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "model = [unclosed").unwrap();
        assert!(matches!(
            Config::load_from(&path),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn missing_required_field_is_parse_error() {
        // 缺必填字段 model：serde 反序列化失败 → Parse
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nomodel.toml");
        std::fs::write(&path, r#"model_provider = "minimax""#).unwrap();
        assert!(matches!(
            Config::load_from(&path),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn missing_provider_is_error() {
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_provider = "nonexistent".into();
        assert!(matches!(
            cfg.resolve_provider(),
            Err(ConfigError::MissingProvider(_))
        ));
    }

    #[test]
    fn load_from_missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert!(matches!(
            Config::load_from(&missing),
            Err(ConfigError::NotFound(_))
        ));
    }

    #[test]
    fn provider_config_debug_redacts_api_key() {
        const REAL_KEY: &str = "sk-ant-api03-real-looking-secret-key";
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = Some(REAL_KEY.into());

        // ProviderConfig 自身与包含它的 Config，Debug 都不得泄露 key 原文。
        let dbg_provider = format!("{:?}", cfg.model_providers["minimax"]);
        let dbg_config = format!("{cfg:?}");
        for output in [&dbg_provider, &dbg_config] {
            assert!(!output.contains(REAL_KEY), "Debug 泄露 api key: {output}");
            assert!(output.contains("***"), "Debug 应含脱敏标记: {output}");
        }
        // 其余字段正常显示。
        assert!(dbg_provider.contains("https://api.minimaxi.com/anthropic"));

        // api_key 为 None 时显示 None，同样无泄露。
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = None;
        let dbg_none = format!("{:?}", cfg.model_providers["minimax"]);
        assert!(dbg_none.contains("api_key: None"));
    }
}
