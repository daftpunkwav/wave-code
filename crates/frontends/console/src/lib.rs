/*!
 * @file ConsoleUi
 * @description The interactive console frontend over the session wire.
 *
 * Responsibilities:
 * - Own the themed chrome: welcome card, editor, footer, and activity.
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
pub mod history;
pub mod messages;
pub mod panes;
pub mod slash;
pub mod state;
pub mod theme;
pub mod transcript;
pub mod ui;
pub mod welcome;

pub use state::AppState;
pub use tui_engine::sanitize::{sanitize_terminal, truncate_chars};
pub use ui::{ConsoleUi, UiContext, run};

#[cfg(test)]
mod dependency_matrix_locked {
    /// The console UI crosses to the session exclusively through the
    /// wire protocol and the actor client, and renders through
    /// tui-engine. The internal dependency set is locked; adding one is
    /// a boundary change: update this test deliberately.
    #[test]
    fn console_ui_internal_dependencies_are_locked() {
        let manifest = include_str!("../Cargo.toml");
        let mut internal: Vec<&str> = manifest
            .lines()
            .filter(|line| line.contains("path ="))
            .map(|line| line.trim())
            .collect();
        internal.sort();
        assert_eq!(
            internal,
            vec![
                "operations-actor = { path = \"../../operations/actor\" }",
                "tui-engine = { path = \"../engine\" }",
                "wavecode-wire = { path = \"../../foundation/wire\" }",
            ],
            "internal deps changed: {internal:?}"
        );
    }
}
