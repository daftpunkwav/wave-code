//! Slash commands and their picker / prompt dialogs.

use super::*;
use crate::theme;
use tui_engine::width::strip_ansi;

use super::tests_common::*;

#[test]
fn plan_command_updates_local_mode() {
    let mut ui = ui();
    let flow = ui.user_submit("/plan");
    assert_eq!(flow, Flow::Continue);
    assert_eq!(ui.state.permission_mode, "plan");
    assert!(
        matches!(
            ui.pending_ops().last(),
            Some(Op::SetPermissionMode { mode }) if mode == "plan"
        ),
        "wire op enqueued: {:?}",
        ui.pending_ops()
    );
}

#[test]
fn model_command_updates_local_model() {
    let mut ui = ui();
    ui.user_submit("/model fast-model");
    assert_eq!(ui.state.model_name, "fast-model");
    assert!(matches!(
        ui.pending_ops().last(),
        Some(Op::SetModel { name }) if name == "fast-model"
    ));
}

/// `/model <alias>` resolves through the picker entries exactly like
/// the picker's live switch: the wire model name rides the op, the
/// entry's default effort applies, and exactly one switch op is sent
/// (the old path double-sent one op per dispatch layer).
#[test]
fn model_command_resolves_an_alias_like_the_picker() {
    let mut ui = ui();
    ui.state.provider_id = "deepseek".to_string();
    ui.model_entries = vec![ModelEntryView {
        label: "GLM-5.3".to_string(),
        provider: "deepseek".to_string(),
        model: "glm-5.3".to_string(),
        effort: Some("max".to_string()),
    }];
    ui.user_submit("/model GLM-5.3");
    assert_eq!(ui.state.model_name, "GLM-5.3");
    assert_eq!(ui.state.thinking_effort.as_deref(), Some("max"));
    let ops = ui.pending_ops();
    assert_eq!(
        ops.iter()
            .filter(|op| matches!(op, Op::SetModel { .. }))
            .count(),
        1,
        "one switch op, not one per dispatch path: {ops:?}"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::SetModel { name } if name == "glm-5.3")),
        "the wire model name rides the op: {ops:?}"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "max")),
        "the entry's default effort applies: {ops:?}"
    );
}

/// A cross-provider alias refuses the live switch like the picker
/// does instead of silently keeping the old provider.
#[test]
fn model_command_refuses_cross_provider_switch_like_the_picker() {
    let mut ui = ui_with_models();
    ui.user_submit("/model MiniMax-M3");
    assert!(
        !ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetModel { .. })),
        "cross-provider live switch refused: {:?}",
        ui.pending_ops()
    );
}

#[test]
fn version_command_prints_version() {
    let mut ui = ui();
    ui.user_submit("/version");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("WaveCode v0.1.0"), "{joined}");
}

#[test]
fn model_no_args_opens_picker_and_alt_s_switches_live() {
    let mut ui = ui_with_models();
    ui.user_submit("/model");
    assert!(ui.dialog.is_some(), "picker opens");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Select a model"), "{joined}");
    // Move down to the same-provider sibling, then Alt+S: live
    // switch happens on the wire.
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::new(
        Key::Char('s'),
        tui_engine::keys::Mods {
            ctrl: false,
            alt: true,
            shift: false,
        },
    ));
    ui.handle_key(KeyEvent::plain(Key::Enter));
    let ops = ui.pending_ops().clone();
    assert!(
        ops.iter().any(|op| matches!(op, Op::SetModel { .. })),
        "same-provider selection switches live: {ops:?}"
    );
}

/// Bare `/provider add` opens the step-at-a-time model wizard;
/// `/model` is a switching command and points at /provider for its
/// retired subcommands.
#[test]
fn provider_add_bare_opens_the_wizard() {
    let mut ui = ui();
    ui.user_submit("/provider add");
    assert!(
        matches!(ui.dialog, Some(Dialog::ModelForm(_))),
        "wizard dialog opens"
    );
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none(), "esc dismisses");
    // The positional form still parses.
    ui.user_submit("/provider add x anthropic p https://h m");
    assert!(ui.dialog.is_none(), "positional add does not open a dialog");
    // /model keeps its switching focus and routes the old subcommands
    // to /provider with a status hint.
    ui.user_submit("/model add");
    assert!(ui.dialog.is_none(), "/model add opens nothing");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("/provider"),
        "migration hint shown: {joined}"
    );
}

#[test]
fn model_picker_enter_persists_default_and_cross_provider_skips_live_switch() {
    let mut ui = ui_with_models();
    ui.user_submit("/model");
    // Move to MiniMax-M3 (different provider), Enter persists.
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::plain(Key::Enter));
    let ops = ui.pending_ops().clone();
    assert!(
        !ops.iter().any(|op| matches!(op, Op::SetModel { .. })),
        "cross-provider selection must not switch live: {ops:?}"
    );
    // The picked model's effort must not leak onto the model that
    // is still live (it keeps sampling until the restart).
    assert!(
        !ops.iter().any(|op| matches!(op, Op::SetThinking { .. })),
        "cross-provider effort must not go live: {ops:?}"
    );
    assert_eq!(ui.state.thinking_effort, None);
    let view = ui.settings.get();
    assert_eq!(view.default_model.as_deref(), Some("MiniMax-M3"));
    assert_eq!(view.default_provider.as_deref(), Some("minimax"));
    assert_eq!(view.default_effort.as_deref(), Some("high"));
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("restart to switch"),
        "cross-provider default explains the restart: {joined}"
    );
}

#[test]
fn cross_provider_session_only_switch_is_rejected_with_guidance() {
    let mut ui = ui_with_models();
    ui.user_submit("/model");
    // Alt+S on the cross-provider entry: nothing can go live and
    // session-only would persist nothing, so it must be rejected
    // with a pointer at Enter instead of a "switched" claim.
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::new(
        Key::Char('s'),
        tui_engine::keys::Mods {
            ctrl: false,
            alt: true,
            shift: false,
        },
    ));
    assert!(
        ui.pending_ops().is_empty(),
        "nothing may go live cross-provider: {:?}",
        ui.pending_ops()
    );
    assert_eq!(
        ui.settings.get().default_model,
        None,
        "session-only must not persist"
    );
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        // The status line wraps at the frame width, so only match a
        // fragment short enough to survive the fold.
        joined.replace('\n', " ").contains("save it as the default"),
        "guidance points at saving a default: {joined}"
    );
}

#[test]
fn model_picker_thinking_selection_enqueues_set_thinking() {
    let mut ui = ui_with_models();
    ui.user_submit("/model");
    // Right moves the draft from the seed (off) to "low".
    ui.handle_key(KeyEvent::plain(Key::Right));
    ui.handle_key(KeyEvent::plain(Key::Enter));
    let ops = ui.pending_ops().clone();
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "low")),
        "thinking draft reaches the wire: {ops:?}"
    );
    assert_eq!(ui.state.thinking_effort.as_deref(), Some("low"));
}

#[test]
fn permissions_no_args_opens_picker_and_enter_applies() {
    let mut ui = ui();
    ui.user_submit("/permissions");
    assert!(ui.dialog.is_some(), "mode picker opens");
    ui.handle_key(KeyEvent::plain(Key::Char('1')));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.state.permission_mode, "plan");
    assert!(
        ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetPermissionMode { mode } if mode == "plan")),
    );
}

/// Bare `/permissions` must not rotate the mode behind the picker: a
/// dismissal has to leave the mode exactly as it was.
#[test]
fn permissions_bare_leaves_mode_until_picked() {
    let mut ui = ui();
    let before = ui.state.permission_mode.clone();
    ui.user_submit("/permissions");
    assert!(ui.dialog.is_some(), "mode picker opens");
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.state.permission_mode, before, "dismissal is a no-op");
    assert!(
        !ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetPermissionMode { .. })),
        "no mode op queued by the bare form",
    );
}

#[test]
fn help_opens_panel_dialog_instead_of_status_lines() {
    let mut ui = ui();
    ui.user_submit("/help");
    assert!(matches!(ui.dialog, Some(Dialog::Help(_))));
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none());
}

#[test]
fn direct_mode_shortcuts_dispatch() {
    let mut ui = ui();
    ui.user_submit("/wave");
    assert_eq!(ui.state.permission_mode, "wave");
    ui.user_submit("/auto");
    assert_eq!(ui.state.permission_mode, "auto");
}

#[test]
fn effort_command_sets_level() {
    let mut ui = ui();
    ui.user_submit("/effort high");
    assert_eq!(ui.state.thinking_effort.as_deref(), Some("high"));
    assert!(
        ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "high")),
    );
}

#[test]
fn theme_no_args_opens_picker_and_enter_applies() {
    let mut ui = ui();
    ui.user_submit("/theme");
    assert!(matches!(ui.dialog, Some(Dialog::Theme(_))), "picker opens");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Select a theme"), "{joined}");
    assert!(joined.contains("deepwave"), "{joined}");
    // Enter on the highlighted row (auto, seeded at startup) applies.
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.theme_name, "auto");
    // Quick-select digit 3 applies deepwave.
    ui.user_submit("/theme");
    ui.handle_key(KeyEvent::plain(Key::Char('3')));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.theme_name, "deepwave");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("theme switched (deepwave)"), "{joined}");
    // Restore the default: the applied deepwave install is a process
    // global, and leaving it behind races every later dark assertion.
    theme::set(theme::Theme::dark());
}

#[test]
fn effort_no_args_opens_picker_and_digit_applies() {
    let mut ui = ui_with_models();
    ui.user_submit("/effort");
    assert!(matches!(ui.dialog, Some(Dialog::Effort(_))), "picker opens");
    // Digit 2 = "low" (off seeds first).
    ui.handle_key(KeyEvent::plain(Key::Char('2')));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.state.thinking_effort.as_deref(), Some("low"));
    assert!(
        ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "low")),
    );
    // Selecting "off" clears the level (digit 1 = the seeded "off").
    ui.user_submit("/effort");
    ui.handle_key(KeyEvent::plain(Key::Char('1')));
    assert_eq!(ui.state.thinking_effort, None);
    assert!(
        ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "off")),
    );
}

#[test]
fn compact_no_args_opens_prompt_and_text_steers() {
    let mut ui = ui();
    ui.user_submit("/compact");
    assert!(matches!(ui.dialog, Some(Dialog::Prompt(_))));
    // Empty submit compacts without steering.
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert!(ui.dialog.is_none());
    assert_eq!(
        ui.pending_ops().last(),
        Some(&Op::Compact { instruction: None }),
        "empty submit compacts now"
    );
    // A typed instruction steers the summary.
    ui.user_submit("/compact");
    for c in "keep the api decisions".chars() {
        ui.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert_eq!(
        ui.pending_ops().last(),
        Some(&Op::Compact {
            instruction: Some("keep the api decisions".to_string())
        }),
        "typed text rides the op"
    );
}

#[test]
fn title_no_args_opens_prompt_and_enter_renames() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = ui();
    ui.state.home = Some(dir.path().to_path_buf());
    // A recorded session gives the rename something to land on.
    ui.submit("hello assistant");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.user_submit("/title");
    assert!(matches!(ui.dialog, Some(Dialog::Prompt(_))));
    for c in "the real topic".chars() {
        ui.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert!(ui.dialog.is_none());
    assert_eq!(
        ui.state.session_title.as_deref(),
        Some("the real topic"),
        "the prompt renames the session"
    );
}

/// `/settings` exposes the full tunable surface; cycling a row flips
/// the persisted value.
#[test]
fn settings_panel_covers_the_tunables_and_cycles_them() {
    let mut ui = ui();
    ui.user_submit("/settings");
    assert!(matches!(ui.dialog, Some(Dialog::Settings(_))));
    let frame = ui.frame(90, 30);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    for label in [
        "render user input as markdown",
        "thinking starts expanded",
        "stream the assistant draft",
        "context meter in footer",
        "rotate footer tips",
        "confirm before exit",
        "input history limit",
    ] {
        assert!(joined.contains(label), "missing row {label:?}: {joined}");
    }
    // The first row toggles markdown rendering and survives a cycle.
    let before = ui.settings.get().render_user_markdown;
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert_eq!(ui.settings.get().render_user_markdown, !before);
}
