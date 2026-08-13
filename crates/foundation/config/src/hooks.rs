//! hooks 配置（阶段 5 拆分自 lib.rs）：HookRule / HookRuleSet 与 ConfigError。

use super::*;

/// 单条 hook 规则（`[hooks.<EventPoint>]` 表的字段，SPEC §9 配置示例）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HookRule {
    /// 工具名匹配器（可选；语义由 hooks crate 定义）。
    pub matcher: Option<String>,
    /// shell 命令串（必填）。
    pub command: String,
    /// 超时毫秒（缺省由 hooks crate 补默认值）。
    pub timeout_ms: Option<u64>,
    /// 每会话只触发一次（缺省 false）。
    pub once: Option<bool>,
}

/// 事件点下的 hook 条目：单表 `[hooks.PreToolUse]` 或表数组
/// `[[hooks.PreToolUse]]` 两种形态都接受（untagged）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum HookRuleSet {
    /// 单表形态。
    One(HookRule),
    /// 表数组形态（多条 hook 按配置序执行）。
    Many(Vec<HookRule>),
}

impl HookRuleSet {
    /// 统一为切片视图（两种形态无差别遍历）。
    pub fn rules(&self) -> &[HookRule] {
        match self {
            Self::One(rule) => std::slice::from_ref(rule),
            Self::Many(rules) => rules,
        }
    }
}

/// 配置加载 / 解析错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 配置文件不存在（或不可读）。
    #[error("配置文件不存在: {}", .0.display())]
    NotFound(PathBuf),
    /// TOML 解析失败。
    #[error("配置解析失败: {0}")]
    Parse(#[from] toml::de::Error),
    /// `model_provider` 未在 `model_providers` 中定义。
    #[error("未定义的 provider: {0}")]
    MissingProvider(String),
    /// provider 缺少可用的 api key。
    #[error("provider {0} 缺少 api key（env_key 环境变量未设置且无内联 api_key）")]
    MissingApiKey(String),
}
