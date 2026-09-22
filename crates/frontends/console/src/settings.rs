/*!
 * @file UiSettings
 * @description Persistent console UI preferences shared with components.
 *
 * Responsibilities:
 * - Define every user-tunable UI behavior (rendering, verbosity, style).
 * - Load/save `~/.wavecode/console-settings.json`.
 * - Share one handle (`Arc<Mutex<UiSettings>>`) so `/settings` changes
 *   reach already-built components immediately.
 *
 * This module must not depend on: runtime, capability, or actor crates.
 */

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// How much of a tool call a card shows by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolDisplay {
    /// Only the state dot and verb (no arguments, no output).
    Names,
    /// Header with argument summary plus collapsed output (default).
    #[default]
    Summary,
    /// Everything: full input parameters and the whole output.
    Full,
}

/// How edit-style tool calls render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditDisplay {
    /// Only the tool header (no diff body).
    Tool,
    /// The clustered diff preview (default).
    #[default]
    Diff,
}

/// Diff layout style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffStyle {
    /// Single column with paired old/new line numbers (default).
    #[default]
    Unified,
    /// Two columns, old left and new right.
    Split,
}

/// User-tunable console preferences.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// Render submitted user input as markdown in the transcript.
    pub render_user_markdown: bool,
    /// Default tool-call verbosity.
    pub tool_display: ToolDisplay,
    /// Edit-style tool rendering.
    pub edit_display: EditDisplay,
    /// Diff layout for edit-style tools.
    pub diff_style: DiffStyle,
    /// `wave`-mode command denylist: any shell command whose text
    /// contains one of these substrings is denied outright (no prompt).
    pub wave_denylist: Vec<String>,
    /// Saved default model (wire name) from the `/model` picker; applied
    /// by the harness at startup (CLI `--model` still wins).
    pub default_model: Option<String>,
    /// Saved default provider id alongside [`UiSettings::default_model`].
    pub default_provider: Option<String>,
    /// Saved default reasoning-effort level (e.g. `low`); `None` and
    /// `off` both mean the parameter stays unset.
    pub default_effort: Option<String>,
    /// External editor command for Ctrl+G (`/editor <cmd>`); falls back
    /// to `$VISUAL` then `$EDITOR` when unset.
    pub editor_command: Option<String>,
    /// External status-line command (Claude Code-compatible: JSON
    /// session snapshot on stdin, first stdout line replaces footer
    /// row 1). Applied by the poll loop about once per second.
    pub status_line_command: Option<String>,
    /// Turn-finished notifications: `Some(false)` disables them; unset
    /// falls back to `WAVECODE_NOTIFY=0`.
    pub notifications_enabled: Option<bool>,
    /// Notification delivery style (`osc9` / `bell` / `both`); unset
    /// falls back to `WAVECODE_NOTIFY_STYLE`.
    pub notification_style: Option<String>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            render_user_markdown: true,
            tool_display: ToolDisplay::Summary,
            edit_display: EditDisplay::Diff,
            diff_style: DiffStyle::Unified,
            wave_denylist: Vec::new(),
            default_model: None,
            default_provider: None,
            default_effort: None,
            editor_command: None,
            status_line_command: None,
            notifications_enabled: None,
            notification_style: None,
        }
    }
}

impl UiSettings {
    /// The settings file path under the home directory, when known.
    pub fn path() -> Option<PathBuf> {
        let home = wavecode_config::home_dir()?;
        if home.as_os_str().is_empty() {
            return None;
        }
        Some(home.join(".wavecode").join("console-settings.json"))
    }

    /// Load from the default path; missing or broken files yield the
    /// defaults.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist to the default path (best effort; a missing home just
    /// skips saving).
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_ok()
            && let Ok(text) = serde_json::to_string_pretty(self)
        {
            let _ = std::fs::write(path, text);
        }
    }
}

/// The shared handle every component reads at render time, so
/// `/settings` changes apply without rebuilding anything.
#[derive(Clone)]
pub struct SharedSettings {
    inner: Arc<Mutex<UiSettings>>,
    /// False only in tests: mutations stay in memory and never touch
    /// the user's settings file.
    persist: bool,
}

impl SharedSettings {
    /// A shared handle holding `settings`.
    pub fn new(settings: UiSettings) -> Self {
        Self {
            inner: Arc::new(Mutex::new(settings)),
            persist: true,
        }
    }

    /// A handle that never writes the settings file (tests only:
    /// picker tests must not rewrite the real user settings).
    #[cfg(test)]
    pub(crate) fn without_persistence(settings: UiSettings) -> Self {
        Self {
            inner: Arc::new(Mutex::new(settings)),
            persist: false,
        }
    }

    /// Load from disk and share.
    pub fn load() -> Self {
        Self::new(UiSettings::load())
    }

    /// Snapshot the current values.
    pub fn get(&self) -> UiSettings {
        self.inner.lock().expect("settings lock").clone()
    }

    /// Apply `edit` to the current values and persist the result.
    pub fn update(&self, edit: impl FnOnce(&mut UiSettings)) {
        let mut guard = self.inner.lock().expect("settings lock");
        edit(&mut guard);
        if self.persist {
            guard.save();
        }
    }

    /// Discard live values and reload from disk (`/reload`); holders
    /// of this handle see the fresh values on their next read.
    pub fn reload(&self) {
        *self.inner.lock().expect("settings lock") = UiSettings::load();
    }
}
