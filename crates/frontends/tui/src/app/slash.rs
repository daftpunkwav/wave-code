//! Slash command surface: candidate completion, routing, and local
//! handling of builtin commands.

use operations_wire::Op;

use std::path::PathBuf;

use super::App;
use super::types::{Item, dim, err, warn};
use crate::text::sanitize_terminal;

/// Builtin slash commands (completion and routing share this list;
/// skill names arrive injected from the assembly side).
const BUILTIN_COMMANDS: &[&str] = &[
    "compact",
    "memory",
    "mcp",
    "plan",
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
            "snapshots" => self.show_snapshots(),
            "rewind" => self.show_rewind(args),
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
    /// `/mcp`; the tui cannot depend on the plan crate, so the
    /// `<home>/.wavecode/plans/default.json` convention is spelled out
    /// here. Proposing and approving run through the model tools.)
    fn show_plan(&mut self) {
        match Self::plan_status_text() {
            Some(text) => self.push_item(Item::plain(sanitize_terminal(&text).into_owned(), dim())),
            None => self.push_item(Item::plain(
                "(no reviewed plan yet; ask the agent to propose one with the plan_propose tool)"
                    .into(),
                dim(),
            )),
        }
    }

    /// Read and leniently render the reviewed-plan file. `None` means no
    /// proposal exists yet (missing file or empty text); corrupt content
    /// reports itself instead of failing the display.
    fn plan_status_text() -> Option<String> {
        const HEAD_CHARS: usize = 500;
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
        let text = std::fs::read_to_string(
            PathBuf::from(home)
                .join(".wavecode")
                .join("plans")
                .join("default.json"),
        )
        .ok()?;
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                return Some(
                    "(reviewed plan file unreadable; delete <home>/.wavecode/plans/default.json to start over)".to_string(),
                );
            }
        };
        let status = value
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("draft");
        let round = value
            .get("updated_round")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let mut out = format!("plan status: {status} (updated round {round})");
        match value.get("plan_text").and_then(|v| v.as_str()) {
            Some(body) if !body.trim().is_empty() => {
                let head: String = body.chars().take(HEAD_CHARS).collect();
                out.push_str(&format!("\nplan:\n{head}"));
                if body.chars().count() > HEAD_CHARS {
                    out.push_str("\n(truncated; ask the agent for the full plan)");
                }
            }
            _ => return None,
        }
        Some(out)
    }

    /// `/snapshots`: list file-content snapshot labels (local display,
    /// no Op; the tui must not depend on tools, so the store layout under
    /// `<home>/.wavecode/snapshots/` is read directly).
    fn show_snapshots(&mut self) {
        const EMPTY_HINT: &str =
            "(no snapshots yet; ask the agent to capture one with the snapshot tool)";
        let Some(root) = Self::snapshot_store_root() else {
            self.push_item(Item::plain(
                "Snapshots unavailable (cannot resolve home directory)".into(),
                warn(),
            ));
            return;
        };
        let mut labels: Vec<String> = match std::fs::read_dir(&root) {
            Err(_) => Vec::new(),
            Ok(dir) => dir
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with(".staging-") && is_snapshot_label(n))
                .collect(),
        };
        labels.sort();
        if labels.is_empty() {
            self.push_item(Item::plain(EMPTY_HINT.into(), dim()));
        } else {
            for label in labels {
                self.push_item(Item::plain(sanitize_terminal(&label).into_owned(), dim()));
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
        if !is_snapshot_label(label) {
            self.push_item(Item::plain(
                format!("invalid snapshot label {label:?} ([A-Za-z0-9_-], max 64 chars)"),
                err(),
            ));
            return;
        }
        let Some(root) = Self::snapshot_store_root() else {
            self.push_item(Item::plain(
                "Snapshots unavailable (cannot resolve home directory)".into(),
                warn(),
            ));
            return;
        };
        match std::fs::read_to_string(root.join(label).join("manifest.json")) {
            Err(_) => self.push_item(Item::plain(
                format!("unknown snapshot {label:?} (see /snapshots for labels)"),
                err(),
            )),
            Ok(text) => {
                let raw = summarize_snapshot_manifest(label, &text);
                let summary = sanitize_terminal(&raw);
                self.push_item(Item::plain(summary.into_owned(), dim()));
                self.push_item(Item::plain(
                    "(rewind itself runs through the restore tool and needs approval)".into(),
                    dim(),
                ));
            }
        }
    }

    /// Snapshot store root (the tui cannot depend on tools, so the
    /// `<home>/.wavecode/snapshots` convention is spelled out here,
    /// mirroring the snapshot store layout).
    fn snapshot_store_root() -> Option<PathBuf> {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(|home| PathBuf::from(home).join(".wavecode").join("snapshots"))
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
            format!(
                "Permission mode switched to {next} (approval policy for write/exec tools follows)"
            ),
            dim(),
        ));
    }
}

/// Snapshot label rule, mirroring the snapshot store
/// (`[A-Za-z0-9_-]{1,64}`; labels become a single directory name).
fn is_snapshot_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 64
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Render a snapshot `manifest.json` leniently: counts and the file list
/// head when present, a fallback line for unknown shapes (never fails, so
/// `/rewind` display stays total).
fn summarize_snapshot_manifest(label: &str, text: &str) -> String {
    const MAX_FILES: usize = 20;
    let manifest: serde_json::Value = match serde_json::from_str(text) {
        Ok(manifest) => manifest,
        Err(_) => return format!("Snapshot '{label}': unreadable manifest."),
    };
    let count = manifest
        .get("file_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let bytes = manifest
        .get("total_bytes")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let created = manifest
        .get("created_at")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let caps = manifest
        .get("caps_hit")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|caps| !caps.is_empty())
        .unwrap_or_else(|| "none".to_owned());
    let mut out = format!(
        "Snapshot '{label}': {count} files ({bytes} bytes), captured at unix {created}. Caps hit: {caps}."
    );
    let files: Vec<&str> = manifest
        .get("files")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|e| e.get("path").and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if !files.is_empty() {
        let head = files
            .iter()
            .take(MAX_FILES)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("\nfiles: {head}"));
        if files.len() > MAX_FILES {
            out.push_str(&format!(", and {} more", files.len() - MAX_FILES));
        }
    }
    out
}
