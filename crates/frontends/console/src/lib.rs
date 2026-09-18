/*!
 * @file ConsoleUi
 * @description The interactive console frontend over the session wire.
 *
 * Responsibilities:
 * - Own the themed chrome: welcome card, editor, and footer.
 * - Translate session wire events into transcript components.
 * - Translate key input into submissions, approvals, and commands.
 *
 * Rendering is delegated to `tui-engine`; session interaction crosses
 * only the wire protocol and the actor client. This crate must not
 * depend on: runtime, state, action, safety, transport, or the
 * composition root.
 */

//! The inline console frontend: themed chrome over the session wire.

pub mod chrome;
pub mod complete;
pub mod controllers;
pub mod dialogs;
pub mod diff;
pub mod gitinfo;
pub mod highlight;
pub mod history;
pub mod messages;
pub mod panes;
pub mod settings;
pub mod slash;
pub mod state;
pub mod theme;
pub mod transcript;
pub mod ui;
pub mod welcome;

pub use dialogs::ModelEntryView;
pub use state::AppState;
pub use tui_engine::sanitize::{sanitize_terminal, truncate_chars};
pub use ui::{
    ConsoleUi, LaunchSpec, SessionFactory, SessionLaunch, UiContext, run, run_with_factory,
};

#[cfg(test)]
mod dependency_matrix_locked {
    /// The console UI crosses to the session exclusively through the
    /// wire protocol and the actor client, and renders through
    /// tui-engine. Every key in every dependency table (`[dependencies]`,
    /// target sections, dev/build) is checked against the combined
    /// whitelist (internal set + known externals), closing both the
    /// `foo.workspace = true` bypass and the target-section bypass.
    /// Adding an internal dependency is a boundary change: update this
    /// test deliberately.
    #[test]
    fn console_ui_internal_dependencies_are_locked() {
        const INTERNAL: [&str; 4] = [
            "tui-engine",
            "wavecode-wire",
            "operations-actor",
            "state-persistence",
        ];
        const EXTERNAL: [&str; 12] = [
            "async-trait",
            "crossterm",
            "tokio",
            "futures",
            "serde",
            "serde_json",
            "uuid",
            "anyhow",
            "base64",
            "syntect",
            "two-face",
            // dev-dependencies only: session-journal tests need temp dirs.
            "tempfile",
        ];
        let manifest = include_str!("../Cargo.toml");
        let mut in_deps = false;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_deps = trimmed.contains("dependencies");
                continue;
            }
            if !in_deps || trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            let raw_key = trimmed.split('=').next().map(str::trim).unwrap_or("");
            let key = raw_key.split('.').next().unwrap_or(raw_key);
            assert!(
                INTERNAL.contains(&key) || EXTERNAL.contains(&key),
                "unexpected dependency `{key}`: the internal set is locked"
            );
        }
    }
}
