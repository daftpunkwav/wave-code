//! The thinking/reasoning transcript component.
//!
//! Live: a triangle-wave spinner, `thinking…`, and a scrolling tail of
//! the last two wrapped lines (dim italic). Finalized: a triangle bullet
//! with a collapsed two-line preview plus an ellipsis row; Ctrl+O expansion
//! is driven through a shared flag so every expandable block responds
//! in one keystroke.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::theme::{self, Token};
use tui_engine::component::Component;
use tui_engine::loader::{Loader, SpinnerStyle};
use tui_engine::width;

/// Wrapped preview lines shown when collapsed (both live and final).
pub const PREVIEW_LINES: usize = 2;

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
    expanded_flag: ExpandedFlag,
    spinner: Loader,
    lines: Option<(usize, String, bool, bool, Vec<String>)>,
}

impl Thinking {
    /// A live (streaming) thinking block.
    pub fn live(expanded_flag: ExpandedFlag) -> Self {
        Self {
            text: String::new(),
            live: true,
            expanded_flag,
            spinner: Loader::new(
                SpinnerStyle::Triangle,
                "thinking…",
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
            expanded_flag,
            spinner: Loader::new(
                SpinnerStyle::Triangle,
                "thinking…",
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

    /// Stop live animation; the block collapses to a preview.
    pub fn finalize(&mut self) {
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
    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let expanded = self.expanded_flag.get();
        if let Some((cached_columns, cached_text, cached_live, cached_expanded, lines)) =
            &self.lines
            && *cached_columns == columns
            && *cached_text == self.text
            && *cached_live == self.live
            && *cached_expanded == expanded
        {
            return lines.clone();
        }
        let body_width = columns.saturating_sub(4);
        let italic_dim = theme.style(Token::TextDim).italic();
        let mut out = Vec::new();
        if self.live {
            let frame = self.spinner.current_frame();
            out.push(format!(
                "{} {}",
                theme.paint(Token::TextDim, frame),
                theme.paint(Token::TextDim, "thinking…")
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
                out.push(format!("  {}", italic_dim.paint(line)));
            }
        } else if self.text.is_empty() {
            return Vec::new();
        } else {
            let mut rows: Vec<String> = Vec::new();
            for (index, segment) in self.text.split('\n').enumerate() {
                for (row_index, line) in width::wrap_line(segment, body_width)
                    .into_iter()
                    .enumerate()
                {
                    if index == 0 && row_index == 0 {
                        rows.push(format!(
                            "{}{}",
                            theme.paint(
                                Token::TextDim,
                                &format!("{} ", crate::chrome::symbols::TRIANGLE_WAVE)
                            ),
                            italic_dim.paint(&line)
                        ));
                    } else {
                        rows.push(format!("  {}", italic_dim.paint(&line)));
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
        self.lines = Some((columns, self.text.clone(), self.live, expanded, out.clone()));
        out
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
        assert!(
            strip_ansi(&lines[0]).ends_with("thinking…"),
            "live header: {lines:?}"
        );
        assert!(strip_ansi(&lines[1]).contains("second line"));
        assert!(strip_ansi(&lines[2]).contains("third line"));
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
        let body_width = 60usize.saturating_sub(4);
        let full: Vec<String> = body
            .split('\n')
            .flat_map(|segment| width::wrap_line(segment, body_width))
            .collect();
        let expected = &full[full.len() - PREVIEW_LINES..];
        for (line, want) in lines[1..1 + expected.len()].iter().zip(expected) {
            assert_eq!(strip_ansi(line), format!("  {want}"), "{lines:?}");
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
        assert_eq!(lines.len(), 4, "bullet row + 2 preview + hint: {lines:?}");
        assert!(strip_ansi(&lines[0]).starts_with("△ one"));
        assert!(
            strip_ansi(&lines[2]).contains("… (1 more lines, ctrl+o to expand)"),
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
        assert_eq!(lines.len(), 4, "3 rows + spacer: {lines:?}");
        assert!(strip_ansi(&lines[2]).contains("three"));
    }
}
