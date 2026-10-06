use std::sync::Mutex;

use runtime_runner::RunLoop;
use state_store::Conversation;

use super::*;
use crate::HeadlessDeny;
use crate::compactor::ContextCompactor;
use crate::hook_adapter::HookAdapter;
use crate::model_adapter::ModelAdapter;
use crate::plan_adapter::TodoPlanTracker;
use crate::policy_adapter::PolicyAdapter;
use crate::tool_adapter::ToolAdapter;
use wavecode_llm::{ChatModel, ChatRequest, EventStream, StreamEvent, Usage};
use wavecode_wire::{Op, Submission};

use super::assembly::seed_conversation;

const CONFIG: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
api_key = "k-inline"
"#;

/// Load a config built on the default fixture text.
fn config_with(tail: &str) -> wavecode_config::Config {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, format!("{CONFIG}{tail}")).unwrap();
    wavecode_config::Config::load_from(&path).unwrap()
}

fn displayed(rules: &[wavecode_sandbox::Rule]) -> Vec<String> {
    rules.iter().map(|rule| rule.to_string()).collect()
}

/// `None` falls back to the shared denylist store under `home`, an
/// explicit override wins, and no home means no store entries.
#[test]
fn denylist_resolution_prefers_the_explicit_override() {
    let dir = tempfile::tempdir().unwrap();
    let wave = dir.path().join(".wavecode");
    std::fs::create_dir_all(&wave).unwrap();
    wavecode_config::denylist::save_to(&wave, &["rm -rf".to_string()]).unwrap();

    assert_eq!(
        resolve_denylist(None, Some(dir.path())),
        vec!["rm -rf".to_string()]
    );
    assert_eq!(
        resolve_denylist(Some(vec!["git push".to_string()]), Some(dir.path())),
        vec!["git push".to_string()]
    );
    assert!(resolve_denylist(None, None).is_empty());
}

/// The text seed is journaled as its baseline, so a session created
/// before the block journal existed does not end up with a journal that
/// describes only its tail.
#[test]
fn a_text_seed_becomes_the_journal_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let journal = state_persistence::history::HistoryJournal::new(dir.path().join("h.jsonl"));
    let seed = vec![
        (false, "fix the test".to_string()),
        (true, "done".to_string()),
    ];
    let mut warnings = Vec::new();
    let conv = seed_conversation(Some(journal.clone()), &seed, &mut warnings);
    assert_eq!(conv.len(), 2);
    assert!(warnings.is_empty(), "{warnings:?}");

    // Resume once: the journal is now the source, and re-resuming must
    // not lengthen the history.
    for _ in 0..2 {
        let mut warnings = Vec::new();
        let conv = seed_conversation(Some(journal.clone()), &seed, &mut warnings);
        assert_eq!(conv.len(), 2, "resume grew the history");
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}

/// A call whose result never reached disk closes as unresolved, and the
/// human is told instead of the gap being papered over.
#[test]
fn a_lost_tool_call_closes_and_warns_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let journal = state_persistence::history::HistoryJournal::new(dir.path().join("h.jsonl"));
    {
        let mut conversation = Conversation::with_sink(std::sync::Arc::new(
            crate::history_journal::JournalSink::new(journal.clone(), 0),
        ));
        conversation.push(state_store::Role::User, "write it");
        conversation.push_blocks(
            state_store::Role::Assistant,
            vec![state_store::Block::ToolUse {
                call_id: "c1".to_string(),
                name: "write".to_string(),
                input: serde_json::json!({"path": "a.txt"}),
            }],
        );
        // process dies before the result lands
    }
    let mut warnings = Vec::new();
    let conv = seed_conversation(Some(journal), &[], &mut warnings);
    assert_eq!(conv.len(), 3, "the closing result is appended");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("lost their outcome"), "{warnings:?}");
    assert!(warnings[0].contains("c1"), "{warnings:?}");
}

/// No journal means the previous behavior exactly: the caller's seed is
/// the whole story, held in memory only.
#[test]
fn without_a_journal_the_seed_is_all_there_is() {
    let mut warnings = Vec::new();
    let conv = seed_conversation(
        None,
        &[(false, "a".to_string()), (true, "b".to_string())],
        &mut warnings,
    );
    assert_eq!(conv.len(), 2);
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// `[permissions]` reaches the tables, and bare denylist entries keep
/// their historical Bash scoping.
#[test]
fn authored_rules_load_into_both_tables() {
    let config = config_with(
        r#"
[permissions]
allow = ["Bash(git *)"]
deny = ["File(.env)"]
"#,
    );
    let perms = load_permissions(&config, None, &["rm -rf".to_string()]);
    assert_eq!(displayed(&perms.allow), ["Bash(git *)"]);
    assert_eq!(displayed(&perms.deny), ["File(.env)", "Bash(rm -rf)"]);
    assert_eq!((perms.authored_allow, perms.persisted_grants), (1, 0));
    assert!(perms.findings.is_empty(), "{:?}", perms.findings);
}

/// Grants a previous session persisted are startup allow rules, and the
/// report tells authored entries apart from clicked ones.
#[test]
fn persisted_grants_join_the_allow_table() {
    let dir = tempfile::tempdir().unwrap();
    let grant = state_persistence::grants::Grant {
        rule: "Bash(cargo test --locked)".to_string(),
        tool: "shell".to_string(),
        granted_at_secs: 1,
        session: "older".to_string(),
    };
    state_persistence::grants::add_grant(dir.path(), &grant).unwrap();
    let config = config_with(
        r#"
[permissions]
allow = ["Bash(git status)"]
"#,
    );
    let perms = load_permissions(&config, Some(dir.path()), &[]);
    assert_eq!(
        displayed(&perms.allow),
        ["Bash(git status)", "Bash(cargo test --locked)"]
    );
    assert_eq!((perms.authored_allow, perms.persisted_grants), (1, 1));
    assert!(perms.findings.is_empty(), "{:?}", perms.findings);
}

/// The whole point of per-entry validation: one typo must never cost the
/// other table, and never fail silently. A bare config entry is a typo
/// too — only the denylist gets Bash scoping.
#[test]
fn an_invalid_entry_costs_only_itself() {
    let config = config_with(
        r#"
[permissions]
allow = ["Bash(git *)", "not a rule"]
deny = ["also not a rule", "File(.env)"]
"#,
    );
    let perms = load_permissions(&config, None, &[]);
    assert_eq!(displayed(&perms.allow), ["Bash(git *)"]);
    assert_eq!(displayed(&perms.deny), ["File(.env)"]);
    assert_eq!(perms.findings.len(), 2, "{:?}", perms.findings);
    assert!(perms.findings.iter().all(|f| f.starts_with("invalid")));
}

/// Deny-first makes a fully shadowed allow dead; doctor says so instead
/// of letting the human wonder why the prompt never went away.
#[test]
fn shadowed_allow_rules_are_reported_as_dead() {
    let config = config_with(
        r#"
[permissions]
allow = ["Bash(git commit *)", "Bash(git status)"]
deny = ["Bash(git *)"]
"#,
    );
    let perms = load_permissions(&config, None, &[]);
    assert_eq!(perms.allow.len(), 2, "dead allows stay loaded");
    assert_eq!(
        perms
            .findings
            .iter()
            .filter(|finding| finding.contains("can never apply"))
            .count(),
        2
    );
}

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
        // Reviewed: assertion diagnostics for a synthetic test session.
        "unexpected drift warning: {:?}",
        handle.warnings
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
        // Reviewed: assertion diagnostics for a synthetic test session.
        "fallback must warn: {:?}",
        handle.warnings
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
    assert_eq!(
        skips.len(),
        2,
        // Reviewed: assertion diagnostics for a synthetic test session.
        "both broken fallbacks warn individually: {:?}",
        handle.warnings
    );
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
        "the resolvable fallback joins the chain without a warning: {:?}",
        handle.warnings
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
fn cli_permission_override_wins_over_config() {
    let mut warnings = Vec::new();
    let mode = resolve_permission_mode(Some("plan"), Some("auto"), &mut warnings);
    assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
    assert!(warnings.is_empty());
}

#[test]
fn invalid_permission_values_warn_and_fall_back() {
    let mut warnings = Vec::new();
    let mode = resolve_permission_mode(Some("plan"), Some("nope"), &mut warnings);
    assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
    assert!(warnings.iter().any(|w| w.contains("--permission-mode")));
    warnings.clear();
    let mode = resolve_permission_mode(Some("nope"), None, &mut warnings);
    assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
    assert!(warnings.iter().any(|w| w.contains("permission_mode")));
}

#[test]
fn config_permission_used_without_override() {
    let mut warnings = Vec::new();
    let mode = resolve_permission_mode(Some("auto"), None, &mut warnings);
    assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
    assert!(warnings.is_empty(), "canonical names never warn");
    let mode = resolve_permission_mode(None, None, &mut warnings);
    assert_eq!(mode, wavecode_protocol::PermissionMode::Auto);
    assert!(warnings.is_empty());
}

#[test]
fn legacy_mode_names_migrate_with_a_visible_warning() {
    // Every legacy alias `PermissionMode::parse` accepts warns, so an
    // old config's mode drift is always visible in the startup output.
    for (raw, expected) in [
        ("guarded", wavecode_protocol::PermissionMode::Auto),
        ("default", wavecode_protocol::PermissionMode::Auto),
        ("acceptEdits", wavecode_protocol::PermissionMode::Auto),
        ("bypassPermissions", wavecode_protocol::PermissionMode::Wave),
        ("yolo", wavecode_protocol::PermissionMode::Wave),
    ] {
        let mut warnings = Vec::new();
        let mode = resolve_permission_mode(Some(raw), None, &mut warnings);
        assert_eq!(mode, expected, "{raw}");
        assert!(
            warnings.iter().any(|w| w.contains("legacy name")),
            "{raw} must warn"
        );
    }
}

#[test]
fn insecure_urls_detected_without_false_loopback_positives() {
    assert!(is_insecure_http_url("http://api.example.com"));
    assert!(!is_insecure_http_url("https://api.example.com"));
    assert!(!is_insecure_http_url("http://127.0.0.1:8080"));
    assert!(!is_insecure_http_url("http://localhost:3000/v1"));
    assert!(!is_insecure_http_url("http://[::1]:9000"));
    assert!(is_insecure_http_url("http://127.0.0.1.evil.example.com"));
    // URL schemes are case-insensitive: uppercase HTTP must warn too.
    assert!(is_insecure_http_url("HTTP://api.example.com"));
    assert!(is_insecure_http_url("Http://api.example.com"));
    assert!(!is_insecure_http_url("HTTP://localhost:3000/v1"));
}

/// Scripted model replaying canned streams in order; exhausted scripts
/// degrade to an empty completion so the loop always terminates.
struct ScriptedModel {
    scripts: std::sync::Mutex<std::collections::VecDeque<Vec<StreamEvent>>>,
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn stream(&self, _req: ChatRequest) -> wavecode_llm::Result<EventStream> {
        let script = self
            .scripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| {
                vec![StreamEvent::MessageComplete {
                    stop_reason: "end_turn".to_string(),
                    usage: Usage {
                        // nosemgrep: codacy.yaml.security.hard-coded-tokens
                        input_tokens: 1,
                        // nosemgrep: codacy.yaml.security.hard-coded-tokens
                        output_tokens: 1,
                        ..Usage::default()
                    },
                }]
            });
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

fn write_turn_script() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            text: "working".to_string(),
        },
        StreamEvent::ToolUseBegin {
            id: "c1".to_string(),
            name: "write".to_string(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: r#"{"path":"hello.txt","content":"wavecode-smoke-ok"}"#.to_string(),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "end_turn".to_string(),
            usage: Usage {
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                input_tokens: 10,
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                output_tokens: 5,
                ..Usage::default()
            },
        },
    ]
}

fn done_script() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            text: "done".to_string(),
        },
        StreamEvent::MessageComplete {
            stop_reason: "end_turn".to_string(),
            usage: Usage {
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                input_tokens: 10,
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                output_tokens: 2,
                ..Usage::default()
            },
        },
    ]
}

/// End-to-end ReAct proof without network: a scripted model drives two
/// rounds (write, then report) through the real policy, real approval
/// bypass, and real filesystem tools in an isolated directory.
///
/// This is the closest offline stand-in for a live coding task: the
/// only substitution is the model itself. Live runs additionally need
/// a reachable provider; everything past sampling is identical.
#[tokio::test]
async fn scripted_loop_writes_a_real_file_across_rounds() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let (registry, todos) = wavecode_tools::Registry::builtin_with_todos();
    let registry = Arc::new(registry);
    let tools = ToolAdapter::new(
        registry.clone(),
        wavecode_tools::ToolCtx {
            cwd: cwd.clone(),
            deny_env: Vec::new(),
        },
    );
    let policy = PolicyAdapter::new(
        wavecode_sandbox::Sandbox::without_rules(wavecode_protocol::PermissionMode::Auto),
        registry.clone(),
    );
    let hooks = HookAdapter::new(
        Arc::new(wavecode_hooks::HookEngine::new(
            std::collections::HashMap::new(),
        )),
        cwd.clone(),
    );
    let model = Arc::new(ScriptedModel {
        scripts: std::sync::Mutex::new([write_turn_script(), done_script()].into_iter().collect()),
    });
    let adapter = ModelAdapter::new(model.clone(), "scripted".to_string(), 100, registry.clone());
    let plans = TodoPlanTracker::new(todos);
    let compactor = ContextCompactor::new(model.clone(), "scripted".to_string());
    let interrupt = infrastructure_base::InterruptHandle::new();
    // Durability rides the same conversation a live session would use:
    // the journal is attached at construction, so every history mutation
    // the run loop makes is mirrored here.
    let journal =
        state_persistence::history::HistoryJournal::new(dir.path().join("s-1.history.jsonl"));
    let mut conversation = Conversation::with_sink(Arc::new(
        crate::history_journal::JournalSink::new(journal.clone(), 0),
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_events = seen.clone();
    let outcome = RunLoop::new(
        tools,
        policy,
        hooks,
        adapter,
        HeadlessDeny,
        plans,
        compactor,
        runtime_runner::RunConfig {
            model_name: "scripted".to_string(),
            context_window: 200_000,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            max_output_tokens: 100,
            max_tool_rounds: 8,
            max_continuations: runtime_runner::MAX_CONTINUATIONS,
            max_plan_nudges: runtime_runner::MAX_PLAN_NUDGES,
            max_goal_continuations: runtime_runner::MAX_GOAL_CONTINUATIONS,
            max_goal_rearms: runtime_runner::MAX_GOAL_REARMS,
            max_stop_blocks: runtime_runner::MAX_STOP_BLOCKS,
            max_reactive_compacts: runtime_runner::MAX_REACTIVE_COMPACTS,
            max_repeat_streak: runtime_runner::MAX_REPEAT_STREAK,
            max_wire_images: runtime_runner::MAX_WIRE_IMAGES,
        },
        interrupt,
    )
    .run_turn(
        &runtime_runner::RunContext {
            run_id: "proof".to_string(),
            submission_id: "proof".to_string(),
            input: "write and report".to_string(),
            images: Vec::new(),
        },
        &mut conversation,
        runtime_runner::TurnInput::text("write and report"),
        "sys",
        &|event| {
            seen_events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        },
    )
    .await;
    assert_eq!(outcome, runtime_runner::StopReason::Completed);
    // The write really landed on disk with exact content.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "wavecode-smoke-ok"
    );
    // Both rounds ran: two assistant messages, one paired tool call.
    let kinds: Vec<String> = seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|event| {
            serde_json::to_value(&event.msg)
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "agent_message_complete")
            .count(),
        2
    );
    assert_eq!(kinds.iter().filter(|k| *k == "tool_call_begin").count(), 1);
    assert_eq!(kinds.iter().filter(|k| *k == "tool_call_end").count(), 1);

    // Every entry the run appended is on disk, and the tool call
    // survives as a block pair rather than as flattened prose.
    let replayed = crate::history_journal::replay_history(&journal);
    assert_eq!(
        replayed.entries.len(),
        conversation.len(),
        "journal fell behind the conversation"
    );
    assert!(replayed.lost_calls.is_empty(), "{:?}", replayed.lost_calls);
    assert!(
        !replayed.gapped && !replayed.torn_tail,
        "clean run reported damage"
    );
    let calls: Vec<&str> = replayed
        .entries
        .iter()
        .flat_map(|entry| entry.blocks.iter())
        .filter_map(|block| match block {
            state_store::Block::ToolUse { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["write"]);
    assert!(
        replayed
            .entries
            .iter()
            .flat_map(|entry| entry.blocks.iter())
            .any(|block| matches!(
                block,
                state_store::Block::ToolResult {
                    is_error: false,
                    ..
                }
            )),
        "the tool result never reached the journal"
    );

    // Resuming from that journal yields the same history, and doing it
    // twice must not grow it: the journal, not the caller, is the source.
    let first = seed_conversation(Some(journal.clone()), &[], &mut Vec::new());
    let second = seed_conversation(Some(journal), &[], &mut Vec::new());
    assert_eq!(first.len(), conversation.len());
    assert_eq!(second.len(), first.len());
}

/// A sequence hole must not hide records appended after resume.
#[test]
fn gapped_journal_is_rewritten_before_new_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let journal = state_persistence::history::HistoryJournal::new(&path);
    journal
            .append(&serde_json::json!({"k":"append","seq":0,"entry":{"role":"user","blocks":[{"text":"kept"}]}}))
            .unwrap();
    journal
            .append(&serde_json::json!({"k":"append","seq":2,"entry":{"role":"assistant","blocks":[{"text":"dropped"}]}}))
            .unwrap();
    let mut warnings = Vec::new();
    let mut conv = seed_conversation(Some(journal.clone()), &[], &mut warnings);
    assert!(
        warnings.iter().any(|warning| warning.contains("prefix")),
        "{warnings:?}"
    );
    assert_eq!(conv.snapshot()[0].text(), "kept");
    // The prefix is durable before any later append: one replace record,
    // and the record that sat past the hole is gone.
    let repaired = crate::history_journal::replay_history(&journal);
    assert!(!repaired.gapped && !repaired.torn_tail, "{repaired:?}");
    assert_eq!(repaired.entries.len(), 1);
    assert_eq!(repaired.entries[0].text(), "kept");
    assert_eq!(journal.read().records.len(), 1);
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("dropped"), "{raw}");
    conv.push(state_store::Role::Assistant, "after");
    let replayed = crate::history_journal::replay_history(&journal);
    assert!(!replayed.gapped && !replayed.torn_tail, "{replayed:?}");
    assert_eq!(replayed.entries.len(), 2);
    assert_eq!(replayed.entries[1].text(), "after");
}
