//! provider 配置（阶段 5 拆分自 lib.rs）：ProviderKind / ProviderConfig。

/// Provider 类型（配置中的 `type` 字段，kebab-case 形式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    OpenAiCompatible,
}

/// 单个 model provider 的配置。
///
/// `Debug` 手写脱敏：`api_key` 永不显示真实值（Some 显示 `***`，None 显示
/// `None`），防日志 / 错误输出泄露密钥；其余字段正常显示。
#[derive(Clone, serde::Deserialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    pub base_url: String,
    /// 指向环境变量名，运行时从该环境变量读取 api key。
    pub env_key: Option<String>,
    /// 内联 api key（M1 便利项，优先级低于 env_key）。
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
            // 脱敏：只保留 Some/None 形态，真实 key 永不进入 Debug 输出。
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("context_window", &self.context_window)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

impl ProviderConfig {
    /// 上下文窗口大小，默认 200_000。
    pub fn context_window(&self) -> u64 {
        self.context_window.unwrap_or(200_000)
    }

    /// 最大输出 token 数，默认 8192。
    pub fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens.unwrap_or(8192)
    }
}
