//! Live assembly: the offline client, the advertised catalog budget,
//! model switches, provider overrides and fallbacks, and the
//! child-surface derivation.

use super::*;

#[tokio::test]
async fn assembly_builds_a_live_client_offline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let mut handle = assemble_session(AssembleOptions {
        config_path: Some(path),
        model_override: None,
        provider_override: None,
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: None,
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();
    // Memory degrades with warnings instead of failing assembly.
    assert!(
        handle
            .warnings
            .iter()
            .any(|w| w.contains("memory disabled"))
    );
    assert!(handle.system.contains("WaveCode"));
    assert!(handle.system.contains("Available tools:"));
    assert!(handle.memory_index.is_empty());
    assert!(handle.mcp_servers.is_empty());
    // The forbidden-spawn-tool list matches the assembled catalog: no
    // drift warning on the happy path (this assembles a full session
    // with the child surface attached).
    assert!(
        !handle
            .warnings
            .iter()
            .any(|w| w.contains("child-forbidden")),
        "unexpected drift warning emitted for a synthetic test session"
    );
    // The client submits without network access; shutdown closes cleanly.
    handle
        .client
        .submit(Submission {
            id: "s1".to_string(),
            op: Op::Shutdown,
        })
        .await
        .unwrap();
    assert!(handle.client.next_event().await.is_none());
}

/// Context budget gate: the advertised tool catalog is paid for on every
/// single request, so its size has to be a number someone chose. Run
/// `cargo test -p operations-bootstrap catalog -- --nocapture` to print
/// the per-tool breakdown; raise the ceiling deliberately, never silently.
#[tokio::test]
async fn advertised_tool_catalog_stays_within_its_budget() {
    /// Ceiling in tokens for the always-on catalog, measured with the
    /// same CJK-aware estimator the context budget uses (37 tools,
    /// ~6.2k tokens as of the `pty_shell` removal).
    const CATALOG_TOKEN_BUDGET: u64 = 7_000;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let handle = assemble_session(AssembleOptions {
        config_path: Some(path),
        model_override: None,
        provider_override: None,
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: Some(dir.path().to_path_buf()),
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();

    // What the model is actually offered: `ModelAdapter::tools` resolves
    // every advertised name through this registry, so the registry's
    // specs are the shipped catalog (no MCP servers configured here).
    let specs = handle.tools_registry.specs();
    let catalog_json = serde_json::to_string(&specs).unwrap();
    let tokens = state_store::estimate_tokens(&catalog_json);
    let json_chars = catalog_json.len() as u64;
    let system_tokens = state_store::estimate_tokens(&handle.system);

    let mut rows: Vec<(usize, String)> = specs
        .iter()
        .map(|spec| {
            (
                serde_json::to_string(spec).unwrap().len(),
                spec.name.clone(),
            )
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    println!(
        "CATALOG tools={} json_chars={} est_tokens={} budget={CATALOG_TOKEN_BUDGET} system_tokens={}",
        specs.len(),
        json_chars,
        tokens,
        system_tokens
    );
    for (chars, name) in &rows {
        println!("  {name}	{chars}");
    }

    assert_eq!(
        specs.len(),
        rows.len(),
        "tool names must be unique: a duplicate would advertise twice"
    );
    assert!(
        tokens <= CATALOG_TOKEN_BUDGET,
        "advertised catalog costs {tokens} tokens, over the {CATALOG_TOKEN_BUDGET} budget"
    );
}

/// The actor's driver reaches the run loop through an `Arc<T>`
/// blanket impl; that impl must forward the live-switch seams or
/// `/model` and `/effort` silently reject in every production
/// session (regression guard: both forwards once went missing).
#[tokio::test]
async fn live_model_and_thinking_switches_survive_the_arc_wrapper() {
    // OpenAI-compatible provider so `set_thinking` has a mutable
    // effort behind it (Anthropic budgets stay config-driven and
    // legitimately reject).
    const OPENAI_CONFIG: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "open-ai-compatible"
base_url = "https://api.example.com/v1"
api_key = "k-inline"
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, OPENAI_CONFIG).unwrap();
    let mut handle = assemble_session(AssembleOptions {
        config_path: Some(path),
        model_override: None,
        provider_override: None,
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: None,
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();
    handle
        .client
        .submit(Submission {
            id: "s-model".to_string(),
            op: Op::SetModel {
                name: "m2".to_string(),
            },
        })
        .await
        .unwrap();
    handle
        .client
        .submit(Submission {
            id: "s-think".to_string(),
            op: Op::SetThinking {
                effort: "low".to_string(),
            },
        })
        .await
        .unwrap();
    handle
        .client
        .submit(Submission {
            id: "s-end".to_string(),
            op: Op::Shutdown,
        })
        .await
        .unwrap();
    let mut rejections = Vec::new();
    while let Some(event) = handle.client.next_event().await {
        if let wavecode_wire::EventMsg::Warning { message } = event.msg {
            rejections.push(message);
        }
    }
    assert!(
        rejections.is_empty(),
        "live switches rejected through the Arc driver: {rejections:?}"
    );
}

/// A stale provider override degrades to the configured provider with
/// a warning, and the reported provider id must name the provider
/// actually resolved to — not the dead override name.
#[tokio::test]
async fn stale_provider_override_falls_back_and_reports_configured_provider() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let handle = assemble_session(AssembleOptions {
        config_path: Some(path),
        model_override: None,
        provider_override: Some("ghost".to_string()),
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: None,
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();
    assert!(
        handle
            .warnings
            .iter()
            .any(|w| w.contains("provider override")),
        "fallback must warn"
    );
    assert_eq!(handle.provider_id, "p1");
    handle
        .client
        .submit(Submission {
            id: "s-end".to_string(),
            op: Op::Shutdown,
        })
        .await
        .unwrap();
}

/// The model catalog merges into the assembly config: a saved default
/// model on a `catalog:` provider (the `/model` picker's saved default)
/// resolves through the synthesized provider instead of falling back
/// with a warning — the regression behind the startup crash when the
/// configured provider was itself unusable.
#[tokio::test]
async fn catalog_provider_override_resolves_after_merge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let mut catalog = wavecode_config::ModelCatalog::default();
    catalog.insert(
        "Alias",
        wavecode_config::ModelSpec {
            provider: "catprov".into(),
            model: "cat-model".into(),
            kind: wavecode_config::ApiKind::OpenaiChat,
            base_url: "https://cat.example.com/v1".into(),
            api_key_env: None,
            api_key: Some("k-cat".into()),
            context_window: Some(200_000),
            max_output: Some(8192),
            reasoning: wavecode_config::ReasoningSpec::default(),
            modalities: wavecode_config::ModalitiesSpec::default(),
        },
    );
    catalog.save(dir.path()).unwrap();
    let handle = assemble_session(AssembleOptions {
        config_path: Some(path),
        model_override: Some("cat-model".to_string()),
        provider_override: Some("catalog:catprov".to_string()),
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: Some(dir.path().to_path_buf()),
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();
    assert_eq!(
        handle.provider_id, "catalog:catprov",
        "the catalog provider resolves to catalog:catprov without fallback"
    );
    assert_eq!(handle.model_name, "cat-model");
    handle
        .client
        .submit(Submission {
            id: "s-end".to_string(),
            op: Op::Shutdown,
        })
        .await
        .unwrap();
}

/// A malformed catalog degrades to an empty one with a warning instead
/// of failing assembly: config.toml models keep the session usable.
#[tokio::test]
async fn malformed_catalog_warns_and_assembly_continues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    std::fs::create_dir_all(dir.path().join(".wavecode")).unwrap();
    std::fs::write(
        dir.path().join(".wavecode").join("models.json"),
        "{ not json",
    )
    .unwrap();
    let handle = assemble_session(AssembleOptions {
        config_path: Some(path),
        model_override: None,
        provider_override: None,
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: Some(dir.path().to_path_buf()),
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();
    assert_eq!(handle.provider_id, "p1");
    assert!(
        handle
            .warnings
            .iter()
            .any(|w| w.contains("model catalog ignored")),
        "the broken catalog warning was not emitted"
    );
    handle
        .client
        .submit(Submission {
            id: "s-end".to_string(),
            op: Op::Shutdown,
        })
        .await
        .unwrap();
}

/// A fallback that cannot resolve (unknown name, missing key) is
/// skipped with one warning each while the session still assembles:
/// a broken fallback entry must never brick startup, the surviving
/// fallback stays silent (it joined the chain), and the primary's
/// identity is untouched.
#[tokio::test]
async fn unresolvable_fallback_providers_warn_and_skip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
api_key = "k-inline"
fallback_providers = ["fb-good", "fb-no-key", "fb-unknown"]

[model_providers.fb-good]
type = "anthropic"
base_url = "https://fb.example.com/anthropic"
api_key = "k-good"

[model_providers.fb-no-key]
type = "anthropic"
base_url = "https://fb.example.com/anthropic"
"#,
    )
    .unwrap();
    let handle = assemble_session(AssembleOptions {
        config_path: Some(path),
        model_override: None,
        provider_override: None,
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: None,
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();
    // Exactly the two broken fallbacks are named, one warning each.
    let skips: Vec<&String> = handle
        .warnings
        .iter()
        .filter(|w| w.contains("skipping fallback provider"))
        .collect();
    assert_eq!(skips.len(), 2, "both broken fallbacks warn individually");
    assert!(
        skips
            .iter()
            .any(|w| w.contains("\"fb-no-key\"") && w.contains("missing an api key")),
        "the keyless fallback names its cause: {skips:?}"
    );
    assert!(
        skips
            .iter()
            .any(|w| w.contains("\"fb-unknown\"") && w.contains("undefined provider")),
        "the unknown fallback names its cause: {skips:?}"
    );
    assert!(
        !handle.warnings.iter().any(|w| w.contains("fb-good")),
        // Reviewed: assertion diagnostics for a synthetic test session.
        "the resolvable fallback joins the chain without a warning"
    );
    // The primary identity is untouched by the fallback detour.
    assert_eq!(handle.provider_id, "p1");
    assert_eq!(handle.model_name, "m1");
    handle
        .client
        .submit(Submission {
            id: "s-end".to_string(),
            op: Op::Shutdown,
        })
        .await
        .unwrap();
}

#[test]

fn spawn_tools_derive_into_the_child_forbidden_set() {
    use wavecode_tools::Tool;
    #[derive(Debug)]
    struct FreshSpawnTool;
    #[async_trait::async_trait]
    impl Tool for FreshSpawnTool {
        fn name(&self) -> &str {
            "fresh_spawn"
        }
        fn description(&self) -> &str {
            "a spawn tool registered after the forbidden list moved to kinds"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn kind(&self) -> wavecode_protocol::ToolKind {
            wavecode_protocol::ToolKind::Spawn
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &wavecode_tools::ToolCtx,
        ) -> wavecode_tools::Result<wavecode_tools::ToolOutput> {
            Err(wavecode_tools::ToolsError::InvalidInput {
                message: "unreachable in this test".to_string(),
            })
        }
    }

    let (registry, _todos) = wavecode_tools::Registry::builtin_with_todos();
    registry.register(std::sync::Arc::new(FreshSpawnTool));
    let native = std::sync::Arc::new(std::sync::Mutex::new(crate::native::NativeExecutor::new()));
    native
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(crate::native::NativeTool {
            name: "fresh_native_spawn".to_string(),
            description: "native spawn".to_string(),
            read_only: false,
            destructive: false,
            kind: wavecode_protocol::ToolKind::Spawn,
            handler: std::sync::Arc::new(|_| (String::new(), false)),
        });

    let forbidden = crate::session::assembly::child_forbidden_tools(&registry, &native);
    // Both catalogs' Spawn tools derive in, and the compaction channel
    // joins through its constant. (The delegation tools `task`/`skill`
    // register late in assembly against the shared registry; this bare
    // registry pins the derivation mechanism itself.)
    for name in ["fresh_spawn", "fresh_native_spawn", "compact_context"] {
        assert!(
            forbidden.contains(name),
            "'{name}' must be forbidden for children: {forbidden:?}"
        );
    }
    // Read-only and shell tools stay invokable for children.
    assert!(!forbidden.contains("read"));
    assert!(!forbidden.contains("shell"));
}

// ---------------------------------------------------------------------------
// Plugin packs: skills/hooks/MCP contents reaching the assembled session
// (merged from the pack-wiring tests that used to live in tests.rs).
// ---------------------------------------------------------------------------

/// Lay out a home dir holding one plugin pack with the given files
/// (the same harness the loader tests in wavecode-skills use).
fn home_with_pack(name: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let pack = home.path().join(".wavecode").join("plugins").join(name);
    std::fs::create_dir_all(&pack).unwrap();
    for (rel, content) in files {
        let path = pack.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }
    home
}

/// Write a skill dir holding one SKILL.md with the given description.
fn write_skill(root: &std::path::Path, name: &str, description: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\ndescription: {description}.\n---\n\nBody.\n"),
    )
    .unwrap();
}

const PACK_MANIFEST: &str = r#"
name = "demo"
version = "0.1.0"
skills_dir = "skills"
hooks_file = "hooks.toml"

[mcp_servers.lint]
command = "lint-server"
args = ["--fast"]
"#;

const PACK_HOOKS: &str = r#"
[[PreToolUse]]
command = "pack.sh"
timeout_ms = 250

[[PostToolUse]]
command = "fmt.sh"

[BogusPoint]
command = "x.sh"
"#;

/// Pack contents reach the assembled session end to end: pack skills
/// merge into the catalog under the user and project sources, the pack
/// MCP entry converts into a pending server, and the pack hook rules
/// join the engine (including the unknown-point warning).
#[tokio::test]
async fn pack_contents_reach_the_session_under_standard_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let home = home_with_pack(
        "demo",
        &[("plugin.toml", PACK_MANIFEST), ("hooks.toml", PACK_HOOKS)],
    );
    // Pack skills merge under the standard sources: the user skill
    // overrides the pack skill, and the project skill overrides both,
    // so only the project description may surface for `greet`.
    let pack_skills = home
        .path()
        .join(".wavecode")
        .join("plugins")
        .join("demo")
        .join("skills");
    write_skill(&pack_skills, "greet", "pack greet helper");
    write_skill(&pack_skills, "packonly", "packonly helper");
    write_skill(
        &home.path().join(".wavecode").join("skills"),
        "greet",
        "user greet helper",
    );
    write_skill(
        &dir.path().join(".wavecode").join("skills"),
        "greet",
        "project greet helper",
    );
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, CONFIG).unwrap();

    let handle = assemble_session(AssembleOptions {
        config_path: Some(config_path),
        model_override: None,
        provider_override: None,
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: Some(home.path().to_path_buf()),
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();

    // Skills: project > user > pack, and the pack-only skill lands.
    assert!(
        handle.system.contains("project greet helper"),
        "the project skill must win the greet slot"
    );
    assert!(!handle.system.contains("user greet helper"));
    assert!(!handle.system.contains("pack greet helper"));
    assert!(handle.system.contains("packonly helper"));
    assert!(handle.skill_names.iter().any(|name| name == "packonly"));

    // MCP: the pack entry converts into a pending server with the OAuth
    // fields absent, and the display line lists it.
    let lint = handle
        .mcp_pending
        .iter()
        .find(|(name, _)| name == "lint")
        .expect("pack server rides the pending list");
    assert_eq!(lint.1.command.as_deref(), Some("lint-server"));
    assert_eq!(lint.1.args, vec!["--fast".to_string()]);
    assert!(lint.1.oauth_token_url.is_none());
    assert!(
        handle
            .mcp_servers
            .iter()
            .any(|line| line.contains("lint") && line.contains("lint-server"))
    );

    // Hooks: the unknown pack point warns with the pack attribution.
    let unknown_point = handle
        .warnings
        .iter()
        .any(|w| w.contains("plugin demo") && w.contains("unknown hook point"));
    assert!(unknown_point, "unknown-point warning missing");
}

/// A user-configured MCP server owns its name: the colliding pack entry
/// warns and drops instead of reaching the connect path.
#[tokio::test]
async fn configured_mcp_server_wins_over_pack_entry() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("{CONFIG}\n[mcp_servers.lint]\ncommand = \"config-server\"\n"),
    )
    .unwrap();
    let home = home_with_pack("demo", &[("plugin.toml", PACK_MANIFEST)]);

    let handle = assemble_session(AssembleOptions {
        config_path: Some(config_path),
        model_override: None,
        provider_override: None,
        permission_override: None,
        thinking_override: None,
        wave_denylist: None,
        cwd: dir.path().to_path_buf(),
        home: Some(home.path().to_path_buf()),
        identity: DEFAULT_IDENTITY.to_string(),
        headless: true,
        initial_history: Vec::new(),
        session_id: None,
    })
    .unwrap();

    let lints: Vec<&(String, wavecode_config::McpServerRaw)> = handle
        .mcp_pending
        .iter()
        .filter(|(name, _)| name == "lint")
        .collect();
    assert_eq!(lints.len(), 1, "the pack entry must not ride along");
    assert_eq!(lints[0].1.command.as_deref(), Some("config-server"));
    let name_owned = handle
        .warnings
        .iter()
        .any(|w| w.contains("already owns that name"));
    assert!(name_owned, "collision warning missing");
}

/// Hook definitions: config rules keep their point slot, pack rules
/// append after them with the same timeout/once semantics, and an
/// unknown pack point warns with the pack attribution.
#[test]
fn pack_hook_rules_join_config_rules_at_the_same_point() {
    let config = config_with("\n[hooks.PreToolUse]\ncommand = \"config.sh\"\n");
    let home = home_with_pack(
        "demo",
        &[("plugin.toml", PACK_MANIFEST), ("hooks.toml", PACK_HOOKS)],
    );
    let plugins = wavecode_skills::plugin::PluginLoader::load(Some(home.path()));
    let mut warnings = Vec::new();
    let defs = crate::session::assembly::hook_defs(&config, &plugins, &mut warnings);

    let pre = &defs[&wavecode_hooks::HookEventPoint::PreToolUse];
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0].command, "config.sh");
    assert_eq!(pre[1].command, "pack.sh");
    assert_eq!(pre[1].timeout_ms, 250);
    assert!(!pre[1].once);

    let post = &defs[&wavecode_hooks::HookEventPoint::PostToolUse];
    assert_eq!(post.len(), 1);
    assert_eq!(post[0].command, "fmt.sh");
    // Absent timeout/once fall back to the engine defaults, same as
    // config-declared rules.
    assert_eq!(post[0].timeout_ms, wavecode_hooks::DEFAULT_TIMEOUT_MS);

    let unknown_point = warnings
        .iter()
        .any(|w| w.contains("plugin demo") && w.contains("BogusPoint"));
    assert!(
        unknown_point,
        "unknown-point warning missing (warnings_count={})",
        warnings.len()
    );
}

/// Pack MCP entries: a configured server wins the name, later packs win
/// among themselves, and the survivors convert in sorted order.
#[test]
fn pack_mcp_entries_convert_with_config_priority_and_pack_order() {
    let config = config_with(
        "\n[mcp_servers.lint]\ncommand = \"config-server\"\n[mcp_servers.only]\ncommand = \"only-server\"\n",
    );
    let home = tempfile::tempdir().unwrap();
    let plugins_root = home.path().join(".wavecode").join("plugins");
    for (pack, command) in [("aaa", "first-server"), ("zzz", "second-server")] {
        let pack_dir = plugins_root.join(pack);
        std::fs::create_dir_all(&pack_dir).unwrap();
        std::fs::write(
            pack_dir.join("plugin.toml"),
            format!("name = \"{pack}\"\nversion = \"1\"\n\n[mcp_servers.fetch]\ncommand = \"{command}\"\n[mcp_servers.lint]\ncommand = \"{command}\"\n"),
        )
        .unwrap();
    }
    let plugins = wavecode_skills::plugin::PluginLoader::load(Some(home.path()));
    let mut warnings = Vec::new();
    let servers = crate::session::assembly::pack_mcp_servers(&config, &plugins, &mut warnings);

    // `lint` collides with config (both packs drop with a warning),
    // `fetch` collides across packs (the later pack wins, warned).
    let names: Vec<&str> = servers.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["fetch"]);
    assert_eq!(
        servers[0].1.command.as_deref(),
        Some("second-server"),
        "the later pack (zzz) wins the shared name"
    );
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("already owns that name"))
            .count(),
        2,
        "both packs' lint entries drop against config (warnings_count={})",
        warnings.len()
    );
    let replaced = warnings
        .iter()
        .any(|w| w.contains("replaces the earlier pack's entry"));
    assert!(
        replaced,
        "pack-vs-pack warning missing (warnings_count={})",
        warnings.len()
    );
}

/// A pack rule the engine can never fire (an empty command, a blank
/// matcher) warns with the pack attribution and is skipped, while the
/// usable rules from the same pack still load.
#[test]
fn unusable_pack_hook_rules_warn_and_skip() {
    let config = config_with("");
    let pack_hooks = r#"
[[PreToolUse]]
command = "pack.sh"

[[PreToolUse]]
command = "   "

[[PostToolUse]]
matcher = "  "
command = "fmt.sh"
"#;
    let home = home_with_pack(
        "demo",
        &[("plugin.toml", PACK_MANIFEST), ("hooks.toml", pack_hooks)],
    );
    let plugins = wavecode_skills::plugin::PluginLoader::load(Some(home.path()));
    let mut warnings = Vec::new();
    let defs = crate::session::assembly::hook_defs(&config, &plugins, &mut warnings);

    // Only the usable rule reaches the engine; the unusable ones warn
    // with the pack name and never join the definitions.
    let pre = &defs[&wavecode_hooks::HookEventPoint::PreToolUse];
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0].command, "pack.sh");
    let post = &defs[&wavecode_hooks::HookEventPoint::PostToolUse];
    assert!(post.is_empty(), "a blank matcher never fires: {post:?}");

    let skipped = warnings
        .iter()
        .filter(|w| w.contains("plugin demo") && w.contains("skipped"))
        .count();
    assert_eq!(skipped, 2, "both unusable rules warn: {warnings:?}");
}

/// An entry that could never connect (neither command nor url) warns
/// with the pack attribution and is skipped, so a later pack's invalid
/// entry cannot displace an earlier pack's valid one on a shared name.
#[test]
fn invalid_pack_mcp_entry_warns_and_skips() {
    let config = config_with("");
    let home = tempfile::tempdir().unwrap();
    let plugins_root = home.path().join(".wavecode").join("plugins");
    let first = plugins_root.join("aaa");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::write(
        first.join("plugin.toml"),
        "name = \"aaa\"\nversion = \"1\"\n\n[mcp_servers.fetch]\ncommand = \"first-server\"\n",
    )
    .unwrap();
    let second = plugins_root.join("zzz");
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(
        second.join("plugin.toml"),
        "name = \"zzz\"\nversion = \"1\"\n\n[mcp_servers.fetch]\nargs = []\n",
    )
    .unwrap();
    let plugins = wavecode_skills::plugin::PluginLoader::load(Some(home.path()));
    let mut warnings = Vec::new();
    let servers = crate::session::assembly::pack_mcp_servers(&config, &plugins, &mut warnings);

    let names: Vec<&str> = servers.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["fetch"]);
    assert_eq!(
        servers[0].1.command.as_deref(),
        Some("first-server"),
        "the later pack's invalid entry cannot displace the valid one"
    );
    assert!(
        warnings.iter().any(|w| w.contains("plugin zzz")
            && w.contains("skipped")
            && w.contains("neither command nor url")),
        "pack-attributed skip warning missing (warnings_count={})",
        warnings.len()
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("replaces the earlier pack's entry")),
        "no replacement happened (warnings_count={})",
        warnings.len()
    );
}
