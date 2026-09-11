//! Slash command surface: candidate completion, routing, and local
//! handling of builtin commands.

use operations_wire::Op;

use super::App;
use super::types::{Item, dim, warn};
use crate::text::sanitize_terminal;

/// Builtin slash commands (completion and routing share this list;
/// skill names arrive injected from the assembly side).
const BUILTIN_COMMANDS: &[&str] = &["compact", "memory", "mcp", "permissions", "quit", "exit"];

impl App {
    /// Slash candidates from the current input (builtins plus known
    /// skills, prefix-filtered).
    pub fn slash_candidates(&self) -> Vec<String> {
        let Some(prefix) = self.input.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(char::is_whitespace) {
            return Vec::new();
        }
        BUILTIN_COMMANDS
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| format!("/{name}"))
            .chain(
                self.ctx
                    .skill_names
                    .iter()
                    .filter(|name| name.starts_with(prefix))
                    .map(|name| format!("/{name}")),
            )
            .collect()
    }

    /// Whether the slash popup shows: `/` prefix, no argument blanks,
    /// not dismissed, and non-empty candidates.
    pub fn slash_visible(&self) -> bool {
        !self.slash_dismissed && !self.slash_candidates().is_empty()
    }

    pub fn slash_selected(&self) -> usize {
        self.slash_selected
    }

    /// Fill the input box with the selected candidate.
    pub(super) fn complete_slash(&mut self) {
        let candidates = self.slash_candidates();
        let idx = self.slash_selected.min(candidates.len().saturating_sub(1));
        if let Some(c) = candidates.get(idx) {
            self.input = c.clone();
            self.cursor = self.input.chars().count();
        }
    }

    /// Enter submits: the line enters the stream (`> ` prefix) and
    /// routes to an Op by slash rules.
    pub(super) fn submit_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.slash_dismissed = false;
        self.slash_selected = 0;
        self.push_item(Item::user(&text));
        match text.strip_prefix('/') {
            None => self.outbox.push(Op::UserInput { text }),
            Some(rest) => self.route_slash(rest),
        }
        self.follow_tail = true;
    }

    /// Slash routing: builtins handled locally or by op; known skill
    /// names route through the model (catalog plus `skill` tool), unknown
    /// names warn.
    fn route_slash(&mut self, rest: &str) {
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        match name {
            "quit" | "exit" => self.quit = true,
            "compact" => self.outbox.push(Op::Compact),
            // `/memory` renders locally from the injected index: the wire
            // carries turns, not inspection reads.
            "memory" => self.show_memory(),
            "mcp" => self.show_mcp(),
            "permissions" => self.cycle_permission_mode(),
            _ => {
                if self.ctx.skill_names.iter().any(|n| n == name) {
                    // No skill wire op exists by design: the model already
                    // holds the catalog and the `skill` tool executes with
                    // proper inline/fork routing, so naming the skill plus
                    // its arguments deterministically triggers it.
                    let request = if args.is_empty() {
                        format!("Please use the '{name}' skill for this request.")
                    } else {
                        format!(
                            "Please use the '{name}' skill for this request. Arguments: {args}"
                        )
                    };
                    self.outbox.push(Op::UserInput { text: request });
                } else {
                    self.push_item(Item::plain(
                        format!(
                            "Unknown command: /{name} (builtins: {}; other / prefixes are skill names)",
                            BUILTIN_COMMANDS
                                .iter()
                                .map(|c| format!("/{c}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                        warn(),
                    ));
                }
            }
        }
    }

    /// `/memory`: render the injected index, or state its absence.
    fn show_memory(&mut self) {
        if self.ctx.memory_index.trim().is_empty() {
            self.push_item(Item::plain(
                "Memory unavailable (session has no memory attached)".into(),
                warn(),
            ));
        } else {
            self.push_item(Item::plain(
                sanitize_terminal(self.ctx.memory_index.trim_end()).into_owned(),
                dim(),
            ));
        }
    }

    /// `/mcp`: show configured server status lines (pre-rendered by the
    /// assembly side; honestly labeled, never faked online).
    fn show_mcp(&mut self) {
        if self.ctx.mcp_server_lines.is_empty() {
            self.push_item(Item::plain(
                "(No MCP servers configured; add a [mcp_servers.<name>] section in config.toml)".into(),
                dim(),
            ));
        } else {
            // Clone first: push_item takes &mut self while the lines live
            // in self.ctx, and every committed item must pass the cap.
            let lines = self.ctx.mcp_server_lines.clone();
            for line in &lines {
                self.push_item(Item::plain(sanitize_terminal(line).into_owned(), dim()));
            }
        }
    }

    /// `/permissions`: cycle four modes, syncing the driver side over
    /// the wire while applying the new mode locally at once.
    fn cycle_permission_mode(&mut self) {
        let next = self.permission_mode.cycle();
        self.permission_mode = next;
        self.outbox.push(Op::SetPermissionMode {
            mode: next.as_str().to_string(),
        });
        self.push_item(Item::plain(
            format!("Permission mode switched to {next} (approval policy for write/exec tools follows)"),
            dim(),
        ));
    }
}
