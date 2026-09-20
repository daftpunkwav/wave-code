//! provider config (split from lib.rs in phase 5): ProviderKind / ProviderConfig.

/// Provider type (the `type` field in config).
///
/// The canonical spellings are the kebab-case names serde derives
/// (`open-ai-compatible`), but the natural hand-written forms
/// (`openai-compatible`, `openai-responses`, `responses`) are accepted as
/// aliases so a plausible-looking config never fails to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Anthropic Messages (`POST {base_url}/v1/messages`).
    Anthropic,
    /// OpenAI Chat Completions (`POST {base_url}/chat/completions`) — the
    /// dialect most third-party gateways speak.
    #[serde(alias = "openai-compatible", alias = "openai_compatible")]
    OpenAiCompatible,
    /// OpenAI Responses (`POST {base_url}/responses`): the only endpoint
    /// serving some models (o1-pro, gpt-5-codex) and the recommended one for
    /// newer reasoning families.
    #[serde(
        alias = "openai-responses",
        alias = "openai_responses",
        alias = "responses"
    )]
    OpenAiResponses,
}

impl ProviderKind {
    /// True for the OpenAI-protocol kinds: both take a switchable string
    /// reasoning effort (Anthropic thinking is a config-only token budget)
    /// and both consult the model capability table when no explicit window
    /// or output cap is configured.
    pub fn carries_reasoning_effort(self) -> bool {
        matches!(self, Self::OpenAiCompatible | Self::OpenAiResponses)
    }
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
    /// Ordered fallback provider names (keys of `model_providers`); empty
    /// means no fallback. Tried in order on transport/server errors only;
    /// auth errors fail fast and never cross providers (keys stay with the
    /// provider they were issued for).
    #[serde(default)]
    pub fallback_providers: Vec<String>,
    /// Local client-side rate limit in requests per minute. Unset (the
    /// default) sends requests unthrottled; set, a token bucket gates
    /// each model request so retries never hammer the endpoint.
    #[serde(default)]
    pub rpm_limit: Option<u32>,
    /// Best-effort per-step reasoning effort forwarded to OpenAI-compatible
    /// endpoints (e.g. low / medium / high); `None` sends nothing so
    /// providers without the param keep working unchanged.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Extended-thinking token budget for Anthropic-protocol providers
    /// (Anthropic `thinking.budget_tokens`); `None` keeps thinking off.
    /// The budget is clamped per request into the API-satisfiable range.
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
    /// Prompt-cache breakpoints on Anthropic-protocol requests. Unset means
    /// enabled (repeated turns are the norm and cache reads are billed at a
    /// fraction of fresh input); set `false` only for gateways that reject
    /// the `cache_control` field.
    #[serde(default)]
    pub prompt_caching: Option<bool>,
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
            .field("fallback_providers", &self.fallback_providers)
            .field("reasoning_effort", &self.reasoning_effort)
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
