//! The inline differential screen renderer (main-screen mode).
//!
//! Renders a logical line array into the live terminal without an
//! alternate screen, preserving native scrollback. Each draw diffs the
//! new line array against the previous one and rewrites only the changed
//! range: the cursor moves to the first changed row, each row is cleared
//! and rewritten, and stale rows below are erased. Lines that scrolled
//! into scrollback are committed and never rewritten. Frames are wrapped
//! in synchronized-output markers when enabled so partial frames never
//! flicker, and every line is reset-terminated so styles never leak.
//!
//! Contract: callers supply lines already wrapped to `width` (all
//! engine components do); the renderer defensively truncates.

use crate::editor::CURSOR_MARKER;
use crate::width;

/// Rendering policy toggles (terminal capability probe results).
#[derive(Debug, Clone, Copy)]
pub struct ScreenOptions {
    /// Wrap frames in CSI 2026 synchronized-output markers.
    pub synchronized: bool,
    /// Erase the scrollback on full redraws (the common-terminal
    /// `\x1b[3J` extension).
    pub clear_scrollback: bool,
}

impl Default for ScreenOptions {
    fn default() -> Self {
        Self {
            synchronized: true,
            clear_scrollback: true,
        }
    }
}

/// The inline diff renderer. Feed it the full logical line array every
/// frame; it writes the minimal byte diff to `out`.
pub struct Screen {
    prev: Vec<String>,
    /// First logical line eligible for rewriting (everything above has
    /// scrolled into native scrollback and is immutable).
    base: usize,
    /// Viewport row (0-based) where the cursor ended last draw.
    cursor_row: usize,
    size: (usize, usize),
    options: ScreenOptions,
    started: bool,
    /// A full repaint must erase and home first: the cursor may sit
    /// anywhere (invalidate/resize paths), and writing from there would
    /// duplicate the frame below the old one.
    needs_clear: bool,
    /// Trailing frame lines (input editor + footer) re-pinned to the
    /// physical bottom rows on every writing frame. `0` disables the
    /// pin.
    pinned_tail: usize,
}

impl Screen {
    /// A renderer with default options.
    pub fn new() -> Self {
        Self::with_options(ScreenOptions::default())
    }

    /// A renderer with explicit capability options.
    pub fn with_options(options: ScreenOptions) -> Self {
        Self {
            prev: Vec::new(),
            base: 0,
            cursor_row: 0,
            size: (0, 0),
            options,
            started: false,
            needs_clear: false,
            pinned_tail: 0,
        }
    }

    /// Pin the frame's last `rows` lines to the physical bottom rows of
    /// the screen (the input editor, popup, and footer). Every frame
    /// that writes anything then re-anchors that region with absolute
    /// positioning, so streaming or scrollback churn above can never
    /// drag the input off-screen. `0` disables the pin.
    pub fn set_pinned_tail(&mut self, rows: usize) {
        self.pinned_tail = rows;
    }

    /// Draw one frame: `lines` is the full logical content, `size` the
    /// current terminal (width, height).
    pub fn draw(
        &mut self,
        out: &mut impl std::io::Write,
        lines: &[String],
        columns: usize,
        height: usize,
    ) {
        let size_changed = self.size != (columns, height);
        if !self.started || size_changed || lines.len() < self.base || height == 0 || columns == 0 {
            self.synchronized_start(out);
            self.full_redraw(out, lines, columns, height);
            self.pin_tail(out, lines, columns, height);
            self.synchronized_end(out);
            self.place_hardware_cursor(out, lines, columns, height);
            return;
        }
        if !self.diff_draw(out, lines, columns, height) {
            return; // identical region: nothing to paint
        }
        self.pin_tail(out, lines, columns, height);
        self.synchronized_end(out);
        self.place_hardware_cursor(out, lines, columns, height);
    }

    /// Rewrite the last [`Screen::pinned_tail`] frame lines at the
    /// physical bottom rows of the screen via absolute positioning.
    /// Runs after every frame that wrote anything: whatever happened
    /// above (streaming, scrollback churn, accounting drift), the input
    /// region stays anchored to the bottom. Skipped while the frame
    /// fits the screen (nothing has scrolled yet; the diff already
    /// rewrites changed tail rows in place).
    fn pin_tail(
        &mut self,
        out: &mut impl std::io::Write,
        lines: &[String],
        columns: usize,
        height: usize,
    ) {
        let tail = self.pinned_tail.min(lines.len()).min(height);
        if tail == 0 || lines.len() <= height {
            return;
        }
        let first_row = height - tail; // 0-based
        let _ = write!(out, "\x1b[{};1H", first_row + 1);
        for (offset, line) in lines[lines.len() - tail..].iter().enumerate() {
            if offset > 0 {
                // The whole region sits inside the screen: plain cursor
                // moves, never scrolls.
                let _ = out.write_all(b"\r\x1b[1B");
            }
            let _ = out.write_all(b"\x1b[K");
            self.write_line(out, line, columns);
        }
        self.cursor_row = height - 1;
    }

    /// Forget all state; the next draw repaints from scratch, erasing
    /// the screen first (the cursor is wherever the last frame left it).
    pub fn invalidate(&mut self) {
        self.started = false;
        self.needs_clear = true;
        self.prev.clear();
        self.base = 0;
        self.cursor_row = 0;
    }

    fn synchronized_start(&self, out: &mut impl std::io::Write) {
        // The hardware cursor stays hidden for the whole frame: it is
        // shown again only at the editor caret (place_hardware_cursor),
        // so a frame that ends below the editor (transcript, footer,
        // streaming rows) can never park a blinking cursor there.
        let _ = out.write_all(b"\x1b[?25l");
        if self.options.synchronized {
            let _ = out.write_all(b"\x1b[?2026h");
        }
    }

    fn synchronized_end(&self, out: &mut impl std::io::Write) {
        if self.options.synchronized {
            let _ = out.write_all(b"\x1b[?2026l");
        }
    }

    /// Write one truncated, reset-terminated line (cursor markers
    /// stripped: they drive `place_hardware_cursor`, not the terminal).
    fn write_line(&self, out: &mut impl std::io::Write, line: &str, columns: usize) {
        // Copy only when the cursor marker is present: unmarked lines
        // (the common case) go straight from the source instead of
        // paying one string clone per line per frame.
        let cleaned;
        let line = if line.contains(CURSOR_MARKER) {
            cleaned = line.replace(CURSOR_MARKER, "");
            cleaned.as_str()
        } else {
            line
        };
        // Lines that already fit stream without building a new string.
        if width::width(line) <= columns {
            let _ = out.write_all(line.as_bytes());
        } else {
            let truncated = width::truncate_to_width(line, columns);
            let _ = out.write_all(truncated.as_bytes());
        }
        // Styles and hyperlinks never leak across lines.
        let _ = out.write_all(b"\x1b[0m\x1b]8;;\x07");
    }

    fn full_redraw(
        &mut self,
        out: &mut impl std::io::Write,
        lines: &[String],
        columns: usize,
        height: usize,
    ) {
        // The very first draw appends below the shell prompt; every
        // other full repaint (resize, invalidate) starts from wherever
        // the last frame left the cursor and must erase + home first,
        // or the frame duplicates below the old one.
        if self.started || self.needs_clear {
            let _ = out.write_all(b"\x1b[2J\x1b[H");
            if self.options.clear_scrollback {
                let _ = out.write_all(b"\x1b[3J");
            }
        }
        self.needs_clear = false;
        // Write all lines; the terminal scrolls the overflow into
        // scrollback.
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                let _ = out.write_all(b"\r\n");
            }
            self.write_line(out, line, columns);
        }
        self.prev = lines.to_vec();
        self.size = (columns, height);
        self.base = lines.len().saturating_sub(height);
        self.cursor_row = lines.len().min(height).saturating_sub(1);
        self.started = true;
    }

    /// Rewrite the changed viewport range; returns false when the region
    /// is identical and nothing was written.
    fn diff_draw(
        &mut self,
        out: &mut impl std::io::Write,
        lines: &[String],
        columns: usize,
        height: usize,
    ) -> bool {
        // Compare the rewrite-eligible region (viewport rows).
        let base_at_entry = self.base;
        let prev_region = &self.prev[base_at_entry..];
        let new_region = &lines[base_at_entry..];
        let mut first_changed = None;
        let mut last_changed = 0usize;
        let max_region = prev_region.len().max(new_region.len());
        for index in 0..max_region {
            if prev_region.get(index) != new_region.get(index) {
                if first_changed.is_none() {
                    first_changed = Some(index);
                }
                last_changed = index;
            }
        }
        let Some(first) = first_changed else {
            return false; // identical region: nothing to paint
        };

        self.synchronized_start(out);
        // Initial move to the first changed viewport row. CUD clamps at
        // the bottom margin without scrolling; every row past the bottom
        // becomes an explicit line-feed scroll instead.
        let mut row = self.cursor_row;
        let mut scrolled = 0usize;
        if first > row {
            let down = first - row;
            let clamped = down.min(height.saturating_sub(1).saturating_sub(row));
            if clamped > 0 {
                let _ = write!(out, "\r\x1b[{clamped}B");
                row += clamped;
            }
            for _ in 0..(down - clamped) {
                let _ = out.write_all(b"\r\n");
                scrolled += 1;
            }
        } else if first < row {
            let up = row - first;
            let _ = write!(out, "\r\x1b[{up}A");
            row = first;
        } else {
            let _ = out.write_all(b"\r");
        }

        // Rewrite the changed range. Invariant: viewport `row` displays
        // logical line `base_at_entry + vis - scrolled`. Removed indexes
        // (past the new content) are cleared instead of written.
        for vis in first..=last_changed {
            if vis > first {
                if row + 1 < height {
                    let _ = out.write_all(b"\r\x1b[1B");
                    row += 1;
                } else {
                    let _ = out.write_all(b"\r\n");
                    scrolled += 1;
                }
            }
            let _ = out.write_all(b"\x1b[K");
            if vis < new_region.len() {
                self.write_line(out, &lines[base_at_entry + vis], columns);
            }
        }

        // Erase stale rows when the visible region shrank. The cursor
        // physically moves down one row before the erase, so the tracked
        // row must follow or the next frame's relative moves drift.
        if new_region.len() < prev_region.len() && row + 1 < height {
            let _ = write!(out, "\r\x1b[1B\x1b[J");
            row += 1;
        }

        self.prev = lines.to_vec();
        self.size = (columns, height);
        self.base = base_at_entry + scrolled;
        self.cursor_row = row;
        true
    }

    /// Position the hardware cursor at an embedded [`CURSOR_MARKER`] and
    /// show it there — the input editor's caret is the only cell where
    /// the cursor is ever visible. Frames without a marker (dialog
    /// chrome, scroll labels) leave the cursor hidden.
    fn place_hardware_cursor(
        &mut self,
        out: &mut impl std::io::Write,
        lines: &[String],
        _columns: usize,
        height: usize,
    ) {
        for (index, line) in lines.iter().enumerate().skip(self.base) {
            if let Some(byte) = line.find(CURSOR_MARKER) {
                let cleaned = line.replace(CURSOR_MARKER, "");
                let col = width::width(&cleaned[..byte]);
                let row = (index - self.base).min(height.saturating_sub(1));
                let _ = write!(out, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1);
                self.cursor_row = row;
                return;
            }
        }
    }
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    fn draw(screen: &mut Screen, out: &mut Vec<u8>, frames: &[&str], width: usize, height: usize) {
        let frame = lines(frames);
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
        draw(
            &mut screen,
            &mut out,
            &[format!("he{CURSOR_MARKER}llo").as_str()],
            40,
            10,
        );
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
        assert!(text.contains("\x1b[J"), "erase-to-end present: {text:?}");
    }

    #[test]
    fn cursor_marker_positions_hardware_cursor() {
        let mut screen = Screen::with_options(ScreenOptions {
            synchronized: false,
            clear_scrollback: false,
        });
        let mut out = Vec::new();
        draw(
            &mut screen,
            &mut out,
            &[format!("he{CURSOR_MARKER}llo").as_str()],
            40,
            10,
        );
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
        assert!(
            text.starts_with("\x1b[?25l\r\x1b[1A\x1b[Kx"),
            "one row up, rewrite in the erased row: {text:?}"
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
}
