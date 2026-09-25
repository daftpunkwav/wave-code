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
//! Frames arrive as [`Segment`]s — reference-counted line arrays. The
//! previous frame is stored as segments too, so storing it is a
//! refcount bump (no per-line copy), and a segment whose allocation
//! matches the previous frame's skips its range by pointer equality
//! instead of comparing every line.
//!
//! Three frame-level invariants: the trailing [`Screen::set_pinned_tail`]
//! lines (the input region) are re-anchored to the physical bottom rows
//! on every frame that wrote anything, the hardware cursor is shown
//! only at the editor's caret marker — every other row renders with the
//! cursor hidden — and [`Screen::set_margin`] indents every row at
//! write time without touching the stored lines.
//!
//! Contract: callers supply lines already wrapped to `width - margin`
//! (all engine components do); the renderer defensively truncates.

use crate::component::Segment;
use crate::editor::CURSOR_MARKER;
use crate::width;

/// Zero-copy flattened view over a segmented frame: global row index →
/// line, without materializing the flat vector.
struct FrameView<'a> {
    segments: &'a [Segment],
    /// Cumulative line offsets per segment (`len = segments + 1`).
    offsets: Vec<usize>,
    total: usize,
}

impl<'a> FrameView<'a> {
    fn new(segments: &'a [Segment]) -> Self {
        let mut offsets = Vec::with_capacity(segments.len() + 1);
        let mut total = 0usize;
        for segment in segments {
            offsets.push(total);
            total += segment.len();
        }
        offsets.push(total);
        Self {
            segments,
            offsets,
            total,
        }
    }

    /// The line at global `row`; `None` past the end (an Option, so
    /// region compares treat "gone" and "empty" uniformly).
    fn get(&self, row: usize) -> Option<&String> {
        // The last segment starting at or before `row`: an empty segment
        // shares its offset with the next one, so it is never selected.
        let index = self
            .offsets
            .partition_point(|&off| off <= row)
            .checked_sub(1)?;
        let segment = self.segments.get(index)?;
        segment.get(row - self.offsets[index])
    }
}

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

/// The inline diff renderer. Feed it the full logical content every
/// frame; it writes the minimal byte diff to `out`.
pub struct Screen {
    prev: Vec<Segment>,
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
    /// Leading blank columns written before every row (the frame
    /// gutter). Applied at write time; the stored lines stay unpadded.
    margin: usize,
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
            margin: 0,
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

    /// Write `columns` leading blank columns before every row (the
    /// frame gutter). Stored lines stay unpadded; the indent lands at
    /// write time so frames carry no per-line padding copies.
    pub fn set_margin(&mut self, columns: usize) {
        self.margin = columns;
    }

    /// Draw one frame: `segments` is the full logical content, `size`
    /// the current terminal (width, height).
    pub fn draw(
        &mut self,
        out: &mut impl std::io::Write,
        segments: &[Segment],
        columns: usize,
        height: usize,
    ) {
        let view = FrameView::new(segments);
        let size_changed = self.size != (columns, height);
        if !self.started || size_changed || view.total < self.base || height == 0 || columns == 0 {
            self.synchronized_start(out);
            self.full_redraw(out, &view, columns, height);
            self.pin_tail(out, &view, columns, height);
            self.synchronized_end(out);
            self.place_hardware_cursor(out, &view, height);
            self.prev = segments.to_vec();
            return;
        }
        if !self.diff_draw(out, segments, &view, columns, height) {
            return; // identical region: nothing to paint
        }
        self.pin_tail(out, &view, columns, height);
        self.synchronized_end(out);
        self.place_hardware_cursor(out, &view, height);
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
        view: &FrameView,
        columns: usize,
        height: usize,
    ) {
        let tail = self.pinned_tail.min(view.total).min(height);
        if tail == 0 || view.total <= height {
            return;
        }
        let first_row = height - tail; // 0-based
        let _ = write!(out, "\x1b[{};1H", first_row + 1);
        for row in (view.total - tail)..view.total {
            if row > view.total - tail {
                // The whole region sits inside the screen: plain cursor
                // moves, never scrolls.
                let _ = out.write_all(b"\r\x1b[1B");
            }
            let _ = out.write_all(b"\x1b[K");
            if let Some(line) = view.get(row) {
                self.write_line(out, line, columns);
            }
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
    /// The frame gutter precedes every row.
    fn write_line(&self, out: &mut impl std::io::Write, line: &str, columns: usize) {
        for _ in 0..self.margin {
            let _ = out.write_all(b" ");
        }
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
        if width::width(line) <= columns.saturating_sub(self.margin) {
            let _ = out.write_all(line.as_bytes());
        } else {
            let truncated = width::truncate_to_width(line, columns.saturating_sub(self.margin));
            let _ = out.write_all(truncated.as_bytes());
        }
        // Styles and hyperlinks never leak across lines.
        let _ = out.write_all(b"\x1b[0m\x1b]8;;\x07");
    }

    fn full_redraw(
        &mut self,
        out: &mut impl std::io::Write,
        view: &FrameView,
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
        for row in 0..view.total {
            if row > 0 {
                let _ = out.write_all(b"\r\n");
            }
            if let Some(line) = view.get(row) {
                self.write_line(out, line, columns);
            }
        }
        self.size = (columns, height);
        self.base = view.total.saturating_sub(height);
        self.cursor_row = view.total.min(height).saturating_sub(1);
        self.started = true;
    }

    /// Rewrite the changed viewport range; returns false when the region
    /// is identical and nothing was written.
    fn diff_draw(
        &mut self,
        out: &mut impl std::io::Write,
        segments: &[Segment],
        view: &FrameView,
        columns: usize,
        height: usize,
    ) -> bool {
        // Compare the rewrite-eligible region (viewport rows). A new
        // segment sharing its allocation with the previous frame's
        // same-range segment is identical by construction — no line
        // comparisons at all.
        let prev_view = FrameView::new(&self.prev);
        let base_at_entry = self.base;
        let mut first_changed = None;
        let mut last_changed = 0usize;
        let record = |row: usize, first: &mut Option<usize>, last: &mut usize| {
            let vis = row - base_at_entry;
            if first.is_none() {
                *first = Some(vis);
            }
            *last = vis;
        };
        for (index, segment) in view.segments.iter().enumerate() {
            let start = view.offsets[index];
            let end = view.offsets[index + 1];
            if end <= base_at_entry {
                continue; // scrolled into immutable scrollback
            }
            let same_range = prev_view.offsets.get(index) == Some(&start)
                && prev_view.offsets.get(index + 1) == Some(&end);
            if same_range && std::sync::Arc::ptr_eq(segment, &self.prev[index]) {
                continue;
            }
            for row in start.max(base_at_entry)..end {
                if prev_view.get(row) != view.get(row) {
                    record(row, &mut first_changed, &mut last_changed);
                }
            }
        }
        // Rows the previous frame had beyond the new content: stale.
        for row in view.total..prev_view.total {
            record(row, &mut first_changed, &mut last_changed);
        }
        let Some(first) = first_changed else {
            return false; // identical region: nothing to paint
        };

        self.synchronized_start(out);
        // When the frame overflows the screen, this diff is going to
        // scroll: whatever currently sits in the pinned tail rows would
        // ride the scroll up into native scrollback as a dead copy of
        // the editor and footer (the two-input-boxes artifact). Erase
        // the tail region first, absolute-positioned, so blank rows are
        // what scrolls; the pin below redraws the fresh tail at the
        // bottom.
        let tail = self.pinned_tail.min(height);
        let pre_cleared = view.total > height && tail > 0;
        if pre_cleared {
            for tail_row in (height - tail)..height {
                let _ = write!(out, "\x1b[{};1H\x1b[K", tail_row + 1);
            }
        }
        // Initial move to the first changed viewport row. CUD clamps at
        // the bottom margin without scrolling; every row past the bottom
        // becomes an explicit line-feed scroll instead.
        let mut row = self.cursor_row;
        let mut scrolled = 0usize;
        if pre_cleared {
            let _ = write!(out, "\x1b[{};1H", first + 1);
            row = first;
        } else if first > row {
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
            if base_at_entry + vis < view.total
                && let Some(line) = view.get(base_at_entry + vis)
            {
                self.write_line(out, line, columns);
            }
        }

        // Erase stale rows when the visible region shrank. The cursor
        // physically moves down one row before the erase, so the tracked
        // row must follow or the next frame's relative moves drift.
        if view.total < prev_view.total && row + 1 < height {
            let _ = write!(out, "\r\x1b[1B\x1b[J");
            row += 1;
        }

        self.prev = segments.to_vec();
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
        view: &FrameView,
        height: usize,
    ) {
        for row in self.base..view.total {
            let Some(line) = view.get(row) else {
                continue;
            };
            if let Some(byte) = line.find(CURSOR_MARKER) {
                let cleaned = line.replace(CURSOR_MARKER, "");
                let col = self.margin + width::width(&cleaned[..byte]);
                let row = (row - self.base).min(height.saturating_sub(1));
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
        assert!(text.contains("\x1b[J"), "stale row erased: {text:?}");
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
        assert!(text.contains("\x1b[J"), "erase-to-end present: {text:?}");
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
}
