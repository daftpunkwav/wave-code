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
use operations_actor::StatusQueries;
use wavecode_wire::Op;

/// The commands offered in slash completion.
pub const COMMANDS: [&str; 14] = [
    "help",
    "clear",
    "usage",
    "version",
    "compact",
    "model",
    "permissions",
    "plan",
    "theme",
    "memory",
    "snapshots",
    "goal",
    "status",
    "exit",
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

/// Dispatch a command against the current state and status queries.
pub fn dispatch(invocation: &Invocation, state: &AppState, status: &dyn StatusQueries) -> Effect {
    match invocation.name.as_str() {
        "help" => Effect::Ops(Vec::new()),
        "clear" => Effect::Ops(Vec::new()), // handled by the UI caller
        "usage" | "version" => Effect::Ops(Vec::new()), // rendered by the caller
        "compact" => Effect::Ops(vec![Op::Compact]),
        "model" => {
            if invocation.args.is_empty() {
                Effect::Ops(Vec::new())
            } else {
                Effect::Ops(vec![Op::SetModel {
                    name: invocation.args.clone(),
                }])
            }
        }
        "permissions" => {
            let mode = if invocation.args.is_empty() {
                cycle_mode(&state.permission_mode)
            } else {
                normalize_mode(&invocation.args)
            };
            Effect::Ops(vec![Op::SetPermissionMode { mode }])
        }
        "plan" => Effect::Ops(vec![Op::SetPermissionMode {
            mode: "plan".to_string(),
        }]),
        "theme" => Effect::Ops(Vec::new()), // applied by the caller (local)
        "exit" | "quit" => Effect::Exit,
        "memory" => {
            let _ = status.plan_status();
            Effect::Ops(Vec::new())
        }
        "snapshots" => {
            let _ = status.snapshot_labels();
            Effect::Ops(Vec::new())
        }
        "goal" | "status" => {
            let _ = status.goal_status();
            Effect::Ops(Vec::new())
        }
        _ => Effect::Fallthrough,
    }
}

/// Cycle plan → guarded → auto → plan.
pub fn cycle_mode(current: &str) -> String {
    match current {
        "plan" => "guarded".to_string(),
        "auto" => "plan".to_string(),
        _ => "auto".to_string(),
    }
}

/// Normalize a user-supplied mode onto the canonical wire names.
pub fn normalize_mode(input: &str) -> String {
    match input.to_lowercase().as_str() {
        "plan" => "plan".to_string(),
        "auto" | "yolo" | "bypasspermissions" => "auto".to_string(),
        _ => "guarded".to_string(),
    }
}

/// The help lines shown by `/help`.
pub fn help_lines() -> Vec<String> {
    vec![
        "Keybindings:".to_string(),
        "  shift+enter / ctrl+j  newline (backslash+enter also works)".to_string(),
        "  shift+tab             cycle permission mode (ask/auto/plan)".to_string(),
        "  ctrl+o                expand tool output and thinking".to_string(),
        "  ctrl+t                expand the todo panel".to_string(),
        "  ctrl+s                steer a running turn".to_string(),
        "  ctrl+c                interrupt; double-press exits".to_string(),
        "  esc                   interrupt the running turn".to_string(),
        "  up / down             input history".to_string(),
        "  !cmd                  run a local shell command".to_string(),
        "Commands:".to_string(),
        "  /help /clear /usage /version /compact /model <name> /permissions [mode] /plan"
            .to_string(),
        "  /theme <light|dark> /memory /snapshots /goal /status /exit".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_support as support;
    use std::path::PathBuf;
    use std::sync::Arc;

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
        let status = Arc::new(support::NullStatus);
        let effect = dispatch(&parse("/compact").unwrap(), &state(), status.as_ref());
        assert_eq!(effect, Effect::Ops(vec![Op::Compact]));

        let effect = dispatch(&parse("/clear").unwrap(), &state(), status.as_ref());
        assert_eq!(effect, Effect::Ops(Vec::new()));

        let effect = dispatch(&parse("/model fast").unwrap(), &state(), status.as_ref());
        assert_eq!(
            effect,
            Effect::Ops(vec![Op::SetModel {
                name: "fast".to_string()
            }])
        );

        let effect = dispatch(&parse("/plan").unwrap(), &state(), status.as_ref());
        assert_eq!(
            effect,
            Effect::Ops(vec![Op::SetPermissionMode {
                mode: "plan".to_string()
            }])
        );

        let effect = dispatch(&parse("/exit").unwrap(), &state(), status.as_ref());
        assert_eq!(effect, Effect::Exit);
    }

    #[test]
    fn unknown_commands_fall_through() {
        let status = Arc::new(support::NullStatus);
        let effect = dispatch(
            &parse("/skill:something").unwrap(),
            &state(),
            status.as_ref(),
        );
        assert_eq!(effect, Effect::Fallthrough);
    }

    #[test]
    fn mode_cycling_and_normalization() {
        assert_eq!(cycle_mode("plan"), "guarded");
        assert_eq!(cycle_mode("guarded"), "auto");
        assert_eq!(cycle_mode("auto"), "plan");
        assert_eq!(normalize_mode("YOLO"), "auto");
        assert_eq!(normalize_mode("plan"), "plan");
        assert_eq!(normalize_mode("nonsense"), "guarded");
    }
}
