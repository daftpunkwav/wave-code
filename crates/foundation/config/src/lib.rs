//! wavecode-config - TOML config loading and provider resolution.
//!
//! M1 implementation: loads the user-level `~/.wavecode/config.toml`, parses the `model` /
//! `model_provider` / `model_providers` sections, and resolves the current provider's
//! api key (priority: the env var pointed to by `env_key` > inline `api_key`).
//!
//! Resolution is single-file: one config over built-in defaults. There are
//! no layered merges (no CLI-arg layer, no project-level config, no
//! profiles). `[permissions]` is only ever read from the user-level file,
//! never from a repo-local one: the agent can write a repo-local file, so
//! widened authority never loads from it (see the module doc in
//! `permissions.rs`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod hooks;
mod mcp;
mod model_catalog;
mod permissions;
mod provider;

/// Top-level config.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    pub model: String,
    pub model_provider: String,
    #[serde(default)]
    pub model_providers: HashMap<String, ProviderConfig>,
    /// Permission mode (three wire names: plan / auto / wave, matching
    /// `wavecode_protocol::PermissionMode`; the legacy aliases guarded /
    /// default / acceptEdits / bypassPermissions / yolo still load and map
    /// onto their successor). None by default; the assembly layer falls
    /// back to auto.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Authored permission rules (`[permissions] allow` / `deny`, sandbox
    /// rule syntax). The config layer only does raw parsing; entry validity
    /// is checked by the assembly layer through the sandbox crate.
    #[serde(default)]
    pub permissions: PermissionsConfig,
    /// hooks config: a `[hooks.<EventPoint>]` table or array of tables.
    /// The config layer only does raw parsing (config has no in-workspace dependencies);
    /// event-point validity checks and execution semantics land in core via the hooks crate.
    #[serde(default)]
    pub hooks: HashMap<String, HookRuleSet>,
    /// MCP server config: `[mcp_servers.<name>]` tables.
    /// The config layer only does raw parsing; the either-or (stdio `command` vs http `url`)
    /// validity check happens in core during conversion via the mcp crate.
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerRaw>,
    /// Selectable model catalog for the `/model` picker: `[models.<alias>]`
    /// tables mapping a display alias onto a provider id and wire model
    /// name. Pure data here; unknown provider ids degrade with a warning
    /// at assembly time.
    #[serde(default)]
    pub models: HashMap<String, ModelEntry>,
    /// Alias into the `[models]` table naming the cheap model used for
    /// routine side sessions: `/btw` answers sample through it instead of
    /// the primary model (cheap side-session cost steering). An unknown
    /// alias degrades with a doctor finding; unset means side sessions
    /// inherit the primary model.
    #[serde(default)]
    pub secondary_model: Option<String>,
    /// Tool-round ceiling per turn (`max_tool_rounds`): a turn that
    /// reaches it stops with `MaxToolRounds`. Unset uses the runner
    /// default (256 — wavecode targets super-long-horizon work); an open
    /// session goal re-arms the ceiling up to 7 more times (8 ceilings
    /// per turn). `0` stops every turn before its first tool round.
    #[serde(default)]
    pub max_tool_rounds: Option<u32>,
}

/// One selectable model entry from the `[models]` config table.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelEntry {
    /// Provider id under `model_providers` this model samples through.
    pub provider: String,
    /// Wire model name sent to the provider.
    pub model: String,
    /// Optional reasoning-effort default (e.g. `low`) applied to the
    /// provider client when this entry is selected.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// User home directory: `USERPROFILE` (Windows) first, `HOME` as fallback; when neither
/// is set, returns `None` (callers explicitly degrade home-dependent behavior instead of guessing a relative path).
/// Single definition point for the whole workspace - memory / core / cli all go through
/// this function instead of each reading `var_os` on their own.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

pub use hooks::{ConfigError, HookRule, HookRuleSet};
pub use mcp::McpServerRaw;
pub use model_catalog::{
    ApiKind, CatalogError, ModalitiesSpec, ModelCatalog, ModelSpec, ReasoningSpec,
};
pub use permissions::PermissionsConfig;
pub use provider::{
    DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT_TOKENS, ProviderConfig, ProviderKind,
};

impl Config {
    /// Loads the user-level `~/.wavecode/config.toml`; returns [`ConfigError::NotFound`] when absent.
    ///
    /// The home directory comes from `USERPROFILE` (Windows), falling back to `HOME`; when neither is set,
    /// returns NotFound explicitly (with the relative-looking `.wavecode/config.toml` as the error path) -
    /// never falls back to a relative-path lookup: an empty home would let the project-local
    /// `.wavecode/config.toml` be silently loaded as user-level config (source confusion, and a silent downgrade).
    pub fn load() -> Result<Self, ConfigError> {
        let Some(home) = home_dir() else {
            return Err(ConfigError::NotFound(
                Path::new(".wavecode").join("config.toml"),
            ));
        };
        Self::load_from(&Path::new(&home).join(".wavecode").join("config.toml"))
    }

    /// Loads config from the given path.
    ///
    /// Missing (or unreadable) file maps to [`ConfigError::NotFound`]; TOML parse failure maps to
    /// [`ConfigError::Parse`].
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        // M1 convention: read failures (including permission issues) are treated as NotFound,
        // just like a missing file; the path in the error is enough to locate the problem.
        let content =
            std::fs::read_to_string(path).map_err(|_| ConfigError::NotFound(path.to_path_buf()))?;
        Ok(toml::from_str(&content)?)
    }

    /// Resolves the current provider; api key priority: the env var pointed to by `env_key` > inline `api_key`
    /// (an env var that exists but is empty counts as unset).
    pub fn resolve_provider(&self) -> Result<(&ProviderConfig, String), ConfigError> {
        self.resolve_named_provider(&self.model_provider.clone())
    }

    /// Resolves a named provider (fallback chain entries); same key
    /// priority as [`Self::resolve_provider`]. Unknown names and missing
    /// keys error so callers can warn-and-skip without touching the primary.
    pub fn resolve_named_provider(
        &self,
        name: &str,
    ) -> Result<(&ProviderConfig, String), ConfigError> {
        let provider = self
            .model_providers
            .get(name)
            .ok_or_else(|| ConfigError::MissingProvider(name.to_string()))?;

        let key = provider
            .env_key
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            // `export KEY=` (empty string) is not a valid key: fall back to the inline api_key
            // instead of sending a request with an empty key.
            // Whitespace-only values are likewise not valid keys for any API:
            // an env var containing only blanks falls back to the inline key.
            .filter(|k| !k.trim().is_empty())
            .or_else(|| provider.api_key.clone())
            // An inline key of only blanks is also rejected (MissingApiKey
            // instead of sending a blank key that the API would refuse).
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| ConfigError::MissingApiKey(name.to_string()))?;

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
        // The models catalog is optional and empty by default.
        assert!(cfg.models.is_empty());
        // The round ceiling is optional; unset defers to the runner
        // default.
        assert_eq!(cfg.max_tool_rounds, None);
    }

    /// `max_tool_rounds` lowers (or raises) the per-turn tool-round
    /// ceiling from the config file. The key must sit before the first
    /// TOML table header to land on the top-level config.
    #[test]
    fn max_tool_rounds_key_parses() {
        let cfg: Config = toml::from_str(&format!("max_tool_rounds = 64\n{TOML_OK}")).unwrap();
        assert_eq!(cfg.max_tool_rounds, Some(64));
    }

    /// All three wire dialects parse from `type`, including the aliases for
    /// the Responses API, and only the OpenAI-protocol kinds carry a
    /// switchable reasoning effort.
    #[test]
    fn provider_kinds_parse_every_wire_dialect() {
        let kind_of = |ty: &str| -> ProviderKind {
            let text = format!(
                r#"
model = "m"
model_provider = "p"

[model_providers.p]
type = "{ty}"
base_url = "https://example.test/v1"
"#
            );
            toml::from_str::<Config>(&text).unwrap().model_providers["p"].kind
        };
        assert_eq!(kind_of("anthropic"), ProviderKind::Anthropic);
        // Both the serde-canonical and the natural spelling parse, so a
        // hand-written config never trips on the kebab-case split.
        assert_eq!(
            kind_of("open-ai-compatible"),
            ProviderKind::OpenAiCompatible
        );
        assert_eq!(kind_of("openai-compatible"), ProviderKind::OpenAiCompatible);
        assert_eq!(kind_of("open-ai-responses"), ProviderKind::OpenAiResponses);
        assert_eq!(kind_of("openai-responses"), ProviderKind::OpenAiResponses);
        // Aliases resolve to the same kind.
        assert_eq!(kind_of("openai_responses"), ProviderKind::OpenAiResponses);
        assert_eq!(kind_of("responses"), ProviderKind::OpenAiResponses);

        assert!(ProviderKind::OpenAiCompatible.carries_reasoning_effort());
        assert!(ProviderKind::OpenAiResponses.carries_reasoning_effort());
        assert!(!ProviderKind::Anthropic.carries_reasoning_effort());

        // An unknown dialect stays a config error instead of silently
        // falling back to another wire format.
        let unknown = toml::from_str::<Config>(
            r#"
model = "m"
model_provider = "p"

[model_providers.p]
type = "gemini"
base_url = "https://example.test/v1"
"#,
        );
        assert!(unknown.is_err());
    }

    /// [models] table: alias -> { provider, model, optional reasoning_effort }.
    #[test]
    fn models_catalog_parses_aliases() {
        let cfg: Config = toml::from_str(&format!(
            r#"{TOML_OK}
[models.fast]
provider = "minimax"
model = "MiniMax-M2.5"

[models.deep]
provider = "minimax"
model = "deepseek-v4"
reasoning_effort = "high"
"#
        ))
        .unwrap();
        let fast = &cfg.models["fast"];
        assert_eq!(fast.provider, "minimax");
        assert_eq!(fast.model, "MiniMax-M2.5");
        assert_eq!(fast.reasoning_effort, None);
        assert_eq!(cfg.models["deep"].reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn routing_fields_default_to_no_fallback() {
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        let prov = &cfg.model_providers["minimax"];
        assert!(prov.fallback_providers.is_empty());
        assert_eq!(prov.reasoning_effort, None);
        let routed: Config = toml::from_str(&TOML_OK.replacen(
            "env_key = \"TEST_KEY\"",
            "env_key = \"TEST_KEY\"\nfallback_providers = [\"backup\"]\nreasoning_effort = \"low\"",
            1,
        ))
        .unwrap();
        let prov = &routed.model_providers["minimax"];
        assert_eq!(prov.fallback_providers, vec!["backup".to_string()]);
        assert_eq!(prov.reasoning_effort.as_deref(), Some("low"));
    }

    /// permission_mode optional field - missing means None, a configured value is kept as-is
    /// (validity is checked by the assembly layer with fallback to default; the config layer does not reject unknown strings).
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

    /// `[permissions]`: both lists optional (absent table means no authored
    /// rules), and entries stay raw strings — the sandbox owns their syntax.
    #[test]
    fn permissions_parse_authored_rule_entries() {
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        assert!(cfg.permissions.allow.is_empty());
        assert!(cfg.permissions.deny.is_empty());
        let cfg: Config = toml::from_str(&format!(
            r#"{TOML_OK}
[permissions]
allow = ["Bash(cargo test *)", "File(src/**)"]
deny = ["Bash(rm -rf *)"]
"#
        ))
        .unwrap();
        assert_eq!(
            cfg.permissions.allow,
            vec!["Bash(cargo test *)".to_string(), "File(src/**)".to_string()]
        );
        assert_eq!(cfg.permissions.deny, vec!["Bash(rm -rf *)".to_string()]);
    }

    /// hooks config: single-table and array-of-tables forms; empty map by default.
    /// Event-point validity is not checked in the config layer (no in-workspace deps; core checks during conversion).
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
        // Empty map by default.
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        assert!(cfg.hooks.is_empty());
    }

    /// mcp_servers config: stdio (command+args+env) and
    /// http (url+headers) forms; empty map by default; the either-or check is not in this layer.
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
        // Empty map by default.
        let cfg: Config = toml::from_str(TOML_OK).unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    // TEST_KEY is process-level state; tests depending on it must run mutually exclusively.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn missing_api_key_is_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        // env unset and no inline api_key -> MissingApiKey
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
        // export KEY= (empty string): treated as unset, falls back to the inline api_key
        unsafe { std::env::set_var("TEST_KEY", "") };
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = Some("k-inline".into());
        let (_p, key) = cfg.resolve_provider().unwrap();
        assert_eq!(key, "k-inline");
        // Empty env plus no inline api_key -> MissingApiKey
        cfg.model_providers.get_mut("minimax").unwrap().api_key = None;
        assert!(matches!(
            cfg.resolve_provider(),
            Err(ConfigError::MissingApiKey(_))
        ));
        unsafe { std::env::remove_var("TEST_KEY") };
    }

    #[test]
    fn whitespace_only_env_key_falls_back_to_inline() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Whitespace-only env values are never valid API keys: fall back
        // to the inline key instead of sending blanks to the API.
        unsafe { std::env::set_var("TEST_KEY", "   ") };
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = Some("k-inline".into());
        let (_p, key) = cfg.resolve_provider().unwrap();
        assert_eq!(key, "k-inline");
        // Whitespace env plus no inline key -> MissingApiKey.
        cfg.model_providers.get_mut("minimax").unwrap().api_key = None;
        assert!(matches!(
            cfg.resolve_provider(),
            Err(ConfigError::MissingApiKey(_))
        ));
        unsafe { std::env::remove_var("TEST_KEY") };
    }

    #[test]
    fn whitespace_only_inline_key_is_missing_api_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("TEST_KEY") };
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = Some("   ".into());
        assert!(matches!(
            cfg.resolve_provider(),
            Err(ConfigError::MissingApiKey(_))
        ));
    }

    #[test]
    fn zero_token_limits_fall_back_to_defaults() {
        // Explicit zero is a misconfiguration, not intent: it can never
        // satisfy a request, so accessors treat it as unset.
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        let prov = cfg.model_providers.get_mut("minimax").unwrap();
        prov.context_window = Some(0);
        prov.max_output_tokens = Some(0);
        assert_eq!(prov.context_window(), DEFAULT_CONTEXT_WINDOW);
        assert_eq!(prov.max_output_tokens(), DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(DEFAULT_CONTEXT_WINDOW, 200_000);
        assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS, 8192);
        // Non-zero values are still honored; None still means default.
        prov.context_window = Some(100_000);
        prov.max_output_tokens = Some(4096);
        assert_eq!(prov.context_window(), 100_000);
        assert_eq!(prov.max_output_tokens(), 4096);
        prov.context_window = None;
        prov.max_output_tokens = None;
        assert_eq!(prov.context_window(), 200_000);
        assert_eq!(prov.max_output_tokens(), 8192);
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
        // Missing required field model: serde deserialization fails -> Parse
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

    /// Both home env vars (USERPROFILE / HOME) unset: explicit NotFound, no fallback to
    /// relative-path lookup (so a project-local `.wavecode/config.toml` is never mistaken for user config).
    #[test]
    fn load_without_home_is_not_found_not_relative_lookup() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_user = std::env::var_os("USERPROFILE");
        let saved_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("HOME");
        }
        let result = Config::load();
        // Restore the process-level env before asserting either way (same ENV_LOCK discipline).
        unsafe {
            if let Some(v) = saved_user {
                std::env::set_var("USERPROFILE", v);
            }
            if let Some(v) = saved_home {
                std::env::set_var("HOME", v);
            }
        }
        assert!(matches!(result, Err(ConfigError::NotFound(_))));
    }

    #[test]
    fn provider_config_debug_redacts_api_key() {
        const REAL_KEY: &str = "sk-ant-api03-real-looking-secret-key";
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = Some(REAL_KEY.into());

        // Neither ProviderConfig itself nor the Config containing it may leak the raw key via Debug.
        let dbg_provider = format!("{:?}", cfg.model_providers["minimax"]);
        let dbg_config = format!("{cfg:?}");
        for output in [&dbg_provider, &dbg_config] {
            assert!(
                !output.contains(REAL_KEY),
                "Debug leaked the api key: {output}"
            );
            assert!(
                output.contains("***"),
                "Debug should carry the redaction marker: {output}"
            );
        }
        // All other fields render normally.
        assert!(dbg_provider.contains("https://api.minimaxi.com/anthropic"));

        // A None api_key renders as None, likewise with no leak.
        let mut cfg: Config = toml::from_str(TOML_OK).unwrap();
        cfg.model_providers.get_mut("minimax").unwrap().api_key = None;
        let dbg_none = format!("{:?}", cfg.model_providers["minimax"]);
        assert!(dbg_none.contains("api_key: None"));
    }
}
