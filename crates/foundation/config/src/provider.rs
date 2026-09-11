//! provider config (split from lib.rs in phase 5): ProviderKind / ProviderConfig.

/// Provider type (the `type` field in config, kebab-case form).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    OpenAiCompatible,
}

/// Config of a single model provider.
///
/// Hand-written redacted `Debug`: `api_key` never shows the real value (Some
/// renders as `***`, None as `None`) so secrets cannot leak via log / error
/// output; all other fields render normally.
#[derive(Clone, serde::Deserialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    pub base_url: String,
    /// Name of the env var the api key is read from at runtime.
    pub env_key: Option<String>,
    /// Inline api key (M1 convenience, lower priority than env_key).
    pub api_key: Option<String>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("env_key", &self.env_key)
            // Redacted: keep only the Some/None shape, the real key never enters Debug output.
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("context_window", &self.context_window)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

/// Default context window (tokens) used when the provider config leaves
/// `context_window` unset or sets it to zero.
/// Single source of truth for the fallback; callers should reference this
/// constant instead of hardcoding the value again.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
/// Default per-round output token cap used when the provider config leaves
/// `max_output_tokens` unset or sets it to zero.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

impl ProviderConfig {
    /// Context window size, default 200_000.
    /// Explicit zero is treated as unset: a zero-token window can never
    /// satisfy a request, so it is always a misconfiguration, not intent.
    pub fn context_window(&self) -> u64 {
        self.context_window
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW)
    }

    /// Max output tokens, default 8192.
    /// Explicit zero is treated as unset (see [`Self::context_window`]).
    pub fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
    }
}
