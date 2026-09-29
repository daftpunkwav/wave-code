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
    /// External status-line command (a JSON session snapshot on stdin,
    /// first stdout line replaces footer row 1). Applied by the poll
    /// loop about once per second.
    pub status_line_command: Option<String>,
    /// Turn-finished notifications: `Some(false)` disables them; unset
    /// falls back to `WAVECODE_NOTIFY=0`.
    pub notifications_enabled: Option<bool>,
    /// Notification delivery style (`osc9` / `bell` / `both`); unset
    /// falls back to `WAVECODE_NOTIFY_STYLE`.
    pub notification_style: Option<String>,
    /// Thinking blocks start expanded (Ctrl+O still toggles live).
    pub thinking_expanded: bool,
    /// Stream the assistant's partial draft while the turn runs;
    /// turning it off shows only thinking blocks and finished messages.
    pub show_streaming_draft: bool,
    /// The footer's context meter (`context: N% (used/max)`).
    pub show_context_footer: bool,
    /// Rotate the footer tip; off shows only release notices on the
    /// right-hand slot.
    pub rotate_tips: bool,
    /// Require the double-press confirm before exiting on an idle
    /// editor; off makes the first Ctrl+C / Ctrl+D exit outright.
    pub confirm_exit: bool,
    /// Input-history entries kept per surface (the read path trims to
    /// this many); 0 falls back to the built-in cap.
    pub history_limit: usize,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            render_user_markdown: true,
            tool_display: ToolDisplay::Summary,
            edit_display: EditDisplay::Diff,
            default_model: None,
            default_provider: None,
            default_effort: None,
            editor_command: None,
            status_line_command: None,
            notifications_enabled: None,
            notification_style: None,
            thinking_expanded: false,
            show_streaming_draft: true,
            show_context_footer: true,
            rotate_tips: true,
            confirm_exit: true,
            history_limit: 0,
        }
    }
}

impl UiSettings {
    /// The effective input-history cap: the configured limit, or the
    /// built-in default when unset (0).
    pub fn history_cap(&self) -> usize {
        if self.history_limit == 0 {
            crate::history::MAX_ENTRIES
        } else {
            self.history_limit
        }
    }

    /// The settings file path under the home directory, when known.
    pub fn path() -> Option<PathBuf> {
        let home = wavecode_config::home_dir()?;
        if home.as_os_str().is_empty() {
            return None;
        }
        Some(home.join(".wavecode").join("console-settings.json"))
    }

    /// Load from the default path; missing or broken files yield the
    /// defaults. Legacy `wave_denylist` entries migrate once into the
    /// config-owned store (see [`load_at`]).
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let dir = wavecode_config::denylist::default_dir();
        Self::load_at(&path, dir.as_deref())
    }

    /// [`Self::load`] against explicit paths (tests inject both).
    fn load_at(path: &std::path::Path, denylist_dir: Option<&std::path::Path>) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let settings: UiSettings = serde_json::from_str(&text).unwrap_or_default();
        // One-time migration: the denylist used to live beside these UI
        // preferences, which only the console surfaces read. It moved to
        // the config-owned store every surface (TUI, exec, REPL, ACP,
        // serve) enforces, so the entries move once and the legacy key
        // is dropped from this file. The store side wins on conflicts;
        // a failed store write leaves the file (and key) untouched.
        let legacy = serde_json::from_str::<LegacySettings>(&text)
            .map(|legacy| legacy.wave_denylist)
            .unwrap_or_default();
        if !legacy.is_empty()
            && let Some(dir) = denylist_dir
        {
            let mut entries = wavecode_config::denylist::load_from(dir);
            for entry in legacy {
                if !entries.contains(&entry) {
                    entries.push(entry);
                }
            }
            if wavecode_config::denylist::save_to(dir, &entries).is_ok() {
                settings.save_at(path);
            }
        }
        settings
    }

    /// Persist to the default path (best effort; a missing home just
    /// skips saving).
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        self.save_at(&path);
    }

    /// [`Self::save`] to an explicit path (tests, migration).
    fn save_at(&self, path: &std::path::Path) {
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_ok()
            && let Ok(text) = serde_json::to_string_pretty(self)
        {
            let _ = std::fs::write(path, text);
        }
    }
}

/// The denylist's former storage shape, parsed only to migrate old
/// files into the config-owned store.
#[derive(serde::Deserialize, Default)]
struct LegacySettings {
    #[serde(default)]
    wave_denylist: Vec<String>,
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
        self.lock().clone()
    }

    /// Apply `edit` to the current values and persist the result.
    pub fn update(&self, edit: impl FnOnce(&mut UiSettings)) {
        let mut guard = self.lock();
        edit(&mut guard);
        if self.persist {
            guard.save();
        }
    }

    /// Discard live values and reload from disk (`/reload`); holders
    /// of this handle see the fresh values on their next read.
    pub fn reload(&self) {
        *self.lock() = UiSettings::load();
    }

    /// Lock the shared settings, recovering from poison: the guarded
    /// value is a plain struct cloned/assigned wholesale, with no
    /// invariant a panic mid-critical-section could break, so a
    /// poisoned guard degrades to the inner value instead of failing
    /// every later read (the render path locks this every frame).
    fn lock(&self) -> std::sync::MutexGuard<'_, UiSettings> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One-time migration: legacy `wave_denylist` entries in the
    /// settings file move into the config-owned store, merge with
    /// entries already there, and the legacy key is dropped from the
    /// file. The loaded settings never carry the field again.
    #[test]
    fn legacy_denylist_migrates_to_the_config_store() {
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        wavecode_config::denylist::save_to(&wave, &["already-there".to_string()]).unwrap();
        let settings_path = wave.join("console-settings.json");
        std::fs::write(
            &settings_path,
            r#"{"render_user_markdown": true, "wave_denylist": ["rm -rf", "already-there"]}"#,
        )
        .unwrap();

        let settings = UiSettings::load_at(&settings_path, Some(&wave));
        assert_eq!(
            wavecode_config::denylist::load_from(&wave),
            vec!["already-there".to_string(), "rm -rf".to_string()]
        );
        // The rewritten file carries no denylist key, so a second load
        // cannot re-import and resurrect deliberately emptied entries.
        let text = std::fs::read_to_string(&settings_path).unwrap();
        assert!(!text.contains("wave_denylist"), "{text}");
        assert!(settings.render_user_markdown);
        let reloaded = UiSettings::load_at(&settings_path, Some(&wave));
        assert_eq!(
            wavecode_config::denylist::load_from(&wave),
            vec!["already-there".to_string(), "rm -rf".to_string()]
        );
        drop(reloaded);
    }
}
