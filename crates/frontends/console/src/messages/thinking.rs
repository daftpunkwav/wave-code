//! The thinking/reasoning transcript component.
//!
//! Live: a spinner, `✻ Thinking for Ns… (ctrl+o to expand)`, and a
//! scrolling tail of the last two wrapped lines (dim italic, `└`
//! branch prefix). Finalized: the header freezes to `✻ Thought for Ns
//! (ctrl+o to expand)` above the same collapsed preview; Ctrl+O
//! expansion is driven through a shared flag so every expandable block
//! responds in one keystroke.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::theme::{self, Token};
use tui_engine::component::{Component, Segment};
use tui_engine::loader::{Loader, SpinnerStyle};
use tui_engine::width;

/// Wrapped preview lines shown when collapsed (both live and final).
pub const PREVIEW_LINES: usize = 2;

/// The branch prefix glyph on preview lines.
const BRANCH: &str = "└";
/// The finalized thinking header glyph.
const FLOWER: &str = "✻";

/// Shared Ctrl+O expansion flag across expandable components.
#[derive(Debug, Clone)]
pub struct ExpandedFlag(Arc<AtomicBool>);

impl ExpandedFlag {
    /// A flag starting collapsed.
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Toggle and return the new state.
    pub fn toggle(&self) -> bool {
        let next = !self.0.load(std::sync::atomic::Ordering::Relaxed);
        self.0.store(next, std::sync::atomic::Ordering::Relaxed);
        next
    }

    /// Current state.
    pub fn get(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for ExpandedFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// The thinking block for one reasoning span.
pub struct Thinking {
    text: String,
    live: bool,
    started: Instant,
    /// Frozen at [`Thinking::finalize`] (finalized header duration).
    duration: Option<Duration>,
    expanded_flag: ExpandedFlag,
    spinner: Loader,
    lines: Option<(usize, String, bool, bool, Segment)>,
}

impl Thinking {
    /// A live (streaming) thinking block.
    pub fn live(expanded_flag: ExpandedFlag) -> Self {
        Self {
            text: String::new(),
            live: true,
            started: Instant::now(),
            duration: None,
            expanded_flag,
            spinner: Loader::new(
                SpinnerStyle::Triangle,
                "",
                theme::current().style(Token::TextDim),
                theme::current().style(Token::TextDim),
            ),
            lines: None,
        }
    }

    /// A finalized block built from complete text (transcript folding).
    pub fn finalized(text: String, expanded_flag: ExpandedFlag) -> Self {
        Self {
            text,
            live: false,
            started: Instant::now(),
            duration: None,
            expanded_flag,
            spinner: Loader::new(
                SpinnerStyle::Triangle,
                "",
                theme::current().style(Token::TextDim),
                theme::current().style(Token::TextDim),
            ),
            lines: None,
        }
    }

    /// Append streamed thinking text.
    pub fn push(&mut self, text: &str) {
        self.text.push_str(text);
        self.lines = None;
    }

    /// Replace the streamed text (the persisted live block mirrors the
    /// full buffer each frame). The render clock is untouched — the
    /// header duration and spinner keep advancing across flushes.
    pub fn set_text(&mut self, text: String) {
        if self.text != text {
            self.text = text;
            self.lines = None;
        }
    }

    /// Stop live animation; the header freezes and the block collapses
    /// to a preview.
    pub fn finalize(&mut self) {
        self.duration = Some(self.started.elapsed());
        self.live = false;
        self.lines = None;
    }

    /// True while streaming.
    pub fn is_live(&self) -> bool {
        self.live
    }

    /// True when collapsed content hides rows (drives the footer hint).
    pub fn has_hidden(&self) -> bool {
        !self.live && !self.expanded_flag.get() && self.wrapped_lines() > PREVIEW_LINES
    }

    fn wrapped_lines(&self) -> usize {
        self.text.split('\n').count()
    }
}

impl Component for Thinking {
    fn render(&mut self, columns: usize) -> Segment {
        let theme = theme::current();
        let expanded = self.expanded_flag.get();
        if let Some((cached_columns, cached_text, cached_live, cached_expanded, lines)) =
            &self.lines
            && *cached_columns == columns
            && *cached_text == self.text
            && *cached_live == self.live
            && *cached_expanded == expanded
        {
            return Arc::clone(lines);
        }
        let body_width = columns.saturating_sub(6);
        let italic_dim = theme.style(Token::TextDim).italic();
        let mut out = Vec::new();
        if self.live {
            let frame = self.spinner.current_frame();
            let secs = self.started.elapsed().as_secs();
            out.push(format!(
                "{} {}{}",
                theme.paint(Token::TextDim, frame),
                theme.paint(Token::TextStrong, &format!("Thinking for {secs}s…")),
                theme.paint(Token::TextDim, " (ctrl+o to expand)")
            ));
            // Scrolling tail: the last wrapped lines only. Segments wrap
            // independently, so collecting backwards from the final one
            // and stopping once the preview is full yields the same tail
            // without wrapping the whole (possibly large) stream text
            // on every frame.
            let mut tail: Vec<String> = Vec::new();
            for segment in self.text.split('\n').rev() {
                if tail.len() >= PREVIEW_LINES {
                    break;
                }
                let wrapped = width::wrap_line(segment, body_width);
                tail.extend(wrapped.into_iter().rev());
            }
            tail.reverse();
            let start = tail.len().saturating_sub(PREVIEW_LINES);
            for line in &tail[start..] {
                out.push(format!("  {BRANCH} {}", italic_dim.paint(line)));
            }
        } else if self.text.is_empty() {
            return Segment::new(Vec::new());
        } else {
            // Frozen header: the spinner swaps for a static glyph and
            // the duration stops ticking.
            let secs = self
                .duration
                .unwrap_or_else(|| self.started.elapsed())
                .as_secs();
            out.push(format!(
                "{FLOWER} {}{}",
                theme.paint(Token::TextStrong, &format!("Thought for {secs}s")),
                theme.paint(Token::TextDim, " (ctrl+o to expand)")
            ));
            let mut rows: Vec<String> = Vec::new();
            for (index, segment) in self.text.split('\n').enumerate() {
                for (row_index, line) in width::wrap_line(segment, body_width)
                    .into_iter()
                    .enumerate()
                {
                    if index == 0 && row_index == 0 {
                        rows.push(format!("  {BRANCH} {}", italic_dim.paint(&line)));
                    } else {
                        rows.push(format!("    {}", italic_dim.paint(&line)));
                    }
                }
            }
            if expanded || rows.len() <= PREVIEW_LINES {
                out.extend(rows);
            } else {
                let hidden = rows.len() - PREVIEW_LINES;
                out.extend(rows[..PREVIEW_LINES].iter().cloned());
                out.push(format!(
                    "  {}",
                    theme.paint(
                        Token::TextDim,
                        &format!("… ({hidden} more lines, ctrl+o to expand)")
                    )
                ));
            }
        }
        out.push(String::new());
        let lines = Arc::new(out);
        self.lines = Some((
            columns,
            self.text.clone(),
            self.live,
            expanded,
            Arc::clone(&lines),
        ));
        lines
    }

    fn invalidate(&mut self) {
        // Only the cached lines carry baked-in styling: the spinner's
        // frame is read per render and painted with the live palette,
        // so rebuilding it here would just reset its animation clock.
        self.lines = None;
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use tui_engine::width::strip_ansi;

    #[test]
    fn live_thinking_shows_tail_lines() {
        theme::set(theme::Theme::synthwave());
        let mut block = Thinking::live(ExpandedFlag::new());
        block.push("first line\nsecond line\nthird line");
        let lines = block.render(60);
        let header = strip_ansi(&lines[0]);
        assert!(header.contains("Thinking for"), "live header: {lines:?}");
        assert!(header.ends_with("(ctrl+o to expand)"), "{lines:?}");
        assert!(strip_ansi(&lines[1]).starts_with("  └ second line"));
        assert!(strip_ansi(&lines[2]).starts_with("  └ third line"));
        assert!(!strip_ansi(&lines[1]).contains("first line"), "tail only");
    }

    #[test]
    fn live_tail_matches_a_full_wrap_of_long_streams() {
        // The tail is collected backwards over the final segments; it
        // must stay identical to wrapping every segment in order.
        theme::set(theme::Theme::synthwave());
        let body = (0..500)
            .map(|i| format!("segment {i} with some wrapping filler text"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut block = Thinking::live(ExpandedFlag::new());
        block.push(&body);
        let lines = block.render(60);
        let body_width = 60usize.saturating_sub(6);
        let full: Vec<String> = body
            .split('\n')
            .flat_map(|segment| width::wrap_line(segment, body_width))
            .collect();
        let expected = &full[full.len() - PREVIEW_LINES..];
        for (line, want) in lines[1..1 + expected.len()].iter().zip(expected) {
            assert_eq!(strip_ansi(line), format!("  └ {want}"), "{lines:?}");
        }
        assert_eq!(
            lines.len(),
            1 + expected.len() + 1,
            "header + preview + spacer only"
        );
    }

    #[test]
    fn finalized_thinking_collapses_with_hint() {
        theme::set(theme::Theme::synthwave());
        let mut block = Thinking::live(ExpandedFlag::new());
        block.push("one\ntwo\nthree");
        block.finalize();
        let lines = block.render(60);
        assert_eq!(
            lines.len(),
            5,
            "header + 2 preview + hint + spacer: {lines:?}"
        );
        let header = strip_ansi(&lines[0]);
        assert!(header.contains("Thought for"), "{lines:?}");
        assert!(header.contains("(ctrl+o to expand)"), "{lines:?}");
        assert!(strip_ansi(&lines[1]).starts_with("  └ one"), "{lines:?}");
        assert!(
            strip_ansi(&lines[3]).contains("… (1 more lines, ctrl+o to expand)"),
            "{lines:?}"
        );
    }

    #[test]
    fn expansion_shows_everything() {
        theme::set(theme::Theme::synthwave());
        let flag = ExpandedFlag::new();
        let mut block = Thinking::live(flag.clone());
        block.push("one\ntwo\nthree");
        block.finalize();
        flag.toggle();
        let lines = block.render(60);
        assert_eq!(lines.len(), 5, "header + 3 rows + spacer: {lines:?}");
        assert!(strip_ansi(&lines[3]).contains("three"));
    }
}
