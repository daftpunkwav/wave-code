//! The model catalog: a single `~/.wavecode/models.json` file holding
//! every model the `/model` picker and slash commands can see.
//!
//! One file, one format (shaped after common provider/model catalogs):
//! per model — provider id, wire name, API kind, endpoint,
//! credentials, context/output limits, reasoning variants, and the
//! input/output modalities. The catalog is the user-facing source of
//! truth; loading merges it into the runtime [`Config`] as `[models]`
//! entries plus synthesized `[model_providers]` so the picker, the
//! thinking-level table, and the client construction all read one
//! catalog.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::provider::{ProviderConfig, ProviderKind};

/// The API dialect a catalog model speaks. The spellings match the
/// official protocol names; the config-TOML aliases resolve through
/// [`ProviderKind`] separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKind {
    /// Anthropic Messages (`POST {base}/v1/messages`).
    #[serde(alias = "anthropic")]
    AnthropicMessages,
    /// OpenAI Chat Completions (`POST {base}/chat/completions`).
    #[serde(alias = "openai-chat", alias = "openai")]
    OpenaiChat,
    /// OpenAI Responses (`POST {base}/responses`).
    #[serde(alias = "openai-responses", alias = "responses")]
    OpenaiResponses,
}

impl ApiKind {
    /// The provider-kind used when synthesizing a `ProviderConfig`.
    pub fn provider_kind(self) -> ProviderKind {
        match self {
            Self::AnthropicMessages => ProviderKind::Anthropic,
            Self::OpenaiChat => ProviderKind::OpenAiCompatible,
            Self::OpenaiResponses => ProviderKind::OpenAiResponses,
        }
    }
}

/// Reasoning variants and their default, e.g. low/high/max.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReasoningSpec {
    /// Whether the model supports an explicit thinking control.
    pub enabled: bool,
    /// Ordered effort/variant names (e.g. `low`, `high`, `max`).
    #[serde(default)]
    pub variants: Vec<String>,
    /// The variant applied when the model is selected.
    #[serde(default)]
    pub default: Option<String>,
}

/// Input/output modality tags (`text`, `image`, `video`, `audio`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModalitiesSpec {
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
}

impl Default for ModalitiesSpec {
    fn default() -> Self {
        Self {
            input: vec!["text".to_string()],
            output: vec!["text".to_string()],
        }
    }
}

/// One catalog model: everything the runtime needs to build a client
/// and everything the UI needs to describe the model.
///
/// Hand-written redacted `Debug`: `api_key` never shows the real value
/// (Some renders as `***`, None as `None`), mirroring
/// [`ProviderConfig`] — secrets cannot leak via log / error output.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelSpec {
    /// Provider id this model groups under (synthesized into
    /// `model_providers` on load).
    pub provider: String,
    /// Wire model name sent to the API.
    pub model: String,
    /// API dialect.
    pub kind: ApiKind,
    /// API endpoint base (no `/v1/messages` suffix).
    pub base_url: String,
    /// Env var holding the API key (preferred over an inline key).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Inline API key (lower priority than `api_key_env`).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Context window in tokens.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Max output tokens.
    #[serde(default)]
    pub max_output: Option<u32>,
    /// Thinking control.
    #[serde(default)]
    pub reasoning: ReasoningSpec,
    /// Accepted input / produced output modalities.
    #[serde(default)]
    pub modalities: ModalitiesSpec,
}

impl std::fmt::Debug for ModelSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelSpec")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            // Redacted: keep only the Some/None shape, the real key never
            // enters Debug output.
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("context_window", &self.context_window)
            .field("max_output", &self.max_output)
            .field("reasoning", &self.reasoning)
            .field("modalities", &self.modalities)
            .finish()
    }
}

impl ModelSpec {
    /// The reasoning effort string the provider client receives
    /// (the catalog default, e.g. `max`); `None` when thinking is off.
    pub fn default_effort(&self) -> Option<String> {
        if self.reasoning.enabled {
            self.reasoning.default.clone()
        } else {
            None
        }
    }

    /// True when the model accepts `modality` as input.
    pub fn accepts_input(&self, modality: &str) -> bool {
        self.modalities
            .input
            .iter()
            .any(|m| m.eq_ignore_ascii_case(modality))
    }

    /// Synthesize the provider config this model samples through. The
    /// provider id is namespaced under the catalog so a config.toml
    /// provider with the same id always wins.
    pub fn to_provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            kind: self.kind.provider_kind(),
            base_url: self.base_url.clone(),
            env_key: self.api_key_env.clone(),
            api_key: self.api_key.clone(),
            context_window: self.context_window,
            max_output_tokens: self.max_output,
            fallback_providers: Vec::new(),
            rpm_limit: None,
            reasoning_effort: self.default_effort(),
            thinking_budget_tokens: None,
            prompt_caching: None,
            prompt_cache_ttl: None,
        }
    }

    /// The `[models]` table entry the picker consumes.
    pub fn to_model_entry(&self) -> crate::ModelEntry {
        crate::ModelEntry {
            provider: self.provider.clone(),
            model: self.model.clone(),
            reasoning_effort: self.default_effort(),
        }
    }
}

/// The catalog file: alias → spec, ordered by alias for stable
/// listings.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelCatalog {
    #[serde(default)]
    pub models: BTreeMap<String, ModelSpec>,
}

/// Catalog load/save failures. `Display` renders the operation and the
/// cause, so callers report it directly (`{e}`) instead of through the
/// Debug shape.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The file exists but is not valid catalog JSON.
    #[error("parse models.json: {0}")]
    Parse(String),
    /// The file could not be read.
    #[error("read models.json: {0}")]
    Read(std::io::Error),
    /// The file could not be written.
    #[error("write models.json: {0}")]
    Write(std::io::Error),
}

impl ModelCatalog {
    /// Path of the catalog file: `~/.wavecode/models.json`.
    pub fn path(home: &Path) -> PathBuf {
        Path::new(home).join(".wavecode").join("models.json")
    }

    /// Load the catalog; a missing file is an empty catalog (the
    /// catalog is optional — config.toml models keep working alone).
    /// A malformed file is an error, never silently swallowed: a typoed
    /// catalog must not look like an empty one. Any other read failure
    /// (a directory in the way, denied permissions) surfaces too — a
    /// silently-empty catalog would hide the user's models.
    pub fn load(home: &Path) -> Result<Self, CatalogError> {
        let path = Self::path(home);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(CatalogError::Read(e)),
        };
        serde_json::from_str(&content).map_err(|e| CatalogError::Parse(e.to_string()))
    }

    /// Write the catalog back to `~/.wavecode/models.json`. The file may
    /// hold inline API keys, so on Unix it is created and kept
    /// owner-only (0600) instead of world-readable. The write lands in
    /// a sibling temp file, is flushed to disk, and is renamed into
    /// place: a crash mid-write can never leave a truncated catalog
    /// behind, and the contents also survive an OS crash or power loss.
    pub fn save(&self, home: &Path) -> Result<(), CatalogError> {
        let path = Self::path(home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CatalogError::Write)?;
        }
        let mut content =
            serde_json::to_string_pretty(self).map_err(|e| CatalogError::Parse(e.to_string()))?;
        content.push('\n');
        crate::atomic_file::write_private_atomic(&path, content.as_bytes())
            .map_err(CatalogError::Write)
    }

    /// The spec for `alias`.
    pub fn get(&self, alias: &str) -> Option<&ModelSpec> {
        self.models.get(alias)
    }

    /// Insert or replace `alias`.
    pub fn insert(&mut self, alias: impl Into<String>, spec: ModelSpec) {
        self.models.insert(alias.into(), spec);
    }

    /// Remove `alias`; returns the removed spec.
    pub fn remove(&mut self, alias: &str) -> Option<ModelSpec> {
        self.models.remove(alias)
    }

    /// Merge the catalog into a runtime [`crate::Config`]: every catalog model
    /// becomes a `[models]` entry, and its synthesized provider is
    /// injected only when the id is not already defined there — a
    /// config.toml provider overrides the catalog, never the reverse.
    pub fn merge_into(&self, config: &mut crate::Config) {
        for (alias, spec) in &self.models {
            config.models.insert(
                alias.clone(),
                crate::ModelEntry {
                    provider: format!("catalog:{}", spec.provider),
                    model: spec.model.clone(),
                    reasoning_effort: spec.default_effort(),
                },
            );
            let provider_id = format!("catalog:{}", spec.provider);
            config
                .model_providers
                .entry(provider_id)
                .or_insert_with(|| spec.to_provider_config());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  "models": {
    "GLM-5.3": {
      "provider": "bigmodel",
      "model": "glm-5.3",
      "kind": "anthropic-messages",
      "base_url": "https://open.bigmodel.cn/api/anthropic",
      "context_window": 1000000,
      "max_output": 128000,
      "reasoning": { "enabled": true, "variants": ["low", "max"], "default": "max" },
      "modalities": { "input": ["text", "image"], "output": ["text"] }
    }
  }
}"#;

    #[test]
    fn sample_parses_and_round_trips() {
        let catalog: ModelCatalog = serde_json::from_str(SAMPLE).unwrap();
        let spec = catalog.get("GLM-5.3").unwrap();
        assert_eq!(spec.kind, ApiKind::AnthropicMessages);
        assert_eq!(spec.context_window, Some(1_000_000));
        assert_eq!(spec.default_effort().as_deref(), Some("max"));
        assert!(spec.accepts_input("image"));
        assert!(!spec.accepts_input("video"));

        let round: ModelCatalog =
            serde_json::from_str(&serde_json::to_string_pretty(&catalog).unwrap()).unwrap();
        assert_eq!(round, catalog);
    }

    #[test]
    fn missing_file_is_an_empty_catalog() {
        let dir = std::env::temp_dir().join("wavecode-catalog-missing");
        let _ = std::fs::remove_dir_all(&dir);
        let catalog = ModelCatalog::load(&dir).unwrap();
        assert!(catalog.models.is_empty());
    }

    #[test]
    fn malformed_catalog_is_an_error_not_empty() {
        let dir = std::env::temp_dir().join("wavecode-catalog-bad");
        std::fs::create_dir_all(dir.join(".wavecode")).unwrap();
        std::fs::write(dir.join(".wavecode").join("models.json"), "{ not json").unwrap();
        assert!(ModelCatalog::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A read failure other than a missing file (here: the catalog path
    /// is a directory) surfaces as an error instead of looking empty.
    #[test]
    fn unreadable_catalog_is_an_error_not_empty() {
        let dir = std::env::temp_dir().join("wavecode-catalog-blocked");
        let path = dir.join(".wavecode").join("models.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&path).unwrap();
        assert!(matches!(
            ModelCatalog::load(&dir),
            Err(CatalogError::Read(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Display renders the operation and its cause, so callers report
    /// `{e}` directly instead of the Debug shape.
    #[test]
    fn catalog_error_displays_operation_and_cause() {
        let error = CatalogError::Parse("bad json".to_string());
        assert_eq!(error.to_string(), "parse models.json: bad json");
    }

    /// The rendered Debug of a spec keeps the Some/None shape of the
    /// inline key but never its value — log and error output included.
    #[test]
    fn debug_output_redacts_the_inline_key() {
        let catalog: ModelCatalog = serde_json::from_str(SAMPLE).unwrap();
        let mut spec = catalog.get("GLM-5.3").unwrap().clone();
        spec.api_key = Some("sk-super-secret".into());
        let rendered = format!("{spec:?}");
        assert!(
            !rendered.contains("sk-super-secret"),
            "key leaked via Debug: {rendered}"
        );
        assert!(rendered.contains("***"), "shape kept: {rendered}");
        let rendered = format!("{catalog:?}");
        assert!(
            !rendered.contains("sk-super-secret"),
            "no key via the catalog Debug either"
        );
    }

    /// On Unix the catalog file (which may hold an inline key) is
    /// written owner-only, including tightening a pre-existing file.
    #[cfg(unix)]
    #[test]
    fn saved_catalog_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("wavecode-catalog-perms");
        let path = dir.join(".wavecode").join("models.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        let mut spec = ModelCatalog::default();
        spec.insert(
            "m",
            ModelSpec {
                provider: "p".into(),
                model: "m-1".into(),
                kind: ApiKind::OpenaiChat,
                base_url: "https://x".into(),
                api_key_env: None,
                api_key: Some("k".into()),
                context_window: None,
                max_output: None,
                reasoning: ReasoningSpec::default(),
                modalities: ModalitiesSpec::default(),
            },
        );
        spec.save(&dir).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "owner-only catalog: {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join("wavecode-catalog-round");
        let _ = std::fs::remove_dir_all(&dir);
        let mut catalog = ModelCatalog::default();
        catalog.insert(
            "m",
            ModelSpec {
                provider: "p".into(),
                model: "m-1".into(),
                kind: ApiKind::OpenaiChat,
                base_url: "https://x".into(),
                api_key_env: Some("KEY".into()),
                api_key: None,
                context_window: Some(200_000),
                max_output: Some(8192),
                reasoning: ReasoningSpec::default(),
                modalities: ModalitiesSpec::default(),
            },
        );
        catalog.save(&dir).unwrap();
        let loaded = ModelCatalog::load(&dir).unwrap();
        assert_eq!(loaded.models["m"].model, "m-1");
        // The write lands through a sibling temp file renamed into
        // place; a completed save leaves no temp litter behind.
        assert!(!ModelCatalog::path(&dir).with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_into_config_synth_providers_once() {
        let catalog: ModelCatalog = serde_json::from_str(SAMPLE).unwrap();
        let mut config: crate::Config = toml::from_str(
            "model = \"m\"
model_provider = \"p\"",
        )
        .unwrap();
        catalog.merge_into(&mut config);
        assert!(config.models.contains_key("GLM-5.3"));
        assert!(config.model_providers.contains_key("catalog:bigmodel"));
        let provider = &config.model_providers["catalog:bigmodel"];
        assert_eq!(provider.context_window(), 1_000_000);
        assert_eq!(provider.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn spec_synthesizes_provider_and_entry() {
        let catalog: ModelCatalog = serde_json::from_str(SAMPLE).unwrap();
        let spec = catalog.get("GLM-5.3").unwrap();
        let provider = spec.to_provider_config();
        assert_eq!(provider.kind, ProviderKind::Anthropic);
        assert_eq!(provider.base_url, "https://open.bigmodel.cn/api/anthropic");
        let entry = spec.to_model_entry();
        assert_eq!(entry.provider, "bigmodel");
        assert_eq!(entry.model, "glm-5.3");
        assert_eq!(entry.reasoning_effort.as_deref(), Some("max"));
    }
}
