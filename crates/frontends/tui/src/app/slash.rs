//! Slash command surface: candidate completion, routing, and local
//! handling of builtin commands.

use wavecode_wire::Op;

use super::App;
use super::types::{Item, dim, err, warn};
use crate::text::sanitize_terminal;

/// Builtin slash commands (completion and routing share this list;
/// skill names arrive injected from the assembly side).
const BUILTIN_COMMANDS: &[&str] = &[
    "compact",
    "memory",
    "mcp",
    "model",
    "plan",
    "goal",
    "snapshots",
    "rewind",
    "permissions",
    "quit",
    "exit",
];

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
            "plan" => self.show_plan(),
            "goal" => self.show_goal(),
            "snapshots" => self.show_snapshots(),
            "rewind" => self.show_rewind(args),
            "model" => self.switch_model(args),
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
                        format!("Please use the '{name}' skill for this request. Arguments: {args}")
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
                "(No MCP servers configured; add a [mcp_servers.<name>] section in config.toml)"
                    .into(),
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

    /// `/plan`: show reviewed-plan status (local display only, mirroring
    /// `/mcp`; current state is queried from the assembly side, so the
    /// display never depends on the storage layout. Proposing and
    /// approving run through the model tools.)
    fn show_plan(&mut self) {
        match self.ctx.status_queries.plan_status() {
            Some(text) => self.push_item(Item::plain(sanitize_terminal(&text).into_owned(), dim())),
            None => self.push_item(Item::plain(
                "(no reviewed plan yet; ask the agent to propose one with the plan tool)".into(),
                dim(),
            )),
        }
    }

    /// `/goal`: show durable-goal status (local display only, mirroring
    /// `/plan`; current state is queried from the assembly side. Setting
    /// and ticking run through the model tools.)
    fn show_goal(&mut self) {
        match self.ctx.status_queries.goal_status() {
            Some(text) => self.push_item(Item::plain(sanitize_terminal(&text).into_owned(), dim())),
            None => self.push_item(Item::plain(
                "(no durable goal yet; ask the agent to set one with the goal tool)".into(),
                dim(),
            )),
        }
    }

    /// `/snapshots`: list file-content snapshot labels (local display,
    /// no Op; labels come from the assembly-side query).
    fn show_snapshots(&mut self) {
        const EMPTY_HINT: &str =
            "(no snapshots yet; ask the agent to capture one with the snapshot tool)";
        let labels = self.ctx.status_queries.snapshot_labels();
        if labels.is_empty() {
            self.push_item(Item::plain(EMPTY_HINT.into(), dim()));
        } else {
            for label in &labels {
                self.push_item(Item::plain(sanitize_terminal(label).into_owned(), dim()));
            }
        }
    }

    /// `/rewind <label>`: show one snapshot's summary (local display only;
    /// the rewind itself runs through the `restore` tool with approval).
    fn show_rewind(&mut self, args: &str) {
        let label = args.trim();
        if label.is_empty() {
            self.push_item(Item::plain(
                "usage: /rewind <label> (see /snapshots for labels)".into(),
                warn(),
            ));
            return;
        }
        match self.ctx.status_queries.snapshot_summary(label) {
            Some(summary) => {
                let summary = sanitize_terminal(&summary);
                self.push_item(Item::plain(summary.into_owned(), dim()));
                self.push_item(Item::plain(
                    "(rewind itself runs through the restore tool and needs approval)".into(),
                    dim(),
                ));
            }
            None => self.push_item(Item::plain(
                format!("unknown snapshot {label:?} (see /snapshots for labels)"),
                err(),
            )),
        }
    }

    /// `/permissions`: cycle the three modes, syncing the driver side
    /// over the wire while applying the new mode locally at once.
    fn cycle_permission_mode(&mut self) {
        let next = self.permission_mode.cycle();
        self.permission_mode = next;
        self.outbox.push(Op::SetPermissionMode {
            mode: next.as_str().to_string(),
        });
        self.push_item(Item::plain(
            format!(
                "Permission mode switched to {next} (approval policy for dangerous tools follows)"
            ),
            dim(),
        ));
    }

    /// `/model <name>`: switch the sampling model for subsequent turns.
    /// The label applies locally at once; the wire op is authoritative —
    /// a rejected switch (unknown name / fixed gateway) arrives back as a
    /// Warning event.
    fn switch_model(&mut self, args: &str) {
        let name = args.trim();
        if name.is_empty() {
            self.push_item(Item::plain(
                format!(
                    "current model: {} (/model <name> to switch)",
                    self.model_name()
                ),
                dim(),
            ));
            return;
        }
        self.model_label = Some(name.to_string());
        self.outbox.push(Op::SetModel {
            name: name.to_string(),
        });
        self.push_item(Item::plain(
            format!("model switch requested: {name}"),
            dim(),
        ));
    }
}
