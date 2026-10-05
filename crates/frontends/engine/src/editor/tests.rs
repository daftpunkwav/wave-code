//! Tests for the editor component ([`crate::editor::Editor`]).

use super::*;
use crate::keys::Mods;
use crate::width::strip_ansi;

fn editor() -> Editor {
    Editor::new(EditorStyle::default())
}

fn type_string(editor: &mut Editor, text: &str) {
    for c in text.chars() {
        let event = KeyEvent::plain(Key::Char(c));
        let _ = editor.handle_key(event);
    }
}

#[test]
fn typing_and_submit_roundtrip() {
    let mut editor = editor();
    type_string(&mut editor, "hello");
    assert_eq!(editor.text(), "hello");
    let action = editor.handle_key(KeyEvent::plain(Key::Enter));
    assert_eq!(action, EditorAction::Submit("hello".to_string()));
    assert!(editor.is_empty());
}

/// A style painted only via one distinctive SGR attribute so tests
/// can assert which span it wrapped.
fn marked_style() -> EditorStyle {
    EditorStyle {
        slash_command: Style::new().bold(),
        shell_command: Style::new().underline(),
        ..EditorStyle::default()
    }
}

/// Plain-text view of one editor body row (borders kept, cursor
/// marker stripped — the screen layer removes it at draw time).
fn plain_row(rows: &[String], index: usize) -> String {
    strip_ansi(&rows[index]).replace(CURSOR_MARKER, "")
}

#[test]
fn leading_slash_token_is_painted() {
    let mut editor = Editor::new(marked_style());
    type_string(&mut editor, "/help now");
    let rows = editor.render_box(40, 4);
    let body = &rows[1];
    assert!(body.contains("\x1b[1m"), "bold slash token: {body:?}");
    assert!(!body.contains("\x1b[4m"), "no shell style: {body:?}");
    assert!(plain_row(&rows, 1).contains("❯ /help now"), "{body:?}");
}

#[test]
fn leading_shell_token_is_painted() {
    let mut editor = Editor::new(marked_style());
    type_string(&mut editor, "!ls -la");
    let rows = editor.render_box(40, 4);
    let body = &rows[1];
    assert!(body.contains("\x1b[4m"), "underlined shell token: {body:?}");
    assert!(!body.contains("\x1b[1m"), "no slash style: {body:?}");
    assert!(plain_row(&rows, 1).contains("❯ !ls -la"), "{body:?}");
}

#[test]
fn plain_text_has_no_token_paint() {
    let mut editor = Editor::new(marked_style());
    type_string(&mut editor, "hello world");
    let rows = editor.render_box(40, 4);
    let body = &rows[1];
    assert!(
        !body.contains("\x1b[1m") && !body.contains("\x1b[4m"),
        "unpainted body: {body:?}"
    );
}

#[test]
fn token_paint_stays_on_first_row() {
    let mut editor = Editor::new(marked_style());
    type_string(&mut editor, "/cmd");
    let _ = editor.handle_key(KeyEvent::new(
        Key::Enter,
        Mods {
            shift: true,
            ..Mods::NONE
        },
    ));
    type_string(&mut editor, "more");
    let rows = editor.render_box(40, 6);
    let body = &rows[2];
    assert!(
        !body.contains("\x1b[1m"),
        "continuation rows stay unpainted: {body:?}"
    );
    assert!(plain_row(&rows, 2).contains("more"), "{body:?}");
}

#[test]
fn empty_enter_passes_through() {
    let mut editor = editor();
    assert_eq!(
        editor.handle_key(KeyEvent::plain(Key::Enter)),
        EditorAction::Passthrough
    );
}

#[test]
fn shift_enter_inserts_newline() {
    let mut editor = editor();
    type_string(&mut editor, "ab");
    let _ = editor.handle_key(KeyEvent::new(
        Key::Enter,
        Mods {
            shift: true,
            ..Mods::NONE
        },
    ));
    type_string(&mut editor, "cd");
    assert_eq!(editor.text(), "ab\ncd");
}

#[test]
fn backslash_enter_exchanges_for_newline() {
    let mut editor = editor();
    type_string(&mut editor, "ab\\");
    let action = editor.handle_key(KeyEvent::plain(Key::Enter));
    assert_eq!(action, EditorAction::Handled);
    type_string(&mut editor, "cd");
    assert_eq!(editor.text(), "abcd".replace("abcd", "ab\ncd"));
}

#[test]
fn backspace_joins_lines_and_deletes() {
    let mut editor = editor();
    editor.insert_text("ab\n");
    editor.insert_text("cd");
    assert_eq!(editor.text(), "ab\ncd");
    editor.handle_key(KeyEvent::plain(Key::Backspace));
    assert_eq!(editor.text(), "ab\nc");
    editor.handle_key(KeyEvent::plain(Key::Backspace));
    assert_eq!(editor.text(), "ab\n");
    editor.handle_key(KeyEvent::plain(Key::Backspace));
    assert_eq!(editor.text(), "ab", "join with previous line");
}

#[test]
fn undo_restores_buffer() {
    let mut editor = editor();
    type_string(&mut editor, "hello");
    editor.handle_key(KeyEvent::new(Key::Char('-'), Mods::CTRL));
    assert_eq!(editor.text(), "hell");
}

#[test]
fn kill_and_yank_roundtrip() {
    let mut editor = editor();
    type_string(&mut editor, "hello world");
    editor.handle_key(KeyEvent::new(Key::Char('a'), Mods::CTRL));
    editor.handle_key(KeyEvent::new(Key::Char('k'), Mods::CTRL));
    assert_eq!(editor.text(), "");
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
    assert_eq!(editor.text(), "hello world");
}

#[test]
fn ctrl_w_kills_word_back() {
    let mut editor = editor();
    type_string(&mut editor, "hello world ");
    editor.handle_key(KeyEvent::new(Key::Char('w'), Mods::CTRL));
    assert_eq!(editor.text(), "hello ");
}

#[test]
fn consecutive_same_position_kills_merge() {
    let mut editor = editor();
    type_string(&mut editor, "one two three");
    editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
    assert_eq!(editor.col, 8, "start of the last word");
    // Two kills starting at the same cursor position accumulate
    // into one yankable entry.
    editor.handle_key(KeyEvent::new(Key::Char('k'), Mods::CTRL));
    assert_eq!(editor.text(), "one two ");
    editor.handle_key(KeyEvent::new(Key::Char('w'), Mods::CTRL));
    assert_eq!(editor.text(), "one ");
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
    assert_eq!(editor.text(), "one two three", "merged kill yanks as one");
}

#[test]
fn kills_from_different_positions_split_entries() {
    let mut editor = editor();
    type_string(&mut editor, "alpha beta");
    editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
    editor.handle_key(KeyEvent::new(Key::Char('u'), Mods::CTRL));
    assert_eq!(editor.text(), "beta");
    // A kill from a different position starts a new ring entry: the
    // next yank returns the newest kill, Alt+Y cycles back.
    editor.handle_key(KeyEvent::new(Key::Char('e'), Mods::CTRL));
    editor.handle_key(KeyEvent::new(Key::Char('w'), Mods::CTRL));
    assert_eq!(editor.text(), "");
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
    assert_eq!(editor.text(), "beta");
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
    assert_eq!(editor.text(), "alpha ");
}

#[test]
fn alt_y_cycles_through_the_ring() {
    let mut editor = editor();
    type_string(&mut editor, "alpha beta");
    // Kill "alpha " backward, then part of "beta" forward: two
    // entries (the cursor must sit before the line end for the
    // forward kill to take text).
    editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
    editor.handle_key(KeyEvent::new(Key::Char('u'), Mods::CTRL));
    assert_eq!(editor.text(), "beta");
    editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
    editor.handle_key(KeyEvent::new(Key::Right, Mods::NONE));
    editor.handle_key(KeyEvent::new(Key::Right, Mods::NONE));
    editor.handle_key(KeyEvent::new(Key::Char('k'), Mods::CTRL));
    assert_eq!(editor.text(), "be");
    // Clear the remaining draft (not a kill: the ring keeps its
    // three entries).
    editor.handle_key(KeyEvent::new(Key::Char('e'), Mods::CTRL));
    editor.handle_key(KeyEvent::plain(Key::Backspace));
    editor.handle_key(KeyEvent::plain(Key::Backspace));
    assert_eq!(editor.text(), "");
    // Yank inserts the newest kill; Alt+Y cycles both entries and
    // wraps.
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
    assert_eq!(editor.text(), "ta");
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
    assert_eq!(editor.text(), "alpha ");
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
    assert_eq!(editor.text(), "ta");
}

#[test]
fn edit_between_yank_and_alt_y_disarms_the_cycle() {
    let mut editor = editor();
    type_string(&mut editor, "ab");
    editor.handle_key(KeyEvent::new(Key::Char('a'), Mods::CTRL));
    editor.handle_key(KeyEvent::new(Key::Char('k'), Mods::CTRL));
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
    assert_eq!(editor.text(), "ab");
    // Typing after the yank invalidates the recorded span.
    editor.handle_key(KeyEvent::plain(Key::Char('X')));
    editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
    assert_eq!(editor.text(), "abX", "stale yank-pop must not rewrite");
}

#[test]
fn alt_b_and_alt_f_jump_words() {
    let mut editor = editor();
    type_string(&mut editor, "hello world");
    // Alt+F twice lands after "hello world"; Alt+B once lands
    // between the words.
    editor.handle_key(KeyEvent::new(Key::Char('f'), Mods::ALT));
    editor.handle_key(KeyEvent::new(Key::Char('f'), Mods::ALT));
    assert_eq!(editor.col, 11, "after the last word");
    editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
    assert_eq!(editor.col, 6, "start of the last word");
    // Insertion at the jumped position proves the cursor moved.
    editor.handle_key(KeyEvent::plain(Key::Char('X')));
    assert_eq!(editor.text(), "hello Xworld");
}

#[test]
fn history_stores_expanded_paste_body() {
    let mut editor = editor();
    let big = "secret\n".repeat(12);
    editor.insert_paste(&big);
    let action = editor.handle_key(KeyEvent::plain(Key::Enter));
    assert!(matches!(action, EditorAction::Submit(_)));
    // The recalled entry must carry the paste body, not the marker.
    let entries = editor.history_entries();
    assert_eq!(entries.first().map(String::as_str), Some(big.as_str()));
    assert!(!entries[0].contains("[paste #"));
}

#[test]
fn stale_trigger_does_not_panic_on_accept() {
    let mut editor = editor();
    editor.set_provider(Box::new(crate::autocomplete::FuzzyProvider::new(vec![
        crate::autocomplete::Completion {
            label: "src/main.rs".into(),
            description: None,
            insert: "src/main.rs ".into(),
        },
    ])));
    editor.insert_text("x @src");
    assert!(editor.popup_open());
    // Move the cursor left of the trigger without typing: the popup
    // goes stale, and accepting must close it, never invert a range.
    editor.handle_key(KeyEvent::plain(Key::Home));
    let action = editor.handle_key(KeyEvent::plain(Key::Tab));
    assert_eq!(action, EditorAction::Handled);
    assert!(!editor.popup_open(), "stale popup dropped");
    assert_eq!(editor.text(), "x @src", "buffer untouched");
}

#[test]
fn history_recall_with_draft() {
    let mut editor = editor();
    editor.remember_history("first");
    editor.remember_history("second");
    type_string(&mut editor, "draft");
    assert!(editor.history_previous());
    assert_eq!(editor.text(), "second");
    assert!(editor.history_previous());
    assert_eq!(editor.text(), "first");
    assert!(editor.history_next());
    assert_eq!(editor.text(), "second");
    assert!(editor.history_next());
    assert_eq!(editor.text(), "draft", "draft restored at end");
    assert!(!editor.history_next());
}

#[test]
fn history_cap_beyond_the_builtin_survives_submits() {
    let mut editor = editor();
    editor.set_history_cap(120);
    for i in 0..110 {
        editor.remember_history(&format!("entry-{i}"));
    }
    assert_eq!(editor.history_entries().len(), 110, "cap is honored");
    // Cap 0 falls back to the built-in default of 100.
    editor.set_history_cap(0);
    for i in 110..210 {
        editor.remember_history(&format!("entry-{i}"));
    }
    assert_eq!(editor.history_entries().len(), 100);
}

#[test]
fn load_history_resets_recall_state() {
    let mut editor = editor();
    editor.remember_history("old-1");
    editor.remember_history("old-2");
    assert!(editor.history_previous());
    // Loading a fresh (shorter) list must drop the stale position,
    // or the next Next would index past the new entries.
    editor.load_history(vec!["new-1".into(), "new-2".into(), "new-3".into()]);
    assert_eq!(editor.text(), "old-2", "buffer untouched by the load");
    assert!(!editor.history_next(), "no recall in progress");
    assert!(editor.history_previous());
    assert_eq!(editor.text(), "new-3", "browsing starts at the newest");
}

#[test]
fn large_paste_collapses_and_expands() {
    let mut editor = editor();
    let big = "x".repeat(1500);
    editor.insert_paste(&big);
    assert!(
        editor.text().contains("[paste #1 1500 chars]"),
        "marker in buffer: {:?}",
        editor.text()
    );
    assert_eq!(editor.expand_markers(editor.text().as_str()), big);
}

#[test]
fn multiline_paste_collapses_by_lines() {
    let mut editor = editor();
    let big = "line\n".repeat(15);
    editor.insert_paste(&big);
    assert!(editor.text().contains("[paste #1 +15 lines]"));
}

/// Clipboard text can carry ANSI/OSC sequences: they are stripped
/// at insert time (small and collapsed pastes alike) so the editor
/// rows never render them into the terminal. Newlines stay; tabs
/// expand to four spaces (their zero display width would desync
/// wrap and cursor math from a tab-stop-advancing terminal).
#[test]
fn paste_strips_escape_sequences_keeps_layout() {
    let mut ed = editor();
    ed.insert_paste("a\x1b[2Jb\tn");
    assert_eq!(ed.text(), "ab    n");
    let mut ed = editor();
    let big = format!("x\x1b]0;t\x07y\n{}", "line\n".repeat(15));
    ed.insert_paste(&big);
    let expanded = ed.expand_markers(ed.text().as_str());
    assert!(!expanded.contains('\x1b'), "{expanded:?}");
    assert!(expanded.contains("xy"));
}

#[test]
fn render_draws_frame_and_prompt() {
    let mut editor = editor();
    type_string(&mut editor, "hi");
    let rows = editor.render_box(20, 24);
    assert_eq!(strip_ansi(&rows[0]), "╭──────────────────╮");
    assert_eq!(
        strip_ansi(&rows[1]),
        "│ ❯ hi             │",
        "cursor at end: {:?}",
        rows[1]
    );
    assert_eq!(strip_ansi(&rows[2]), "╰──────────────────╯");
}

#[test]
fn render_embeds_cursor_marker() {
    let mut editor = editor();
    type_string(&mut editor, "abc");
    editor.handle_key(KeyEvent::plain(Key::Left));
    let rows = editor.render_box(20, 24);
    assert!(
        rows[1].contains(CURSOR_MARKER),
        "marker present in: {:?}",
        rows[1]
    );
    assert_eq!(width::width(&strip_ansi(&rows[1])), 20);
}

#[test]
fn long_input_wraps_into_visual_rows() {
    let mut editor = editor();
    type_string(&mut editor, "aaaaaaaaaaaaaaaaaaaaaa"); // 22 chars
    let rows = editor.render_box(20, 24);
    // Body budget: 20 - 2 - 2 - 2 = 14 → two visual rows + borders.
    assert_eq!(rows.len(), 4, "wrapped into 2 body rows: {rows:?}");
    assert!(strip_ansi(&rows[1]).starts_with("│ ❯ aaaaaaaaa"));
}

#[test]
fn vertical_movement_walks_visual_rows() {
    let mut editor = editor();
    // 20 a's wrap into two visual rows at body budget 14 (width 20).
    editor.insert_text("aaaaaaaaaaaaaaaaaaaa\nb");
    let _ = editor.render_box(20, 24);
    // insert_text leaves the cursor at the end of line 1.
    assert!(editor.move_up(), "moves to wrapped second row");
    assert!(editor.move_up(), "moves to the first visual row");
    assert!(!editor.move_up(), "first visual row falls through");
    assert!(editor.move_down(), "moves into wrapped second row");
    assert!(editor.move_down(), "moves to logical line 1");
    assert!(!editor.move_down(), "stays on last row");
}

#[test]
fn popup_opens_on_slash_and_accepts() {
    let mut editor = editor();
    editor.set_provider(Box::new(crate::autocomplete::FuzzyProvider::new(vec![
        crate::autocomplete::Completion {
            label: "help".into(),
            description: None,
            insert: "/help ".into(),
        },
    ])));
    type_string(&mut editor, "/h");
    assert!(editor.popup_open(), "popup after slash token");
    let action = editor.handle_key(KeyEvent::plain(Key::Enter));
    assert_eq!(action, EditorAction::Submit("/help ".to_string()));
}

#[test]
fn popup_navigation_and_escape() {
    let mut editor = editor();
    editor.set_provider(Box::new(crate::autocomplete::FuzzyProvider::new(vec![
        crate::autocomplete::Completion {
            label: "help".into(),
            description: None,
            insert: "/help ".into(),
        },
    ])));
    type_string(&mut editor, "/h");
    assert_eq!(
        editor.handle_key(KeyEvent::plain(Key::Esc)),
        EditorAction::Handled
    );
    assert!(!editor.popup_open());
    assert_eq!(editor.text(), "/h", "escape keeps the token");
}
