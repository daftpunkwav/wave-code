//! Permission rules, persisted grants, denylist resolution, and
//! permission-mode fallback: the config-to-sandbox surface.

use super::*;

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
