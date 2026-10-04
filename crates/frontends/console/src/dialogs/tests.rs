//! Unit tests for the dialog components in `super`: approvals,
//! questions, the picker dialogs, the model wizard, and the
//! free-text prompt.

use super::*;
use crate::theme;
use tui_engine::keys::Mods;
use tui_engine::width::strip_ansi;

#[test]
fn approval_quick_select_and_enter() {
    theme::set(theme::Theme::dark());
    let mut dialog = ApprovalDialog::new("c1".to_string(), ApprovalKind::Exec, "npm test");
    let answer = dialog.handle_key(KeyEvent::plain(Key::Char('1')));
    match answer {
        Some(Answer::Approval { call_id, decision }) => {
            assert_eq!(call_id, "c1");
            assert_eq!(decision, WireDecision::AllowOnce);
        }
        other => panic!("expected approval: {other:?}"),
    }
}

#[test]
fn approval_escape_denies() {
    theme::set(theme::Theme::dark());
    let mut dialog = ApprovalDialog::new("c2".to_string(), ApprovalKind::Write, "write x");
    match dialog.handle_key(KeyEvent::plain(Key::Esc)) {
        Some(Answer::Approval {
            decision: WireDecision::Deny { .. },
            ..
        }) => {}
        other => panic!("expected deny: {other:?}"),
    }
}

#[test]
fn approval_selection_moves_and_enters() {
    theme::set(theme::Theme::dark());
    let mut dialog = ApprovalDialog::new("c3".to_string(), ApprovalKind::Exec, "ls");
    // One Down lands on the middle "always allow" choice.
    dialog.handle_key(KeyEvent::plain(Key::Down));
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::Approval {
            decision: WireDecision::AllowAlways,
            ..
        })
    ));
    // Two Downs land on deny.
    let mut dialog = ApprovalDialog::new("c3b".to_string(), ApprovalKind::Exec, "ls");
    dialog.handle_key(KeyEvent::plain(Key::Down));
    dialog.handle_key(KeyEvent::plain(Key::Down));
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::Approval {
            decision: WireDecision::Deny { .. },
            ..
        })
    ));
}

#[test]
fn approval_quick_select_always() {
    theme::set(theme::Theme::dark());
    let mut dialog = ApprovalDialog::new("c5".to_string(), ApprovalKind::Exec, "npm test");
    let answer = dialog.handle_key(KeyEvent::plain(Key::Char('2')));
    match answer {
        Some(Answer::Approval {
            decision: WireDecision::AllowAlways,
            ..
        }) => {}
        other => panic!("expected allow-always: {other:?}"),
    }
}

#[test]
fn question_options_and_free_text() {
    theme::set(theme::Theme::dark());
    let answer = {
        let mut dialog = QuestionDialog::new(
            "q1".to_string(),
            "Pick one",
            vec!["alpha".to_string(), "beta".to_string()],
        );
        dialog.handle_key(KeyEvent::plain(Key::Down));
        dialog.handle_key(KeyEvent::plain(Key::Enter))
    };
    assert!(matches!(
        answer,
        Some(Answer::Question { answer, .. }) if answer == "beta"
    ));

    let mut dialog = QuestionDialog::new("q2".to_string(), "Why?", Vec::new());
    for c in "because".chars() {
        dialog.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::Question { answer, .. }) if answer == "because"
    ));
}

/// Free-text typing accepts digits too: a numeric shortcut would
/// submit the named option mid-word ("2 hours" picking #2).
#[test]
fn question_free_text_accepts_digits() {
    theme::set(theme::Theme::dark());
    let mut dialog = QuestionDialog::new(
        "q3".to_string(),
        "How long?",
        vec!["one hour".to_string(), "one day".to_string()],
    );
    for c in "2 hours".chars() {
        if let Some(answer) = dialog.handle_key(KeyEvent::plain(Key::Char(c))) {
            panic!("digit '{c}' submitted mid-word: {answer:?}");
        }
    }
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::Question { answer, .. }) if answer == "2 hours"
    ));
}

#[test]
fn approval_renders_focus_frame() {
    theme::set(theme::Theme::dark());
    let mut dialog = ApprovalDialog::new("c4".to_string(), ApprovalKind::Exec, "echo hi");
    let lines = dialog.render(70);
    let joined: String = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Run this command?"), "{joined}");
    assert!(joined.contains("$ echo hi"), "{joined}");
    assert!(joined.contains("Yes, allow once"), "{joined}");
    assert!(strip_ansi(&lines[0]).starts_with('╭'));
}

#[test]
fn write_approval_colors_diff_lines() {
    theme::set(theme::Theme::dark());
    let detail = "write: src/lib.rs\n+fn added() {}\n-fn gone() {}";
    let mut dialog = ApprovalDialog::new("c5".to_string(), ApprovalKind::Write, detail);
    let lines = dialog.render(70);
    let added = lines
        .iter()
        .find(|l| strip_ansi(l).contains("+fn added() {}"))
        .expect("added row present");
    let removed = lines
        .iter()
        .find(|l| strip_ansi(l).contains("-fn gone() {}"))
        .expect("removed row present");
    // Diff rows carry their token color instead of the plain text
    // style the `$` command rows use; each row must carry the SGR
    // foreground of its own diff token (added vs removed).
    let theme = theme::current();
    let fg = |token: Token| {
        let color = theme.color(token);
        format!("38;2;{};{};{}", color.r, color.g, color.b)
    };
    let fgs = |line: &str| {
        line.split("\x1b[")
            .skip(1)
            .map(|seq| seq.split('m').next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("|")
    };
    assert!(
        fgs(added).contains(&fg(Token::DiffAdded)),
        "added row must paint DiffAdded: {:?} vs {}",
        fgs(added),
        fg(Token::DiffAdded)
    );
    assert!(
        fgs(removed).contains(&fg(Token::DiffRemoved)),
        "removed row must paint DiffRemoved: {:?} vs {}",
        fgs(removed),
        fg(Token::DiffRemoved)
    );
    // The head line keeps the `$` command shape.
    let joined: String = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("$ write: src/lib.rs"), "{joined}");
}

fn picker_entries() -> Vec<ModelEntryView> {
    vec![
        ModelEntryView {
            label: "deepseek-chat".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            effort: None,
        },
        ModelEntryView {
            label: "deepseek-reasoner".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-reasoner".to_string(),
            effort: Some("high".to_string()),
        },
        ModelEntryView {
            label: "MiniMax-M3".to_string(),
            provider: "minimax".to_string(),
            model: "MiniMax-M3".to_string(),
            effort: None,
        },
    ]
}

#[test]
fn model_picker_search_filters_and_enter_resolves() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelPickerDialog::new(
        picker_entries(),
        "deepseek-chat".to_string(),
        None,
        vec!["off".to_string(), "low".to_string(), "high".to_string()],
    );
    // Type to filter down to the reasoner entry.
    for c in "deepseek-re".chars() {
        dialog.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    match answer {
        Some(Answer::ModelSelected { label, model, .. }) => {
            assert_eq!(label, "deepseek-reasoner");
            assert_eq!(model, "deepseek-reasoner");
        }
        other => panic!("expected model selection: {other:?}"),
    }
}

#[test]
fn fuzzy_filter_ranks_lower_scores_first() {
    // `fuzzy::score` ranks lower scores higher: the adjacent match
    // ("ma") must surface before the gappy one ("ama").
    let entries = vec![
        ModelEntryView {
            label: "ama".to_string(),
            provider: "p".to_string(),
            model: "ama".to_string(),
            effort: None,
        },
        ModelEntryView {
            label: "ma".to_string(),
            provider: "p".to_string(),
            model: "ma".to_string(),
            effort: None,
        },
    ];
    assert_eq!(filter_indices(&entries, "ma"), vec![1, 0]);
}

#[test]
fn model_picker_alt_s_marks_session_only() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelPickerDialog::new(picker_entries(), "none".to_string(), None, Vec::new());
    // No thinking levels: effort stays None.
    let answer = dialog.handle_key(KeyEvent::new(
        Key::Char('s'),
        Mods {
            ctrl: false,
            alt: true,
            shift: false,
        },
    ));
    assert!(matches!(
        answer,
        Some(Answer::ModelSelected {
            session_only: true,
            effort: None,
            ..
        })
    ));
}

#[test]
fn model_picker_thinking_row_switches_with_arrows() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelPickerDialog::new(
        picker_entries(),
        "deepseek-chat".to_string(),
        Some("low".to_string()),
        vec!["off".to_string(), "low".to_string(), "high".to_string()],
    );
    // The highlighted entry (deepseek-chat) has no effort default;
    // the draft seeds from the live effort ("low"). Right → "high".
    dialog.handle_key(KeyEvent::plain(Key::Right));
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::ModelSelected {
            effort: Some(level),
            ..
        }) if level == "high"
    ));
    // Left twice cycles around: high → low → off (as None).
    dialog.handle_key(KeyEvent::plain(Key::Left));
    dialog.handle_key(KeyEvent::plain(Key::Left));
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::ModelSelected { effort: None, .. })
    ));
}

#[test]
fn model_picker_tab_cycles_providers() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelPickerDialog::new(picker_entries(), "none".to_string(), None, Vec::new());
    // Tab once moves to the "deepseek" tab; only its entries show.
    dialog.handle_key(KeyEvent::plain(Key::Tab));
    let lines = dialog.render(80);
    let joined: String = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("[deepseek]"), "{joined}");
    assert!(
        !joined.contains("MiniMax-M3"),
        "other provider hidden: {joined}"
    );
}

// ---- the /provider wizard ----

/// Type into the wizard's current text field and advance.
fn wizard_text(dialog: &mut ModelWizardDialog, text: &str) {
    for c in text.chars() {
        dialog.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    dialog.handle_key(KeyEvent::plain(Key::Enter));
}

/// The wizard resolves a full MiniMax-style spec: presets for the
/// sizes, every thinking level ticked, the key stored as an env
/// reference.
#[test]
fn wizard_resolves_a_full_model() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelWizardDialog::new(None);
    macro_rules! trace {
        ($dialog:expr, $key:expr) => {{
            $dialog.handle_key($key);
            eprintln!("after {:?} -> step {:?}", $key, $dialog.step);
        }};
    }
    wizard_text(&mut dialog, "minimax");
    trace!(dialog, KeyEvent::plain(Key::Enter));
    // dialog.handle_key(KeyEvent::plain(Key::Enter)); // api: anthropic-messages
    wizard_text(&mut dialog, "https://api.minimax.chat");
    wizard_text(&mut dialog, "env:MINIMAX_API_KEY");
    wizard_text(&mut dialog, "MiniMax-M3.1-Flash-Preview");
    wizard_text(&mut dialog, "mini-flash");
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // context: 256k preset
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // max output: 128k preset
    // Thinking: tick all five presets.
    for _ in 0..5 {
        dialog.handle_key(KeyEvent::plain(Key::Char(' ')));
        dialog.handle_key(KeyEvent::plain(Key::Down));
    }
    dialog.handle_key(KeyEvent::plain(Key::Enter));
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // input: text ticked
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // output: text ticked
    let answer = dialog.handle_key(KeyEvent::plain(Key::Char('s')));
    let Some(Answer::ModelForm { entries }) = answer else {
        panic!("wizard did not resolve: {answer:?}");
    };
    assert_eq!(entries.len(), 1);
    let (alias, spec) = &entries[0];
    assert_eq!(alias, "mini-flash");
    assert_eq!(spec.provider, "minimax");
    assert_eq!(spec.model, "MiniMax-M3.1-Flash-Preview");
    assert_eq!(spec.context_window, Some(256 * 1024));
    assert_eq!(spec.max_output, Some(128 * 1024));
    assert!(spec.reasoning.enabled);
    assert_eq!(
        spec.reasoning.variants,
        vec!["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(spec.reasoning.default.as_deref(), Some("low"));
    assert_eq!(spec.api_key_env.as_deref(), Some("MINIMAX_API_KEY"));
    assert_eq!(spec.modalities.input, vec!["text"]);
    assert_eq!(spec.modalities.output, vec!["text"]);
}

/// The review page stages a model and resets the per-model fields,
/// so several models share one provider pass; save submits both.
#[test]
fn wizard_stages_two_models_on_one_provider() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelWizardDialog::new(None);
    wizard_text(&mut dialog, "minimax");
    dialog.handle_key(KeyEvent::plain(Key::Enter));
    wizard_text(&mut dialog, "https://api.minimax.chat");
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // blank api key
    wizard_text(&mut dialog, "MiniMax-M3.1-Flash-Preview");
    wizard_text(&mut dialog, "mini-flash");
    // Five Enters: context 256k, output 128k, thinking, input, output.
    for _ in 0..5 {
        dialog.handle_key(KeyEvent::plain(Key::Enter));
    }
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // review: stage
    assert_eq!(dialog.entries.len(), 1);
    assert!(dialog.model_id.is_empty(), "model fields reset");
    assert_eq!(dialog.provider, "minimax", "provider kept");
    // Second model on the same provider: model, alias, context,
    // output, thinking, input, output = seven Enters to review.
    wizard_text(&mut dialog, "MiniMax-M2.5");
    wizard_text(&mut dialog, "mini-old");
    for _ in 0..7 {
        dialog.handle_key(KeyEvent::plain(Key::Enter));
    }
    // Esc with staged entries submits them.
    let answer = dialog.handle_key(KeyEvent::plain(Key::Esc));
    let Some(Answer::ModelForm { entries }) = answer else {
        panic!("wizard did not resolve: {answer:?}");
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].0, "mini-old");
    assert_eq!(entries[1].1.provider, "minimax");
}

/// A custom context size is typed after picking the custom row and
/// parses through plain digit counts.
#[test]
fn wizard_accepts_a_custom_context_size() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelWizardDialog::new(None);
    wizard_text(&mut dialog, "p");
    dialog.handle_key(KeyEvent::plain(Key::Enter));
    wizard_text(&mut dialog, "https://h");
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // -> model id
    wizard_text(&mut dialog, "m"); // model id
    wizard_text(&mut dialog, "a"); // alias
    // Now on the context step: three Downs land on the custom row.
    dialog.handle_key(KeyEvent::plain(Key::Down));
    dialog.handle_key(KeyEvent::plain(Key::Down));
    dialog.handle_key(KeyEvent::plain(Key::Down)); // custom...
    dialog.handle_key(KeyEvent::plain(Key::Enter));
    for c in "1000000".chars() {
        dialog.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    dialog.handle_key(KeyEvent::plain(Key::Enter));
    // Fast-forward the rest and save.
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // max output 128k
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // thinking
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // input
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // output
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // review: stage
    // Esc with staged entries submits them.
    let answer = dialog.handle_key(KeyEvent::plain(Key::Esc));
    let Some(Answer::ModelForm { entries }) = answer else {
        panic!("wizard did not resolve: {answer:?}");
    };
    assert_eq!(entries[0].1.context_window, Some(1_000_000));
}

/// Saving without the required identity fields keeps the wizard
/// open with the error named.
#[test]
fn wizard_validates_before_saving() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelWizardDialog::new(None);
    // Straight to the review page with everything blank.
    dialog.step = WizardStep::Review;
    let answer = dialog.handle_key(KeyEvent::plain(Key::Char('s')));
    assert!(answer.is_none(), "blank wizard must not resolve");
    assert_eq!(dialog.error.as_deref(), Some("model name is required"));
}

/// Left and right cycle the API dialect; a provider preset seeds
/// the first four steps.
#[test]
fn wizard_cycles_api_and_accepts_presets() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelWizardDialog::new(None);
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // provider blank -> api
    dialog.handle_key(KeyEvent::plain(Key::Right));
    assert_eq!(dialog.api, 1);
    dialog.handle_key(KeyEvent::plain(Key::Left));
    dialog.handle_key(KeyEvent::plain(Key::Left));
    assert_eq!(dialog.api, 2);

    let preset = crate::ui::ProviderPreset {
        provider: "bigmodel".to_string(),
        api: "openai-chat".to_string(),
        base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
        api_key_env: Some("ZHIPU_API_KEY".to_string()),
    };
    let dialog = ModelWizardDialog::new(Some(preset));
    assert_eq!(dialog.provider, "bigmodel");
    assert_eq!(dialog.api, 1);
    assert_eq!(dialog.base_url, "https://open.bigmodel.cn/api/paas/v4");
    assert_eq!(dialog.api_key, "ZHIPU_API_KEY");
}

/// The settings panel cycles both directions: Left walks each row
/// back down instead of sticking (the tool-display row lands on
/// names, the history-limit row walks 1000 → 500 → default).
#[test]
fn settings_cycle_walks_both_directions() {
    theme::set(theme::Theme::dark());
    let settings = crate::settings::SharedSettings::without_persistence(Default::default());
    let mut dialog = SettingsDialog::new(settings.clone());
    use crate::settings::ToolDisplay;
    // Row 2: tool call display (defaults to summary). Left lands on
    // names; left again rests; three rights wrap back around.
    dialog.handle_key(KeyEvent::plain(Key::Down));
    dialog.handle_key(KeyEvent::plain(Key::Left));
    assert_eq!(settings.get().tool_display, ToolDisplay::Names);
    dialog.handle_key(KeyEvent::plain(Key::Left));
    assert_eq!(settings.get().tool_display, ToolDisplay::Names);
    for _ in 0..3 {
        dialog.handle_key(KeyEvent::plain(Key::Right));
    }
    assert_eq!(settings.get().tool_display, ToolDisplay::Names);
    // Row 10: input history limit. Two rights reach 1000; two lefts
    // walk back down to the default.
    for _ in 0..8 {
        dialog.handle_key(KeyEvent::plain(Key::Down));
    }
    dialog.handle_key(KeyEvent::plain(Key::Right));
    dialog.handle_key(KeyEvent::plain(Key::Right));
    assert_eq!(settings.get().history_limit, 1000);
    dialog.handle_key(KeyEvent::plain(Key::Left));
    assert_eq!(settings.get().history_limit, 500);
    dialog.handle_key(KeyEvent::plain(Key::Left));
    assert_eq!(settings.get().history_limit, 0);
}

/// Esc leaves the wizard from the choice and multiselect steps too:
/// their components previously swallowed it against the on-screen
/// "esc cancel" hint. Esc inside a custom-entry field still only
/// closes the entry.
#[test]
fn wizard_esc_leaves_component_steps() {
    theme::set(theme::Theme::dark());
    let mut dialog = ModelWizardDialog::new(None);
    wizard_text(&mut dialog, "p");
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // api -> base url
    wizard_text(&mut dialog, "https://h");
    dialog.handle_key(KeyEvent::plain(Key::Enter)); // -> model id
    wizard_text(&mut dialog, "m");
    wizard_text(&mut dialog, "a"); // alias -> context (a SizeChoice step)
    let answer = dialog.handle_key(KeyEvent::plain(Key::Esc));
    assert!(
        matches!(answer, Some(Answer::Dismissed)),
        "esc leaves the context step: {answer:?}"
    );
    // A multiselect step: Esc inside the custom-entry field closes
    // the entry; Esc outside it leaves the wizard.
    let mut dialog = ModelWizardDialog::new(None);
    dialog.step = WizardStep::Thinking;
    dialog.handle_key(KeyEvent::plain(Key::Char('+')));
    assert!(
        dialog.handle_key(KeyEvent::plain(Key::Esc)).is_none(),
        "esc inside the custom entry only closes it"
    );
    let answer = dialog.handle_key(KeyEvent::plain(Key::Esc));
    assert!(matches!(answer, Some(Answer::Dismissed)));
}

/// The provider opening list picks an existing provider (Some) or
/// the add-new row (None).
#[test]
fn provider_picker_picks_existing_or_new() {
    theme::set(theme::Theme::dark());
    let mut dialog = ProviderPickerDialog::new(vec![
        ("bigmodel".to_string(), 3),
        ("minimax".to_string(), 1),
    ]);
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::ProviderPicked { name: Some(name) }) if name == "bigmodel"
    ));
    dialog.handle_key(KeyEvent::plain(Key::Down));
    dialog.handle_key(KeyEvent::plain(Key::Down)); // the add row
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::ProviderPicked { name: None })
    ));
}

/// The list window slides with the selection: a selection past the
/// fixed page still renders with its marker, and the collapse line
/// tracks what is left below the window.
#[test]
fn model_picker_window_slides_with_selection() {
    theme::set(theme::Theme::dark());
    let entries: Vec<ModelEntryView> = (0..12)
        .map(|i| ModelEntryView {
            label: format!("model-{i:02}"),
            provider: "p".to_string(),
            model: format!("model-{i:02}"),
            effort: None,
        })
        .collect();
    let mut dialog = ModelPickerDialog::new(entries, "absent".to_string(), None, Vec::new());
    for _ in 0..9 {
        dialog.handle_key(KeyEvent::plain(Key::Down));
    }
    assert_eq!(dialog.selected, 9);
    let joined: String = dialog
        .render(80)
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("❯ model-09"), "selection visible: {joined}");
    assert!(joined.contains("▼ 2 more"), "collapse tracks: {joined}");
}

#[test]
fn permission_picker_digits_apply() {
    theme::set(theme::Theme::dark());
    let mut dialog = PermissionPickerDialog::new("auto");
    let answer = dialog.handle_key(KeyEvent::plain(Key::Char('3')));
    assert!(matches!(
        answer,
        Some(Answer::PermissionSelected { mode }) if mode == "wave"
    ));
}

#[test]
fn help_panel_scrolls_and_closes() {
    theme::set(theme::Theme::dark());
    let lines = (0..40).map(|i| format!("line {i}")).collect();
    let mut panel = HelpPanel::new(lines);
    panel.handle_key(KeyEvent::plain(Key::PageDown));
    let rendered = panel.render(80);
    let joined: String = rendered
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("40/40 lines"), "{joined}");
    // At the last page the panel is pinned: further scrolling neither
    // shrinks the frame nor changes the visible window.
    for key in [Key::Down, Key::PageDown, Key::Down, Key::Down] {
        panel.handle_key(KeyEvent::plain(key));
    }
    assert_eq!(
        panel
            .render(80)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>(),
        rendered.iter().map(|l| strip_ansi(l)).collect::<Vec<_>>(),
        "render must stay stable at the bottom"
    );
    assert!(
        panel.handle_key(KeyEvent::plain(Key::Char('q'))).is_some(),
        "q closes the panel"
    );
}

#[test]
fn session_picker_searches_and_resumes() {
    theme::set(theme::Theme::dark());
    let rows = vec![
        SessionRow {
            id: "id-refactor".to_string(),
            title: "refactor the runner".to_string(),
            cwd: "/tmp/a".to_string(),
            age: "5m ago".to_string(),
            turns: 3,
        },
        SessionRow {
            id: "id-docs".to_string(),
            title: "write docs".to_string(),
            cwd: "/tmp/b".to_string(),
            age: "1h ago".to_string(),
            turns: 1,
        },
    ];
    let mut dialog = SessionPickerDialog::new(rows, "/tmp/a");
    // cwd scope hides the /tmp/b session until Ctrl+A toggles it.
    dialog.handle_key(KeyEvent::new(
        Key::Char('a'),
        Mods {
            ctrl: true,
            alt: false,
            shift: false,
        },
    ));
    for c in "docs".chars() {
        dialog.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(
        answer,
        Some(Answer::ResumeSession { id }) if id == "id-docs"
    ));
}

fn user_entry(text: &str) -> crate::state::DialogueEntry {
    crate::state::DialogueEntry {
        from_user: true,
        text: text.to_string(),
    }
}

fn assistant_entry(text: &str) -> crate::state::DialogueEntry {
    crate::state::DialogueEntry {
        from_user: false,
        text: text.to_string(),
    }
}

#[test]
fn undo_picker_lists_newest_first_with_stable_distances() {
    let history = vec![
        user_entry("one"),
        assistant_entry("a1"),
        user_entry("two"),
        assistant_entry("a2"),
        // A blank first line hides the label but the turn still
        // counts: the kernel rewinds it like any other user turn.
        user_entry("  \nbody"),
        assistant_entry("a3"),
        user_entry("four"),
    ];
    let picker = UndoPickerDialog::new(&history);
    let labels: Vec<&str> = picker.rows.iter().map(|row| row.label.as_str()).collect();
    assert_eq!(labels, vec!["four", "two", "one"]);
    let turns: Vec<u32> = picker.rows.iter().map(|row| row.turns).collect();
    // "two" sits two user turns back even though the blank turn in
    // between has no row of its own ("one" sits three back).
    assert_eq!(turns, vec![1, 3, 4]);
    assert!(!picker.is_empty());
}

#[test]
fn undo_picker_caps_rows_and_reports_empty() {
    let long: Vec<crate::state::DialogueEntry> =
        (0..12).map(|i| user_entry(&format!("turn {i}"))).collect();
    let picker = UndoPickerDialog::new(&long);
    assert_eq!(picker.rows.len(), MAX_UNDO_ROWS);
    assert_eq!(picker.rows[0].turns, 1);
    assert_eq!(picker.rows[MAX_UNDO_ROWS - 1].turns, MAX_UNDO_ROWS as u32);

    let empty = vec![user_entry("   "), assistant_entry("only noise")];
    assert!(UndoPickerDialog::new(&empty).is_empty());
    assert!(UndoPickerDialog::new(&[]).is_empty());
}

#[test]
fn theme_picker_marks_current_and_applies_builtins() {
    theme::set(theme::Theme::dark());
    let mut dialog = ThemePickerDialog::new("deepwave", vec![("sunset".to_string(), None)]);
    assert_eq!(dialog.selected, 2, "the live theme preselects");
    let lines = dialog.render(70);
    let joined: String = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Select a theme"), "{joined}");
    assert!(joined.contains("← current"), "{joined}");
    assert!(joined.contains("the teal ocean identity"), "{joined}");
    // Custom rows describe themselves once highlighted.
    dialog.handle_key(KeyEvent::plain(Key::Down));
    dialog.handle_key(KeyEvent::plain(Key::Down));
    let lines = dialog.render(70);
    let joined: String = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("custom theme"), "{joined}");
    // Digit quick-select resolves the matching builtin.
    match dialog.handle_key(KeyEvent::plain(Key::Char('4'))) {
        Some(Answer::ThemeSelected { name }) => assert_eq!(name, "light"),
        other => panic!("expected a theme selection: {other:?}"),
    }
    // Arrows move with wrap-around; Enter resolves the cursor row.
    let mut dialog = ThemePickerDialog::new("auto", Vec::new());
    dialog.handle_key(KeyEvent::plain(Key::Up));
    let answer = dialog.handle_key(KeyEvent::plain(Key::Enter));
    match answer {
        Some(Answer::ThemeSelected { name }) => assert_eq!(name, "light"),
        other => panic!("expected a theme selection: {other:?}"),
    }
}

#[test]
fn effort_picker_seeds_off_and_resolves_levels() {
    theme::set(theme::Theme::dark());
    let mut dialog =
        EffortPickerDialog::new(Some("high"), &["low".to_string(), "high".to_string()]);
    assert_eq!(dialog.levels, vec!["off", "low", "high"], "off seeds first");
    assert_eq!(dialog.selected, 2, "the live level preselects");
    match dialog.handle_key(KeyEvent::plain(Key::Enter)) {
        Some(Answer::EffortSelected { level: Some(level) }) => assert_eq!(level, "high"),
        other => panic!("expected an effort selection: {other:?}"),
    }
    // Without provider levels the picker collapses to off (None).
    let mut dialog = EffortPickerDialog::new(None, &[]);
    assert_eq!(dialog.levels, vec!["off"]);
    match dialog.handle_key(KeyEvent::plain(Key::Enter)) {
        Some(Answer::EffortSelected { level: None }) => {}
        other => panic!("expected off: {other:?}"),
    }
}

#[test]
fn prompt_dialog_edits_prefilled_text_and_resolves() {
    theme::set(theme::Theme::dark());
    let mut dialog = PromptDialog::new(
        "Session title",
        PromptPurpose::SessionTitle,
        "old name",
        "hint",
    );
    // Render shows the prefilled value under the framed title.
    let lines = dialog.render(70);
    let joined: String = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("old name"), "{joined}");
    // Backspace edits, typed characters append, Enter submits.
    dialog.handle_key(KeyEvent::plain(Key::Backspace));
    for c in "re".chars() {
        dialog.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    match dialog.handle_key(KeyEvent::plain(Key::Enter)) {
        Some(Answer::Prompt { purpose, value }) => {
            assert_eq!(purpose, PromptPurpose::SessionTitle);
            assert_eq!(value, "old namre");
        }
        other => panic!("expected a prompt answer: {other:?}"),
    }
    // Esc dismisses.
    assert_eq!(
        dialog.handle_key(KeyEvent::plain(Key::Esc)),
        Some(Answer::Dismissed)
    );
}
