//! hooks config (split from lib.rs in phase 5): HookRule / HookRuleSet and ConfigError.

use super::*;

/// A single hook rule (fields of the `[hooks.<EventPoint>]` table, SPEC section 9 config example).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HookRule {
    /// Tool name matcher (optional; semantics defined by the hooks crate).
    pub matcher: Option<String>,
    /// Shell command string (required).
    pub command: String,
    /// Timeout in milliseconds (defaults filled in by the hooks crate when absent).
    pub timeout_ms: Option<u64>,
    /// Trigger at most once per session (defaults to false).
    pub once: Option<bool>,
}

/// Hook entries under one event point: either a single table `[hooks.PreToolUse]`
/// or an array of tables `[[hooks.PreToolUse]]` (untagged).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum HookRuleSet {
    /// Single-table form.
    One(HookRule),
    /// Array-of-tables form (multiple hooks run in config order).
    Many(Vec<HookRule>),
}

impl HookRuleSet {
    /// Unified slice view (iterate both forms the same way).
    pub fn rules(&self) -> &[HookRule] {
        match self {
            Self::One(rule) => std::slice::from_ref(rule),
            Self::Many(rules) => rules,
        }
    }
}

/// Config load / parse errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Config file missing (or unreadable).
    #[error("config file not found: {}", .0.display())]
    NotFound(PathBuf),
    /// TOML parse failure.
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    /// `model_provider` is not defined in `model_providers`.
    #[error("undefined provider: {0}")]
    MissingProvider(String),
    /// Provider has no usable api key.
    #[error("provider {0} is missing an api key (env_key env var unset and no inline api_key)")]
    MissingApiKey(String),
}
