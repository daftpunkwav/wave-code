use super::*;
use std::sync::Arc;

/// Wrap literal rows into one segment (the common test frame).
fn frame(values: &[&str]) -> Vec<Segment> {
    vec![Arc::new(
        values.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )]
}

/// A frame from owned strings (marker rows build dynamically).
fn frame_owned(values: Vec<String>) -> Vec<Segment> {
    vec![Arc::new(values)]
}

fn draw(screen: &mut Screen, out: &mut Vec<u8>, frames: &[&str], width: usize, height: usize) {
    let frame = frame(frames);
    screen.draw(out, &frame, width, height);
}

#[test]
fn first_draw_writes_all_lines_with_resets() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    // The frame opens by hiding the hardware cursor: rows below the
    // editor (transcript, footer) must never carry a blinking caret.
    assert!(text.starts_with("\x1b[?25lalpha"), "hide first: {text:?}");
    assert!(text.contains("alpha\x1b[0m"), "reset appended: {text:?}");
    assert!(text.contains("beta"));
    // No caret row in the frame: the cursor stays hidden.
    assert!(
        !text.contains("\x1b[?25h"),
        "no show without caret: {text:?}"
    );
}

#[test]
fn appended_lines_write_only_the_suffix() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 10);
    out.clear();
    draw(&mut screen, &mut out, &["alpha", "beta", "gamma"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains("alpha"),
        "unchanged lines untouched: {text:?}"
    );
    assert!(text.contains("gamma"));
    assert!(text.contains("\x1b[1B"), "cursor moved down: {text:?}");
}

#[test]
fn changed_visible_line_rewrites_range() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha", "beta", "gamma"], 40, 10);
    out.clear();
    draw(&mut screen, &mut out, &["alpha", "BETA", "gamma"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("BETA"), "{text:?}");
    assert!(!text.contains("alpha\x1b[0m"), "line 0 untouched: {text:?}");
    assert!(!text.contains("gamma\x1b[0m"), "line 2 untouched: {text:?}");
    assert!(
        text.starts_with("\x1b[?25l\r\x1b[1A"),
        "hide then cursor moved up: {text:?}"
    );
}

#[test]
fn identical_frame_writes_nothing() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha"], 40, 10);
    out.clear();
    draw(&mut screen, &mut out, &["alpha"], 40, 10);
    assert!(out.is_empty());
}

/// A segment shared by pointer with the previous frame skips the
/// per-line compare: same allocation means same content.
#[test]
fn shared_segment_frame_writes_nothing() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let shared: Vec<Segment> = vec![Arc::new(vec!["alpha".to_string()])];
    let mut out = Vec::new();
    screen.draw(&mut out, &shared, 40, 10);
    out.clear();
    // Hand back the very same Arc: the pointer path must find
    // nothing to paint (an unchanged transcript between frames).
    screen.draw(&mut out, &shared, 40, 10);
    assert!(out.is_empty(), "shared segment skipped: {out:?}");
}

/// Multi-segment frames diff per line: a change in the second
/// segment rewrites only its own row; the first segment's rows stay
/// untouched.
#[test]
fn multi_segment_frames_diff_per_line() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let first: Segment = Arc::new(vec!["alpha".to_string(), "beta".to_string()]);
    let mut out = Vec::new();
    screen.draw(
        &mut out,
        &[Arc::clone(&first), Arc::new(vec!["gamma".to_string()])],
        40,
        10,
    );
    out.clear();
    screen.draw(
        &mut out,
        &[first, Arc::new(vec!["GAMMA".to_string()])],
        40,
        10,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("GAMMA"), "{text:?}");
    assert!(
        !text.contains("alpha\x1b[0m") && !text.contains("beta\x1b[0m"),
        "first segment untouched: {text:?}"
    );
}

/// Segment boundaries shifting between frames fall back to the
/// per-line compare on global rows: only genuinely different lines
/// are rewritten, exactly like the old flat array diff.
#[test]
fn shifted_segment_boundaries_diff_by_line() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let x: Segment = Arc::new(vec!["X".to_string()]);
    let y: Segment = Arc::new(vec!["Y1".to_string(), "Y2".to_string()]);
    let mut out = Vec::new();
    screen.draw(&mut out, &[Arc::clone(&x), Arc::clone(&y)], 40, 10);
    out.clear();
    // Swap the segments: every global row changes content, so every
    // row is rewritten even though both allocations are shared.
    screen.draw(&mut out, &[y, x], 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("Y1"), "{text:?}");
    assert!(text.contains("Y2"), "{text:?}");
    assert_eq!(text.matches('X').count(), 1, "X rewritten once: {text:?}");
}

/// A pane appearing between frames (one segment becoming two) diffs
/// like the flat array: the appended row is written, existing rows
/// stay untouched.
#[test]
fn segment_count_growth_writes_only_the_append() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    screen.draw(
        &mut out,
        &[Arc::new(vec!["a".to_string(), "b".to_string()])],
        40,
        10,
    );
    out.clear();
    let a: Segment = Arc::new(vec!["a".to_string()]);
    screen.draw(
        &mut out,
        &[a, Arc::new(vec!["b".to_string(), "c".to_string()])],
        40,
        10,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("c"), "appended row written: {text:?}");
    assert!(
        !text.contains("a\x1b[0m") && !text.contains("b\x1b[0m"),
        "existing rows untouched: {text:?}"
    );
}

/// A pane disappearing (two segments becoming one) erases the stale
/// rows instead of leaving ghosts.
#[test]
fn removed_segment_erases_stale_rows() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    let a: Segment = Arc::new(vec!["a".to_string()]);
    screen.draw(
        &mut out,
        &[a, Arc::new(vec!["b".to_string(), "c".to_string()])],
        40,
        10,
    );
    out.clear();
    screen.draw(
        &mut out,
        &[Arc::new(vec!["a".to_string(), "b".to_string()])],
        40,
        10,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\x1b[3;1H\x1b[K"),
        "stale row erased by the viewport repaint: {text:?}"
    );
    assert!(!text.contains("c\x1b[0m"), "no ghost: {text:?}");
}

/// Empty segments share their offset with the next one and never
/// claim a row: rendering and diffing treat them as absent.
#[test]
fn empty_segments_render_and_diff_like_flat_lines() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let empty: Segment = Arc::new(Vec::new());
    let a: Segment = Arc::new(vec!["a".to_string()]);
    let mut out = Vec::new();
    screen.draw(
        &mut out,
        &[
            Arc::clone(&empty),
            Arc::clone(&a),
            Arc::clone(&empty),
            Arc::new(vec!["b".to_string()]),
        ],
        40,
        10,
    );
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(
        text.contains("a\x1b[0m") && text.contains("b\x1b[0m"),
        "{text:?}"
    );
    out.clear();
    screen.draw(
        &mut out,
        &[
            empty,
            a,
            Arc::new(Vec::new()),
            Arc::new(vec!["B".to_string()]),
        ],
        40,
        10,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("B\x1b[0m"), "changed row written: {text:?}");
    assert!(
        !text.contains("a\x1b[0m"),
        "unchanged segment untouched: {text:?}"
    );
}

/// A pointer-shared segment straddling the scrollback base skips
/// its compare: rows below base are immutable scrollback, and the
/// shared allocation proves the visible remainder identical.
#[test]
fn shared_segment_straddling_base_skips_compare() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let first: Segment = Arc::new(vec![
        "r0".to_string(),
        "r1".to_string(),
        "r2".to_string(),
        "r3".to_string(),
    ]);
    let mut out = Vec::new();
    screen.draw(
        &mut out,
        &[
            Arc::clone(&first),
            Arc::new(vec!["s4".to_string(), "s5".to_string()]),
        ],
        40,
        3,
    );
    assert_eq!(screen.base, 3, "base inside the first segment");
    out.clear();
    screen.draw(
        &mut out,
        &[first, Arc::new(vec!["S4".to_string(), "s5".to_string()])],
        40,
        3,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("S4"), "changed row written: {text:?}");
    assert!(
        !text.contains("r3\x1b[0m"),
        "straddled shared segment skipped: {text:?}"
    );
}

#[test]
fn resize_triggers_full_redraw() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha"], 40, 10);
    out.clear();
    draw(&mut screen, &mut out, &["alpha"], 60, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with("\x1b[?25l\x1b[2J\x1b[H"),
        "hide, erase + home: {text:?}"
    );
    assert!(text.contains("alpha"));
}

#[test]
fn invalidate_draw_clears_and_homes_first() {
    // Repaint-after-invalidate starts from wherever the last frame
    // left the cursor; without erase + home the new frame duplicates
    // below the old one (the double-welcome bug).
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 10);
    screen.invalidate();
    out.clear();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with("\x1b[?25l\x1b[2J\x1b[H"),
        "erase + home after hide: {text:?}"
    );
    assert_eq!(text.matches("alpha").count(), 1, "single copy: {text:?}");
}

/// Theme switches repaint via `invalidate`: with the default
/// options the repaint must also erase the scrollback, so rows
/// drawn under the previous palette cannot survive above the
/// viewport.
#[test]
fn invalidate_clears_the_scrollback_when_enabled() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: true,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 10);
    screen.invalidate();
    out.clear();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\x1b[3J"),
        "scrollback erased on the repaint: {text:?}"
    );
}

#[test]
fn synchronized_markers_wrap_frames() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: true,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    // Hide precedes the synchronized window; the frame ends with the
    // sync-end marker (the caret Show, when any, lands after it).
    assert!(text.starts_with("\x1b[?25l\x1b[?2026h"), "{text:?}");
    assert!(text.ends_with("\x1b[?2026l"));
}

#[test]
fn cursor_is_shown_only_at_the_caret_marker() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    // With a caret row: positioned and shown.
    let mut out = Vec::new();
    let frame = frame_owned(vec![format!("he{CURSOR_MARKER}llo")]);
    screen.draw(&mut out, &frame, 40, 10);
    let text = String::from_utf8(out).unwrap();
    let caret = text.find("\x1b[1;3H").expect("caret positioned");
    assert!(
        text[caret..].starts_with("\x1b[1;3H\x1b[?25h"),
        "shown right after positioning: {text:?}"
    );
    // Exactly one show per frame, at the caret.
    assert_eq!(text.matches("\x1b[?25h").count(), 1, "{text:?}");
}

#[test]
fn shrinking_content_clears_stale_rows() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 10);
    out.clear();
    draw(&mut screen, &mut out, &["alpha"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    // A shrink repaints the viewport with per-row absolute erases: the
    // stale second row is gone and nothing below the frame survives.
    assert!(
        text.contains("\x1b[1;1H\x1b[K"),
        "viewport repaint from the top: {text:?}"
    );
    assert!(
        text.contains("\x1b[2;1H\x1b[K"),
        "stale row erased: {text:?}"
    );
    assert!(!text.contains("beta"), "ghost content gone: {text:?}");
}

/// A frame shrinking while a tail is pinned: the previous frame's
/// bottom-anchored tail copy must not survive as a second input box
/// (the dialog-close artifact). The whole-viewport repaint erases it.
#[test]
fn shrunken_frame_repaints_the_viewport_without_stranding_the_tail() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    screen.set_pinned_tail(2);
    let mut out = Vec::new();
    let tall: Vec<String> = [
        "t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "DIALOG", "editor", "footer",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    screen.draw(&mut out, &frame_owned(tall), 40, 6);
    out.clear();
    let short: Vec<String> = [
        "t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "editor", "footer",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    screen.draw(&mut out, &frame_owned(short), 40, 6);
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains("DIALOG"),
        "the closed dialog leaves no copy: {text:?}"
    );
    assert!(
        text.contains("\x1b[6;1H\x1b[K"),
        "viewport rows past the frame erase: {text:?}"
    );
    // The tail stays anchored at the physical bottom rows (5 and 6).
    assert!(text.contains("\x1b[5;1H"), "pin rewrote row 5: {text:?}");
}

/// A frame shrinking exactly onto the rewrite base has no
/// rewrite-eligible rows left: only a full repaint can bring the
/// surviving rows back into the viewport (a diff would leave the
/// screen blank while the frame still holds content).
#[test]
fn shrink_onto_the_base_repaints_the_frame() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["a", "b", "c", "d", "e"], 40, 2);
    assert_eq!(screen.base, 3, "overflowed frame: base at total-height");
    out.clear();
    // Five rows shrink to three: total lands exactly on the base.
    draw(&mut screen, &mut out, &["a", "b", "c"], 40, 2);
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(
        text.contains("\x1b[2J"),
        "full repaint, not a blank diff: {text:?}"
    );
    assert!(text.contains('c'), "viewport shows the last rows: {text:?}");
    // The follow-up frame diffs from a sane base again.
    out.clear();
    draw(&mut screen, &mut out, &["a", "b", "C"], 40, 2);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("C") && !text.contains("\x1b[2J"), "{text:?}");
}

/// A frame emptied entirely (total 0 over a non-empty previous frame,
/// e.g. a closed picker above an empty transcript) routes through the
/// viewport repaint — not the full erase, which would wipe native
/// scrollback: every physical row clears, the old lines never rewrite,
/// and the next non-empty frame diffs incrementally again.
#[test]
fn fully_emptied_frame_clears_rows_without_a_full_erase() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 4);
    out.clear();
    draw(&mut screen, &mut out, &[], 40, 4);
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(
        !text.contains("\x1b[2J"),
        "an empty frame must not wipe the screen: {text:?}"
    );
    // Every viewport row erases in place (total 0 re-anchors the base
    // to physical row 1).
    assert_eq!(
        text.matches("\x1b[K").count(),
        4,
        "each of the 4 rows clears: {text:?}"
    );
    assert!(
        text.contains("\x1b[1;1H\x1b[K"),
        "clearing starts at physical row 1: {text:?}"
    );
    assert!(
        !text.contains("alpha") && !text.contains("beta"),
        "the emptied lines are erased, not rewritten: {text:?}"
    );
    // Renderer state stays sane afterwards: the next frame diffs again.
    out.clear();
    draw(&mut screen, &mut out, &["gamma"], 40, 4);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("gamma"), "{text:?}");
    assert!(
        !text.contains("\x1b[2J"),
        "back to incremental drawing: {text:?}"
    );
}

#[test]
fn cursor_marker_positions_hardware_cursor() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    let frame = frame_owned(vec![format!("he{CURSOR_MARKER}llo")]);
    screen.draw(&mut out, &frame, 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\x1b[1;3H"),
        "cursor to row 1 col 3: {text:?}"
    );
    assert!(!text.contains("he\x1b_wllo"), "marker stripped: {text:?}");
}

#[test]
fn scroll_into_scrollback_advances_base() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(
        &mut screen,
        &mut out,
        &["a", "b", "c", "d", "e", "f"],
        40,
        3,
    );
    assert_eq!(screen.base, 3, "three lines scrolled off");
    out.clear();
    draw(
        &mut screen,
        &mut out,
        &["a", "b", "c", "d", "e", "f", "g"],
        40,
        3,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("g"), "{text:?}");
    assert!(!text.contains("\x1b[2J"), "no full redraw: {text:?}");
}

#[test]
fn shrink_then_grow_stays_aligned() {
    // Drift regression: the shrink erase moves the physical cursor
    // down one row; the tracked row must follow or the next frame
    // writes at the wrong viewport row.
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["alpha", "beta"], 40, 10);
    out.clear();
    draw(&mut screen, &mut out, &["alpha"], 40, 10);
    out.clear();
    draw(&mut screen, &mut out, &["alpha", "x"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains("\x1b[1B"),
        "no downward travel expected: {text:?}"
    );
    // The shrink repaint left the physical cursor at the bottom row;
    // the grow diff travels up from there to the changed row.
    assert!(
        text.starts_with("\x1b[?25l\r\x1b[8A\x1b[Kx"),
        "up from the bottom row, rewrite: {text:?}"
    );
    assert!(text.contains("x\x1b[0m"), "{text:?}");
}

#[test]
fn overlong_lines_truncate_to_width() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["0123456789"], 5, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with("\x1b[?25l01234"),
        "hide, then truncated: {text:?}"
    );
}

#[test]
fn margin_indents_every_row_at_write_time() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    screen.set_margin(2);
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["hi"], 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("  hi\x1b[0m"), "indented row: {text:?}");
    // The caret column shifts by the margin.
    let frame = frame_owned(vec![format!("{CURSOR_MARKER}x")]);
    let mut out = Vec::new();
    screen.draw(&mut out, &frame, 40, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\x1b[1;3H"),
        "caret col = margin + 1: {text:?}"
    );
    // Overlong lines truncate against the margin budget.
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["0123456789"], 5, 10);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("  012"),
        "content truncated to width - margin: {text:?}"
    );
}

/// A terminal narrower than the gutter drops the indent instead of
/// writing rows past the column budget (the wrap would corrupt the
/// frame).
#[test]
fn margin_narrower_than_the_terminal_drops_the_indent() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    screen.set_margin(4);
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["abcdef"], 2, 10);
    let text = String::from_utf8(out).unwrap();
    // margin clamps to 2 columns; the content truncates to nothing.
    assert!(
        text.contains("  \x1b[0m") || text.contains("\r\n  \x1b[0m"),
        "indent clamped to the width, no extra cells: {text:?}"
    );
    assert!(
        !text.contains("abcdef"),
        "no content fits a 2-column budget: {text:?}"
    );
}

/// The gutter margin composes with the pinned tail: the bottom
/// re-anchor erases the whole row (`\x1b[K` from column 1 clears the
/// previous indent) and rewrites it with the margin, so no stale
/// padding can survive a repaint.
#[test]
fn margin_and_pinned_tail_compose() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    screen.set_margin(2);
    screen.set_pinned_tail(1);
    let mut out = Vec::new();
    draw(
        &mut screen,
        &mut out,
        &["a", "b", "c", "d", "footer"],
        40,
        4,
    );
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(
        text.contains("  footer\x1b[0m"),
        "pinned row indented: {text:?}"
    );
    out.clear();
    // The changed row sits at the first rewrite-eligible row (the
    // ones above have scrolled into scrollback).
    draw(
        &mut screen,
        &mut out,
        &["a", "B", "c", "d", "footer"],
        40,
        4,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("  B\x1b[0m"), "diff row indented: {text:?}");
    assert!(
        text.contains("\x1b[4;1H"),
        "pin re-anchors the bottom row: {text:?}"
    );
    assert_eq!(text.matches("footer").count(), 1, "{text:?}");
}

#[test]
fn pinned_tail_repaints_at_the_screen_bottom() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    screen.set_pinned_tail(1);
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["a", "b", "c", "footer"], 40, 3);
    out.clear();
    // Only a viewport row above the tail changed; the pin must still
    // re-anchor the footer to the physical bottom row.
    draw(&mut screen, &mut out, &["a", "B", "c", "footer"], 40, 3);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("B"), "{text:?}");
    assert!(
        text.contains("\x1b[3;1H"),
        "absolute move to the last row: {text:?}"
    );
    // The diff never touched the footer; only the pin rewrote it.
    assert_eq!(text.matches("footer").count(), 1, "{text:?}");
}

#[test]
fn pin_stays_off_for_short_frames_and_identical_ones() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    screen.set_pinned_tail(1);
    let mut out = Vec::new();
    draw(&mut screen, &mut out, &["a", "footer"], 40, 10);
    out.clear();
    // The frame fits the screen: the diff already rewrites changed
    // rows in place; no bottom pin.
    draw(&mut screen, &mut out, &["A", "footer"], 40, 10);
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(
        !text.contains("\x1b[10;1H"),
        "no bottom pin for short frames: {text:?}"
    );
    assert!(
        !text.contains("footer"),
        "unchanged tail untouched: {text:?}"
    );
    out.clear();
    // Identical scrolled frames write nothing at all.
    draw(
        &mut screen,
        &mut out,
        &["a", "b", "c", "d", "e", "footer"],
        40,
        3,
    );
    out.clear();
    draw(
        &mut screen,
        &mut out,
        &["a", "b", "c", "d", "e", "footer"],
        40,
        3,
    );
    assert!(out.is_empty(), "identical frame: {out:?}");
}

#[test]
fn pinned_tail_is_bounded_by_the_screen() {
    let mut screen = Screen::with_options(ScreenOptions {
        synchronized: false,
        clear_scrollback: false,
    });
    screen.set_pinned_tail(50);
    let mut out = Vec::new();
    // Tail larger than the screen clamps to the whole screen.
    draw(
        &mut screen,
        &mut out,
        &["a", "b", "c", "d", "footer"],
        40,
        3,
    );
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\x1b[1;1H"),
        "clamped to the top row: {text:?}"
    );
    assert!(text.contains("footer"), "{text:?}");
}
