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
        // A frame that shrank to (or below) the base has no
        // rewrite-eligible rows: the whole viewport is stale scrollback
        // and only a full repaint can re-show the surviving rows. An
        // empty frame (total 0) stays on the diff path, where the stale
        // rows clear without an erase.
        let shrank_to_base = view.total <= self.base && view.total > 0;
        if !self.started || size_changed || shrank_to_base || height == 0 || columns == 0 {
            self.synchronized_start(out);
            self.full_redraw(out, &view, columns, height);
            self.pin_tail(out, &view, columns, height);
            self.synchronized_end(out);
            self.place_hardware_cursor(out, &view, height);
            self.prev = segments.to_vec();
            return;
        }
        // A frame that SHRANK (a dialog closed, turns rewound) leaves
        // the previous frame's bottom-anchored rows stranded: the diff's
        // bottom-erase arithmetic drifts around the pinned tail and can
        // strand a dead copy of the editor and footer mid-screen (the
        // two-input-boxes artifact). Repaint the whole viewport instead
        // — rare, user-visible transitions where one extra repaint is
        // invisible and correctness is guaranteed.
        if view.total < FrameView::new(&self.prev).total {
            self.synchronized_start(out);
            self.viewport_redraw(out, &view, columns, height);
            self.pin_tail(out, &view, columns, height);
            self.synchronized_end(out);
            self.place_hardware_cursor(out, &view, height);
            self.prev = segments.to_vec();
            self.size = (columns, height);
            return;
        }
        if !self.diff_draw(out, segments, &view, columns, height) {
            return; // identical region: nothing to paint
        }
        self.pin_tail(out, &view, columns, height);
        self.synchronized_end(out);
        self.place_hardware_cursor(out, &view, height);
    }

    /// Repaint every physical viewport row: physical row 1 shows
    /// logical [`Self::base`], so rows past the shrunken frame's end
    /// erase and the rest rewrite in place. Scrollback above the
    /// viewport is untouched. Leaves the cursor at the physical bottom
    /// row ([`Self::place_hardware_cursor`] refines it).
    fn viewport_redraw(
        &mut self,
        out: &mut impl std::io::Write,
        view: &FrameView,
        columns: usize,
        height: usize,
    ) {
        for row in 0..height {
            let _ = write!(out, "\x1b[{};1H\x1b[K", row + 1);
            if let Some(line) = view.get(self.base + row) {
                self.write_line(out, line, columns);
            }
        }
        self.cursor_row = height.saturating_sub(1);
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
        // A terminal narrower than the gutter drops the indent instead
        // of writing rows past the budget (the wrap would corrupt the
        // frame).
        let margin = self.margin.min(columns);
        for _ in 0..margin {
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
        if width::width(line) <= columns.saturating_sub(margin) {
            let _ = out.write_all(line.as_bytes());
        } else {
            let truncated = width::truncate_to_width(line, columns.saturating_sub(margin));
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
                // The prefix before the first marker holds no marker, so
                // its width needs no marker-stripping copy (this runs
                // every frame; the editor line always carries one).
                let col = self.margin + width::width(&line[..byte]);
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
#[cfg(test)]
#[cfg(test)]
mod tests;
