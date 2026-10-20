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
 * only the wire protocol and the actor client, while session identity
 * and journaling ride `state-persistence`. This crate must not depend
 * on: runtime, action, safety, transport, or the composition root.
 */

//! The inline console frontend: themed chrome over the session wire.

pub mod chrome;
pub mod complete;
pub mod controllers;
pub mod dialogs;
pub mod diff;
pub mod git_info;
pub mod health;
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
pub use tui_engine::terminal::install_panic_restore;
pub use ui::{
    ConsoleUi, LaunchSpec, Redactor, SessionFactory, SessionLaunch, UiContext, run,
    run_with_factory,
};

/// Serializes the tests that mutate a process-global environment
/// variable: `set_var` / `remove_var` change process-wide state, so a
/// test that changes one name must not run beside another that reads
/// it. One lock for the whole crate: separate per-module mutexes would
/// not exclude each other.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Re-run this test binary as a child process with `WAVECODE_SHELL`
/// pointing at a program that does not exist, running only the named
/// test.
///
/// The shell override is process-global, so a test that sets it
/// in-process changes what every parallel test that spawns a command
/// sees. The child process carries the override in its own environment
/// and leaves this process untouched. The child must be an `#[ignore]`d
/// test: the parent asks for it by name.
#[cfg(test)]
pub(crate) fn run_isolated_with_shell_override(test_name: &str) -> std::process::Output {
    let exe = std::env::current_exe().expect("the test binary path");
    let output = std::process::Command::new(exe)
        .args(["--exact", test_name, "--ignored", "--nocapture"])
        .env("WAVECODE_SHELL", "wavecode-definitely-missing-shell")
        .output()
        .expect("the override child must run");
    // The child must have run the named test: a name that matches
    // nothing exits successfully with nothing run, which would make the
    // parent pass without proving anything.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1 passed"),
        "the override child must run exactly one test ({test_name}): {stdout}"
    );
    output
}

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
        const INTERNAL: [&str; 7] = [
            // Process-tree kill for the local `!` shell. The tier rules
            // already allow every crate to depend on infrastructure.
            "infrastructure-base",
            "tui-engine",
            "wavecode-wire",
            // User paths resolve through the shared home definition.
            "wavecode-config",
            // Mode cycle and legacy aliases come from the shared
            // vocabulary crate, not a local mirror.
            "wavecode-protocol",
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
