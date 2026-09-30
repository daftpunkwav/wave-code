//! `wavecode doctor`: validate the local files a session depends on —
//! config, provider credentials, UI settings, custom themes, session
//! records, and the OS confinement backend — without contacting any
//! provider. Secrets are never printed, only where a key was found.

use std::path::Path;

/// One `doctor` check outcome.
pub(crate) struct DoctorCheck {
    pub(crate) ok: bool,
    pub(crate) line: String,
}
fn ok(line: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        ok: true,
        line: line.into(),
    }
}

fn fail(line: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        ok: false,
        line: line.into(),
    }
}

/// The `wave` denylist store under `home` — the same file session
/// assembly loads, so the report cannot name rules a session ignores.
///
/// Missing or unreadable stores yield no entries: the surrounding
/// checks are what report why, rather than every consumer repeating
/// the warning.
fn settings_denylist(home: &Path) -> Vec<String> {
    wavecode_config::denylist::load_from(&home.join(".wavecode"))
}

/// `doctor`: validate every local file a session depends on — config,
/// provider credentials, UI settings, custom themes, and session
/// records — without contacting any provider. Secrets are never
/// printed, only where a key was found.
pub(crate) fn doctor_checks(
    config_path: Option<&std::path::Path>,
    home: Option<&Path>,
) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    let Some(home) = home else {
        checks.push(fail(
            "home: no HOME/USERPROFILE — sessions, settings, and config cannot be located",
        ));
        return checks;
    };

    // Config + provider credentials: an explicit --config path wins,
    // otherwise the file sits under the (already resolved) home.
    let path = match config_path {
        Some(path) => path.to_path_buf(),
        None => home.join(".wavecode").join("config.toml"),
    };
    match wavecode_config::Config::load_from(&path) {
        Err(wavecode_config::ConfigError::NotFound(path)) => checks.push(fail(format!(
            "config: not found at {} — see the example block in the startup error or docs/",
            path.display()
        ))),
        Err(error) => checks.push(fail(format!("config: {error}"))),
        Ok(config) => {
            checks.push(ok(format!(
                "config: {} (model {} via {})",
                path.display(),
                config.model,
                config.model_provider
            )));
            match config.resolve_provider() {
                Err(wavecode_config::ConfigError::MissingApiKey(name)) => checks.push(fail(
                    format!("provider {name}: no api key (set env_key or inline api_key)"),
                )),
                Err(error) => checks.push(fail(format!("provider: {error}"))),
                Ok((provider, _key)) => {
                    // The key source is named, never the value. An inline
                    // api_key sits in plaintext in config.toml (readable by
                    // any shell/script tool running on this machine), so
                    // the report says so and points at env_key instead of
                    // presenting the setup as equivalent.
                    let source = provider
                        .env_key
                        .as_deref()
                        .filter(|name| std::env::var(name).is_ok_and(|v| !v.trim().is_empty()))
                        .map(|name| format!("env {name}"))
                        .unwrap_or_else(|| {
                            "inline api_key (stored in plaintext in config.toml; \
                             prefer env_key)"
                                .to_string()
                        });
                    checks.push(ok(format!(
                        "provider {}: key from {source}",
                        config.model_provider
                    )));
                }
            }
            for (alias, entry) in &config.models {
                if config.model_providers.contains_key(&entry.provider) {
                    checks.push(ok(format!(
                        "models.{alias}: {} via {}",
                        entry.model, entry.provider
                    )));
                } else {
                    checks.push(fail(format!(
                        "models.{alias}: unknown provider {:?} (not in [model_providers])",
                        entry.provider
                    )));
                }
            }
            // The secondary alias steers side-session cost: a dangling
            // pointer degrades silently at runtime, so doctor surfaces it.
            if let Some(alias) = &config.secondary_model {
                match config.models.get(alias) {
                    Some(entry) => checks.push(ok(format!(
                        "secondary_model.{alias}: {} via {}",
                        entry.model, entry.provider
                    ))),
                    None => checks.push(fail(format!(
                        "secondary_model: alias {alias:?} is not in [models]"
                    ))),
                }
            }
            // Permission rules: built by the same function session
            // assembly uses, so this report cannot drift from what a
            // session actually loads. A rule that never applied is not a
            // crash — it is the one thing the user will otherwise hunt for.
            let permissions = operations_bootstrap::load_permissions(
                &config,
                Some(home),
                &settings_denylist(home),
            );
            if permissions.findings.is_empty() {
                checks.push(ok(format!(
                    "permissions: {} allow, {} grant(s), {} deny",
                    permissions.authored_allow,
                    permissions.persisted_grants,
                    permissions.deny.len()
                )));
            } else {
                for finding in &permissions.findings {
                    checks.push(fail(format!("permissions: {finding}")));
                }
            }
            // MCP servers: validated through the same conversion the session
            // connect path runs (`operations_bootstrap::mcp_config_findings`
            // -> `McpServerConfig::from_raw`), so a misconfigured entry is
            // visible here instead of only surfacing as a skipped server at
            // startup.
            let mcp_findings = operations_bootstrap::mcp_config_findings(&config);
            if config.mcp_servers.is_empty() {
                checks.push(ok("mcp: none configured"));
            } else if mcp_findings.is_empty() {
                checks.push(ok(format!(
                    "mcp: {} server(s) configured, entries valid",
                    config.mcp_servers.len()
                )));
            } else {
                for finding in mcp_findings {
                    checks.push(fail(format!("mcp: {finding}")));
                }
            }
        }
    }

    // The model catalog (`~/.wavecode/models.json`): session launch
    // merges it on top of config.toml and the console's /provider
    // editor writes it, so a malformed file would degrade silently —
    // a missing file is the normal optional case, a broken one is not.
    match wavecode_config::ModelCatalog::load(home) {
        Ok(catalog) => checks.push(ok(format!(
            "models.json: {} model(s)",
            catalog.models.len()
        ))),
        Err(error) => checks.push(fail(format!("models.json: {error}"))),
    }

    // Console settings (a broken file silently degrades at runtime, so
    // doctor is the place where the breakage becomes visible).
    let settings_path = home.join(".wavecode").join("console-settings.json");
    if !settings_path.exists() {
        checks.push(ok("settings: defaults (no console-settings.json yet)"));
    } else {
        match std::fs::read_to_string(&settings_path)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                serde_json::from_str::<console_ui::settings::UiSettings>(&text)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }) {
            Ok(()) => checks.push(ok(format!("settings: {}", settings_path.display()))),
            Err(error) => checks.push(fail(format!(
                "settings: {} parses as defaults; fix or delete the file ({error})",
                settings_path.display()
            ))),
        }
    }

    // Custom themes: every file must resolve.
    let themes = console_ui::theme::file::list(home);
    if themes.is_empty() {
        checks.push(ok("themes: none custom"));
    } else {
        for name in &themes {
            match console_ui::theme::file::load(home, name) {
                Ok(_) => checks.push(ok(format!("theme {name}: parses"))),
                Err(error) => checks.push(fail(format!("theme {name}: {error}"))),
            }
        }
    }

    // Session records: the index must parse and every journal file
    // named by the index should still exist. Health reads through the
    // persistence layer's own salvage rules, so this report cannot
    // drift from what the picker and resume actually load.
    let index = state_persistence::sessions::index_path(home);
    if !index.exists() {
        checks.push(ok("sessions: none recorded yet"));
    } else {
        let health = state_persistence::sessions::index_health(home);
        if health.sessions.is_empty() {
            // A broken index reads as empty; the health report tells a
            // valid empty index apart from one that failed to parse.
            if health.parses {
                checks.push(ok("sessions: index present, none recorded"));
            } else {
                checks.push(fail(format!(
                    "sessions: {} does not parse (read as empty; resumable sessions are lost until it is fixed or removed)",
                    index.display()
                )));
            }
        } else {
            let missing: Vec<&str> = health
                .sessions
                .iter()
                .filter(|meta| {
                    !state_persistence::sessions::session_journal_file(home, &meta.id)
                        .is_some_and(|path| path.exists())
                })
                .map(|meta| meta.id.as_str())
                .collect();
            if missing.is_empty() {
                checks.push(ok(format!(
                    "sessions: {} recorded, all journals present",
                    health.sessions.len()
                )));
            } else {
                checks.push(fail(format!(
                    "sessions: {} recorded, missing journals for {}",
                    health.sessions.len(),
                    missing.join(", ")
                )));
            }
        }
    }
    // OS confinement: policy rules on intent, this is the machine boundary
    // behind it. Report what actually holds rather than letting an
    // "available" backend read like a jail (the Windows job backend controls
    // process trees only — no filesystem or network boundary).
    checks.push(ok(format!(
        "sandbox: {}",
        operations_bootstrap::confinement_status()
    )));
    checks
}

#[cfg(test)]
mod tests {
    use super::{DoctorCheck, doctor_checks, settings_denylist};

    /// A home with nothing set up degrades gracefully: exactly the
    /// missing-config check fails, everything else reads as defaults.
    #[test]
    fn doctor_on_a_fresh_home_fails_only_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let checks = doctor_checks(None, Some(dir.path()));
        let failed: Vec<&str> = checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.line.as_str())
            .collect();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].contains("config"), "{failed:?}");
        let lines: Vec<&str> = checks.iter().map(|c| c.line.as_str()).collect();
        assert!(
            lines.iter().any(|l| l.contains("settings: defaults")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("sessions: none")),
            "{lines:?}"
        );
    }

    /// A complete setup passes every check, including provider key
    /// resolution through an inline api_key (never printed).
    #[test]
    fn doctor_passes_on_a_complete_setup() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "test-model"
model_provider = "test-provider"

[model_providers.test-provider]
type = "open-ai-compatible"
base_url = "http://127.0.0.1:9"
api_key = "doctor-test-secret"

[models.fast]
provider = "test-provider"
model = "test-model-fast"
"#,
        )
        .unwrap();
        let checks = doctor_checks(None, Some(dir.path()));
        let failed: Vec<&DoctorCheck> = checks.iter().filter(|check| !check.ok).collect();
        assert!(
            failed.is_empty(),
            "{:?}",
            failed.iter().map(|c| &c.line).collect::<Vec<_>>()
        );
        let lines: Vec<&str> = checks.iter().map(|c| c.line.as_str()).collect();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("models.fast: test-model-fast")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("key from inline api_key")),
            "{lines:?}"
        );
        // The plaintext-storage acceptance is disclosed, not silent.
        assert!(
            lines
                .iter()
                .any(|l| l.contains("stored in plaintext") && l.contains("prefer env_key")),
            "{lines:?}"
        );
        // The key itself must never appear in doctor output.
        assert!(
            !lines.iter().any(|l| l.contains("doctor-test-secret")),
            "secret leaked: {lines:?}"
        );
    }

    /// A typo in a rule and an allow a deny rule fully shadows are both
    /// things the user cannot see from behavior alone: the first loads as
    /// nothing, the second loads and never fires.
    #[test]
    fn doctor_reports_invalid_and_dead_permission_rules() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "test-model"
model_provider = "test-provider"

[model_providers.test-provider]
type = "anthropic"
base_url = "https://api.example.com"
api_key = "k"

[permissions]
allow = ["not a rule", "Bash(git *)"]
deny = ["Bash(*)"]
"#,
        )
        .unwrap();
        let lines: Vec<String> = doctor_checks(None, Some(dir.path()))
            .into_iter()
            .filter(|check| check.line.starts_with("permissions:"))
            .map(|check| check.line)
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("invalid allow rule")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Bash(git *)") && l.contains("can never apply")),
            "{lines:?}"
        );
    }

    /// The denylist lives in the config-owned store, so the grant
    /// report reads that file to agree with what a session enforces.
    #[test]
    fn doctor_sees_the_settings_denylist() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        wavecode_config::denylist::save_to(&wave, &["rm -rf".to_string()]).unwrap();
        assert_eq!(settings_denylist(dir.path()), vec!["rm -rf".to_string()]);
        assert!(settings_denylist(&std::path::PathBuf::from("/no/such/home")).is_empty());
    }

    /// Every run reports the machine boundary it actually has, so a partial
    /// backend is never presented as a jail.
    #[test]
    fn doctor_discloses_the_confinement_backend() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = doctor_checks(None, Some(dir.path()))
            .into_iter()
            .map(|check| check.line)
            .collect();
        assert!(
            lines.iter().any(|l| l.starts_with("sandbox: ")),
            "{lines:?}"
        );
    }

    /// A broken console-settings.json surfaces here even though runtime
    /// loading silently falls back to defaults.
    #[test]
    fn doctor_reports_broken_settings_and_unknown_model_providers() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "m"
model_provider = "p"

[model_providers.p]
type = "open-ai-compatible"
base_url = "http://127.0.0.1:9"
api_key = "inline-key"

[models.broken]
provider = "no-such-provider"
model = "m2"
"#,
        )
        .unwrap();
        std::fs::write(wave.join("console-settings.json"), "{not json").unwrap();
        let checks = doctor_checks(None, Some(dir.path()));
        let failed: Vec<&str> = checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.line.as_str())
            .collect();
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(
            failed.iter().any(|l| l.contains("models.broken")),
            "{failed:?}"
        );
        assert!(failed.iter().any(|l| l.contains("settings:")), "{failed:?}");
    }

    /// The MCP section runs the session connect path's own validation: a
    /// misconfigured `[mcp_servers]` entry fails here with the same
    /// reason a session would skip it for, and a valid entry passes.
    #[test]
    fn doctor_reports_misconfigured_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "m"
model_provider = "p"

[model_providers.p]
type = "anthropic"
base_url = "https://api.example.com"
api_key = "k"

[mcp_servers."bad__name"]
command = "npx"

[mcp_servers.both]
command = "npx"
url = "https://mcp.example.com"
"#,
        )
        .unwrap();
        let checks = doctor_checks(None, Some(dir.path()));
        let mcp_lines: Vec<&str> = checks
            .iter()
            .filter(|check| check.line.starts_with("mcp:"))
            .map(|check| check.line.as_str())
            .collect();
        assert_eq!(mcp_lines.len(), 2, "{mcp_lines:?}");
        assert!(
            mcp_lines
                .iter()
                .all(|line| line.contains("must be non-empty without `__`")
                    || line.contains("sets both command and url")),
            "{mcp_lines:?}"
        );
        let failed: Vec<&str> = checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.line.as_str())
            .collect();
        assert_eq!(failed.len(), 2, "{failed:?}");
    }

    /// The model catalog rides the same doctor pass: a malformed
    /// `models.json` fails (session launch merges it on top), while a
    /// missing file is the normal optional case and reads as empty.
    #[test]
    fn doctor_reports_a_broken_models_json() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "m"
model_provider = "p"

[model_providers.p]
type = "anthropic"
base_url = "https://api.example.com"
api_key = "k"
"#,
        )
        .unwrap();
        // No models.json yet: healthy empty.
        let lines: Vec<String> = doctor_checks(None, Some(dir.path()))
            .into_iter()
            .map(|check| check.line)
            .collect();
        assert!(
            lines.iter().any(|l| l == "models.json: 0 model(s)"),
            "{lines:?}"
        );

        std::fs::write(wave.join("models.json"), "{not json").unwrap();
        let failed: Vec<String> = doctor_checks(None, Some(dir.path()))
            .into_iter()
            .filter(|check| !check.ok)
            .map(|check| check.line)
            .collect();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].contains("models.json"), "{failed:?}");
    }

    /// A well-formed `[mcp_servers]` entry reads as healthy.
    #[test]
    fn doctor_passes_valid_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "m"
model_provider = "p"

[model_providers.p]
type = "anthropic"
base_url = "https://api.example.com"
api_key = "k"

[mcp_servers.playwright]
command = "npx"
args = ["@playwright/mcp@latest"]
"#,
        )
        .unwrap();
        let lines: Vec<String> = doctor_checks(None, Some(dir.path()))
            .into_iter()
            .map(|check| check.line)
            .collect();
        assert!(
            lines
                .iter()
                .any(|l| l == "mcp: 1 server(s) configured, entries valid"),
            "{lines:?}"
        );
    }

    /// An empty-but-valid session index is healthy; a broken one fails.
    /// `list_sessions` reads both as empty, so doctor must parse the
    /// file itself to tell them apart.
    #[test]
    fn doctor_tells_an_empty_index_apart_from_a_broken_one() {
        // The other checks (config, settings) are out of scope here;
        // only the sessions line is asserted.
        let sessions_line = |dir: &tempfile::TempDir| {
            doctor_checks(None, Some(dir.path()))
                .into_iter()
                .find(|check| check.line.starts_with("sessions:"))
                .unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".wavecode").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        std::fs::write(state_persistence::sessions::index_path(dir.path()), "[]").unwrap();
        let check = sessions_line(&dir);
        assert!(check.ok, "{}", check.line);
        assert_eq!(check.line, "sessions: index present, none recorded");

        std::fs::write(
            state_persistence::sessions::index_path(dir.path()),
            "{not json",
        )
        .unwrap();
        let check = sessions_line(&dir);
        assert!(!check.ok, "{}", check.line);
        assert!(check.line.contains("does not parse"), "{}", check.line);
    }
}
