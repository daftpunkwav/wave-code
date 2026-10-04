//! Shared fixtures for the `ui` test modules: the standard
//! [`ConsoleUi`] builder and the seeded model-picker entries.

use super::*;
use crate::theme;
use test_support::{NullStatus, TestLink};

pub(super) fn ui() -> ConsoleUi {
    theme::set(theme::Theme::dark());
    let mut ui = ConsoleUi::new(
        Box::new(TestLink::new()),
        &UiContext {
            model_name: "test-model".to_string(),
            provider_id: String::new(),
            thinking_effort: None,
            thinking_levels: Vec::new(),
            cwd: PathBuf::from("/home/user/work/proj/sub"),
            permission_mode: "auto".to_string(),
            skill_names: Vec::new(),
            mcp_servers: vec!["fs".to_string()],
            memory_files: Vec::new(),
            status: Arc::new(NullStatus),
            session_id: "session-0001".to_string(),
            session_title: None,
            model_entries: Vec::new(),
            home: None,
            update_notice: None,
            redactor: None,
        },
        "0.1.0",
    );
    // Picker and settings tests mutate settings; keep them off the
    // real user file.
    ui.settings = crate::settings::SharedSettings::without_persistence(Default::default());
    ui
}

pub(super) fn picker_entries() -> Vec<crate::dialogs::ModelEntryView> {
    vec![
        crate::dialogs::ModelEntryView {
            label: "deepseek-chat".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            effort: None,
        },
        crate::dialogs::ModelEntryView {
            label: "deepseek-reasoner".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-reasoner".to_string(),
            effort: None,
        },
        crate::dialogs::ModelEntryView {
            label: "MiniMax-M3".to_string(),
            provider: "minimax".to_string(),
            model: "MiniMax-M3".to_string(),
            effort: Some("high".to_string()),
        },
    ]
}

pub(super) fn ui_with_models() -> ConsoleUi {
    let mut ui = ui();
    ui.model_entries = picker_entries();
    ui.state.model_name = "deepseek-chat".to_string();
    ui.state.provider_id = "deepseek".to_string();
    ui.thinking_levels = vec!["off".to_string(), "low".to_string(), "high".to_string()];
    ui
}
