//! Fullscreen TUI assembly over a live harness session.
//!
//! Owns the resume seed resolution (`TuiSeed`, `resolve_tui_seed`),
//! the `/model` picker catalog helpers (`build_model_entries`,
//! `thinking_levels_for`), the `UiContext`/`SessionFactory` wiring
//! (`ui_ctx_of`, `make_tui_factory`), and `run_tui_new`. Config and
//! catalog loading helpers (`load_config_opt`, `resolve_secondary`,
//! `effective_model`, `print_config_error`) live here too; the
//! model/provider override pairing comes from the crate root, shared
//! with the other assembly paths.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use operations_bootstrap::{AssembleOptions, DEFAULT_IDENTITY, assemble_session};
use uuid::Uuid;

use crate::model_provider_overrides;
use crate::update;

/// Session seed resolved from `--session` / `--continue`.
#[derive(Debug)]
struct TuiSeed {
    session_id: String,
    title: Option<String>,
    history: Vec<(bool, String)>,
}

/// Resolve the resume seed: an explicit `--session <id>` wins over
/// `--continue` (most recent session recorded for this directory).
/// Unknown ids and missing sessions surface as errors with guidance.
fn resolve_tui_seed(
    session: &Option<String>,
    continue_last: bool,
    cwd: &Path,
    home: &Option<PathBuf>,
) -> anyhow::Result<Option<TuiSeed>> {
    use state_persistence::sessions::{list_sessions, load_session_history};
    let Some(home) = home else {
        // A missing home cannot honor an explicit resume request; only
        // the flagless case stays a silent fresh session.
        if session.is_some() || continue_last {
            anyhow::bail!("home directory unavailable; cannot resume sessions");
        }
        return Ok(None);
    };
    if let Some(id) = session {
        if !state_persistence::sessions::is_valid_session_id(id) {
            anyhow::bail!("invalid session id {id:?}");
        }
        let history = load_session_history(home, id)
            .map_err(|e| anyhow::anyhow!("cannot load session {id}: {e}"))?;
        let title = list_sessions(home)
            .into_iter()
            .find(|meta| meta.id == *id)
            .and_then(|meta| (!meta.title.is_empty()).then_some(meta.title));
        return Ok(Some(TuiSeed {
            session_id: id.clone(),
            title,
            history,
        }));
    }
    if continue_last {
        let cwd_text = cwd.to_string_lossy().to_string();
        let target = list_sessions(home)
            .into_iter()
            .find(|meta| meta.cwd == cwd_text);
        let Some(meta) = target else {
            anyhow::bail!("no sessions to continue under {cwd_text}; starting a fresh session");
        };
        let history = load_session_history(home, &meta.id)
            .map_err(|e| anyhow::anyhow!("cannot load session {}: {e}", meta.id))?;
        return Ok(Some(TuiSeed {
            session_id: meta.id,
            title: (!meta.title.is_empty()).then_some(meta.title),
            history,
        }));
    }
    Ok(None)
}

/// Model catalog for the `/model` picker: the config `[models]` aliases
/// plus one entry for the configured default model. Pure conversion;
/// unknown provider ids pass through (the picker's live switch refuses
/// them via the same-provider check).
fn build_model_entries(
    config: &wavecode_config::Config,
    provider_id: &str,
    model_name: &str,
    effort: Option<&str>,
) -> Vec<console_ui::dialogs::ModelEntryView> {
    use console_ui::dialogs::ModelEntryView;
    let mut entries: Vec<ModelEntryView> = config
        .models
        .iter()
        .map(|(alias, entry)| ModelEntryView {
            label: alias.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            effort: entry.reasoning_effort.clone(),
        })
        .collect();
    let default_here = ModelEntryView {
        label: model_name.to_string(),
        provider: provider_id.to_string(),
        model: model_name.to_string(),
        effort: effort.map(str::to_string),
    };
    if !entries
        .iter()
        .any(|entry| entry.label == default_here.label && entry.provider == default_here.provider)
    {
        entries.push(default_here);
    }
    entries
}

/// Thinking levels the picker offers: the OpenAI-protocol providers (Chat
/// Completions and Responses) take a string reasoning effort; budget-driven
/// Anthropic thinking is config-only, so the row hides there.
fn thinking_levels_for(config: &wavecode_config::Config, provider_id: &str) -> Vec<String> {
    match config.model_providers.get(provider_id) {
        Some(provider) if provider.kind.carries_reasoning_effort() => {
            vec![
                "off".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ]
        }
        _ => Vec::new(),
    }
}

/// Build the UI context from a live handle.
#[allow(clippy::too_many_arguments)]
fn ui_ctx_of(
    handle: &operations_bootstrap::SessionHandle,
    cwd: PathBuf,
    home: &Option<PathBuf>,
    session_id: String,
    title: Option<String>,
    model_entries: Vec<console_ui::dialogs::ModelEntryView>,
    thinking_levels: Vec<String>,
    update_notice: Option<Arc<std::sync::Mutex<Option<String>>>>,
) -> console_ui::UiContext {
    console_ui::UiContext {
        model_name: handle.model_name.clone(),
        provider_id: handle.provider_id.clone(),
        thinking_effort: handle.thinking_effort.clone(),
        thinking_levels,
        cwd,
        permission_mode: handle.permission_mode.clone(),
        skill_names: handle.skill_names.clone(),
        mcp_servers: handle.mcp_servers.clone(),
        memory_files: handle.instruction_sources.clone(),
        status: handle.status.clone(),
        session_id,
        session_title: title,
        model_entries,
        home: home.clone(),
        update_notice,
        redactor: handle.secret_redactor(),
    }
}

/// On-demand session factory for resume and `/new`: assembles a session
/// from the same inputs as the initial one and connects MCP servers.
/// `block_in_place` is safe here: the console run loop stays on the
/// multi-thread runtime worker and never blocks a worker while turns run.
#[allow(clippy::too_many_arguments)]
fn make_tui_factory(
    config_path: Option<PathBuf>,
    model: Option<String>,
    permission_mode: Option<String>,
    cwd: PathBuf,
    home: Option<PathBuf>,
    model_entries: Vec<console_ui::dialogs::ModelEntryView>,
    thinking_levels: Vec<String>,
    update_notice: Option<Arc<std::sync::Mutex<Option<String>>>>,
) -> std::sync::Arc<console_ui::SessionFactory> {
    std::sync::Arc::new(move |spec: &console_ui::LaunchSpec| {
        let runtime = tokio::runtime::Handle::current();
        let config_path = config_path.clone();
        let model = model.clone();
        let permission_mode = permission_mode.clone();
        let cwd = cwd.clone();
        let home = home.clone();
        let model_entries = model_entries.clone();
        let thinking_levels = thinking_levels.clone();
        let update_notice = update_notice.clone();
        let history = spec.history.clone();
        let resume_id = spec.session_id.clone();
        let readonly = spec.readonly;
        let model_hint = spec.model_override.clone();
        tokio::task::block_in_place(|| {
            runtime.block_on(async move {
                let settings = console_ui::settings::UiSettings::load();
                // The provider pairing keys on the CLI model, not the
                // launch hint: the live /model choice never crosses
                // providers (same-provider check), so it samples through
                // whatever provider the original assembly resolved to.
                let (base_model, provider_override) =
                    model_provider_overrides(model.as_deref(), &settings);
                // A read-only side session (`/btw`) is routine work: when
                // the config names a `secondary_model` alias resolvable in
                // `[models]`, sample through it instead of the primary —
                // unless the caller pinned a live model hint (an explicit
                // choice outranks the cost steering). An unresolvable
                // alias degrades to the primary with a warning.
                let secondary = if readonly {
                    load_config_opt(config_path.as_deref())
                        .ok()
                        .and_then(|config| resolve_secondary(&config))
                } else {
                    None
                };
                let (model_override, provider_override, thinking_override) =
                    match (&secondary, &model_hint) {
                        (Some((model, provider, effort)), None) => (
                            Some(model.clone()),
                            provider.clone(),
                            effort.clone().or(settings.default_effort.clone()),
                        ),
                        _ => (
                            model_hint.clone().or(base_model),
                            provider_override,
                            settings.default_effort.clone(),
                        ),
                    };
                let session_id = resume_id.unwrap_or_else(|| Uuid::new_v4().to_string());
                let mut handle = assemble_session(AssembleOptions {
                    session_id: Some(session_id.clone()),
                    config_path,
                    // A launch hint (the live /model choice) wins; a
                    // read-only side session runs in plan mode so
                    // approvals and destructive work are impossible by
                    // mode, never by trust.
                    model_override,
                    provider_override,
                    permission_override: if readonly {
                        Some("plan".to_string())
                    } else {
                        permission_mode
                    },
                    thinking_override,
                    // None: every surface loads the shared denylist store.
                    wave_denylist: None,
                    cwd: cwd.clone(),
                    home: home.clone(),
                    identity: DEFAULT_IDENTITY.to_string(),
                    headless: false,
                    initial_history: history.clone(),
                })
                .map_err(|e| format!("session assembly failed: {e}"))?;
                handle.connect_mcp_servers().await;
                let title = home.as_deref().and_then(|home_root| {
                    state_persistence::sessions::list_sessions(home_root)
                        .into_iter()
                        .find(|meta| meta.id == session_id)
                        .and_then(|meta| (!meta.title.is_empty()).then_some(meta.title))
                });
                let ctx = ui_ctx_of(
                    &handle,
                    cwd.clone(),
                    &home,
                    session_id,
                    title,
                    model_entries,
                    thinking_levels,
                    update_notice,
                );
                Ok(console_ui::SessionLaunch {
                    link: Box::new(handle.client),
                    ctx,
                    history,
                })
            })
        })
    })
}

/// Fullscreen TUI over a live harness session.
///
/// Interactive approval parking works here (headless stays false);
/// config failures keep the exit-code-2-with-guidance contract.
/// `--session` / `--continue` seed the conversation from the recorded
/// session journal; every launch (initial or resumed) journals its
/// turns under `~/.wavecode/sessions/`.
pub(crate) async fn run_tui_new(
    config: Option<PathBuf>,
    model: Option<String>,
    permission_mode: Option<String>,
    session: Option<String>,
    continue_last: bool,
    cwd: PathBuf,
    home: Option<PathBuf>,
) -> anyhow::Result<()> {
    let settings = console_ui::settings::UiSettings::load();
    // Model precedence: CLI > saved picker default > config; the
    // provider rides the saved default (see `model_provider_overrides`).
    let (model_override, provider_override) = model_provider_overrides(model.as_deref(), &settings);
    // Derive the picker catalog and thinking levels from the config the
    // session will assemble from; a broken config degrades to an empty
    // catalog (assembly below reports the real error).
    let (model_entries, thinking_levels) = match load_config_opt(config.as_deref()) {
        Ok(cfg) => {
            let provider_id = provider_override
                .clone()
                .unwrap_or_else(|| cfg.model_provider.clone());
            let effort = settings.default_effort.clone().or_else(|| {
                cfg.model_providers
                    .get(&provider_id)
                    .and_then(|p| p.reasoning_effort.clone())
            });
            (
                build_model_entries(
                    &cfg,
                    &provider_id,
                    &effective_model(&model_override, &cfg),
                    effort.as_deref(),
                ),
                thinking_levels_for(&cfg, &provider_id),
            )
        }
        Err(_) => (Vec::new(), Vec::new()),
    };
    // Resume seed: an explicit --session errors hard on a missing
    // record; --continue degrades to a fresh session with a warning.
    let seed = match (&session, continue_last) {
        (Some(_), _) => resolve_tui_seed(&session, false, &cwd, &home)?,
        (None, true) => match resolve_tui_seed(&None, true, &cwd, &home) {
            Ok(seed) => seed,
            Err(error) => {
                eprintln!("[warn] {error}");
                None
            }
        },
        _ => None,
    };
    let (session_id, title, initial_history) = match &seed {
        Some(seed) => (
            seed.session_id.clone(),
            seed.title.clone(),
            seed.history.clone(),
        ),
        None => (Uuid::new_v4().to_string(), None, Vec::new()),
    };
    let mut handle = match assemble_session(AssembleOptions {
        config_path: config.clone(),
        model_override: model_override.clone(),
        provider_override,
        permission_override: permission_mode.clone(),
        thinking_override: settings.default_effort.clone(),
        cwd: cwd.clone(),
        home: home.clone(),
        identity: DEFAULT_IDENTITY.to_string(),
        headless: false,
        initial_history,
        // None: every surface loads the shared denylist store.
        wave_denylist: None,
        session_id: Some(session_id.clone()),
    }) {
        Ok(handle) => handle,
        Err(operations_bootstrap::SessionError::Config(e)) => {
            print_config_error(&e);
            std::process::exit(2)
        }
        Err(operations_bootstrap::SessionError::Model(message)) => {
            eprintln!("[fail] {message}");
            std::process::exit(2)
        }
    };
    handle.connect_mcp_servers().await;
    for warning in &handle.warnings {
        eprintln!("[warn] {warning}");
    }
    // Release check runs beside the session: the footer picks the
    // notice up on a later tick, never delaying the first frame.
    let update_slot = Arc::new(std::sync::Mutex::new(None));
    let slot = update_slot.clone();
    tokio::spawn(async move {
        let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        else {
            return;
        };
        let Ok(Some((tag, url))) = update::fetch_latest(&client).await else {
            return;
        };
        if matches!(
            update::classify(&tag, &url),
            update::UpdateStatus::Available { .. }
        ) {
            // Kept short: the footer's right slot drops content that
            // does not fit an 80-column terminal; `wavecode update`
            // carries the details.
            *slot.lock().expect("update slot lock") = Some(format!("update available: {tag}"));
        }
    });
    let ctx = ui_ctx_of(
        &handle,
        cwd.clone(),
        &home,
        session_id,
        title,
        model_entries.clone(),
        thinking_levels.clone(),
        Some(update_slot.clone()),
    );
    let factory = make_tui_factory(
        config,
        model,
        permission_mode,
        cwd,
        home,
        model_entries,
        thinking_levels,
        Some(update_slot),
    );
    console_ui::run_with_factory(handle.client, ctx, Some(factory)).await
}

/// Load the config for catalog derivation, from `path` or the default
/// location.
fn load_config_opt(
    path: Option<&std::path::Path>,
) -> Result<wavecode_config::Config, wavecode_config::ConfigError> {
    let mut config = match path {
        Some(path) => wavecode_config::Config::load_from(path),
        None => wavecode_config::Config::load(),
    }?;
    // The model catalog (`~/.wavecode/models.json`) merges on top: its
    // models become `[models]` entries and synthesized
    // `catalog:<provider>` providers (config.toml providers win on id
    // collisions). Catalog load failures degrade to an empty catalog —
    // config.toml models keep the session usable.
    if let Some(home) = wavecode_config::home_dir() {
        match wavecode_config::ModelCatalog::load(&home) {
            Ok(catalog) => catalog.merge_into(&mut config),
            Err(e) => eprintln!("model catalog ignored: {e}"),
        }
    }
    Ok(config)
}

/// Resolve the config's `secondary_model` alias into the
/// `(model, provider, effort)` triple side sessions sample through.
/// `None` when unset — or when the alias dangles, which warns here and
/// degrades to the primary model (doctor reports the same finding).
fn resolve_secondary(
    config: &wavecode_config::Config,
) -> Option<(String, Option<String>, Option<String>)> {
    let alias = config.secondary_model.as_deref()?;
    match config.models.get(alias) {
        Some(entry) => Some((
            entry.model.clone(),
            Some(entry.provider.clone()),
            entry.reasoning_effort.clone(),
        )),
        None => {
            eprintln!(
                "[warn] secondary_model {alias:?} is not in [models]; side sessions use the primary model"
            );
            None
        }
    }
}

/// The effective model name: override or config default.
fn effective_model(model_override: &Option<String>, config: &wavecode_config::Config) -> String {
    model_override
        .clone()
        .unwrap_or_else(|| config.model.clone())
}

/// Print a config error with creation guidance on missing files.
fn print_config_error(err: &wavecode_config::ConfigError) {
    eprintln!("Error: {err}");
    if let wavecode_config::ConfigError::NotFound(path) = err {
        eprintln!(
            r#"
Please create config file {}, example contents:

model = "claude-sonnet-4-5"
model_provider = "anthropic"

[model_providers.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com"
# api key, choose one (env_key wins):
# Option 1 (recommended): env_key names an env var, read at runtime
env_key = "ANTHROPIC_API_KEY"
# Option 2: inline api_key (keep secret, do not commit)
# api_key = "sk-ant-..."

# Other wire dialects: type = "openai-compatible" (Chat Completions, what
# most third-party gateways speak) or type = "openai-responses" (OpenAI
# Responses; needed for o1-pro / gpt-5-codex). base_url points at the API
# root, e.g. "https://api.openai.com/v1".
"#,
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The secondary alias resolves through `[models]` for side
    /// sessions; unset degrades to None, a dangling alias warns and also
    /// degrades (doctor reports the same finding).
    #[test]
    fn secondary_model_resolution_has_three_outcomes() {
        let mut config = picker_config();
        assert_eq!(resolve_secondary(&config), None, "unset stays None");

        config.secondary_model = Some("fast".to_string());
        assert_eq!(
            resolve_secondary(&config),
            Some((
                "fast-model".to_string(),
                Some("fastp".to_string()),
                Some("low".to_string())
            ))
        );

        config.secondary_model = Some("ghost".to_string());
        assert_eq!(
            resolve_secondary(&config),
            None,
            "a dangling alias degrades instead of failing assembly"
        );
    }

    #[test]
    fn permission_mode_labels_follow_wire_names() {
        use console_ui::ui::permission_mode_label;
        assert_eq!(permission_mode_label("plan"), "Plan Mode");
        assert_eq!(permission_mode_label("auto"), "Auto Mode");
        assert_eq!(permission_mode_label("wave"), "Wave Mode");
        // Unknown names degrade to the auto label.
        assert_eq!(permission_mode_label("typo"), "Auto Mode");
    }

    /// A two-provider config (one OpenAI-compatible, one Anthropic) with
    /// one `[models]` alias, for the picker-catalog helpers below.
    fn picker_config() -> wavecode_config::Config {
        let provider = |kind| wavecode_config::ProviderConfig {
            kind,
            base_url: "https://api.example.com".to_string(),
            env_key: None,
            api_key: Some("k".to_string()),
            context_window: None,
            max_output_tokens: None,
            fallback_providers: Vec::new(),
            rpm_limit: None,
            reasoning_effort: None,
            thinking_budget_tokens: None,
            prompt_caching: None,
            prompt_cache_ttl: None,
        };
        let mut model_providers = std::collections::HashMap::new();
        model_providers.insert(
            "fastp".to_string(),
            provider(wavecode_config::ProviderKind::OpenAiCompatible),
        );
        model_providers.insert(
            "anthropic".to_string(),
            provider(wavecode_config::ProviderKind::Anthropic),
        );
        let mut models = std::collections::HashMap::new();
        models.insert(
            "fast".to_string(),
            wavecode_config::ModelEntry {
                provider: "fastp".to_string(),
                model: "fast-model".to_string(),
                reasoning_effort: Some("low".to_string()),
            },
        );
        wavecode_config::Config {
            model: "default-model".to_string(),
            model_provider: "anthropic".to_string(),
            model_providers,
            permission_mode: None,
            permissions: wavecode_config::PermissionsConfig::default(),
            hooks: std::collections::HashMap::new(),
            mcp_servers: std::collections::HashMap::new(),
            models,
            secondary_model: None,
            max_tool_rounds: None,
        }
    }

    /// The thinking-level row exists only where a string effort applies;
    /// Anthropic budgets and unknown providers hide it.
    #[test]
    fn thinking_levels_follow_provider_kind() {
        let cfg = picker_config();
        assert_eq!(
            thinking_levels_for(&cfg, "fastp"),
            ["off", "low", "medium", "high"]
        );
        assert!(thinking_levels_for(&cfg, "anthropic").is_empty());
        assert!(thinking_levels_for(&cfg, "ghost").is_empty());
    }

    /// The picker catalog carries the `[models]` aliases plus exactly one
    /// entry for the live default model.
    #[test]
    fn model_catalog_merges_aliases_and_default_entry() {
        let cfg = picker_config();
        let entries = build_model_entries(&cfg, "anthropic", "default-model", None);
        let fast = entries
            .iter()
            .find(|e| e.label == "fast")
            .expect("alias entry present");
        assert_eq!(fast.provider, "fastp");
        assert_eq!(fast.model, "fast-model");
        assert_eq!(fast.effort.as_deref(), Some("low"));
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.label == "default-model" && e.provider == "anthropic")
                .count(),
            1
        );
    }

    /// `--session` resolves the journal snapshot and index title; a path
    /// escape is rejected before any filesystem access.
    #[test]
    fn resume_seed_loads_journal_and_rejects_bad_ids() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let history = vec![(false, "hello".to_string()), (true, "hi".to_string())];
        state_persistence::sessions::record_turn(
            &home,
            "s-1",
            "/tmp",
            "hello",
            &history,
            "Completed",
            &|t: &str| t.to_string(),
        )
        .unwrap();
        let seed = resolve_tui_seed(
            &Some("s-1".to_string()),
            false,
            Path::new("/tmp"),
            &Some(home.clone()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(seed.session_id, "s-1");
        assert_eq!(seed.history, history);
        assert!(
            resolve_tui_seed(
                &Some("../evil".to_string()),
                false,
                Path::new("/tmp"),
                &Some(home),
            )
            .is_err()
        );
    }

    /// `--continue` with nothing recorded errors with guidance; a missing
    /// home cannot honor explicit resume requests, while the flagless
    /// case stays a silent fresh session.
    #[test]
    fn continue_without_recorded_sessions_errors_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_tui_seed(
            &None,
            true,
            Path::new("/somewhere"),
            &Some(dir.path().to_path_buf()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no sessions"));
        assert!(resolve_tui_seed(&None, true, Path::new("/tmp"), &None).is_err());
        assert!(
            resolve_tui_seed(&Some("s-1".to_string()), false, Path::new("/tmp"), &None).is_err()
        );
        assert!(
            resolve_tui_seed(&None, false, Path::new("/tmp"), &None)
                .unwrap()
                .is_none()
        );
    }
}
