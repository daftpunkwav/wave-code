//! On-disk file checks shared by `wavecode doctor` and the console `/doctor`.
//!
//! Permission rules, MCP entries, and the OS confinement line stay in the
//! harness: they need the composition root, which this crate cannot name.
//! Everything here is a file the console already knows how to load.

use std::path::Path;

/// One local-file check. `line` has no `ok` / `warn` prefix; each surface
/// adds its own.
pub struct FileCheck {
    pub ok: bool,
    pub line: String,
}

fn pass(line: impl Into<String>) -> FileCheck {
    FileCheck {
        ok: true,
        line: line.into(),
    }
}

fn fail(line: impl Into<String>) -> FileCheck {
    FileCheck {
        ok: false,
        line: line.into(),
    }
}

/// Catalog, console settings, custom themes, and session records under `home`.
///
/// Sentences are the doctor contract: the CLI prints them as-is and the
/// console prefixes a marker. Do not rephrase them in either caller.
pub fn local_file_checks(home: &Path) -> Vec<FileCheck> {
    let mut checks = Vec::new();

    match wavecode_config::ModelCatalog::load(home) {
        Ok(catalog) => checks.push(pass(format!(
            "models.json: {} model(s)",
            catalog.models.len()
        ))),
        Err(error) => checks.push(fail(format!("models.json: {error}"))),
    }

    let settings_path = home.join(".wavecode").join("console-settings.json");
    if !settings_path.exists() {
        checks.push(pass(
            "settings: defaults (no console-settings.json yet)".to_string(),
        ));
    } else {
        match std::fs::read_to_string(&settings_path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                serde_json::from_str::<crate::settings::UiSettings>(&text)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }) {
            Ok(()) => checks.push(pass(format!("settings: {}", settings_path.display()))),
            Err(error) => checks.push(fail(format!(
                "settings: {} parses as defaults; fix or delete the file ({error})",
                settings_path.display()
            ))),
        }
    }

    let themes = crate::theme::file::list(home);
    if themes.is_empty() {
        checks.push(pass("themes: none custom".to_string()));
    } else {
        for name in &themes {
            match crate::theme::file::load(home, name) {
                Ok(_) => checks.push(pass(format!("theme {name}: parses"))),
                Err(error) => checks.push(fail(format!("theme {name}: {error}"))),
            }
        }
    }

    let report = state_persistence::sessions::session_store_report(home);
    let index = state_persistence::sessions::index_path(home);
    if !report.index_exists {
        checks.push(pass("sessions: none recorded yet".to_string()));
    } else if report.session_count == 0 {
        if report.parses {
            checks.push(pass("sessions: index present, none recorded".to_string()));
        } else {
            checks.push(fail(format!(
                "sessions: {} does not parse (read as empty; resumable sessions are lost until it is fixed or removed)",
                index.display()
            )));
        }
    } else if report.missing_journal_ids.is_empty() {
        checks.push(pass(format!(
            "sessions: {} recorded, all journals present",
            report.session_count
        )));
    } else {
        checks.push(fail(format!(
            "sessions: {} recorded, missing journals for {}",
            report.session_count,
            report.missing_journal_ids.join(", ")
        )));
    }

    checks
}
