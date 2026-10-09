/*!
 * @file SessionAssembly
 * @description Full-session composition root for the new harness stack.
 *
 * Responsibilities:
 * - Load configuration and resolve the model provider.
 * - Assemble memory, skills, hooks, registry, policy, and adapters.
 * - Build the run loop, child service, native tools, actor, and client.
 * - Collect startup warnings instead of failing on soft degradation.
 *
 * This module must not be depended on by: runtime, state, action, safety,
 * capabilities, or any lower layer. It is the top of the DAG.
 */

//! Session assembly: config file to a live client handle.
//!
//! Two-phase tool wiring: the composite executor enters the run loop
//! first, then child task tools register against the built driver. Hard
//! failures (missing config, provider, credentials) abort assembly;
//! soft degradation (memory, skills, hooks, catalog) warns and continues.
//!
//! The model-independent composition machinery lives in `assembly`
//! (pure movement; see that module). `assemble_session_after_model`
//! and `build_secret_store` are re-exported here unchanged, so every
//! external path keeps resolving.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use operations_actor::{ActorClient, SessionSurface, SubmitError};
use safety_gate::{ApprovalGate, QuestionGate};

mod assembly;

#[cfg(test)]
mod tests;

pub use assembly::{assemble_session_after_model, build_secret_store};
/// Approval wait timeout applied to parked decisions.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Session contract types: defined beside the actor so the RPC gateway
/// can name them without reaching into this composition root.
pub use operations_actor::{AssembleOptions, DEFAULT_IDENTITY, SessionError};
/// Tool dispatch rounds per turn; single-sourced from the runner so the
/// loop's ceiling and the assembly default cannot drift apart.
pub use runtime_runner::DEFAULT_MAX_TOOL_ROUNDS;

/// Assembled live session.
pub struct SessionHandle {
    /// Client for submitting operations and streaming events.
    pub client: ActorClient,
    /// Shared approval gate behind parked decisions.
    pub approvals: Arc<ApprovalGate>,
    /// Shared question gate behind parked interactive questions.
    pub questions: Arc<QuestionGate>,
    /// Shared interrupt handle for stops and drops.
    pub interrupt: infrastructure_base::InterruptHandle,
    /// Assembled system prompt (also injected into every turn).
    pub system: String,
    /// Resolved model name for status displays.
    pub model_name: String,
    /// Provider id the primary client resolves through.
    pub provider_id: String,
    /// Effective reasoning-effort level for status displays (OpenAI-
    /// compatible providers only; `None` for budget-driven thinking).
    pub thinking_effort: Option<String>,
    /// Effective permission mode wire name for status displays.
    pub permission_mode: String,
    /// Directly invokable skill names for completion sources.
    pub skill_names: Vec<String>,
    /// Persistent memory index text (empty when memory degraded).
    pub memory_index: String,
    /// Instruction files actually loaded into context (`AGENTS.md`,
    /// `AGENTS.local.md`, `.wavecode/rules/*.md` tiers), in concat
    /// order — the display source of truth for frontends, so a
    /// `/memory` view can never drift from what the session injected.
    pub instruction_sources: Vec<PathBuf>,
    /// Configured MCP servers as one display line each.
    pub mcp_servers: Vec<String>,
    /// Startup warnings in assembly order.
    pub warnings: Vec<String>,
    /// Shared tool registry for late-registered tools (skills, MCP).
    tools_registry: Arc<wavecode_tools::Registry>,
    /// Started runtime plugins owned for the session's lifetime so their
    /// services stay reachable and unload runs on teardown. Service
    /// injection into the run loop is not wired yet; access via
    /// [`SessionHandle::plugins`].
    plugins: runtime_plugin::Registry,
    /// On-demand status views over plan / goal / snapshot state, shared
    /// with frontends so slash commands never touch storage layout.
    pub status: Arc<dyn operations_actor::StatusQueries>,
    /// MCP servers awaiting live connection (sorted by name).
    mcp_pending: Vec<(String, wavecode_config::McpServerRaw)>,
    /// Credential store the journal redaction gate masks with; shared
    /// with the child journals. `None` only in tests.
    secrets: Option<std::sync::Arc<safety_secrets::SecretsStore>>,
}

impl SessionHandle {
    /// Live-connect pending MCP servers and bridge their tools.
    ///
    /// Replaces the configured-only status lines with live results and
    /// appends degradation warnings; idempotent once connected. Run it
    /// after assembly and before the first turn so forked tools exist
    /// before the model samples.
    ///
    /// Snapshot semantics: the system prompt and the child tool-surface
    /// policy were built during assembly, before this runs, so MCP tools
    /// appear in the live sampling catalog but not in the prompt's tool
    /// list, and children never inherit them.
    pub async fn connect_mcp_servers(&mut self) {
        if self.mcp_pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.mcp_pending);
        let report = wavecode_mcp::connect_all(&pending, &self.tools_registry).await;
        self.mcp_servers = report.lines;
        self.warnings.extend(report.warnings);
    }

    /// Started runtime plugins (service map and lifecycle).
    pub fn plugins(&self) -> &runtime_plugin::Registry {
        &self.plugins
    }

    /// The credential mask every persisted text should ride (`None`
    /// in tests): wraps the shared store, so frontends journal with
    /// the same redaction the child journals use.
    pub fn secret_redactor(&self) -> Option<std::sync::Arc<SecretRedactor>> {
        self.secrets.clone().map(|store| {
            let moved: std::sync::Arc<SecretRedactor> =
                std::sync::Arc::new(move |text: &str| store.redact(text));
            moved
        })
    }
}

/// Shared credential mask: maps persisted text to its redacted form
/// without leaking the store behind it.
pub type SecretRedactor = dyn Fn(&str) -> String + Send + Sync;

/// The gateway-facing session surface, served by the assembled handle.
///
/// Delegates to the actor client and the shared gates; the servers never
/// need more than this (see [`operations_actor::SessionSurface`]).
#[async_trait::async_trait]
impl SessionSurface for SessionHandle {
    async fn submit(&mut self, submission: wavecode_wire::Submission) -> Result<(), SubmitError> {
        self.client.submit(submission).await
    }

    async fn next_event(&mut self) -> Option<wavecode_wire::Event> {
        self.client.next_event().await
    }

    fn permission_mode(&self) -> &str {
        &self.permission_mode
    }

    fn approvals(&self) -> Arc<ApprovalGate> {
        self.approvals.clone()
    }

    fn questions(&self) -> Arc<QuestionGate> {
        self.questions.clone()
    }

    fn interrupt(&self) -> infrastructure_base::InterruptHandle {
        self.interrupt.clone()
    }
}

/// Resolve the effective permission mode: CLI override wins over config.
///
/// Unknown values warn and fall back to `Auto` so a typo never locks
/// the session into a mode the policy rejected. Legacy mode names parse
/// onto their successor but warn, so silent behavior drift for old
/// config files is at least visible in the startup warnings.
pub fn resolve_permission_mode(
    config_value: Option<&str>,
    cli_override: Option<&str>,
    warnings: &mut Vec<String>,
) -> wavecode_protocol::PermissionMode {
    if let Some(raw) = cli_override {
        return wavecode_protocol::PermissionMode::parse(raw).unwrap_or_else(|| {
            warnings.push(format!(
                "unrecognized --permission-mode {raw:?}; falling back to auto"
            ));
            wavecode_protocol::PermissionMode::Auto
        });
    }
    config_value
        .and_then(|raw| {
            let parsed = wavecode_protocol::PermissionMode::parse(raw);
            if parsed.is_some()
                && matches!(
                    raw,
                    "guarded" | "default" | "acceptEdits" | "bypassPermissions" | "yolo"
                )
            {
                warnings.push(format!(
                    "permission_mode {raw:?} is a legacy name; use plan, auto, or wave"
                ));
            }
            parsed.or_else(|| {
                warnings.push(format!(
                    "unrecognized permission_mode {raw:?}; falling back to auto"
                ));
                None
            })
        })
        .unwrap_or(wavecode_protocol::PermissionMode::Auto)
}

/// The startup permission tables plus what a human needs to know about them.
///
/// One builder serves both consumers: session assembly takes the tables,
/// `wavecode doctor` takes the counts and findings. A separate audit path
/// could drift from what sessions actually load, which is worse than no
/// audit at all.
pub struct Permissions {
    /// Validated allow rules in source order (authored entries, then grants).
    pub allow: Vec<wavecode_sandbox::Rule>,
    /// Validated deny rules in source order (authored entries, then the
    /// session denylist). Deny-first ordering lives in the sandbox, not here.
    pub deny: Vec<wavecode_sandbox::Rule>,
    /// Authored `[permissions] allow` entries that parsed.
    pub authored_allow: usize,
    /// Persisted "always allow" grants that loaded.
    pub persisted_grants: usize,
    /// One line per entry needing a human: invalid syntax, or an allow a
    /// deny rule provably shadows. Assembly surfaces these as startup
    /// warnings; doctor fails on them.
    pub findings: Vec<String>,
}

/// Build the startup permission tables.
///
/// Allow comes from two sources: entries a human authored in the user-level
/// config (`[permissions] allow`) and the literal grants earlier sessions
/// persisted through "always allow". Deny is config plus the session
/// denylist.
///
/// Config entries must already be `Scope(pattern)`; only denylist entries get
/// bare-command Bash scoping (they come from a settings field of command
/// fragments, not rule syntax). Entries validate one at a time: a typo costs
/// only its own line and surfaces as a finding, rather than failing the table
/// it sits in — losing the deny table over a bad allow entry would silently
/// widen authority. Matching semantics stay entirely in the sandbox.
pub fn load_permissions(
    config: &wavecode_config::Config,
    home: Option<&std::path::Path>,
    session_denylist: &[String],
) -> Permissions {
    let mut findings = Vec::new();
    let mut allow_entries = config.permissions.allow.clone();
    let mut grants = 0usize;
    if let Some(home) = home {
        let stored = state_persistence::grants::load_grants(home);
        if stored.malformed > 0 {
            findings.push(format!(
                "{} always-allow grant line(s) in {} are unreadable and were skipped",
                stored.malformed,
                state_persistence::grants::grants_path(home).display()
            ));
        }
        grants = stored.grants.len();
        allow_entries.extend(stored.grants.into_iter().map(|grant| grant.rule));
    }
    let deny_entries: Vec<String> = config
        .permissions
        .deny
        .iter()
        .cloned()
        .chain(session_denylist.iter().map(String::as_str).map(bash_scope))
        .collect();
    let allow = validate_entries(&allow_entries, "allow", &mut findings);
    let deny = validate_entries(&deny_entries, "deny", &mut findings);
    // Deny-first means an allow a deny rule fully covers can never change a
    // verdict: still harmless, but the human writing it expects otherwise.
    for rule in &allow {
        if let Some(ban) = deny.iter().find(|ban| rule.is_covered_by(ban)) {
            findings.push(format!(
                "allow rule {rule} can never apply: {ban} denies everything it matches"
            ));
        }
    }
    let authored_allow = allow.len().saturating_sub(grants);
    Permissions {
        allow,
        deny,
        authored_allow,
        persisted_grants: grants,
        findings,
    }
}

/// Parse entries one at a time, reporting each failure instead of stopping.
fn validate_entries(
    entries: &[String],
    table: &str,
    findings: &mut Vec<String>,
) -> Vec<wavecode_sandbox::Rule> {
    entries
        .iter()
        .filter_map(|entry| {
            wavecode_sandbox::Rule::parse(entry)
                .map_err(|error| findings.push(format!("invalid {table} rule: {error}")))
                .ok()
        })
        .collect()
}

/// Validate every `[mcp_servers.<name>]` entry the way a session's
/// connect path will: name validity, the either-or rule, and endpoint
/// validation all run through the mcp crate's single conversion
/// (`McpServerConfig::from_raw`), so this report cannot drift from what
/// a session actually enforces at startup. One line per broken entry;
/// an empty result means every configured server would connect.
///
/// Doctor-facing helper (same pattern as [`load_permissions`] /
/// [`confinement_status`]): frontends call it through the composition
/// root instead of naming the mcp crate.
pub fn mcp_config_findings(config: &wavecode_config::Config) -> Vec<String> {
    config
        .mcp_servers
        .iter()
        .filter_map(|(name, raw)| {
            wavecode_mcp::McpServerConfig::from_raw(name, raw)
                .err()
                .map(|reason| format!("{name}: {reason}"))
        })
        .collect()
}

/// One line on the OS confinement a session's shell spawns will get.
///
/// Frontends read this through the composition root so they never name the
/// sandbox crate. A backend whose plain status line would overstate the
/// isolation it holds declares its gap itself
/// (`SandboxBackend::status_appendix`), so a new partial backend cannot
/// silently lose the gap disclosure by falling out of a consumer-side
/// backend-name check.
pub fn confinement_status() -> String {
    let backend = wavecode_sandbox::Sandbox::detect_backend();
    let line = wavecode_sandbox::status_line(&backend);
    match backend.status_appendix() {
        Some(gap) => format!("{line}; {gap}"),
        None => line,
    }
}

/// Resolve the effective denylist: an explicit override wins; `None`
/// loads the shared store under `home` so every surface (TUI, exec,
/// REPL, ACP, serve) enforces the same user rules — a hardcoded empty
/// here once left RPC-served sessions silently unguarded. No home
/// means no store, hence no entries.
fn resolve_denylist(override_list: Option<Vec<String>>, home: Option<&Path>) -> Vec<String> {
    override_list.unwrap_or_else(|| {
        home.map(|home| wavecode_config::denylist::load_from(&home.join(".wavecode")))
            .unwrap_or_default()
    })
}

/// Bare denylist entries get the Bash scope; already-scoped ones pass
/// through untouched.
fn bash_scope(entry: &str) -> String {
    let trimmed = entry.trim();
    if trimmed.starts_with("Bash(") || trimmed.starts_with("File(") {
        entry.to_string()
    } else {
        format!("Bash({entry})")
    }
}

/// Merge the model catalog (`~/.wavecode/models.json`) into the
/// assembly config: its models become `[models]` entries and
/// synthesized `catalog:<provider>` providers. Without this a saved
/// default model on a catalog-only provider resolves against a config
/// that has never heard of it — the override falls back with a
/// warning and, when the configured provider is itself unusable,
/// assembly aborts outright. Load failures degrade to an empty catalog
/// with a warning; config.toml models keep the session usable. No
/// home means no catalog file to find.
fn merge_model_catalog(
    config: &mut wavecode_config::Config,
    home: Option<&Path>,
    warnings: &mut Vec<String>,
) {
    let Some(home_root) = home else {
        return;
    };
    match wavecode_config::ModelCatalog::load(home_root) {
        Ok(catalog) => catalog.merge_into(config),
        Err(e) => warnings.push(format!("model catalog ignored: {e}")),
    }
}

/// What the model chain samples through: the resolved provider with its
/// key, the wire model name, and the reasoning-effort override. One
/// bundle so the chain builder takes a single sampling argument beside
/// its two side channels (fallback resolution config, warnings).
struct ModelChainSpec<'a> {
    provider: &'a wavecode_config::ProviderConfig,
    api_key: String,
    model_name: &'a str,
    thinking_override: Option<&'a str>,
}

/// Build the primary model client plus its ordered fallback chain, each
/// wrapped in in-layer transient-failure retries.
///
/// The primary and every fallback share one constructor; each fallback
/// resolves its own provider entry and key, so credentials never cross
/// providers. Unresolvable fallbacks (unknown name, missing key, client
/// init failure) warn and skip instead of failing the session. The
/// primary has nothing to degrade to — a client that cannot even build
/// (TLS init) aborts assembly, matching the hard-failure convention.
/// Retries live in this layer (backoff + deadline + auth fail-fast);
/// cross-provider failover stays in `FallbackModel`, so the two never
/// amplify each other. Without the retry wrap a single 5xx/429 at
/// request establishment fails the whole turn.
fn build_model_chain(
    config: &wavecode_config::Config,
    spec: ModelChainSpec<'_>,
    warnings: &mut Vec<String>,
) -> Result<Arc<dyn wavecode_llm::ChatModel>, SessionError> {
    let primary: Arc<dyn wavecode_llm::ChatModel> = crate::model_adapter::build_chat_model(
        spec.provider,
        spec.api_key,
        spec.model_name,
        spec.thinking_override,
    )
    .map_err(SessionError::Model)?;
    let mut chain: Vec<Arc<dyn wavecode_llm::ChatModel>> = vec![primary];
    for name in &spec.provider.fallback_providers {
        match config.resolve_named_provider(name) {
            Ok((fallback_provider, fallback_key)) => {
                match crate::model_adapter::build_chat_model(
                    fallback_provider,
                    fallback_key,
                    spec.model_name,
                    spec.thinking_override,
                ) {
                    Ok(model) => chain.push(model),
                    Err(error) => {
                        warnings.push(format!("skipping fallback provider {name:?}: {error}"))
                    }
                }
            }
            Err(error) => warnings.push(format!("skipping fallback provider {name:?}: {error}")),
        }
    }
    let chain: Vec<Arc<dyn wavecode_llm::ChatModel>> = chain
        .into_iter()
        .map(|model| {
            Arc::new(wavecode_llm::retry::RetryingModel::new(
                model,
                wavecode_llm::retry::RetryPolicy::default(),
            )) as Arc<dyn wavecode_llm::ChatModel>
        })
        .collect();
    Ok(if chain.len() > 1 {
        Arc::new(crate::model_adapter::FallbackModel::new(chain))
    } else {
        chain
            .into_iter()
            .next()
            .expect("primary model always present")
    })
}

/// Assemble a live session: config to client handle.
///
/// Must be called inside a tokio runtime (the actor task spawns here).
/// Drives no turns; sampling starts on the first submitted input, so
/// assembly itself needs no network access.
pub fn assemble_session(options: AssembleOptions) -> Result<SessionHandle, SessionError> {
    let AssembleOptions {
        config_path,
        model_override,
        provider_override,
        permission_override,
        thinking_override,
        cwd,
        home,
        identity,
        headless,
        initial_history,
        wave_denylist,
        session_id,
    } = options;
    let mut warnings = Vec::new();
    let wave_denylist = resolve_denylist(wave_denylist, home.as_deref());

    // 1. Configuration and provider resolution (hard failure surface).
    let mut config = match config_path {
        Some(path) => wavecode_config::Config::load_from(&path)?,
        None => wavecode_config::Config::load()?,
    };
    merge_model_catalog(&mut config, home.as_deref(), &mut warnings);
    // A provider override (saved default model on another provider)
    // degrades to the configured provider with a warning when unknown,
    // matching the permission-mode fallback style: a stale saved default
    // must never brick startup. The reported provider id always names
    // the provider actually resolved to.
    let (provider, api_key, provider_id) = match provider_override.as_deref() {
        Some(name) => match config.resolve_named_provider(name) {
            Ok((provider, api_key)) => (provider, api_key, name.to_string()),
            Err(error) => {
                warnings.push(format!(
                    "provider override {name:?} unusable ({error}); using configured provider"
                ));
                let (provider, api_key) = config.resolve_provider()?;
                (provider, api_key, config.model_provider.clone())
            }
        },
        None => {
            let (provider, api_key) = config.resolve_provider()?;
            (provider, api_key, config.model_provider.clone())
        }
    };
    if is_insecure_http_url(&provider.base_url) {
        warnings.push(format!(
            "base_url uses plain http to a non-loopback host ({}); credentials travel in cleartext",
            provider.base_url
        ));
    }

    // 2. Provider model client; the model-independent remainder lives in
    // `assemble_session_after_model` so tests can inject a stub model.
    // Production behavior is unchanged: this resolves config and builds
    // the provider client, then delegates everything below.
    // Effort display state: only OpenAI-compatible providers carry a
    // switchable string level; Anthropic budgets stay config-only.
    let thinking_effort = if provider.kind.carries_reasoning_effort() {
        thinking_override
            .clone()
            .or_else(|| provider.reasoning_effort.clone())
    } else {
        None
    };
    let model_name = model_override.unwrap_or_else(|| config.model.clone());
    let model = build_model_chain(
        &config,
        ModelChainSpec {
            provider,
            api_key,
            model_name: &model_name,
            thinking_override: thinking_override.as_deref(),
        },
        &mut warnings,
    )?;
    let deny_env = provider
        .env_key
        .as_deref()
        .filter(|name| !name.is_empty())
        .map(|name| vec![name.to_owned()])
        .unwrap_or_default();
    // Effective model limits: explicit config wins; OpenAI-compatible
    // providers without explicit limits consult the capability table so
    // DeepSeek-class models sample with their real window instead of the
    // Anthropic-shaped default.
    let (context_window, max_output_tokens) = if provider.kind.carries_reasoning_effort()
        && provider.context_window.is_none()
        && provider.max_output_tokens.is_none()
    {
        let caps = wavecode_llm::ModelCapabilities::resolve_or(
            &model_name,
            provider.context_window(),
            provider.max_output_tokens(),
        );
        (caps.context_window, caps.max_output_tokens)
    } else {
        (provider.context_window(), provider.max_output_tokens())
    };
    // Whether the window resolves per model name (same condition as the
    // resolution above): when it does, the loop's budget gate follows a
    // `/model` switch onto the new model's window.
    let per_model_window = if provider.kind.carries_reasoning_effort()
        && provider.context_window.is_none()
        && provider.max_output_tokens.is_none()
    {
        Some(provider.context_window())
    } else {
        None
    };
    Ok(assemble_session_after_model(WithModel {
        session_id: session_id.clone(),
        config,
        model,
        model_name,
        provider_id,
        thinking_effort,
        deny_env,
        context_window,
        max_output_tokens,
        per_model_window,
        permission_override,
        cwd,
        home,
        identity,
        headless,
        initial_history,
        wave_denylist,
        warnings,
    }))
}

/// Model-independent half of session assembly (test seam carrier).
///
/// Production fills this from config plus the provider-built client in
/// [`assemble_session`]; tests fill it directly around a stub model so
/// prompt paths run hermetically. Changing these fields must not change
/// what production assembles for the same inputs. Public only so the
/// gateway's server tests can assemble sessions the production way.
///
/// Version policy (on record, deliberate): workspace-internal seam, no
/// `#[non_exhaustive]`, no builder. Every constructor lives in this repo
/// (this crate plus the gateway's test assemblies) and builds it by
/// exhaustive literal, so adding a field breaks compilation at each site
/// and forces same-commit review — the guarantee the struct doc above
/// depends on. Fields have no meaningful defaults (`model`, `cwd`), so
/// `Default`-based construction would only hide mistakes; renaming or
/// re-typing a field is a breaking change updated in the same commit.
pub struct WithModel {
    /// Full config for hooks, permission mode, and MCP descriptions.
    pub config: wavecode_config::Config,
    /// Chat model: the provider client in production, a stub in tests.
    pub model: Arc<dyn wavecode_llm::ChatModel>,
    /// Effective model name for status displays and sampling.
    pub model_name: String,
    /// Provider id the primary client resolves through.
    pub provider_id: String,
    /// Effective reasoning-effort level for status displays.
    pub thinking_effort: Option<String>,
    /// Env names hidden from tools, resolved from the provider.
    pub deny_env: Vec<String>,
    /// Effective context window, resolved from the provider.
    pub context_window: u64,
    /// Effective output cap, resolved from the provider.
    pub max_output_tokens: u32,
    /// Capability-table fallback for per-name window resolution; `Some`
    /// when the provider leaves limits to the table, so a `/model`
    /// switch moves the loop's budget gate onto the new window. `None`
    /// when the window is fixed by explicit provider config.
    pub per_model_window: Option<u64>,
    /// `--permission-mode` override winning over the configured mode.
    pub permission_override: Option<String>,
    /// Working directory for tools and relative paths.
    pub cwd: PathBuf,
    /// Home directory; `None` degrades memory without failing.
    pub home: Option<PathBuf>,
    /// Identity block prepended to the system prompt.
    pub identity: String,
    /// True for non-interactive drivers: approvals deny openly.
    pub headless: bool,
    /// Seed history as (from_model, text) pairs.
    pub initial_history: Vec<(bool, String)>,
    /// `wave`-mode denylist entries (Bash rule syntax).
    pub wave_denylist: Vec<String>,
    /// Session id whose turn journal this assembly records to, when the
    /// caller already minted one (see [`AssembleOptions::session_id`]).
    pub session_id: Option<String>,
    /// Warnings accumulated before the model-independent half.
    pub warnings: Vec<String>,
}

/// True for http URLs outside loopback hosts (credentials at risk).
fn is_insecure_http_url(base_url: &str) -> bool {
    // URL schemes are case-insensitive (RFC 3986); match `http://` in any
    // casing so `HTTP://evil.example.com` cannot bypass the warning.
    let rest = match base_url.get(.."http://".len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case("http://") => &base_url["http://".len()..],
        _ => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority
        .strip_prefix('[')
        .and_then(|rest| rest.split(']').next())
    {
        Some(v6) => v6,
        None => authority.split(':').next().unwrap_or_default(),
    };
    !matches!(host, "localhost" | "127.0.0.1" | "::1")
}
