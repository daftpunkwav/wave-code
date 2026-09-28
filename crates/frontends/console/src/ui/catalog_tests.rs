//! Functional tests for the `/provider` catalog subcommands
//! (`list` / `add` / `set` / `remove`): each drives the slash command
//! against a catalog file in a temporary home and checks both the
//! status line and the on-disk effect. Plain `/model <name>` switching
//! and the picker live in `ui/tests.rs`.

use super::*;
use crate::theme;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use test_support::{NullStatus, TestLink};
use tui_engine::width::strip_ansi;
use wavecode_config::ModelCatalog;

/// A console over a throwaway home directory.
fn ui_with_home(name: &str) -> (ConsoleUi, PathBuf) {
    theme::set(theme::Theme::dark());
    let home = std::env::temp_dir().join(format!("wavecode-ui-catalog-{name}"));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".wavecode")).unwrap();
    let ui = ConsoleUi::new(
        Box::new(TestLink::new()),
        &UiContext {
            model_name: "test-model".to_string(),
            provider_id: String::new(),
            thinking_effort: None,
            thinking_levels: Vec::new(),
            cwd: PathBuf::from("/home/user/work"),
            permission_mode: "auto".to_string(),
            skill_names: Vec::new(),
            mcp_servers: Vec::new(),
            status: Arc::new(NullStatus),
            session_id: "session-0001".to_string(),
            session_title: None,
            model_entries: Vec::new(),
            home: Some(home.clone()),
            update_notice: None,
            redactor: None,
        },
        "0.0.0-test",
    );
    (ui, home)
}

/// Every transcript entry rendered to plain text.
fn transcript_plain(ui: &mut ConsoleUi) -> String {
    let mut text = String::new();
    for index in 0..ui.transcript.len() {
        if let Some(entry) = ui.transcript.get_mut(index) {
            for line in entry.component.render(80).iter() {
                text.push_str(&strip_ansi(line));
                text.push('\n');
            }
        }
    }
    text
}

/// Seed a catalog holding one spec and return the file path.
fn seed(home: &Path) -> PathBuf {
    let path = ModelCatalog::path(home);
    std::fs::write(
        &path,
        r#"{"models":{"glm":{"provider":"bigmodel","model":"glm-5.3","kind":"anthropic-messages","base_url":"https://open.bigmodel.cn/api/anthropic","context_window":1000000}}}"#,
    )
    .unwrap();
    path
}

#[test]
fn list_on_an_empty_catalog_reports_empty() {
    let (mut ui, home) = ui_with_home("list-empty");
    ui.user_submit("/provider list");
    let text = transcript_plain(&mut ui);
    assert!(
        text.contains("model catalog is empty (/provider add ...)"),
        "{text}"
    );
    assert!(!ModelCatalog::path(&home).exists(), "no file created");
}

#[test]
fn add_writes_the_catalog_and_list_shows_it() {
    let (mut ui, home) = ui_with_home("add");
    ui.user_submit(
        "/provider add glm anthropic-messages bigmodel https://open.bigmodel.cn/api/anthropic \
         glm-5.3 100000 128000",
    );
    let text = transcript_plain(&mut ui);
    assert!(text.contains("added glm (restart applies it)"), "{text}");

    let catalog = ModelCatalog::load(&home).unwrap();
    let spec = catalog.get("glm").expect("spec written");
    assert_eq!(spec.model, "glm-5.3");
    assert_eq!(spec.base_url, "https://open.bigmodel.cn/api/anthropic");
    assert_eq!(spec.context_window, Some(100_000));
    assert_eq!(spec.max_output, Some(128_000));
    assert_eq!(spec.kind, wavecode_config::ApiKind::AnthropicMessages);

    ui.user_submit("/provider list");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("glm · glm-5.3"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn add_rejects_an_unknown_api_kind_without_writing() {
    let (mut ui, home) = ui_with_home("add-bad-kind");
    ui.user_submit("/provider add glm soap bigmodel https://x glm-5.3");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("unknown api kind soap"), "{text}");
    assert!(!ModelCatalog::path(&home).exists(), "nothing written");
}

#[test]
fn set_patches_one_field_and_saves() {
    let (mut ui, home) = ui_with_home("set");
    seed(&home);
    ui.user_submit("/provider set glm context 123000");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("glm updated (context = 123000)"), "{text}");
    let catalog = ModelCatalog::load(&home).unwrap();
    assert_eq!(catalog.get("glm").unwrap().context_window, Some(123_000));

    // The thinking field toggles the reasoning spec; off clears it.
    ui.user_submit("/provider set glm thinking max");
    ui.user_submit("/provider set glm thinking off");
    let catalog = ModelCatalog::load(&home).unwrap();
    let spec = catalog.get("glm").unwrap();
    assert!(!spec.reasoning.enabled);
    assert_eq!(spec.reasoning.default, None);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn set_unknown_field_or_alias_reports() {
    let (mut ui, home) = ui_with_home("set-bad");
    seed(&home);
    ui.user_submit("/provider set glm color blue");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("unknown field color"), "{text}");
    ui.user_submit("/provider set nope context 5");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("no model named nope"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

/// An unparseable numeric value reports and leaves the field (and the
/// file) untouched — no silent reset-to-default dressed up as success.
#[test]
fn set_rejects_unparseable_numbers_without_writing() {
    let (mut ui, home) = ui_with_home("set-bad-number");
    seed(&home);
    ui.user_submit("/provider set glm context abc");
    ui.user_submit("/provider set glm output 1.5");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("invalid context value"), "{text}");
    assert!(text.contains("invalid output value"), "{text}");
    assert!(!text.contains("updated"), "no false success: {text}");
    let catalog = ModelCatalog::load(&home).unwrap();
    let spec = catalog.get("glm").unwrap();
    assert_eq!(spec.context_window, Some(1_000_000), "field untouched");
    assert_eq!(spec.max_output, None, "field untouched");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn remove_drops_the_spec_and_saves() {
    let (mut ui, home) = ui_with_home("remove");
    seed(&home);
    ui.user_submit("/provider remove glm");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("removed glm"), "{text}");
    let catalog = ModelCatalog::load(&home).unwrap();
    assert!(catalog.get("glm").is_none(), "spec dropped on disk");

    // Removing an unknown alias reports instead of writing.
    ui.user_submit("/provider remove glm");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("no model named glm"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A malformed models.json cancels the subcommands with a visible
/// error instead of looking like an empty catalog.
#[test]
fn malformed_catalog_cancels_the_subcommands() {
    let (mut ui, home) = ui_with_home("malformed");
    let path = ModelCatalog::path(&home);
    std::fs::write(&path, "{ not json").unwrap();
    ui.user_submit("/provider list");
    ui.user_submit("/provider add glm anthropic bigmodel https://x glm-5.3");
    ui.user_submit("/provider remove glm");
    let text = transcript_plain(&mut ui);
    assert!(
        text.matches("model catalog load failed:").count() == 3,
        "every subcommand reports the load failure: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ not json",
        "the broken file is left untouched"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// `/memory` lists the AGENTS.md instruction chain for the session:
/// every file from the working directory upward, each with its line
/// count.
#[test]
fn memory_lists_the_agents_chain() {
    let (mut ui, home) = ui_with_home("memory");
    let cwd = std::env::temp_dir().join("wavecode-memory-cwd");
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(cwd.join("nested")).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "# root\nrules here\n").unwrap();
    std::fs::write(cwd.join("nested").join("AGENTS.md"), "# nested\n").unwrap();
    ui.state.cwd = cwd.join("nested");
    ui.user_submit("/memory");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("AGENTS.md"), "{text}");
    assert!(text.contains("(2 lines)"), "root file counted: {text}");
    assert!(text.contains("(1 lines)"), "nested file counted: {text}");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);
}

/// An empty scope says so instead of printing nothing.
#[test]
fn memory_without_files_reports_empty() {
    let (mut ui, home) = ui_with_home("memory-empty");
    ui.user_submit("/memory");
    let text = transcript_plain(&mut ui);
    assert!(text.contains("no AGENTS.md in scope"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}
