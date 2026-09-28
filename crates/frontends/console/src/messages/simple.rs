//! Transcript message components: user, assistant, and status lines.
//!
//! While the assistant is streaming, the message bullet is a white dot
//! blinking on a steady cadence; the finished message settles on the
//! steady white dot.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::theme::{self, Token};
use tui_engine::component::{Component, Segment};
use tui_engine::markdown::{Markdown, MarkdownStyle, SyntaxHighlighter};
use tui_engine::width::{self};
/// Two-space continuation indent for message bodies.
pub const MESSAGE_INDENT: &str = "  ";
/// The status bullet prefix (neutral system status, not a wave kind;
/// the glyph is [`crate::chrome::symbols::DONE`] plus a space).
pub const STATUS_BULLET: &str = "● ";
/// The user message bullet: the keyed-in chevron.
pub const USER_BULLET: &str = "❯ ";
/// The assistant message bullet: a white dot.
pub const ASSISTANT_BULLET: &str = "● ";
/// Half-period of the streaming dot's blink (on for one phase, off for
/// the next — a steady, regular pulse while the turn streams).
pub const STREAM_FRAME_INTERVAL: Duration = Duration::from_millis(530);

/// The streaming bullet for `elapsed`: the dot alternating with a
/// blank cell on a fixed cadence.
fn stream_bullet(elapsed: Duration) -> &'static str {
    let phase = (elapsed.as_millis() / STREAM_FRAME_INTERVAL.as_millis()) % 2;
    if phase == 0 { "●" } else { " " }
}

/// A user message: bullet + bold role-colored body. With markdown
/// enabled (the default) the body renders through the markdown engine
/// in the user role color; plain mode wraps the raw text instead.
pub struct UserMessage {
    text: String,
    markdown: Option<Markdown>,
    /// Waiting for this turn's first response byte (request phase).
    pending: bool,
    lines: Option<(usize, Segment)>,
}

/// Blend `color` toward white by `k` (0 = unchanged, 1 = white).
fn lighten(color: tui_engine::color::Color, k: f32) -> tui_engine::color::Color {
    let mix = |v: u8| (v as f32 + (255.0 - v as f32) * k).round() as u8;
    tui_engine::color::Color::rgb(mix(color.r), mix(color.g), mix(color.b))
}

impl UserMessage {
    /// A message from the submitted text. `markdown` selects the
    /// rendering mode (the editor never renders; only the transcript
    /// does).
    pub fn new(text: impl Into<String>, markdown: bool) -> Self {
        let text = text.into();
        let markdown = markdown.then(|| Self::markdown_renderer(false));
        Self {
            text,
            markdown,
            pending: false,
            lines: None,
        }
    }

    /// A markdown renderer dressed for user input: every token style
    /// carries the input background, so painted spans re-open the band
    /// after each internal reset (wrapping the rendered lines in one
    /// outer paint would lose the background at the first inner reset).
    /// `pending` lightens the role color until the turn's first byte.
    fn markdown_renderer(pending: bool) -> Markdown {
        let theme = theme::current();
        let bg = theme.palette().input_bg;
        let fg = if pending {
            lighten(theme.palette().role_user, 0.45)
        } else {
            theme.palette().role_user
        };
        let role = tui_engine::color::Style::new().fg(fg).bg(bg);
        MarkdownStyle {
            text: role,
            heading: role,
            code: theme.style(Token::CodeSpan).bg(bg),
            link: role.underline(),
            fence: role,
            quote: role.italic(),
            rule: role,
            diff_added: theme.style(Token::DiffAdded).bg(bg),
            diff_removed: theme.style(Token::DiffRemoved).bg(bg),
            diff_meta: theme.style(Token::DiffMeta).bg(bg),
        }
        .into_markdown(Box::new(tui_engine::markdown::PlainHighlighter))
    }

    /// Mark this turn's input as still waiting for the model's first
    /// response byte (the request phase): the text lightens until the
    /// first byte lands. The renderer is rebuilt so its baked styles
    /// pick up the lightened role color.
    pub fn set_pending(&mut self, pending: bool) {
        if self.pending != pending {
            self.pending = pending;
            if self.markdown.is_some() {
                self.markdown = Some(Self::markdown_renderer(pending));
            }
            self.lines = None;
        }
    }
}

impl Component for UserMessage {
    fn render(&mut self, columns: usize) -> Segment {
        if let Some((cached_width, lines)) = &self.lines
            && *cached_width == columns
        {
            return Arc::clone(lines);
        }
        let theme = theme::current();
        let body_width = columns.saturating_sub(width::width(USER_BULLET));
        let styled_body = self.markdown.is_some();
        let body: Vec<String> = match &mut self.markdown {
            // The rendered lines already carry fg + input bg (the
            // renderer's styles bake the band in), so they are used as
            //-is: an outer paint here would lose the background at the
            // first inner reset.
            Some(markdown) => markdown
                .render(&self.text, body_width)
                .iter()
                .cloned()
                .collect(),
            None => width::wrap_text(&self.text, body_width),
        };
        // The bullet band: role color over the input highlight,
        // lightened while the request phase runs.
        let palette = theme.palette();
        let fg = if self.pending {
            lighten(palette.role_user, 0.45)
        } else {
            palette.role_user
        };
        let style = tui_engine::color::Style::new()
            .fg(fg)
            .bg(palette.input_bg)
            .bold();
        let indent = style.paint(MESSAGE_INDENT);
        let mut out = Vec::new();
        for (index, line) in body.iter().enumerate() {
            let line = if styled_body {
                line.clone()
            } else {
                style.paint(line)
            };
            let mut row = if index == 0 {
                format!("{}{line}", style.paint(USER_BULLET))
            } else {
                format!("{indent}{line}")
            };
            // The highlight band stretches across the full terminal
            // width: pad with background-painted spaces to the edge.
            let pad = columns.saturating_sub(width::width(&row));
            if pad > 0 {
                row.push_str(&style.paint(&" ".repeat(pad)));
            }
            out.push(row);
        }
        out.push(String::new());
        let lines = Arc::new(out);
        self.lines = Some((columns, Arc::clone(&lines)));
        lines
    }

    fn invalidate(&mut self) {
        self.lines = None;
        // The markdown renderer bakes the palette AND the input
        // background into its styles at construction: rebuild it so the
        // next render carries the current theme (clearing the cache
        // alone would repaint with the old colors).
        if let Some(markdown) = &mut self.markdown {
            *markdown = Self::markdown_renderer(self.pending);
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// An assistant message rendered as markdown with a white dot bullet.
/// Built with [`AssistantMessage::streaming`] while the turn is live,
/// the dot blinks on a steady cadence until the cache is broken.
pub struct AssistantMessage {
    markdown: Markdown,
    text: String,
    live: bool,
    started: Instant,
    lines: Option<(usize, usize, usize, Segment)>,
}

impl AssistantMessage {
    /// A completed assistant message. `highlighter` plugs syntax
    /// coloring into code blocks.
    pub fn new(text: impl Into<String>, highlighter: Box<dyn SyntaxHighlighter>) -> Self {
        Self {
            markdown: Self::markdown(highlighter),
            text: text.into(),
            live: false,
            started: Instant::now(),
            lines: None,
        }
    }

    /// A live assistant message whose bullet animates while streaming.
    pub fn streaming(text: impl Into<String>, highlighter: Box<dyn SyntaxHighlighter>) -> Self {
        Self {
            live: true,
            ..Self::new(text, highlighter)
        }
    }

    /// Refresh the streamed text in place: the render clock keeps
    /// running so the bullet animation survives delta flushes, and the
    /// caches drop so the next render parses the new buffer.
    pub fn update_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.markdown.clear_cache();
        self.lines = None;
    }

    fn markdown(highlighter: Box<dyn SyntaxHighlighter>) -> Markdown {
        Self::markdown_styles().into_markdown(highlighter)
    }

    /// The assistant styles from the live theme. Rebuilt on theme
    /// switches: the renderer bakes colors in, so a stale style would
    /// keep painting old-theme text over the new background.
    fn markdown_styles() -> MarkdownStyle {
        let theme = theme::current();
        MarkdownStyle {
            // Agent speech renders in the neutral band: body prose
            // near-white, lines (frames, table borders, rules) in a
            // true near-white gray — color stays with the user input.
            text: theme.style(Token::Text),
            heading: theme.style(Token::TextStrong),
            code: theme.style(Token::TextStrong),
            link: theme.style(Token::TextDim).underline(),
            fence: theme.style(Token::Neutral),
            quote: theme.style(Token::TextDim).italic(),
            rule: theme.style(Token::Neutral),
            diff_added: theme.style(Token::DiffAdded),
            diff_removed: theme.style(Token::DiffRemoved),
            diff_meta: theme.style(Token::DiffMeta),
        }
    }

    /// Settle a live message into its final static form.
    pub fn settle(&mut self) {
        self.live = false;
        self.lines = None;
    }

    /// The message text this instance renders (streaming-sync checks).
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Component for AssistantMessage {
    fn render(&mut self, columns: usize) -> Segment {
        let frame = if self.live {
            (self.started.elapsed().as_millis() / STREAM_FRAME_INTERVAL.as_millis()) as usize
        } else {
            0
        };
        if let Some((cached_columns, cached_frame, cached_width, lines)) = &self.lines
            && *cached_columns == columns
            && *cached_frame == frame
            && *cached_width == width::width(&self.text)
        {
            return Arc::clone(lines);
        }
        let theme = theme::current();
        let body_width = columns.saturating_sub(width::width(ASSISTANT_BULLET));
        // Live: the white dot blinking on a steady cadence. Final: the
        // steady dot — both occupy the same two columns, text never
        // reflows. The bullet rides the strong white band.
        let bullet = if self.live {
            format!("{} ", stream_bullet(self.started.elapsed()))
        } else {
            ASSISTANT_BULLET.to_string()
        };
        let mut out = Vec::new();
        for (index, line) in self
            .markdown
            .render(&self.text, body_width)
            .iter()
            .enumerate()
        {
            if index == 0 {
                out.push(format!("{}{line}", theme.paint(Token::TextStrong, &bullet)));
            } else {
                out.push(format!("{MESSAGE_INDENT}{line}"));
            }
        }
        out.push(String::new());
        let lines = Arc::new(out);
        self.lines = Some((columns, frame, width::width(&self.text), Arc::clone(&lines)));
        lines
    }

    fn invalidate(&mut self) {
        self.lines = None;
        // The renderer bakes the theme into its styles at construction:
        // restyle in place so the next render repaints under the new
        // theme instead of the one this message was built with. The
        // highlighter bakes the syntax theme the same way — without a
        // rebuild, dark-era code blocks stay near-invisible on the
        // light paper.
        self.markdown.set_style(Self::markdown_styles());
        self.markdown
            .set_highlighter(crate::highlight::highlighter());
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Severity of a status line: picks the bullet and text color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Neutral system note (dim).
    Info,
    /// Harness warning or loop notice (amber).
    Warning,
    /// Hard failure (red).
    Error,
}

/// A status line: `● text` tinted by severity — dim for neutral notes,
/// amber for warnings and loop notices, red for errors.
#[derive(Debug, Clone)]
pub struct StatusLine {
    text: String,
    severity: Severity,
}

impl StatusLine {
    /// A neutral status note.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Info,
        }
    }

    /// A warning status note (harness warnings, loop notices).
    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Warning,
        }
    }

    /// An error status note.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Error,
        }
    }
}

impl Component for StatusLine {
    fn render(&mut self, columns: usize) -> Segment {
        let theme = theme::current();
        let token = match self.severity {
            Severity::Info => Token::TextDim,
            Severity::Warning => Token::Warning,
            Severity::Error => Token::Error,
        };
        let body_width =
            columns.saturating_sub(width::width(STATUS_BULLET) + width::width(MESSAGE_INDENT));
        let mut out = Vec::new();
        for (index, line) in width::wrap_text(&self.text, body_width)
            .into_iter()
            .enumerate()
        {
            if index == 0 {
                out.push(format!(
                    "{MESSAGE_INDENT}{}{}",
                    theme.paint(token, STATUS_BULLET),
                    theme.paint(token, &line)
                ));
            } else {
                out.push(format!(
                    "{}{}{}",
                    MESSAGE_INDENT,
                    MESSAGE_INDENT,
                    theme.paint(token, &line)
                ));
            }
        }
        out.push(String::new());
        Arc::new(out)
    }

    fn invalidate(&mut self) {}

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
    fn user_message_has_bullet_and_wraps() {
        theme::set(theme::Theme::dark());
        let mut message = UserMessage::new("hello world", false);
        let lines = message.render(20);
        assert!(strip_ansi(&lines[0]).starts_with("❯ hello"));
        assert!(lines.last().unwrap().is_empty(), "trailing spacer");
    }

    /// The input background survives the markdown renderer's internal
    /// resets: the renderer's styles bake the band in, so every segment
    /// between resets re-opens it instead of dropping to the terminal
    /// default (an outer paint would be cut short at the first inner
    /// reset).
    #[test]
    fn user_input_background_survives_inner_resets() {
        theme::set(theme::Theme::dark());
        let mut message = UserMessage::new("run `cargo test` now", true);
        let lines = message.render(60);
        let line = &lines[0];
        let band = "48;2;37;42;50";
        assert!(line.contains(band), "band opened: {line:?}");
        for (index, part) in line.split("\x1b[0m").enumerate() {
            if !part.is_empty() {
                assert!(
                    // fg and bg share one SGR sequence; the band params
                    // must ride along in whatever opens the segment.
                    part.contains(band),
                    "background lost after reset {index}: {line:?}"
                );
            }
        }
        // The styled body is used as-is: no outer fg/bg paint wraps it
        // (that wrap is what cut the band short).
        assert!(
            !line.contains("\x1b[0m\x1b[0m"),
            "no double reset from nested paint: {line:?}"
        );
    }

    /// The highlight band stretches across the full terminal width:
    /// every row (wrapped rows included) pads to `columns` with
    /// band-painted spaces, so no default-background gap shows at the
    /// right edge.
    #[test]
    fn user_band_spans_the_full_terminal_width() {
        theme::set(theme::Theme::dark());
        let long = "a wrapped input line that exceeds the width ".repeat(3);
        let mut message = UserMessage::new(long, true);
        for columns in [30, 61] {
            let lines = message.render(columns);
            let body = &lines[..lines.len() - 1]; // drop the trailing spacer
            assert!(body.len() >= 2, "wrapped at {columns}: {lines:?}");
            for line in body {
                assert_eq!(
                    width::width(&strip_ansi(line)),
                    columns,
                    "full-width row at {columns}: {line:?}"
                );
            }
        }
    }

    /// The pending phase lightens the role color (renderer rebuilt) and
    /// the settled phase restores it.
    #[test]
    fn pending_lightens_then_restores_the_role_color() {
        theme::set(theme::Theme::dark());
        let mut message = UserMessage::new("hello", true);
        message.set_pending(true);
        let lines = message.render(60);
        assert!(
            lines[0].contains("38;2;157;206;255"),
            "lightened while pending: {:?}",
            lines[0]
        );
        message.set_pending(false);
        let lines = message.render(60);
        assert!(
            lines[0].contains("38;2;77;165;255"),
            "role color restored: {:?}",
            lines[0]
        );
    }

    /// A message rendered under one theme repaints under the new theme
    /// after invalidate: the markdown renderer is restyled, not just the
    /// line cache — dark near-white body text must not survive a switch
    /// onto the light paper (and light ink onto a dark ground).
    #[test]
    fn theme_switch_repaints_assistant_markdown() {
        theme::set(theme::Theme::dark());
        let mut message = AssistantMessage::new(
            "hello world",
            Box::new(tui_engine::markdown::PlainHighlighter),
        );
        let lines = message.render(60);
        assert!(
            lines[0].contains("\x1b[38;2;222;226;231m"),
            "dark body: {:?}",
            lines[0]
        );
        theme::set(theme::Theme::light());
        message.invalidate();
        let lines = message.render(60);
        assert!(
            lines[0].contains("\x1b[38;2;31;35;40m"),
            "light body: {:?}",
            lines[0]
        );
        assert!(
            !lines[0].contains("\x1b[38;2;222;226;231m"),
            "dark body gone: {:?}",
            lines[0]
        );
    }

    /// A theme switch repaints the user band under the new palette:
    /// invalidate rebuilds the renderer instead of only clearing its
    /// cache (the baked styles would keep the old colors). The order
    /// mirrors apply_theme: the palette is installed first, then the
    /// transcript invalidates.
    #[test]
    fn theme_switch_repaints_the_user_band() {
        theme::set(theme::Theme::dark());
        let mut message = UserMessage::new("hello", true);
        message.render(60);
        theme::set(theme::Theme::light());
        message.invalidate();
        let lines = message.render(60);
        // Light input_bg #DCE1E7; the dark band must be gone.
        assert!(
            lines[0].contains("48;2;220;225;231"),
            "new band: {:?}",
            lines[0]
        );
        assert!(
            !lines[0].contains("48;2;37;42;50"),
            "old band gone: {:?}",
            lines[0]
        );
        // The role color follows the theme too (light role_user).
        assert!(
            lines[0].contains("38;2;9;105;218"),
            "light role color: {:?}",
            lines[0]
        );
    }

    /// The assistant bullet carries exactly one space before the body.
    #[test]
    fn assistant_bullet_has_single_space() {
        theme::set(theme::Theme::dark());
        let mut message = AssistantMessage::new(
            "hello world",
            Box::new(tui_engine::markdown::PlainHighlighter),
        );
        let lines = message.render(60);
        assert!(strip_ansi(&lines[0]).starts_with("● hello world"));
    }

    #[test]
    fn status_line_uses_bullet() {
        theme::set(theme::Theme::dark());
        let mut line = StatusLine::new("compacting");
        let lines = line.render(60);
        assert_eq!(strip_ansi(&lines[0]), "  ● compacting");
    }

    #[test]
    fn error_status_colors_text() {
        theme::set(theme::Theme::dark());
        let mut line = StatusLine::error("boom");
        let lines = line.render(60);
        assert!(lines[0].contains("\x1b[38;2;248;81;73m"), "{:?}", lines[0]);
    }

    /// Warnings and loop notices ride the amber Warning role, not the
    /// dim body tone.
    #[test]
    fn warning_status_lines_render_amber() {
        theme::set(theme::Theme::dark());
        let mut line = StatusLine::warning("warning: tool round limit reached (256)");
        let lines = line.render(60);
        assert!(
            lines[0].contains("\x1b[38;2;210;153;34m"),
            "amber warning: {:?}",
            lines[0]
        );
        // Neutral notes stay dim.
        let mut line = StatusLine::new("compacting");
        let lines = line.render(60);
        assert!(
            !lines[0].contains("\x1b[38;2;224;175;104m"),
            "{:?}",
            lines[0]
        );
    }

    /// Synthwave: inline code spans read as code (bright white), not
    /// the dim body tone.
    #[test]
    fn assistant_inline_code_uses_the_bright_code_span() {
        theme::set(theme::Theme::dark());
        let mut message = AssistantMessage::new(
            "run `cargo test` now",
            Box::new(tui_engine::markdown::PlainHighlighter),
        );
        let lines = message.render(60);
        assert!(
            lines[0].contains("\x1b[38;2;255;255;255m"),
            "code span in bright white: {:?}",
            lines[0]
        );
    }

    /// Synthwave: agent headings render bold (the pseudo-heading is
    /// white and bold; SGR 1 follows the text). Color stays with the
    /// user input.
    #[test]
    fn assistant_headings_ride_the_strong_white() {
        theme::set(theme::Theme::dark());
        let mut message = AssistantMessage::new(
            "## 执行环境",
            Box::new(tui_engine::markdown::PlainHighlighter),
        );
        let lines = message.render(60);
        assert!(
            lines[0].contains("\x1b[38;2;255;255;255;1m执行环境"),
            "heading painted bold strong-white: {:?}",
            lines[0]
        );
    }

    /// Deepwave keeps its neutral identity when selected.
    #[test]
    fn deepwave_keeps_strong_headings_and_teal_code_spans() {
        theme::set(theme::Theme::deepwave());
        let mut message = AssistantMessage::new(
            "## 执行环境\nrun `cargo test`",
            Box::new(tui_engine::markdown::PlainHighlighter),
        );
        let lines = message.render(60);
        assert!(
            lines[0].contains("\x1b[38;2;255;255;255;1m执行环境"),
            "heading painted bold strong-white: {:?}",
            lines[0]
        );
        assert!(
            lines[2].contains("cargo test"),
            "code span text intact: {:?}",
            lines[2]
        );
    }

    /// A live draft's bullet keeps breathing across text updates: the
    /// render clock must survive `update_text` (the streamed-frames
    /// regression where every flush reset it to the first frame).
    #[test]
    fn streamed_draft_bullet_keeps_animating_across_updates() {
        theme::set(theme::Theme::dark());
        // The blink itself: dot on, then blank, on a fixed cadence
        // (pure function of elapsed — no timing races).
        assert_eq!(stream_bullet(Duration::ZERO), "●");
        assert_eq!(stream_bullet(STREAM_FRAME_INTERVAL * 3 / 2), " ");
        let mut message =
            AssistantMessage::streaming("first", Box::new(tui_engine::markdown::PlainHighlighter));
        let lines = message.render(60);
        assert!(
            strip_ansi(&lines[0]).starts_with("● "),
            "dot visible in the first blink phase: {:?}",
            lines[0]
        );
        message.update_text("first second");
        let lines = message.render(60);
        assert!(strip_ansi(&lines[0]).starts_with("● "), "{lines:?}");
        // Settling renders the static dot, exactly one space before the
        // body.
        message.settle();
        let lines = message.render(60);
        assert!(strip_ansi(&lines[0]).starts_with("● first second"));
    }
}
