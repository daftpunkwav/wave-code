/*!
 * @file SlashCommands
 * @description Registry, parsing, and dispatch effects for interactive slash commands.
 *
 * Responsibilities:
 * - Parse slash command invocations from input buffer.
 * - Dispatch commands to UI effects (e.g. /clear, /theme, /help) or session wire operations.
 * - Provide command auto-completion and help descriptions.
 *
 * This module must not depend on: runtime or capability crates.
 */

//! Slash commands: registry, parsing, and dispatch effects.
//!
//! Commands return the side effect for the UI to run; unknown tokens
//! fall through as plain user input (skill or free prompt), matching
//! the REPL's contract.

use crate::state::AppState;
use wavecode_wire::Op;

/// The commands offered in slash completion.
pub const COMMANDS: [&str; 37] = [
    "help",
    "btw",
    "new",
    "clear",
    "sessions",
    "resume",
    "fork",
    "title",
    "model",
    "provider",
    "doctor",
    "agents",
    "hooks",
    "release-notes",
    "effort",
    "permissions",
    "auto",
    "wave",
    "plan",
    "init",
    "mcp",
    "settings",
    "theme",
    "usage",
    "version",
    "status",
    "memory",
    "snapshots",
    "goal",
    "compact",
    "undo",
    "editor",
    "reload",
    "copy",
    "export",
    "exit",
    "quit",
];

/// One command invocation: `/name args…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Lowercased command name (no slash).
    pub name: String,
    /// Raw argument text (may be empty).
    pub args: String,
}

/// Parse a submit line into a command invocation; `None` when the line
/// is not a slash command.
pub fn parse(line: &str) -> Option<Invocation> {
    let line = line.trim();
    let rest = line.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let (name, args) = match rest.split_once(' ') {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    Some(Invocation {
        name: name.to_lowercase(),
        args: args.to_string(),
    })
}

/// What the UI should do after dispatching a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Queue the ops and keep running.
    Ops(Vec<Op>),
    /// Exit the session.
    Exit,
    /// Not a known command: send the raw text as user input.
    Fallthrough,
}

/// Dispatch a command against the current state.
pub fn dispatch(invocation: &Invocation, _state: &AppState) -> Effect {
    match invocation.name.as_str() {
        "help" => Effect::Ops(Vec::new()),
        "clear" | "new" => Effect::Ops(Vec::new()), // handled by the UI caller
        "copy" | "export" | "settings" => Effect::Ops(Vec::new()), // UI caller
        "usage" | "version" => Effect::Ops(Vec::new()), // rendered by the caller
        "btw" | "sessions" | "resume" | "fork" | "title" | "init" | "mcp" | "status" | "undo"
        | "editor" | "reload" | "doctor" | "agents" | "hooks" | "release-notes" | "memory"
        | "snapshots" | "goal" => {
            // Dialogs, panels, and local displays: the caller owns the
            // behavior.
            Effect::Ops(Vec::new())
        }
        "compact" => {
            let focus = invocation.args.trim();
            Effect::Ops(vec![Op::Compact {
                instruction: (!focus.is_empty()).then(|| focus.to_string()),
            }])
        }
        "model" => {
            // The UI caller owns /model entirely: bare opens the picker
            // and a name switches with picker semantics (alias
            // resolution, effort, same-provider guard). No op here —
            // one would double-send beside the caller's resolved
            // switch. Catalog editing lives under /provider.
            Effect::Ops(Vec::new())
        }
        "provider" => {
            // The UI caller owns /provider entirely: bare opens the
            // provider surface (picking a saved provider preseeds the
            // wizard), bare `add` opens the wizard while the positional
            // form inserts directly, and list/set/remove edit the saved
            // catalog.
            Effect::Ops(Vec::new())
        }
        "effort" => {
            if invocation.args.is_empty() {
                Effect::Ops(Vec::new()) // shows the current level
            } else {
                Effect::Ops(vec![Op::SetThinking {
                    effort: invocation.args.trim().to_lowercase(),
                }])
            }
        }
        "permissions" => {
            if invocation.args.is_empty() {
                // Bare opens the picker in the UI caller; an op here
                // would rotate the mode behind the picker and leave it
                // applied even after the picker is dismissed.
                Effect::Ops(Vec::new())
            } else {
                Effect::Ops(vec![Op::SetPermissionMode {
                    mode: normalize_mode(&invocation.args),
                }])
            }
        }
        // Direct mode shortcuts: one-line aliases of /permissions
        // modes, so a mode flip never costs a picker round trip.
        "auto" => Effect::Ops(vec![Op::SetPermissionMode {
            mode: "auto".to_string(),
        }]),
        "wave" => Effect::Ops(vec![Op::SetPermissionMode {
            mode: "wave".to_string(),
        }]),
        "plan" => Effect::Ops(vec![Op::SetPermissionMode {
            mode: "plan".to_string(),
        }]),
        "theme" => Effect::Ops(Vec::new()), // applied by the caller (local)
        "exit" | "quit" => Effect::Exit,
        _ => Effect::Fallthrough,
    }
}

/// Cycle plan → auto → wave → plan.
pub fn cycle_mode(current: &str) -> String {
    match current {
        "plan" => "auto".to_string(),
        "auto" => "wave".to_string(),
        _ => "plan".to_string(),
    }
}

/// Normalize a user-supplied mode onto the canonical wire names
/// (legacy aliases included; mirrors `PermissionMode::parse`).
///
/// Unknown inputs pass through unchanged instead of silently widening to
/// `auto`: the actor rejects unparseable names with a visible warning and
/// the mode stays put, so a typo in `/permissions <name>` can never flip a
/// plan session into auto.
pub fn normalize_mode(input: &str) -> String {
    match input.to_lowercase().as_str() {
        "plan" => "plan".to_string(),
        "auto" | "guarded" | "default" | "acceptedits" => "auto".to_string(),
        "wave" | "yolo" | "bypasspermissions" => "wave".to_string(),
        other => other.to_string(),
    }
}

/// The help content: keybinding rows followed by one command row per
/// line, each `name — description`. Rendered by the scrollable help
/// panel; plain lines so the panel stays width-agnostic.
pub fn help_lines() -> Vec<String> {
    vec![
        "Keybindings".to_string(),
        "  shift+enter / ctrl+j   newline (backslash+enter also works)".to_string(),
        "  shift+tab              cycle permission mode (plan/auto/wave)".to_string(),
        "  ctrl+o                 expand tool output and thinking".to_string(),
        "  ctrl+t                 expand the todo panel".to_string(),
        "  ctrl+s                 steer a running turn".to_string(),
        "  ctrl+g                 edit the draft in an external editor".to_string(),
        "  ctrl+m                 toggle mermaid diagram rendering".to_string(),
        "  ctrl+d                 exit (empty editor; double-press)".to_string(),
        "  ctrl+c                 interrupt; double-press exits".to_string(),
        "  esc                    interrupt the running turn / close panels".to_string(),
        "  esc esc                rewind the conversation (idle only)".to_string(),
        "  up / down              input history".to_string(),
        "  alt+b / alt+f          jump one word back / forward".to_string(),
        "  !cmd                   run a local shell command".to_string(),
        "  @path                  mention a file".to_string(),
        "Commands".to_string(),
        "  /help — this panel".to_string(),
        "  /new — start a fresh session (new context, new journal)".to_string(),
        "  /clear — clear the screen only (context is kept)".to_string(),
        "  /sessions, /resume — pick and resume a past session".to_string(),
        "  /fork — snapshot this session into a resumable copy".to_string(),
        "  /title — rename the session (a picker asks for the name)".to_string(),
        "  /model — open the model picker; /model <name> switches directly".to_string(),
        "  /provider — guided setup: pick a provider or walk the new-model wizard (api, endpoint, key, limits, thinking levels, modalities)".to_string(),
        "  /provider list — show the catalog; /provider set|remove edit saved entries".to_string(),
        "  /effort — pick the reasoning effort; /effort <level> switches directly".to_string(),
        "  /permissions — open the mode picker; /plan /auto /wave switch directly".to_string(),
        "  /init — ask the agent to write AGENTS.md for this repo".to_string(),
        "  /mcp — list configured MCP servers".to_string(),
        "  /settings — rendering, tool display, footer, confirmations, history".to_string(),
        "  /theme — pick the theme interactively; /theme <name> switches directly".to_string(),
        "  /usage — token usage, context window, and remaining space".to_string(),
        "  /doctor — one-pass health check (config, credentials, catalog, settings)".to_string(),
        "  /agents — background jobs (promoted shell commands), with state".to_string(),
        "  /hooks — the configured hook table".to_string(),
        "  /release-notes — the newest changelog sections".to_string(),
        "  /status — session, model, mode, and context summary".to_string(),
        "  /memory — the AGENTS.md instruction files in scope".to_string(),
        "  /snapshots — file-content snapshot labels".to_string(),
        "  /goal — durable goal status".to_string(),
        "  /compact — compact the context now; add text to steer the summary".to_string(),
        "  /undo — pick a turn to rewind; /undo <n> drops the last n turns".to_string(),
        "  /editor — set the external editor for ctrl+g".to_string(),
        "  /reload — re-read settings and the theme from disk".to_string(),
        "  /copy — copy the last assistant message".to_string(),
        "  /export — write the dialogue to markdown (the prompt asks for the path)".to_string(),
        "  /btw — ask a side question (read-only, answered in a panel)".to_string(),
        "  /exit, /quit — end the session".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state() -> AppState {
        AppState::new(
            "m".to_string(),
            PathBuf::from("."),
            "guarded".to_string(),
            Vec::new(),
        )
    }

    #[test]
    fn parses_commands_and_args() {
        let invocation = parse("/model claude-x").unwrap();
        assert_eq!(invocation.name, "model");
        assert_eq!(invocation.args, "claude-x");
        assert!(parse("/compact").is_some());
        assert_eq!(parse("/compact").unwrap().args, "");
        assert!(parse("hello world").is_none());
        assert!(parse("/").is_none());
    }

    #[test]
    fn dispatch_maps_to_ops() {
        let effect = dispatch(&parse("/compact").unwrap(), &state());
        assert_eq!(effect, Effect::Ops(vec![Op::Compact { instruction: None }]));
        let effect = dispatch(
            &parse("/compact keep the api decisions").unwrap(),
            &state(),
        );
        assert_eq!(
            effect,
            Effect::Ops(vec![Op::Compact {
                instruction: Some("keep the api decisions".to_string())
            }])
        );

        let effect = dispatch(&parse("/clear").unwrap(), &state());
        assert_eq!(effect, Effect::Ops(Vec::new()));

        // Caller-handled commands dispatch as no-ops on the wire.
        for name in ["copy", "export", "theme", "usage", "version"] {
            let effect = dispatch(&parse(&format!("/{name}")).unwrap(), &state());
            assert_eq!(effect, Effect::Ops(Vec::new()), "{name}");
        }

        // `/model <name>` is caller-owned (picker semantics and the
        // catalog subcommands); dispatch carries no op.
        let effect = dispatch(&parse("/model fast").unwrap(), &state());
        assert_eq!(effect, Effect::Ops(Vec::new()));

        let effect = dispatch(&parse("/plan").unwrap(), &state());
        assert_eq!(
            effect,
            Effect::Ops(vec![Op::SetPermissionMode {
                mode: "plan".to_string()
            }])
        );

        // Direct mode shortcuts.
        for (name, mode) in [("/auto", "auto"), ("/wave", "wave")] {
            let effect = dispatch(&parse(name).unwrap(), &state());
            assert_eq!(
                effect,
                Effect::Ops(vec![Op::SetPermissionMode {
                    mode: mode.to_string()
                }]),
                "{name}"
            );
        }

        // Reasoning-effort shortcut reaches the new wire op.
        let effect = dispatch(&parse("/effort HIGH").unwrap(), &state());
        assert_eq!(
            effect,
            Effect::Ops(vec![Op::SetThinking {
                effort: "high".to_string()
            }])
        );

        // Caller-handled commands dispatch as no-ops on the wire.
        for name in [
            "btw", "new", "sessions", "resume", "fork", "title", "init", "mcp", "status", "undo",
            "editor", "reload",
        ] {
            let effect = dispatch(&parse(&format!("/{name}")).unwrap(), &state());
            assert_eq!(effect, Effect::Ops(Vec::new()), "{name}");
        }

        let effect = dispatch(&parse("/exit").unwrap(), &state());
        assert_eq!(effect, Effect::Exit);
    }

    #[test]
    fn unknown_commands_fall_through() {
        let effect = dispatch(&parse("/skill:something").unwrap(), &state());
        assert_eq!(effect, Effect::Fallthrough);
    }

    #[test]
    fn mode_cycling_and_normalization() {
        assert_eq!(cycle_mode("plan"), "auto");
        assert_eq!(cycle_mode("auto"), "wave");
        assert_eq!(cycle_mode("wave"), "plan");
        assert_eq!(normalize_mode("YOLO"), "wave");
        assert_eq!(normalize_mode("plan"), "plan");
        assert_eq!(normalize_mode("guarded"), "auto");
        // The remaining legacy aliases map like PermissionMode::parse.
        assert_eq!(normalize_mode("acceptEdits"), "auto");
        assert_eq!(normalize_mode("bypassPermissions"), "wave");
        // Unknown names pass through so the actor's rejection warning
        // fires and the mode stays, instead of silently widening to auto.
        assert_eq!(normalize_mode("nonsense"), "nonsense");
    }
}
