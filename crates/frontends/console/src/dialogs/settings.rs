//! Interactive settings panel: rows of (setting, value) cycled in
//! place and persisted through the shared settings handle.

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};

/// The interactive settings panel: rows of (setting, value), Up/Down to
/// move, Left/Right or Enter to cycle the value. Changes apply
/// immediately and persist to the settings file.
pub struct SettingsDialog {
    settings: crate::settings::SharedSettings,
    title: String,
    selected: usize,
}

/// One settings row as displayed: label plus the current value text.
struct SettingsRow {
    label: &'static str,
    value: String,
}

impl SettingsDialog {
    /// A panel bound to the shared settings handle.
    pub fn new(settings: crate::settings::SharedSettings) -> Self {
        Self {
            settings,
            title: "Settings (left/right to change, esc to close)".to_string(),
            selected: 0,
        }
    }

    pub(super) fn title(&self) -> String {
        self.title.clone()
    }

    /// Snapshot the rows in display order.
    fn rows(&self) -> Vec<SettingsRow> {
        use crate::settings::{EditDisplay, ToolDisplay};
        let view = self.settings.get();
        vec![
            SettingsRow {
                label: "render user input as markdown",
                value: if view.render_user_markdown {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
            },
            SettingsRow {
                label: "tool call display",
                value: match view.tool_display {
                    ToolDisplay::Names => "names",
                    ToolDisplay::Summary => "summary",
                    ToolDisplay::Full => "full",
                }
                .to_string(),
            },
            SettingsRow {
                label: "edit tool rendering",
                value: match view.edit_display {
                    EditDisplay::Tool => "tool only",
                    EditDisplay::Diff => "diff",
                }
                .to_string(),
            },
            SettingsRow {
                label: "wave denylist",
                value: format!(
                    "{} entries (wave-denylist.json)",
                    wavecode_config::denylist::default_dir()
                        .map(|dir| wavecode_config::denylist::load_from(&dir).len())
                        .unwrap_or(0)
                ),
            },
            SettingsRow {
                label: "thinking starts expanded",
                value: Self::bool_value(view.thinking_expanded),
            },
            SettingsRow {
                label: "stream the assistant draft",
                value: Self::bool_value(view.show_streaming_draft),
            },
            SettingsRow {
                label: "context meter in footer",
                value: Self::bool_value(view.show_context_footer),
            },
            SettingsRow {
                label: "rotate footer tips",
                value: Self::bool_value(view.rotate_tips),
            },
            SettingsRow {
                label: "confirm before exit",
                value: Self::bool_value(view.confirm_exit),
            },
            SettingsRow {
                label: "input history limit",
                value: if view.history_limit == 0 {
                    "default (100)".to_string()
                } else {
                    format!("{}", view.history_limit)
                },
            },
        ]
    }

    /// The on/off cell for a boolean row.
    fn bool_value(value: bool) -> String {
        if value { "on" } else { "off" }.to_string()
    }

    /// Cycle the selected row's value one step (direction: +1 / -1).
    fn cycle(&self, direction: isize) {
        use crate::settings::{EditDisplay, ToolDisplay};
        let rows = self.rows();
        let label = rows.get(self.selected).map(|r| r.label);
        self.settings.update(|view| match label {
            Some("render user input as markdown") => {
                view.render_user_markdown = !view.render_user_markdown;
            }
            Some("tool call display") => {
                // names → summary → full, wrapping on the right and
                // resting on names when walking left; each (value,
                // direction) pair names its own target.
                view.tool_display = match (view.tool_display, direction) {
                    (ToolDisplay::Names, 1) => ToolDisplay::Summary,
                    (ToolDisplay::Summary, 1) => ToolDisplay::Full,
                    (ToolDisplay::Summary, -1) => ToolDisplay::Names,
                    (ToolDisplay::Full, -1) => ToolDisplay::Summary,
                    _ => ToolDisplay::Names,
                };
            }
            Some("edit tool rendering") => {
                view.edit_display = match view.edit_display {
                    EditDisplay::Tool => EditDisplay::Diff,
                    EditDisplay::Diff => EditDisplay::Tool,
                };
            }
            Some("thinking starts expanded") => {
                view.thinking_expanded = !view.thinking_expanded;
            }
            Some("stream the assistant draft") => {
                view.show_streaming_draft = !view.show_streaming_draft;
            }
            Some("context meter in footer") => {
                view.show_context_footer = !view.show_context_footer;
            }
            Some("rotate footer tips") => {
                view.rotate_tips = !view.rotate_tips;
            }
            Some("confirm before exit") => {
                view.confirm_exit = !view.confirm_exit;
            }
            Some("input history limit") => {
                // Three stops: default (100) → 500 → 1000 → default.
                // A cycle keeps the row keyboard-only, like every other
                // setting here; the left direction walks back down
                // (1000 → 500 → default) and rests at the default.
                view.history_limit = match (view.history_limit, direction) {
                    (0, 1) => 500,
                    (500, 1) => 1000,
                    (500, -1) => 0,
                    (1000, -1) => 500,
                    _ => 0,
                };
            }
            _ => {}
        });
    }

    /// Handle one key; `Some(Answer::Dismissed)` when the panel closes.
    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        let count = self.rows().len();
        match (event.key, event.mods) {
            (Key::Esc, _) => Some(Answer::Dismissed),
            (Key::Up, _) => {
                self.selected = if self.selected == 0 {
                    count.saturating_sub(1)
                } else {
                    self.selected - 1
                };
                None
            }
            (Key::Down, _) => {
                self.selected = (self.selected + 1) % count.max(1);
                None
            }
            (Key::Left, _) => {
                self.cycle(-1);
                None
            }
            (Key::Right, _) | (Key::Enter, _) => {
                self.cycle(1);
                None
            }
            _ => None,
        }
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let rows = self.rows();
        let mut body = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            // Painted like every other selection glyph (footer's mode
            // badge does the same): unpainted, it drops off the theme
            // contract and goes unreadable on a recolored light paper.
            let marker = theme.paint(
                Token::Primary,
                if index == self.selected { "▍" } else { " " },
            );
            let label = theme.paint(Token::Text, row.label);
            let value = if index == self.selected {
                theme.bold(Token::Primary, &row.value)
            } else {
                theme.paint(Token::TextDim, &row.value)
            };
            body.push(format!("{marker} {label}  {value}"));
        }
        body.push(String::new());
        body.push(theme.paint(
            Token::TextMuted,
            "the wave denylist lives in ~/.wavecode/console-settings.json",
        ));
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
